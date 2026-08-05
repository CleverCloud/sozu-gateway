//! Gateway API (`gateway.networking.k8s.io`) → IR mapping (Phase 2).
//!
//! Gateway API objects compile into the **same** IR as Ingress, reusing the
//! shared cluster/backend resolver, so both APIs converge on one Sōzu state.
//!
//! Scope (anything else is reported as a [`Problem`] and skipped, so a feature
//! gap never silently mis-routes):
//!  - `GatewayClass` selected by `controllerName`;
//!  - `Gateway` HTTP/HTTPS listeners mapped to the static `:80`/`:443` listeners
//!    by protocol (`listener.port` must match the *advertised* gateway port for
//!    the protocol — the Service-exposed port, not the pod bind); HTTPS loads
//!    its `certificateRefs` (Terminate only);
//!  - `HTTPRoute` attached by `parentRef` (optional `sectionName`), with path
//!    (`PathPrefix`/`Exact`/`RegularExpression`) and method matches, and either
//!    one Service `backendRef` or a redirect-only rule (no backend);
//!  - filters (Phase 3): RequestHeaderModifier / ResponseHeaderModifier,
//!    RequestRedirect (scheme + status);
//!  - cross-namespace `backendRefs`/`certificateRefs` honour `ReferenceGrant`.
//!
//! Not yet: header/query matches, weighted multi-backend split (incl. a
//! single weight-0 drain), rule timeouts, per-backendRef filters, TLS
//! Passthrough, RequestMirror, redirect host/path/port, and URLRewrite.
//! Header/query match and weighted split are Sōzu hard limits; the last two are
//! merely unwired — both were measured working on Sōzu 2.2.0 (PROTOCOL.md §13),
//! with two conditions any wiring owes first: a literal `$` in a rewrite value
//! makes Sōzu reject the frontend outright (and translation is all-or-nothing),
//! and a path rewrite drops the query string that `ReplaceFullPath` keeps.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use serde::Serialize;
use sozu_gw_gateway_api::gateway::{
    GatewayListenersAllowedRoutesNamespacesFrom as ApiAllowedFrom, GatewayListenersTlsMode,
};
use sozu_gw_gateway_api::httproute::{
    HttpRouteRulesFilters, HttpRouteRulesFiltersRequestRedirectScheme, HttpRouteRulesFiltersType,
    HttpRouteRulesMatchesMethod, HttpRouteRulesMatchesPath, HttpRouteRulesMatchesPathType,
};
use sozu_gw_gateway_api::{TcpRoute, UdpRoute};
use sozu_gw_ir as ir;

use crate::{
    add_service_route, extract_cert, meta_nn, BuildConfig, ExposedProtocol, FingerprintedCert,
    FrontendSource, Index, Inputs, PortRef, Problem, SourcedFrontend,
};

const GW_GROUP: &str = "gateway.networking.k8s.io";

/// Acceptance of one of our `GatewayClass`es.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatewayClassResult {
    pub name: String,
    pub accepted: bool,
}

/// Status of one `Gateway` we own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatewayResult {
    pub namespace: String,
    pub name: String,
    /// `metadata.uid` of the source Gateway (see `IngressResult::uid`).
    pub uid: Option<String>,
    pub accepted: bool,
    pub programmed: bool,
    pub problems: Vec<Problem>,
    /// Per-listener status (one entry per declared listener, in spec order).
    pub listeners: Vec<ListenerStatus>,
}

/// Status of one listener, written to `Gateway.status.listeners[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListenerStatus {
    pub name: String,
    /// Route kinds this listener admits (e.g. `["HTTPRoute"]`); empty if none.
    pub supported_kinds: Vec<String>,
    /// Number of routes attached to this listener.
    pub attached_routes: i32,
    pub accepted: bool,
    pub accepted_reason: &'static str,
    pub programmed: bool,
    pub programmed_reason: &'static str,
    pub resolved_refs: bool,
    pub resolved_refs_reason: &'static str,
}

/// Status of one `HTTPRoute` for a single parentRef. The parentRef's
/// `sectionName`/`port` are part of its identity — a route may carry several
/// parentRefs to the same Gateway, each with its own result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteParentResult {
    pub gateway_namespace: String,
    pub gateway_name: String,
    pub section_name: Option<String>,
    pub port: Option<i32>,
    pub accepted: bool,
    /// Gateway API `Accepted` condition reason (e.g. `Accepted`, `NoMatchingParent`).
    pub accepted_reason: &'static str,
    pub resolved_refs: bool,
    /// Gateway API `ResolvedRefs` condition reason (e.g. `ResolvedRefs`,
    /// `BackendNotFound`, `InvalidKind`, `RefNotPermitted`).
    pub resolved_refs_reason: &'static str,
    pub problems: Vec<Problem>,
}

/// Which Gateway API route kind a [`RouteResult`] describes.
///
/// Status conditions and Events are written back onto the *object*, so the
/// kind has to travel with the result: the controller reads it to pick the
/// `Api<K>` to patch and the Event's `involvedObject.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RouteKind {
    HttpRoute,
    TcpRoute,
    UdpRoute,
}

impl RouteKind {
    /// The kind's Kubernetes spelling (`kind:` in the manifest).
    pub fn as_str(self) -> &'static str {
        match self {
            RouteKind::HttpRoute => "HTTPRoute",
            RouteKind::TcpRoute => "TCPRoute",
            RouteKind::UdpRoute => "UDPRoute",
        }
    }
}

/// Status of one route object across all of its parents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteResult {
    pub kind: RouteKind,
    pub namespace: String,
    pub name: String,
    /// `metadata.uid` of the source route (see `IngressResult::uid`).
    pub uid: Option<String>,
    pub parents: Vec<RouteParentResult>,
}

pub(crate) struct GatewayBuildResults {
    pub classes: Vec<GatewayClassResult>,
    pub gateways: Vec<GatewayResult>,
    pub routes: Vec<RouteResult>,
    /// Layer-4 frontends from TCPRoute/UDPRoute, port conflicts already settled.
    pub l4_frontends: Vec<ir::L4Frontend>,
}

/// A listener protocol this controller serves, and the one route kind that
/// binds to it.
///
/// `TLS` is deliberately absent: SNI-routed passthrough is a separate piece of
/// work, and a listener protocol we half-understand is exactly the kind of
/// silent approximation the honesty rule forbids. It falls through to
/// [`Problem::UnsupportedProtocol`] with everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListenerProtocol {
    Http,
    Https,
    Tcp,
    Udp,
}

impl ListenerProtocol {
    fn parse(protocol: &str) -> Option<Self> {
        match protocol {
            "HTTP" => Some(ListenerProtocol::Http),
            "HTTPS" => Some(ListenerProtocol::Https),
            "TCP" => Some(ListenerProtocol::Tcp),
            "UDP" => Some(ListenerProtocol::Udp),
            _ => None,
        }
    }

    /// The exposure-table protocol a listener of this kind attaches to.
    fn exposed(self) -> ExposedProtocol {
        match self {
            ListenerProtocol::Http => ExposedProtocol::Http,
            ListenerProtocol::Https => ExposedProtocol::Https,
            ListenerProtocol::Tcp => ExposedProtocol::Tcp,
            ListenerProtocol::Udp => ExposedProtocol::Udp,
        }
    }

    /// The single route kind this listener admits.
    fn route_kind(self) -> RouteKind {
        match self {
            ListenerProtocol::Http | ListenerProtocol::Https => RouteKind::HttpRoute,
            ListenerProtocol::Tcp => RouteKind::TcpRoute,
            ListenerProtocol::Udp => RouteKind::UdpRoute,
        }
    }

    /// The layer-4 transport, for the protocols that have one. `None` for
    /// HTTP/HTTPS, whose listeners are static and never enter the IR.
    fn l4(self) -> Option<ir::L4Protocol> {
        match self {
            ListenerProtocol::Tcp => Some(ir::L4Protocol::Tcp),
            ListenerProtocol::Udp => Some(ir::L4Protocol::Udp),
            ListenerProtocol::Http | ListenerProtocol::Https => None,
        }
    }
}

/// A listener we accepted on one of our Gateways.
struct ListenerInfo {
    name: String,
    hostname: Option<String>,
    /// `None` for a protocol we do not serve (the listener is then not routable).
    protocol: Option<ListenerProtocol>,
    /// The listener's declared port, matched against a parentRef's optional port.
    port: i32,
    /// Where Sōzu listens for this listener: the bind of the exposure entry
    /// serving its `(protocol, port)`. Resolved once, here, rather than looked
    /// up per route — the table may expose several ports for one protocol, and
    /// picking the first would land a listener's routes on a port its Gateway
    /// never declared.
    bind: Option<SocketAddr>,
    /// Which namespaces this listener admits routes from (`allowedRoutes.namespaces`).
    allow_from: AllowedFrom,
    /// Can a route bind here at all? Routes attach to a routable listener for
    /// status/counting even when it is not `programmed`.
    routable: bool,
    /// Successfully programmed into Sōzu (HTTP, HTTPS with a loaded cert, or a
    /// layer-4 listener whose port the gateway exposes). Frontends are only
    /// emitted for programmed listeners.
    programmed: bool,
    programmed_reason: &'static str,
    accepted: bool,
    accepted_reason: &'static str,
    resolved_refs: bool,
    resolved_refs_reason: &'static str,
    /// Route kinds this listener admits (its own kind, or a filtered subset).
    supported_kinds: Vec<String>,
}

impl ListenerInfo {
    /// Does this listener admit `kind`? A listener serves exactly one route
    /// kind, minus whatever `allowedRoutes.kinds` filtered out.
    fn admits_kind(&self, kind: RouteKind) -> bool {
        self.supported_kinds.iter().any(|k| k == kind.as_str())
    }
}

/// Does an `allowedRoutes.kinds` entry name the kind this listener serves?
fn is_wanted_kind(
    k: &sozu_gw_gateway_api::gateway::GatewayListenersAllowedRoutesKinds,
    wanted: RouteKind,
) -> bool {
    k.group.as_deref().unwrap_or(GW_GROUP) == GW_GROUP && k.kind == wanted.as_str()
}

/// The route kinds a listener admits, and whether every requested kind is
/// supported. `allowedRoutes.kinds` unset → the listener protocol's own kind; a
/// requested kind it does not serve → dropped from the set and flagged
/// (→ `InvalidRouteKinds`).
fn listener_supported_kinds(
    l: &sozu_gw_gateway_api::gateway::GatewayListeners,
    wanted: RouteKind,
) -> (Vec<String>, bool) {
    match l.allowed_routes.as_ref().and_then(|ar| ar.kinds.as_ref()) {
        Some(kinds) if !kinds.is_empty() => {
            let supported = kinds.iter().any(|k| is_wanted_kind(k, wanted));
            let all_ok = kinds.iter().all(|k| is_wanted_kind(k, wanted));
            let set = if supported {
                vec![wanted.as_str().to_string()]
            } else {
                vec![]
            };
            (set, all_ok)
        }
        _ => (vec![wanted.as_str().to_string()], true),
    }
}

/// Build the status of one declared listener (whether or not we can program it).
fn build_listener(
    cfg: &BuildConfig,
    inputs: &Inputs,
    index: &Index,
    gw_ns: &str,
    l: &sozu_gw_gateway_api::gateway::GatewayListeners,
    certificates: &mut Vec<FingerprintedCert>,
    problems: &mut Vec<Problem>,
) -> ListenerInfo {
    let protocol = ListenerProtocol::parse(&l.protocol);
    let routable = protocol.is_some();
    let mut info = ListenerInfo {
        name: l.name.clone(),
        hostname: l.hostname.clone(),
        protocol,
        port: l.port,
        bind: None,
        allow_from: AllowedFrom::of(l),
        routable,
        programmed: false,
        programmed_reason: "Programmed",
        accepted: true,
        accepted_reason: "Accepted",
        resolved_refs: true,
        resolved_refs_reason: "ResolvedRefs",
        supported_kinds: vec![],
    };

    // An unevaluable namespace selector fails closed exactly like the other
    // fail-closed listener paths: the listener admits no routes (see
    // `AllowedFrom::admits`), must not read cleanly Programmed, and — like
    // the port-mismatch path — loads none of its certificates into Sōzu
    // (material for a listener that serves nothing has no business there).
    let selector_unsupported = routable && matches!(info.allow_from, AllowedFrom::Selector);
    if selector_unsupported {
        info.programmed = false;
        info.programmed_reason = "Invalid";
        problems.push(Problem::NamespaceSelectorUnsupported {
            listener: l.name.clone(),
        });
    }

    let Some(protocol) = protocol else {
        info.accepted = false;
        info.accepted_reason = "UnsupportedProtocol";
        info.programmed = false;
        info.programmed_reason = "Invalid";
        problems.push(Problem::UnsupportedProtocol {
            protocol: l.protocol.clone(),
        });
        return info;
    };

    // `listener.port` declares the externally advertised port — what clients
    // connect to on the LoadBalancer Service — NOT the pod-level bind (under
    // the chart defaults the Service maps 80 → 8080 / 443 → 8443, so
    // comparing against the bind would reject every standard port-80/443
    // Gateway). The gateway only serves the ports its exposure table carries;
    // a mismatch fails closed: programming its routes anyway would silently
    // serve them on a port the Gateway never declared.
    //
    // The set can hold several entries per protocol, so this is a membership
    // test — and the *matched* entry is what the listener binds to, not the
    // first entry of that protocol.
    let wanted = protocol.exposed();
    let entry = u16::try_from(l.port)
        .ok()
        .and_then(|p| cfg.exposed(wanted, p));
    let Some(entry) = entry else {
        info.accepted = false;
        info.accepted_reason = "PortUnavailable";
        info.programmed = false;
        info.programmed_reason = "Invalid";
        problems.push(Problem::PortNotExposed {
            listener: l.name.clone(),
            declared: l.port,
            protocol: l.protocol.clone(),
            exposed: cfg.advertised_ports(wanted),
        });
        return listener_kinds(info, l, protocol);
    };
    info.bind = Some(BuildConfig::bind_addr(entry.bind));

    // A layer-4 port may be reserved for one namespace. There is no hostname to
    // arbitrate with down here — the port carries exactly one route — so the
    // table names who may claim it, and a Gateway from elsewhere is refused
    // rather than allowed to race for it.
    if protocol.l4().is_some() {
        if let Some(owner) = entry.owner.as_deref().filter(|o| *o != gw_ns) {
            info.accepted = false;
            info.accepted_reason = "PortUnavailable";
            info.programmed = false;
            info.programmed_reason = "Invalid";
            problems.push(Problem::ListenerPortNotOwned {
                listener: l.name.clone(),
                port: entry.port,
                owner: owner.to_string(),
                claimed_by: gw_ns.to_string(),
            });
            return listener_kinds(info, l, protocol);
        }
    }

    if !selector_unsupported {
        match protocol {
            ListenerProtocol::Http => info.programmed = true,
            ListenerProtocol::Https => {
                let (loaded, reason) = load_listener_certs(
                    inputs,
                    index,
                    gw_ns,
                    l,
                    info.bind.expect("bind resolved above"),
                    certificates,
                    problems,
                );
                if loaded {
                    info.programmed = true;
                } else {
                    info.programmed = false;
                    info.programmed_reason = "Invalid";
                    info.resolved_refs = false;
                    info.resolved_refs_reason = reason;
                }
            }
            // A layer-4 listener needs nothing loaded: the exposure entry
            // reserves the socket for it, and Sōzu binds that socket the moment
            // a route attaches (the L4 listener is created from the IR, unlike
            // the static HTTP/HTTPS ones). Programmed therefore reports what it
            // is meant to — the data plane is configured up to the point where
            // it can serve traffic — and a listener with no routes yet is in
            // exactly the same state as an HTTP one with no routes yet.
            ListenerProtocol::Tcp | ListenerProtocol::Udp => info.programmed = true,
        }
    }

    listener_kinds(info, l, protocol)
}

/// Fill in the listener's admitted route kinds (and flag a requested kind it
/// cannot serve). Applied on every return path, including the refusals: a
/// listener that admits no routes still has to say which kind it *would* have.
fn listener_kinds(
    mut info: ListenerInfo,
    l: &sozu_gw_gateway_api::gateway::GatewayListeners,
    protocol: ListenerProtocol,
) -> ListenerInfo {
    let (kinds, all_ok) = listener_supported_kinds(l, protocol.route_kind());
    info.supported_kinds = kinds;
    if !all_ok {
        info.resolved_refs = false;
        info.resolved_refs_reason = "InvalidRouteKinds";
    }
    info
}

/// A listener's `allowedRoutes.namespaces.from` policy. `Selector` is
/// unsupported — there is no Namespace label index to evaluate it against — so
/// it fails CLOSED: the listener admits no routes at all and the gap is
/// reported ([`Problem::NamespaceSelectorUnsupported`]). Treating it as
/// permissive would silently admit every namespace on a control the Gateway
/// owner set precisely to restrict admission.
#[derive(Clone, Copy)]
enum AllowedFrom {
    Same,
    All,
    Selector,
}

impl AllowedFrom {
    fn of(l: &sozu_gw_gateway_api::gateway::GatewayListeners) -> Self {
        match l
            .allowed_routes
            .as_ref()
            .and_then(|ar| ar.namespaces.as_ref())
            .and_then(|ns| ns.from.as_ref())
        {
            Some(ApiAllowedFrom::All) => AllowedFrom::All,
            Some(ApiAllowedFrom::Selector) => AllowedFrom::Selector,
            _ => AllowedFrom::Same, // unset defaults to Same
        }
    }

    /// Does this listener admit a route from `route_ns` (gateway in `gw_ns`)?
    fn admits(self, route_ns: &str, gw_ns: &str) -> bool {
        match self {
            AllowedFrom::All => true,
            AllowedFrom::Same => route_ns == gw_ns,
            // Unsupported means unsupported: an unevaluable selector admits
            // nothing — not even the Gateway's own namespace (`from: Selector`
            // replaces `Same`, it does not extend it).
            AllowedFrom::Selector => false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_gateway(
    cfg: &BuildConfig,
    inputs: &Inputs,
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    referenced: &mut BTreeSet<String>,
    frontends: &mut Vec<SourcedFrontend>,
    certificates: &mut Vec<FingerprintedCert>,
) -> GatewayBuildResults {
    // 1. GatewayClasses we own (controllerName matches).
    let mut classes = Vec::new();
    let mut our_classes: BTreeSet<String> = BTreeSet::new();
    for gc in &inputs.gateway_classes {
        let Some(name) = gc.metadata.name.clone() else {
            continue;
        };
        let accepted = gc.spec.controller_name == cfg.controller_name;
        if accepted {
            our_classes.insert(name.clone());
        }
        classes.push(GatewayClassResult { name, accepted });
    }

    // 2. Gateways of our class -> accepted listeners + loaded certificates.
    let mut gateways = Vec::new();
    let mut gw_listeners: BTreeMap<(String, String), Vec<ListenerInfo>> = BTreeMap::new();
    for gw in &inputs.gateways {
        if !our_classes.contains(&gw.spec.gateway_class_name) {
            continue;
        }
        let (ns, name) = meta_nn(&gw.metadata.namespace, &gw.metadata.name);
        let mut problems = Vec::new();
        // Gateway API v1.6.1 added these to the Gateway spec. Neither is
        // honoured, and both are reported rather than dropped: `tls` carries
        // client-certificate validation (a security control Sōzu's proto has no
        // field for at all), and `allowedListeners` decides which namespaces may
        // contribute listeners — a permission, not a preference.
        if gw.spec.tls.is_some() {
            problems.push(Problem::GatewaySpecUnsupported { field: "spec.tls" });
        }
        if gw.spec.allowed_listeners.is_some() {
            problems.push(Problem::GatewaySpecUnsupported {
                field: "spec.allowedListeners",
            });
        }
        // Every declared listener gets a status entry, even unsupported / cert-less
        // ones (a route can still attach to them; they just aren't programmed).
        let listeners: Vec<ListenerInfo> = gw
            .spec
            .listeners
            .iter()
            .map(|l| build_listener(cfg, inputs, index, &ns, l, certificates, &mut problems))
            .collect();

        let programmed = listeners.iter().any(|l| l.programmed);
        gw_listeners.insert((ns.clone(), name.clone()), listeners);
        gateways.push(GatewayResult {
            namespace: ns,
            name,
            uid: crate::obj_uid(&gw.metadata),
            accepted: true,
            programmed,
            problems,
            // Filled after route attachment, once attachedRoutes is known.
            listeners: Vec::new(),
        });
    }

    // 3. HTTPRoutes attached to our Gateways. `attached` counts, per listener,
    // the routes bound to it (`Gateway.status.listeners[].attachedRoutes`).
    let mut routes = Vec::new();
    let mut attached: BTreeMap<(String, String, String), i32> = BTreeMap::new();
    for route in &inputs.http_routes {
        let (rns, rname) = meta_nn(&route.metadata.namespace, &route.metadata.name);
        let mut parents = Vec::new();

        for pref in route.spec.parent_refs.iter().flatten() {
            let is_gateway = pref.group.as_deref().unwrap_or(GW_GROUP) == GW_GROUP
                && pref.kind.as_deref().unwrap_or("Gateway") == "Gateway";
            if !is_gateway {
                continue;
            }
            let gw_ns = pref.namespace.clone().unwrap_or_else(|| rns.clone());
            let Some(listeners) = gw_listeners.get(&(gw_ns.clone(), pref.name.clone())) else {
                continue; // not one of our Gateways
            };
            // Listeners the parentRef addresses (by sectionName + port), then
            // narrowed to those that admit the route's namespace.
            let addressable: Vec<&ListenerInfo> = listeners
                .iter()
                .filter(|l| l.routable)
                .filter(|l| pref.section_name.as_ref().is_none_or(|sn| sn == &l.name))
                .filter(|l| pref.port.is_none_or(|p| p == l.port))
                .collect();
            // A non-accepted listener (port-mismatched) can never serve the
            // route, so it is no more of a binding target than one that
            // rejects the namespace: excluding it here keeps the route from
            // reading healthy — and from counting toward attachedRoutes —
            // on a listener that will not carry its traffic.
            let candidates: Vec<&ListenerInfo> = addressable
                .iter()
                .copied()
                .filter(|l| l.accepted)
                // A listener serves exactly one route kind, minus whatever
                // `allowedRoutes.kinds` filtered out. Failing here rather than
                // in `addressable` is what makes the reason right: the Gateway
                // API spells a kind refusal `NotAllowedByListeners`, and
                // `NoMatchingParent` means the parentRef addressed nothing.
                .filter(|l| l.admits_kind(RouteKind::HttpRoute))
                .filter(|l| l.allow_from.admits(&rns, &gw_ns))
                .collect();

            let mut problems = Vec::new();
            let mut resolved_refs = true;
            let mut resolved_refs_reason = "ResolvedRefs";
            // No addressable listener -> NoMatchingParent; addressable but none
            // admits this namespace -> NotAllowedByListeners.
            let (accepted, accepted_reason) = if addressable.is_empty() {
                (false, "NoMatchingParent")
            } else if candidates.is_empty() {
                (false, "NotAllowedByListeners")
            } else {
                // Attribute this rule's frontends to the (route, parent) pair
                // so a route-key collision can be reported on its result.
                let source = FrontendSource::HttpRoute {
                    namespace: rns.clone(),
                    name: rname.clone(),
                    gateway_namespace: gw_ns.clone(),
                    gateway_name: pref.name.clone(),
                    section_name: pref.section_name.clone(),
                    port: pref.port,
                };
                let mut accepted_override: Option<&'static str> = None;
                for rule in route.spec.rules.iter().flatten() {
                    attach_rule(
                        inputs,
                        index,
                        clusters,
                        backends,
                        referenced,
                        frontends,
                        &rns,
                        route.spec.hostnames.as_deref(),
                        &candidates,
                        rule,
                        &source,
                        &mut problems,
                        &mut resolved_refs,
                        &mut resolved_refs_reason,
                        &mut accepted_override,
                    );
                }
                // A rule carrying a filter we cannot honour is skipped, and the
                // parent stops reading Accepted so the gap is visible in
                // `kubectl get httproute` and not only in the problem list.
                match accepted_override {
                    Some(reason) => (false, reason),
                    None => (true, "Accepted"),
                }
            };

            // An accepted route binds to each candidate listener (programmed or
            // not) — count it toward that listener's attachedRoutes.
            if accepted {
                for c in &candidates {
                    *attached
                        .entry((gw_ns.clone(), pref.name.clone(), c.name.clone()))
                        .or_insert(0) += 1;
                }
            }

            parents.push(RouteParentResult {
                gateway_namespace: gw_ns,
                gateway_name: pref.name.clone(),
                section_name: pref.section_name.clone(),
                port: pref.port,
                accepted,
                accepted_reason,
                resolved_refs,
                resolved_refs_reason,
                problems,
            });
        }

        if !parents.is_empty() {
            routes.push(RouteResult {
                kind: RouteKind::HttpRoute,
                namespace: rns,
                name: rname,
                uid: crate::obj_uid(&route.metadata),
                parents,
            });
        }
    }

    // 4. TCPRoutes / UDPRoutes on the layer-4 listeners. Same Service→pod-IP
    // resolver, same Gateway admission rules; the port conflicts they can
    // create are settled inside, never left to the translator.
    let l4_frontends = attach_l4_routes(
        inputs,
        index,
        clusters,
        backends,
        referenced,
        &gw_listeners,
        &mut routes,
        &mut attached,
    );

    // Assemble per-listener status now that attachedRoutes is known.
    for g in &mut gateways {
        let Some(listeners) = gw_listeners.get(&(g.namespace.clone(), g.name.clone())) else {
            continue;
        };
        g.listeners = listeners
            .iter()
            .map(|l| ListenerStatus {
                name: l.name.clone(),
                supported_kinds: l.supported_kinds.clone(),
                attached_routes: attached
                    .get(&(g.namespace.clone(), g.name.clone(), l.name.clone()))
                    .copied()
                    .unwrap_or(0),
                accepted: l.accepted,
                accepted_reason: l.accepted_reason,
                programmed: l.programmed,
                programmed_reason: l.programmed_reason,
                resolved_refs: l.resolved_refs,
                resolved_refs_reason: l.resolved_refs_reason,
            })
            .collect();
    }

    GatewayBuildResults {
        classes,
        gateways,
        routes,
        l4_frontends,
    }
}

/// Load an HTTPS listener's `certificateRefs` (Terminate only). Returns whether
/// at least one certificate was loaded (so the listener can serve TLS).
#[allow(clippy::too_many_arguments)]
fn load_listener_certs(
    inputs: &Inputs,
    index: &Index,
    gateway_ns: &str,
    listener: &sozu_gw_gateway_api::gateway::GatewayListeners,
    bind: SocketAddr,
    certificates: &mut Vec<FingerprintedCert>,
    problems: &mut Vec<Problem>,
) -> (bool, &'static str) {
    let Some(tls) = &listener.tls else {
        problems.push(Problem::TlsEntryWithoutSecret);
        return (false, "InvalidCertificateRef");
    };
    if !matches!(tls.mode, None | Some(GatewayListenersTlsMode::Terminate)) {
        problems.push(Problem::UnsupportedTlsMode {
            mode: "Passthrough".to_string(),
        });
        return (false, "InvalidCertificateRef");
    }

    let names = listener
        .hostname
        .clone()
        .map(|h| vec![h])
        .unwrap_or_default();
    let mut loaded = false;
    let mut ref_not_permitted = false;
    for cref in tls.certificate_refs.iter().flatten() {
        let is_secret = cref.group.as_deref().unwrap_or("").is_empty()
            && cref.kind.as_deref().unwrap_or("Secret") == "Secret";
        if !is_secret {
            problems.push(Problem::InvalidCertificate {
                secret: cref.name.clone(),
                reason: "unsupported certificateRef kind".to_string(),
            });
            continue;
        }
        let secret_ns = cref
            .namespace
            .clone()
            .unwrap_or_else(|| gateway_ns.to_string());
        if secret_ns != gateway_ns
            && !reference_granted(
                inputs, &secret_ns, "", "Secret", &cref.name, gateway_ns, GW_GROUP, "Gateway",
            )
        {
            problems.push(Problem::BackendRefNotPermitted {
                reference: format!("Secret {secret_ns}/{}", cref.name),
            });
            ref_not_permitted = true;
            continue;
        }
        match index.secrets.get(&(secret_ns, cref.name.clone())) {
            None => problems.push(Problem::SecretNotFound {
                secret: cref.name.clone(),
            }),
            Some(secret) => match extract_cert(secret) {
                Ok((leaf, chain, key, fingerprint)) => {
                    certificates.push(FingerprintedCert {
                        fingerprint,
                        cert: ir::Certificate {
                            // The listener's own bind, not "the HTTPS bind":
                            // the exposure table may carry several HTTPS ports,
                            // and a certificate loaded onto the wrong one
                            // serves nobody while reading as loaded.
                            listener: bind,
                            certificate: leaf,
                            chain,
                            key,
                            names: names.clone(),
                        },
                    });
                    loaded = true;
                }
                Err(reason) => problems.push(Problem::InvalidCertificate {
                    secret: cref.name.clone(),
                    reason,
                }),
            },
        }
    }
    let reason = if loaded {
        "ResolvedRefs"
    } else if ref_not_permitted {
        // A forbidden cross-namespace certificateRef is the listener's headline
        // failure (Gateway API ListenerReasonRefNotPermitted).
        "RefNotPermitted"
    } else {
        "InvalidCertificateRef"
    };
    (loaded, reason)
}

/// Resolve one HTTPRoute rule into frontends on the candidate listeners.
/// Record a `ResolvedRefs` failure, keeping the first reason seen across a route's
/// rules (the Gateway API reports a single reason per parent).
fn fail_ref(resolved: &mut bool, reason: &mut &'static str, new_reason: &'static str) {
    if *resolved {
        *reason = new_reason;
    }
    *resolved = false;
}

#[allow(clippy::too_many_arguments)]
fn attach_rule(
    inputs: &Inputs,
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    referenced: &mut BTreeSet<String>,
    frontends: &mut Vec<SourcedFrontend>,
    route_ns: &str,
    route_hostnames: Option<&[String]>,
    candidates: &[&ListenerInfo],
    rule: &sozu_gw_gateway_api::httproute::HttpRouteRules,
    source: &FrontendSource,
    problems: &mut Vec<Problem>,
    resolved_refs: &mut bool,
    resolved_refs_reason: &mut &'static str,
    accepted_override: &mut Option<&'static str>,
) {
    // backendRefs: exactly one Service backend (Sōzu cannot weight-split).
    // Parse the route filters into IR filters (Phase 3). Unsupported filters /
    // sub-fields are reported and skipped, never silently mis-applied.
    let ParsedFilters {
        filters,
        unprogrammable,
    } = parse_filters(rule.filters.as_deref().unwrap_or(&[]), problems);
    // A filter we cannot honour takes the whole rule out: the alternative is
    // serving a response the author never asked for. The gap must show in the
    // status too, not only in the problem list, so the route never reads
    // healthy while nothing (or worse, something wrong) is programmed.
    if unprogrammable {
        *accepted_override = Some("UnsupportedValue");
        return;
    }

    // `rule.timeouts` has no Sōzu equivalent. RequestMirror precedent: the
    // unsupported piece is reported and dropped — the rule still routes,
    // just without the timeout, and the gap is visible instead of silent.
    if rule
        .timeouts
        .as_ref()
        .is_some_and(|t| t.request.is_some() || t.backend_request.is_some())
    {
        problems.push(Problem::TimeoutsUnsupported);
    }

    // Resolve the backend. A redirect-only rule has no backendRefs (the Gateway
    // API even forbids combining RequestRedirect with backendRefs), so it yields
    // a frontend with no cluster; otherwise exactly one Service backendRef is
    // required (Sōzu cannot weight-split across clusters).
    let refs: Vec<_> = rule.backend_refs.iter().flatten().collect();
    let cluster_id: Option<String> = if refs.is_empty() {
        if filters.redirect.is_some() {
            None
        } else {
            problems.push(Problem::NoReadyEndpoints {
                service: "<none>".to_string(),
            });
            fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
            return;
        }
    } else if refs.len() > 1 {
        problems.push(Problem::WeightedBackendsUnsupported);
        fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
        return;
    } else {
        let br = refs[0];
        let is_service = br.group.as_deref().unwrap_or("").is_empty()
            && br.kind.as_deref().unwrap_or("Service") == "Service";
        if !is_service {
            problems.push(Problem::NonServiceBackend);
            fail_ref(resolved_refs, resolved_refs_reason, "InvalidKind");
            return;
        }
        // A single backendRef with `weight: 0` (the standard drain pattern)
        // must receive NO traffic; with every weight zero the spec calls for
        // a 500 on matching requests. Sōzu can neither weight nor synthesize
        // that 500, so fail closed — report and skip the rule — instead of
        // serving the drained backend 100% of the traffic. Any positive
        // weight on a single ref *is* 100% and keeps working. Like every
        // other skipped rule, the skip must show in the status, not only in
        // the problem list: downgrade ResolvedRefs the same way the
        // weighted-split path does, so the route never reads fully healthy
        // while nothing is programmed.
        if br.weight == Some(0) {
            problems.push(Problem::ZeroWeightBackendUnsupported {
                service: br.name.clone(),
            });
            fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
            return;
        }
        // Per-backendRef filters have no Sōzu equivalent (filters wire onto
        // the frontend, at rule level). RequestMirror precedent: report the
        // unsupported piece and route without it, never half-apply it.
        if br.filters.as_ref().is_some_and(|f| !f.is_empty()) {
            problems.push(Problem::FilterUnsupported {
                kind: format!("filters on backendRef {}", br.name),
            });
        }
        let backend_ns = br.namespace.clone().unwrap_or_else(|| route_ns.to_string());
        if backend_ns != route_ns
            && !reference_granted(
                inputs,
                &backend_ns,
                "",
                "Service",
                &br.name,
                route_ns,
                GW_GROUP,
                "HTTPRoute",
            )
        {
            problems.push(Problem::BackendRefNotPermitted {
                reference: format!("Service {backend_ns}/{}", br.name),
            });
            fail_ref(resolved_refs, resolved_refs_reason, "RefNotPermitted");
            return;
        }
        let Some(port) = br.port else {
            problems.push(Problem::ServicePortNotFound {
                service: br.name.clone(),
                port: "<unspecified>".to_string(),
            });
            fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
            return;
        };
        match add_service_route(
            index,
            clusters,
            backends,
            referenced,
            &backend_ns,
            &br.name,
            &PortRef::Number(port),
            problems,
        ) {
            Err(problem) => {
                problems.push(problem);
                fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
                return;
            }
            Ok((cid, has_endpoints)) => {
                if !has_endpoints {
                    problems.push(Problem::NoReadyEndpoints {
                        service: br.name.clone(),
                    });
                }
                Some(cid)
            }
        }
    };

    // Reduce the rule's matches to (path, method) pairs. No `matches` means
    // "match everything" → prefix "/". Header/query matches are skipped.
    let mut route_matches: Vec<(ir::PathMatch, Option<String>)> = Vec::new();
    match rule.matches.as_ref() {
        None => route_matches.push((ir::PathMatch::Prefix("/".to_string()), None)),
        Some(ms) if ms.is_empty() => {
            route_matches.push((ir::PathMatch::Prefix("/".to_string()), None))
        }
        Some(ms) => {
            for m in ms {
                if m.headers.as_ref().is_some_and(|h| !h.is_empty())
                    || m.query_params.as_ref().is_some_and(|q| !q.is_empty())
                {
                    problems.push(Problem::HeaderOrQueryMatchUnsupported);
                    continue;
                }
                route_matches.push((
                    path_match(m.path.as_ref()),
                    m.method.as_ref().and_then(method_string),
                ));
            }
        }
    }

    for (path, method) in &route_matches {
        for l in candidates {
            // A bound-but-unprogrammed listener (e.g. a cert-less HTTPS listener)
            // counts toward attachedRoutes but carries no frontend.
            if !l.programmed {
                continue;
            }
            let hosts = effective_hostnames(route_hostnames, l.hostname.as_deref());
            if hosts.is_empty() {
                // The route's hostnames don't intersect this listener's hostname:
                // the route attaches on a different listener, not a problem.
                continue;
            }
            let (Some(bind), Some(protocol)) = (l.bind, l.protocol) else {
                continue; // unreachable: a programmed listener resolved both
            };
            for hostname in hosts {
                frontends.push(SourcedFrontend {
                    frontend: ir::Frontend {
                        hostname,
                        path: path.clone(),
                        method: method.clone(),
                        cluster_id: cluster_id.clone(),
                        tls: protocol == ListenerProtocol::Https,
                        filters: filters.clone(),
                        listener: bind,
                    },
                    source: source.clone(),
                });
            }
        }
    }
}

fn path_match(path: Option<&HttpRouteRulesMatchesPath>) -> ir::PathMatch {
    let Some(path) = path else {
        return ir::PathMatch::Prefix("/".to_string());
    };
    let value = path
        .value
        .clone()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/".to_string());
    match path.r#type {
        Some(HttpRouteRulesMatchesPathType::Exact) => ir::PathMatch::Exact(value),
        Some(HttpRouteRulesMatchesPathType::RegularExpression) => ir::PathMatch::Regex(value),
        // PathPrefix (the default) or unset.
        _ => ir::PathMatch::Prefix(value),
    }
}

/// A rule's parsed filters, plus whether one of them makes the rule
/// unprogrammable.
struct ParsedFilters {
    filters: ir::FrontendFilters,
    /// A filter that determines the *response* could not be honoured. Dropping
    /// just that sub-field and programming the rest would answer requests with
    /// something the route author never asked for, so the whole rule is skipped
    /// (`Accepted=False`, `UnsupportedValue`) instead.
    unprogrammable: bool,
}

/// Parse HTTPRoute filters into neutral IR filters. Supported: header modifiers
/// (set/add→set, remove→delete) and RequestRedirect (scheme + status).
/// Unsupported filters/sub-fields (incl. URLRewrite) are reported.
fn parse_filters(filters: &[HttpRouteRulesFilters], problems: &mut Vec<Problem>) -> ParsedFilters {
    let mut ff = ir::FrontendFilters::default();
    let mut unprogrammable = false;
    for filter in filters {
        match &filter.r#type {
            HttpRouteRulesFiltersType::RequestHeaderModifier => {
                if let Some(m) = &filter.request_header_modifier {
                    for s in m.set.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Request,
                            key: s.name.clone(),
                            value: Some(s.value.clone()),
                        });
                    }
                    // Sōzu has no header "append" — `add` is applied as set.
                    for a in m.add.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Request,
                            key: a.name.clone(),
                            value: Some(a.value.clone()),
                        });
                    }
                    for r in m.remove.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Request,
                            key: r.clone(),
                            value: None,
                        });
                    }
                }
            }
            HttpRouteRulesFiltersType::ResponseHeaderModifier => {
                if let Some(m) = &filter.response_header_modifier {
                    for s in m.set.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Response,
                            key: s.name.clone(),
                            value: Some(s.value.clone()),
                        });
                    }
                    for a in m.add.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Response,
                            key: a.name.clone(),
                            value: Some(a.value.clone()),
                        });
                    }
                    for r in m.remove.iter().flatten() {
                        ff.header_mods.push(ir::HeaderMod {
                            on: ir::HeaderTarget::Response,
                            key: r.clone(),
                            value: None,
                        });
                    }
                }
            }
            HttpRouteRulesFiltersType::RequestRedirect => {
                if let Some(r) = &filter.request_redirect {
                    // `Unknown` folds to None, which the scheme-less branch
                    // below then refuses: a scheme this build cannot name is
                    // not a scheme it may guess at.
                    let scheme = r.scheme.as_ref().and_then(|s| match s {
                        HttpRouteRulesFiltersRequestRedirectScheme::Http => Some(ir::Scheme::Http),
                        HttpRouteRulesFiltersRequestRedirectScheme::Https => {
                            Some(ir::Scheme::Https)
                        }
                        HttpRouteRulesFiltersRequestRedirectScheme::Unknown => None,
                    });
                    // Exhaustive on purpose. The catch-all this replaces turned
                    // every status Sōzu cannot emit into a 302: a route asking
                    // for 303 or 307 was served as something else entirely, and
                    // the author had no way to find out. v1.6.1 widened the
                    // allowed set from {301,302} to {301,302,303,307,308},
                    // which made the silence far more likely to be hit.
                    let status = match r.status_code {
                        None | Some(302) => Some(ir::RedirectStatus::Found),
                        Some(301) => Some(ir::RedirectStatus::MovedPermanently),
                        Some(308) => Some(ir::RedirectStatus::PermanentRedirect),
                        // 303 and 307 have no RedirectPolicy variant, and the
                        // difference is method rewriting — precisely what an
                        // author picking them cares about.
                        Some(_) => None,
                    };
                    let Some(status) = status else {
                        problems.push(Problem::FilterUnsupported {
                            kind: format!(
                                "RequestRedirect statusCode {}",
                                r.status_code.unwrap_or_default()
                            ),
                        });
                        unprogrammable = true;
                        continue;
                    };
                    // The only part of a redirect Sōzu can express is the
                    // scheme: `redirect_scheme` unset means USE_SAME, i.e. the
                    // Location echoes the request's own scheme, host and path.
                    // So anything else the author asked for cannot be honoured,
                    // and programming the rule anyway would answer every
                    // matching request with a redirect to itself — an infinite
                    // loop, served under a green route status. Fail closed, the
                    // way every other unsupported piece does.
                    // A `port` equal to the scheme's well-known port asks for
                    // nothing extra: Gateway API derives the redirect port from
                    // the scheme when it is unset, and Sōzu's `redirect_scheme`
                    // emits `https://<host><path>` with no explicit port. So
                    // `scheme: https` and `scheme: https, port: 443` are the
                    // same redirect, and both are expressible.
                    let port_is_implied = matches!(
                        (scheme, r.port),
                        (_, None)
                            | (Some(ir::Scheme::Https), Some(443))
                            | (Some(ir::Scheme::Http), Some(80))
                    );
                    if r.hostname.is_some() || r.path.is_some() || !port_is_implied {
                        problems.push(Problem::FilterUnsupported {
                            kind: "RequestRedirect hostname/path/port".to_string(),
                        });
                        unprogrammable = true;
                    } else if scheme.is_none() {
                        // A scheme-less redirect has nothing left to change:
                        // USE_SAME + same host + same path is that same loop.
                        problems.push(Problem::FilterUnsupported {
                            kind: "RequestRedirect without scheme".to_string(),
                        });
                        unprogrammable = true;
                    } else {
                        ff.redirect = Some(ir::Redirect { scheme, status });
                    }
                }
            }
            HttpRouteRulesFiltersType::UrlRewrite => {
                // Not wired — but no longer for the reason this used to give.
                // Measured on Sōzu 2.2.0 (PROTOCOL.md §13): `rewrite_path` alone
                // rewrites the forwarded path toward the *same* backend, and
                // `rewrite_host` changes only the forwarded Host — the proxy
                // still dials the cluster's configured address. The `408` this
                // comment claimed was taken against 2.1.0 and does not reproduce.
                //
                // Two things must be settled before wiring it, both measured:
                // a literal `$` makes Sōzu reject the frontend (translation is
                // all-or-nothing, so one such route fails every reconcile), and
                // a path rewrite drops the query string `ReplaceFullPath` keeps.
                // `ReplacePrefixMatch` stays impossible: the compiled prefix
                // regex's only capture group is the element boundary.
                problems.push(Problem::FilterUnsupported {
                    kind: "URLRewrite".to_string(),
                });
            }
            // Sōzu has no CORS support: no response-header synthesis keyed on
            // Origin, no preflight handling. Serving the rule without the
            // filter would answer cross-origin requests the route author meant
            // to gate, so the rule is skipped like any other filter we cannot
            // honour.
            HttpRouteRulesFiltersType::Cors => {
                problems.push(Problem::FilterUnsupported {
                    kind: "CORS".to_string(),
                });
                unprogrammable = true;
            }
            HttpRouteRulesFiltersType::RequestMirror | HttpRouteRulesFiltersType::ExtensionRef => {
                problems.push(Problem::FilterUnsupported {
                    kind: format!("{:?}", filter.r#type),
                });
            }
            // A filter type this build's generated types do not name: the
            // cluster's CRDs are newer than the controller. The rule is
            // skipped rather than served without the filter, because a
            // filter is there to change what the response is.
            HttpRouteRulesFiltersType::Unknown => {
                problems.push(Problem::FilterUnsupported {
                    kind: "a filter type this controller build does not know (its Gateway API \
                           CRDs are newer than the types it was generated against)"
                        .to_string(),
                });
                unprogrammable = true;
            }
        }
    }
    ParsedFilters {
        filters: ff,
        unprogrammable,
    }
}

/// The wire spelling of an HTTP method (`GET`, `POST`, …) via its serde rename.
fn method_string(method: &HttpRouteRulesMatchesMethod) -> Option<String> {
    serde_json::to_value(method)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
}

/// A `*.example.com` wildcard covers exactly one extra label.
fn wildcard_covers(wildcard: &str, host: &str) -> bool {
    wildcard.strip_prefix("*.").is_some_and(|suffix| {
        host.strip_suffix(suffix)
            .and_then(|prefix| prefix.strip_suffix('.'))
            .is_some_and(|label| !label.is_empty() && !label.contains('.'))
    })
}

/// The intersection of a route hostname and a listener hostname: the MORE
/// SPECIFIC of the two when they are compatible (`None` otherwise). When one
/// side is a wildcard covering the other, the covered — narrower — name *is*
/// the intersection: programming the route's own string when the listener is
/// the narrower side would emit a wildcard frontend and route hostnames the
/// listener never admits (the Gateway API intersects, it doesn't widen).
fn host_intersection(route: &str, listener: &str) -> Option<String> {
    if route == listener || wildcard_covers(listener, route) {
        Some(route.to_string())
    } else if wildcard_covers(route, listener) {
        Some(listener.to_string())
    } else {
        None
    }
}

/// The hostnames a route serves on a listener: the route's hostnames intersected
/// with the listener's hostname constraint (a missing listener hostname matches
/// any; a route with no hostnames inherits the listener's; both missing is a
/// catch-all `*`, which Sōzu routes as `DomainRule::Any`).
fn effective_hostnames(route: Option<&[String]>, listener: Option<&str>) -> Vec<String> {
    match (route, listener) {
        (Some(routes), Some(l)) => routes
            .iter()
            .filter_map(|h| host_intersection(h, l))
            .collect(),
        (Some(routes), None) => routes.to_vec(),
        (None, Some(l)) => vec![l.to_string()],
        (None, None) => vec!["*".to_string()],
    }
}

// ----------------------------------------------------------------------------
// Layer-4 routes (TCPRoute / UDPRoute)
// ----------------------------------------------------------------------------

/// A TCPRoute or UDPRoute flattened into the one shape they share.
///
/// The two CRDs are structurally identical — `rules` is `minItems: 1,
/// maxItems: 1`, the rule carries `backendRefs` and nothing else, there is no
/// hostname, no match and no filter — but kopium generates them as two
/// unrelated type trees. Flattening once here is cheaper than a trait over
/// four borrowed accessor types, and the count of layer-4 routes is small.
struct L4RouteView {
    kind: RouteKind,
    protocol: ir::L4Protocol,
    namespace: String,
    name: String,
    uid: Option<String>,
    /// Tie-break key for a port two routes both claim. Absent on an object the
    /// apiserver somehow never stamped, which sorts last: an established route
    /// should not lose its port to one whose age we cannot establish.
    creation: Option<String>,
    parent_refs: Vec<L4ParentRefView>,
    backend_refs: Vec<L4BackendRefView>,
}

struct L4ParentRefView {
    group: Option<String>,
    kind: Option<String>,
    name: String,
    namespace: Option<String>,
    port: Option<i32>,
    section_name: Option<String>,
}

struct L4BackendRefView {
    group: Option<String>,
    kind: Option<String>,
    name: String,
    namespace: Option<String>,
    port: Option<i32>,
    weight: Option<i32>,
}

/// `metadata.creationTimestamp` as its RFC 3339 spelling, which sorts
/// chronologically as a string (fixed-width, UTC, `Z`-suffixed).
fn creation_key(
    meta: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> Option<String> {
    meta.creation_timestamp.as_ref().map(|t| t.0.to_string())
}

impl L4RouteView {
    fn of_tcp(route: &TcpRoute) -> Self {
        let (namespace, name) = meta_nn(&route.metadata.namespace, &route.metadata.name);
        Self {
            kind: RouteKind::TcpRoute,
            protocol: ir::L4Protocol::Tcp,
            namespace,
            name,
            uid: crate::obj_uid(&route.metadata),
            creation: creation_key(&route.metadata),
            parent_refs: route
                .spec
                .parent_refs
                .iter()
                .flatten()
                .map(|p| L4ParentRefView {
                    group: p.group.clone(),
                    kind: p.kind.clone(),
                    name: p.name.clone(),
                    namespace: p.namespace.clone(),
                    port: p.port,
                    section_name: p.section_name.clone(),
                })
                .collect(),
            backend_refs: route
                .spec
                .rules
                .iter()
                .flat_map(|r| r.backend_refs.iter())
                .map(|b| L4BackendRefView {
                    group: b.group.clone(),
                    kind: b.kind.clone(),
                    name: b.name.clone(),
                    namespace: b.namespace.clone(),
                    port: b.port,
                    weight: b.weight,
                })
                .collect(),
        }
    }

    fn of_udp(route: &UdpRoute) -> Self {
        let (namespace, name) = meta_nn(&route.metadata.namespace, &route.metadata.name);
        Self {
            kind: RouteKind::UdpRoute,
            protocol: ir::L4Protocol::Udp,
            namespace,
            name,
            uid: crate::obj_uid(&route.metadata),
            creation: creation_key(&route.metadata),
            parent_refs: route
                .spec
                .parent_refs
                .iter()
                .flatten()
                .map(|p| L4ParentRefView {
                    group: p.group.clone(),
                    kind: p.kind.clone(),
                    name: p.name.clone(),
                    namespace: p.namespace.clone(),
                    port: p.port,
                    section_name: p.section_name.clone(),
                })
                .collect(),
            backend_refs: route
                .spec
                .rules
                .iter()
                .flat_map(|r| r.backend_refs.iter())
                .map(|b| L4BackendRefView {
                    group: b.group.clone(),
                    kind: b.kind.clone(),
                    name: b.name.clone(),
                    namespace: b.namespace.clone(),
                    port: b.port,
                    weight: b.weight,
                })
                .collect(),
        }
    }

    /// `namespace/name`, how a conflict names the route that won.
    fn key(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

/// One claim a route parent makes on a layer-4 socket, held until conflicts are
/// resolved. Two routes cannot share a socket — there is no hostname to
/// multiplex on — so exactly one claim per socket survives.
struct L4Claim {
    protocol: ir::L4Protocol,
    listener: SocketAddr,
    /// The port the listener *declares* — what a user reads and dials. Carried
    /// separately from `listener`, which holds the in-pod bind: a conflict
    /// message naming the bind would name a number nobody wrote down.
    advertised: u16,
    cluster_id: String,
    /// Where a loss is reported: index into the route results, then into that
    /// result's parents.
    route: usize,
    parent: usize,
    /// The listener this claim attaches to, for `attachedRoutes`.
    listener_name: (String, String, String),
    /// Tie-break: oldest `creationTimestamp` first (absent last), then
    /// `namespace/name`.
    order: (bool, String, String),
}

/// Resolve one layer-4 route's backendRef down to a cluster id.
///
/// Every rejection here is one HTTPRoute already knows: Sōzu cannot weight a
/// split, cannot drain by weight, and dials Services only. They apply
/// unchanged at layer 4 — `weight: 0` especially, which is easy to forget
/// because there is no traffic-shaping story down here to remind you of it.
#[allow(clippy::too_many_arguments)]
fn resolve_l4_backend(
    inputs: &Inputs,
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    referenced: &mut BTreeSet<String>,
    view: &L4RouteView,
    problems: &mut Vec<Problem>,
    resolved_refs: &mut bool,
    resolved_refs_reason: &mut &'static str,
) -> Option<String> {
    if view.backend_refs.len() > 1 {
        problems.push(Problem::WeightedBackendsUnsupported);
        fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
        return None;
    }
    // The CRD requires at least one backendRef; an empty list means the object
    // predates that validation or reached us some other way. There is no
    // redirect-style backend-less rule at layer 4, so there is nothing to route.
    let Some(br) = view.backend_refs.first() else {
        problems.push(Problem::NoReadyEndpoints {
            service: "<none>".to_string(),
        });
        fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
        return None;
    };
    let is_service = br.group.as_deref().unwrap_or("").is_empty()
        && br.kind.as_deref().unwrap_or("Service") == "Service";
    if !is_service {
        problems.push(Problem::NonServiceBackend);
        fail_ref(resolved_refs, resolved_refs_reason, "InvalidKind");
        return None;
    }
    if br.weight == Some(0) {
        problems.push(Problem::ZeroWeightBackendUnsupported {
            service: br.name.clone(),
        });
        fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
        return None;
    }
    let backend_ns = br
        .namespace
        .clone()
        .unwrap_or_else(|| view.namespace.clone());
    if backend_ns != view.namespace
        && !reference_granted(
            inputs,
            &backend_ns,
            "",
            "Service",
            &br.name,
            &view.namespace,
            GW_GROUP,
            view.kind.as_str(),
        )
    {
        problems.push(Problem::BackendRefNotPermitted {
            reference: format!("Service {backend_ns}/{}", br.name),
        });
        fail_ref(resolved_refs, resolved_refs_reason, "RefNotPermitted");
        return None;
    }
    let Some(port) = br.port else {
        problems.push(Problem::ServicePortNotFound {
            service: br.name.clone(),
            port: "<unspecified>".to_string(),
        });
        fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
        return None;
    };
    match add_service_route(
        index,
        clusters,
        backends,
        referenced,
        &backend_ns,
        &br.name,
        &PortRef::Number(port),
        problems,
    ) {
        Err(problem) => {
            problems.push(problem);
            fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
            None
        }
        Ok((cid, has_endpoints)) => {
            if !has_endpoints {
                problems.push(Problem::NoReadyEndpoints {
                    service: br.name.clone(),
                });
            }
            Some(cid)
        }
    }
}

/// Attach the layer-4 routes to our Gateways' TCP/UDP listeners, resolve the
/// port conflicts they can create, and emit the surviving [`ir::L4Frontend`]s.
///
/// **Conflicts are settled here, not in the translator.** The translator's
/// `check_l4_conflicts` returns an error that `reconcile` propagates with `?`,
/// which fails the *entire* reconcile — every HTTP route in the cluster
/// included. One tenant's second TCPRoute must not be able to stop routing for
/// everyone else, so the loser is dropped with a Problem on its own status and
/// the translator's guard is left as a net that should never fire.
///
/// The tie-break is oldest `creationTimestamp`, then `namespace/name`. The
/// second key is not decoration: `creationTimestamp` has one-second
/// granularity, so two routes applied together routinely tie, and without it
/// the winner would follow cache iteration order and flip between reconciles.
#[allow(clippy::too_many_arguments)]
fn attach_l4_routes(
    inputs: &Inputs,
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    referenced: &mut BTreeSet<String>,
    gw_listeners: &BTreeMap<(String, String), Vec<ListenerInfo>>,
    routes: &mut Vec<RouteResult>,
    attached: &mut BTreeMap<(String, String, String), i32>,
) -> Vec<ir::L4Frontend> {
    let mut views: Vec<L4RouteView> = Vec::new();
    views.extend(inputs.tcp_routes.iter().map(|r| L4RouteView::of_tcp(r)));
    views.extend(inputs.udp_routes.iter().map(|r| L4RouteView::of_udp(r)));

    let mut claims: Vec<L4Claim> = Vec::new();
    for view in &views {
        let mut parents = Vec::new();
        for pref in &view.parent_refs {
            let is_gateway = pref.group.as_deref().unwrap_or(GW_GROUP) == GW_GROUP
                && pref.kind.as_deref().unwrap_or("Gateway") == "Gateway";
            if !is_gateway {
                continue;
            }
            let gw_ns = pref
                .namespace
                .clone()
                .unwrap_or_else(|| view.namespace.clone());
            let Some(listeners) = gw_listeners.get(&(gw_ns.clone(), pref.name.clone())) else {
                continue; // not one of our Gateways
            };
            let addressable: Vec<&ListenerInfo> = listeners
                .iter()
                .filter(|l| l.routable)
                .filter(|l| pref.section_name.as_ref().is_none_or(|sn| sn == &l.name))
                .filter(|l| pref.port.is_none_or(|p| p == l.port))
                .collect();
            let candidates: Vec<&ListenerInfo> = addressable
                .iter()
                .copied()
                .filter(|l| l.accepted)
                // A TCPRoute on an HTTP listener is `NotAllowedByListeners`,
                // not `NoMatchingParent`: the parentRef addressed a listener,
                // that listener just does not serve this kind.
                .filter(|l| l.admits_kind(view.kind))
                .filter(|l| l.allow_from.admits(&view.namespace, &gw_ns))
                .collect();

            let mut problems = Vec::new();
            let mut resolved_refs = true;
            let mut resolved_refs_reason = "ResolvedRefs";
            let (accepted, accepted_reason) = if addressable.is_empty() {
                (false, "NoMatchingParent")
            } else if candidates.is_empty() {
                (false, "NotAllowedByListeners")
            } else {
                let cluster_id = resolve_l4_backend(
                    inputs,
                    index,
                    clusters,
                    backends,
                    referenced,
                    view,
                    &mut problems,
                    &mut resolved_refs,
                    &mut resolved_refs_reason,
                );
                if let Some(cluster_id) = cluster_id {
                    // One claim per (parent, socket). A parentRef without a
                    // sectionName can address several listeners; each is its own
                    // socket and its own possible conflict.
                    for l in candidates.iter().filter(|l| l.programmed) {
                        let Some(bind) = l.bind else { continue };
                        claims.push(L4Claim {
                            protocol: view.protocol,
                            listener: bind,
                            advertised: u16::try_from(l.port).unwrap_or_default(),
                            cluster_id: cluster_id.clone(),
                            route: routes.len(),
                            parent: parents.len(),
                            listener_name: (gw_ns.clone(), pref.name.clone(), l.name.clone()),
                            order: (
                                view.creation.is_none(),
                                view.creation.clone().unwrap_or_default(),
                                view.key(),
                            ),
                        });
                    }
                }
                // Accepted is about binding to this parent; a backendRef we
                // cannot resolve downgrades ResolvedRefs, not Accepted — the
                // same split the HTTPRoute path applies.
                (true, "Accepted")
            };

            parents.push(RouteParentResult {
                gateway_namespace: gw_ns,
                gateway_name: pref.name.clone(),
                section_name: pref.section_name.clone(),
                port: pref.port,
                accepted,
                accepted_reason,
                resolved_refs,
                resolved_refs_reason,
                problems,
            });
        }
        if !parents.is_empty() {
            routes.push(RouteResult {
                kind: view.kind,
                namespace: view.namespace.clone(),
                name: view.name.clone(),
                uid: view.uid.clone(),
                parents,
            });
        }
    }

    // Settle the sockets. Sorted by the documented tie-break so the winner is
    // a property of the objects, never of iteration order. The claim index
    // breaks a remaining tie within one route, keeping the pass deterministic.
    let mut settled: Vec<usize> = (0..claims.len()).collect();
    settled.sort_by(|a, b| (&claims[*a].order, *a).cmp(&(&claims[*b].order, *b)));

    let mut l4_frontends = Vec::new();
    let mut winners: BTreeMap<(ir::L4Protocol, SocketAddr), (String, String)> = BTreeMap::new();
    for i in settled {
        let claim = &claims[i];
        let socket = (claim.protocol, claim.listener);
        match winners.get(&socket) {
            None => {
                winners.insert(socket, (claim.cluster_id.clone(), claim.order.2.clone()));
                l4_frontends.push(ir::L4Frontend {
                    protocol: claim.protocol,
                    listener: claim.listener,
                    cluster_id: claim.cluster_id.clone(),
                });
                *attached.entry(claim.listener_name.clone()).or_insert(0) += 1;
            }
            // Same socket, same cluster: two routes (or two parentRefs of one
            // route) asking for the identical thing. Sōzu would apply either
            // with the same effect, so this is a benign overlap, not a clash.
            Some((cluster, _)) if *cluster == claim.cluster_id => {
                *attached.entry(claim.listener_name.clone()).or_insert(0) += 1;
            }
            Some((_, winner)) => {
                let problem = Problem::L4RouteConflict {
                    port: claim.advertised,
                    protocol: match claim.protocol {
                        ir::L4Protocol::Tcp => "TCP",
                        ir::L4Protocol::Udp => "UDP",
                    },
                    winner: winner.clone(),
                };
                let parent = &mut routes[claim.route].parents[claim.parent];
                parent.accepted = false;
                parent.accepted_reason = "RouteConflict";
                if !parent.problems.contains(&problem) {
                    parent.problems.push(problem);
                }
            }
        }
    }
    l4_frontends.sort_by_key(|f| (f.protocol, f.listener));
    l4_frontends
}

/// Is a cross-namespace reference allowed by a `ReferenceGrant` in the target
/// namespace?
#[allow(clippy::too_many_arguments)]
fn reference_granted(
    inputs: &Inputs,
    to_ns: &str,
    to_group: &str,
    to_kind: &str,
    to_name: &str,
    from_ns: &str,
    from_group: &str,
    from_kind: &str,
) -> bool {
    inputs.reference_grants.iter().any(|grant| {
        let grant_ns = grant.metadata.namespace.as_deref().unwrap_or("default");
        grant_ns == to_ns
            && grant
                .spec
                .from
                .iter()
                .any(|f| f.group == from_group && f.kind == from_kind && f.namespace == from_ns)
            && grant.spec.to.iter().any(|t| {
                t.group == to_group
                    && t.kind == to_kind
                    && t.name.as_deref().is_none_or(|n| n == to_name)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::{host_intersection, wildcard_covers};

    #[test]
    fn wildcard_covers_exactly_one_extra_label() {
        assert!(wildcard_covers("*.example.com", "a.example.com"));
        assert!(!wildcard_covers("*.example.com", "a.b.example.com"));
        assert!(!wildcard_covers("*.example.com", "example.com"));
        // Not a suffix-string match: `notexample.com` must not count.
        assert!(!wildcard_covers("*.example.com", "a.notexample.com"));
        // Only `*.`-prefixed patterns are wildcards; a bare `*` (the builder's
        // catch-all spelling, not representable as a Gateway hostname) is not.
        assert!(!wildcard_covers("*", "example.com"));
        assert!(!wildcard_covers("a.example.com", "a.example.com"));
    }

    #[test]
    fn host_intersection_picks_the_more_specific_name() {
        // Equal → either.
        assert_eq!(
            host_intersection("app.example.com", "app.example.com").as_deref(),
            Some("app.example.com")
        );
        // Wildcard route × specific listener → the listener (narrower) wins.
        assert_eq!(
            host_intersection("*.example.com", "test.example.com").as_deref(),
            Some("test.example.com")
        );
        // Specific route × wildcard listener → the route (narrower) wins.
        assert_eq!(
            host_intersection("test.example.com", "*.example.com").as_deref(),
            Some("test.example.com")
        );
        // Incompatible → empty intersection.
        assert_eq!(host_intersection("a.example.com", "b.example.com"), None);
        assert_eq!(host_intersection("*.example.com", "example.com"), None);
    }
}
