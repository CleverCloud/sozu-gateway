//! Gateway API status reporting (Phase 2).
//!
//! Writes Accepted/Programmed (Gateway, GatewayClass) and Accepted/ResolvedRefs
//! (routes, per parent) conditions back to the objects.
//!
//! **Loop-safe:** it reads the current status, reuses `lastTransitionTime` for
//! conditions whose (status, reason, message) are unchanged, and skips the PATCH
//! entirely when nothing changed — so the controller's own status writes never
//! re-trigger a reconcile. **Best-effort:** every failure is logged, never
//! propagated, so status reporting can never break routing.
//!
//! The route writer is generic over the route kind. Every Gateway API route
//! carries the same `status.parents[]` shape, but kopium generates a separate
//! `…Status` / `…StatusParents` / `…StatusParentsParentRef` triple per kind with
//! nothing in common — they are foreign types, so the trait tying them together
//! ([`RouteParents`]) is declared here and implemented once per kind.

use std::sync::Arc;

use crate::scope::GatewayScope;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::networking::v1::{Ingress, IngressLoadBalancerIngress};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::api::{Patch, PatchParams};
use kube::core::NamespaceResourceScope;
use kube::{Api, Client, Resource};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, warn};

use sozu_gw_builder::{
    GatewayClassResult, GatewayResult, IngressResult, Inputs, Problem, RouteKind, RouteResult,
};
use sozu_gw_gateway_api::gateway::{
    GatewayStatusAddresses, GatewayStatusListeners, GatewayStatusListenersSupportedKinds,
};
use sozu_gw_gateway_api::gatewayclass::GatewayClassStatusSupportedFeatures;
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute, TcpRoute, UdpRoute};

const GW_GROUP: &str = "gateway.networking.k8s.io";

/// One desired condition before timestamping.
struct Desired {
    type_: &'static str,
    status: bool,
    reason: &'static str,
    message: String,
}

/// Compose a condition message from problem details, so `kubectl describe`
/// shows *which* Secret/Service/port is wrong instead of a generic sentence
/// (the detail otherwise only reaches controller logs). Sorted and deduped —
/// the message participates in `lastTransitionTime` reuse, so it must be
/// deterministic across reconciles — and capped so a pathological object
/// cannot bloat its own status.
fn problems_message(problems: &[&Problem], fallback: &str) -> String {
    if problems.is_empty() {
        return fallback.to_string();
    }
    let mut lines: Vec<String> = problems.iter().map(|p| p.to_string()).collect();
    lines.sort();
    lines.dedup();
    const MAX_SHOWN: usize = 5;
    let extra = lines.len().saturating_sub(MAX_SHOWN);
    let mut msg = lines[..lines.len().min(MAX_SHOWN)].join("; ");
    if extra > 0 {
        msg.push_str(&format!(" (+{extra} more)"));
    }
    msg
}

pub async fn write_status(
    client: &Client,
    controller_name: &str,
    gateway_classes: &[GatewayClassResult],
    gateways: &[GatewayResult],
    routes: &[RouteStatusUpdate],
    gateway_addresses: Option<&[GatewayStatusAddresses]>,
    scope: &GatewayScope,
) {
    for gc in gateway_classes
        .iter()
        .filter(|gc| gc.accepted && scope.is_default())
    {
        if let Err(e) = write_gatewayclass(client, gc).await {
            warn!(name = %gc.name, error = %e, "failed to write GatewayClass status");
        }
    }
    for gw in gateways {
        if let Err(e) = write_gateway(client, gw, gateway_addresses).await {
            warn!(namespace = %gw.namespace, name = %gw.name, error = %e, "failed to write Gateway status");
        }
    }
    for update in routes {
        let route = &update.route;
        // One arm per route kind: the writer is generic, but `Api<K>` needs a
        // concrete type, so the kind carried by the build result picks it.
        let written = match route.kind {
            RouteKind::HttpRoute => {
                write_route::<HttpRoute>(client, controller_name, update, scope).await
            }
            RouteKind::TcpRoute => {
                write_route::<TcpRoute>(client, controller_name, update, scope).await
            }
            RouteKind::UdpRoute => {
                write_route::<UdpRoute>(client, controller_name, update, scope).await
            }
        };
        if let Err(e) = written {
            warn!(kind = route.kind.as_str(), namespace = %route.namespace, name = %route.name, error = %e, "failed to write route status");
        }
    }
}

fn now() -> Time {
    Time(k8s_openapi::jiff::Timestamp::now())
}

/// Build conditions, reusing the previous `lastTransitionTime` when a condition's
/// observable fields are unchanged (so repeated writes are byte-identical).
///
/// `observed_generation` is set to the object's `metadata.generation`: the Gateway
/// API requires every condition to carry it, and conformance checks that it tracks
/// the latest generation. `lastTransitionTime` still only moves when `status`
/// flips — a generation bump alone updates `observedGeneration` without resetting
/// the transition time.
fn build_conditions(
    desired: &[Desired],
    current: Option<&[Condition]>,
    generation: Option<i64>,
) -> Vec<Condition> {
    desired
        .iter()
        .map(|d| {
            let status = if d.status { "True" } else { "False" }.to_string();
            let previous = current.and_then(|cs| cs.iter().find(|c| c.type_ == d.type_));
            let last_transition_time = match previous {
                Some(p) if p.status == status && p.reason == d.reason && p.message == d.message => {
                    p.last_transition_time.clone()
                }
                _ => now(),
            };
            Condition {
                type_: d.type_.to_string(),
                status,
                reason: d.reason.to_string(),
                message: d.message.clone(),
                last_transition_time,
                observed_generation: generation,
            }
        })
        .collect()
}

fn conditions_equal(a: &[Condition], b: &[Condition]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

/// The Gateway API features published in `GatewayClass.status.supportedFeatures`.
///
/// `FeatureName` is an **upstream, boolean-per-feature** vocabulary, and the
/// conformance tooling cross-checks it. A name here whose tests do not pass is a
/// lie told in the one machine-readable channel that exists for honesty — and a
/// lie the next conformance run contradicts on the spot. So an entry is added
/// only when a **recorded run** in `docs/E2E-RESULTS.md` §6 shows its tests
/// passing, never because the code appears to implement the feature.
///
/// The list is empty today, and that is a result rather than a placeholder: at
/// the last recorded run no feature's tests passed cleanly. Publishing it empty
/// is the statement "we claim nothing", which is different from saying nothing
/// at all — and it is what makes a later addition visible as a change.
///
/// **Must stay sorted**: the CRD requires ascending order by name and keys the
/// list on it (`x-kubernetes-list-type: map`).
const SUPPORTED_FEATURES: &[&str] = &[];

/// Is the published feature list already what we want?
///
/// A GatewayClass that has never been written carries `None`, which is *not*
/// equal to an empty list: "we claim nothing" is a statement and has to be
/// published once, or the field would stay absent forever.
fn features_equal(
    current: Option<&[GatewayClassStatusSupportedFeatures]>,
    desired: &[GatewayClassStatusSupportedFeatures],
) -> bool {
    current.is_some_and(|f| {
        f.len() == desired.len() && f.iter().zip(desired).all(|(a, b)| a.name == b.name)
    })
}

async fn write_gatewayclass(client: &Client, gc: &GatewayClassResult) -> Result<(), kube::Error> {
    let api: Api<GatewayClass> = Api::all(client.clone());
    let current = api.get(&gc.name).await?;
    let cur = current
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref());
    let desired = build_conditions(
        &[Desired {
            type_: "Accepted",
            status: true,
            reason: "Accepted",
            message: "Accepted by sozu-gateway".to_string(),
        }],
        cur,
        current.metadata.generation,
    );
    let features: Vec<GatewayClassStatusSupportedFeatures> = SUPPORTED_FEATURES
        .iter()
        .map(|name| GatewayClassStatusSupportedFeatures {
            name: name.to_string(),
        })
        .collect();
    // The no-op guard has to cover the features too. Comparing conditions alone
    // means a list changed by a version bump — the whole reason it is versioned
    // against the run log — would be computed and then never written, because
    // the conditions it travels with did not move.
    let features_unchanged = features_equal(
        current
            .status
            .as_ref()
            .and_then(|s| s.supported_features.as_deref()),
        &features,
    );
    if cur.is_some_and(|c| conditions_equal(&desired, c)) && features_unchanged {
        return Ok(());
    }
    let patch = json!({ "status": { "conditions": desired, "supportedFeatures": features } });
    api.patch_status(&gc.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(name = %gc.name, "GatewayClass status updated");
    Ok(())
}

/// Build `Gateway.status.listeners[]`, reusing each listener condition's previous
/// `lastTransitionTime` (matched by listener name) so repeated writes are stable.
fn build_listeners_status(
    gw: &GatewayResult,
    current: &Gateway,
    generation: Option<i64>,
) -> Vec<GatewayStatusListeners> {
    let cur_listeners = current
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_deref())
        .unwrap_or_default();
    gw.listeners
        .iter()
        .map(|l| {
            let prev = cur_listeners
                .iter()
                .find(|cl| cl.name == l.name)
                .map(|cl| cl.conditions.as_slice());
            // Problems that name this listener carry the user-facing detail
            // for its False conditions.
            let listener_problems: Vec<&Problem> = gw
                .problems
                .iter()
                .filter(|p| p.listener() == Some(l.name.as_str()))
                .collect();
            let conditions = build_conditions(
                &[
                    Desired {
                        type_: "Accepted",
                        status: l.accepted,
                        reason: l.accepted_reason,
                        message: if l.accepted {
                            "Listener accepted by sozu-gateway".to_string()
                        } else {
                            problems_message(
                                &listener_problems,
                                "Listener cannot be accepted as declared",
                            )
                        },
                    },
                    Desired {
                        type_: "Programmed",
                        status: l.programmed,
                        reason: l.programmed_reason,
                        message: if l.programmed {
                            "Listener programmed into Sōzu".to_string()
                        } else {
                            problems_message(
                                &listener_problems,
                                "Listener could not be programmed into Sōzu",
                            )
                        },
                    },
                    Desired {
                        type_: "ResolvedRefs",
                        status: l.resolved_refs,
                        reason: l.resolved_refs_reason,
                        message: if l.resolved_refs {
                            "Listener references resolved".to_string()
                        } else {
                            problems_message(
                                &listener_problems,
                                "Listener references could not be resolved",
                            )
                        },
                    },
                ],
                prev,
                generation,
            );
            GatewayStatusListeners {
                name: l.name.clone(),
                supported_kinds: Some(
                    l.supported_kinds
                        .iter()
                        .map(|k| GatewayStatusListenersSupportedKinds {
                            group: Some(GW_GROUP.to_string()),
                            kind: k.clone(),
                        })
                        .collect(),
                ),
                attached_routes: l.attached_routes,
                conditions,
            }
        })
        .collect()
}

fn build_gateway_conditions(
    gw: &GatewayResult,
    current: &Gateway,
    awaiting_address: bool,
) -> Vec<Condition> {
    let cur = current
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref());
    let all_problems: Vec<&Problem> = gw.problems.iter().collect();
    build_conditions(
        &[
            Desired {
                type_: "Accepted",
                status: gw.accepted,
                reason: gw.accepted_reason,
                message: if gw.accepted_reason == "Accepted" {
                    "Accepted by sozu-gateway".to_string()
                } else {
                    problems_message(&all_problems, "One or more listeners are invalid")
                },
            },
            Desired {
                type_: "Programmed",
                status: gw.programmed && !awaiting_address,
                reason: if !gw.programmed {
                    "Invalid"
                } else if awaiting_address {
                    "AddressNotAssigned"
                } else {
                    "Programmed"
                },
                message: if !gw.programmed {
                    problems_message(&all_problems, "No listeners could be programmed")
                } else if awaiting_address {
                    "Waiting for the publish Service to receive an address".to_string()
                } else {
                    "Listeners programmed into Sōzu".to_string()
                },
            },
        ],
        cur,
        current.metadata.generation,
    )
}

async fn write_gateway(
    client: &Client,
    gw: &GatewayResult,
    addresses: Option<&[GatewayStatusAddresses]>,
) -> Result<(), kube::Error> {
    let awaiting_address = addresses.is_some_and(|a| a.is_empty());
    let addresses = addresses.unwrap_or_default();
    let api: Api<Gateway> = Api::namespaced(client.clone(), &gw.namespace);
    let current = api.get(&gw.name).await?;
    let cur = current
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref());
    let desired = build_gateway_conditions(gw, &current, awaiting_address);
    let listeners = build_listeners_status(gw, &current, current.metadata.generation);
    // Publish the LoadBalancer address into the Gateway's status (what
    // external-dns's gateway-httproute source reads). Skipped when there is no
    // address yet. Scope alone cannot attribute an old address to a previous
    // instance; preserve it while Programmed=False signals the pending state.
    let cur_addresses = current
        .status
        .as_ref()
        .and_then(|s| s.addresses.as_deref())
        .unwrap_or_default();
    let cur_listeners = current
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_deref())
        .unwrap_or_default();
    let addresses_unchanged = addresses.is_empty()
        || serde_json::to_value(cur_addresses).ok() == serde_json::to_value(addresses).ok();
    let listeners_unchanged =
        serde_json::to_value(cur_listeners).ok() == serde_json::to_value(&listeners).ok();
    let conditions_unchanged = cur.is_some_and(|c| conditions_equal(&desired, c));
    if conditions_unchanged && addresses_unchanged && listeners_unchanged {
        return Ok(());
    }
    let mut status = serde_json::Map::new();
    status.insert("conditions".to_string(), json!(desired));
    status.insert("listeners".to_string(), json!(listeners));
    if !addresses.is_empty() {
        status.insert("addresses".to_string(), json!(addresses));
    }
    let patch = json!({ "status": status });
    api.patch_status(&gw.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(namespace = %gw.namespace, name = %gw.name, "Gateway status updated");
    Ok(())
}

/// Only address-bearing Service types can make publication a readiness gate.
/// NodePort deployments retain listener-only status until node address
/// publication is supported.
pub(crate) fn published_gateway_addresses(svc: &Service) -> Option<Vec<GatewayStatusAddresses>> {
    matches!(
        svc.spec.as_ref().and_then(|s| s.type_.as_deref()),
        Some("LoadBalancer" | "ClusterIP")
    )
    .then(|| gateway_addresses(svc))
}

/// Map the publish Service's load-balancer address(es) to Gateway status
/// addresses (`IPAddress` for an IP, `Hostname` otherwise).
pub(crate) fn gateway_addresses(svc: &Service) -> Vec<GatewayStatusAddresses> {
    let points = lb_points(svc);
    if points.is_empty() {
        if let Some(spec) = svc
            .spec
            .as_ref()
            .filter(|s| s.type_.as_deref() == Some("ClusterIP"))
        {
            return spec
                .cluster_ips
                .iter()
                .flatten()
                .chain(spec.cluster_ip.iter())
                .filter_map(|ip| ip.parse::<std::net::IpAddr>().ok())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|ip| GatewayStatusAddresses {
                    r#type: Some("IPAddress".into()),
                    value: ip.to_string(),
                })
                .collect();
        }
    }
    points
        .into_iter()
        .filter_map(|p| {
            if let Some(ip) = p.ip {
                Some(GatewayStatusAddresses {
                    r#type: Some("IPAddress".to_string()),
                    value: ip,
                })
            } else {
                p.hostname.map(|h| GatewayStatusAddresses {
                    r#type: Some("Hostname".to_string()),
                    value: h,
                })
            }
        })
        .collect()
}

/// One entry of a route's `status.parents[]`, in the shape **every** Gateway
/// API route kind shares.
///
/// kopium emits a distinct struct per kind with no trait in common, so this is
/// the controller-side neutral one. It serialises to byte-identical JSON (same
/// field names, same `skip_serializing_if`), which is what makes it usable both
/// as what we read the current status into and as what we patch back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteParentStatus {
    pub conditions: Vec<Condition>,
    #[serde(rename = "controllerName")]
    pub controller_name: String,
    #[serde(rename = "parentRef")]
    pub parent_ref: RouteParentRef,
}

/// The `parentRef` a status entry answers for. Its identity is the **whole**
/// reference, `sectionName` and `port` included: a route may legally name the
/// same Gateway several times, once per listener, and each of those parentRefs
/// gets its own entry. Matching on `(name, namespace)` alone collapses them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RouteParentRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "sectionName"
    )]
    pub section_name: Option<String>,
}

/// A Gateway API route kind that reports per-parent status.
///
/// The one thing a route object must offer the writer is its current
/// `status.parents[]` — everything else (condition building, the loop-safety
/// comparison, the patch) is kind-independent. Implementations are mechanical
/// field copies rather than a serde round-trip so the compiler, not a runtime
/// shape mismatch, catches a generated struct that drifts.
pub trait RouteParents {
    fn route_parents(&self) -> Vec<RouteParentStatus>;
}

/// Implement [`RouteParents`] for one kopium-generated route kind. The bodies
/// are identical; only the types differ, and they have no trait in common to
/// abstract over.
macro_rules! impl_route_parents {
    ($kind:ty) => {
        impl RouteParents for $kind {
            fn route_parents(&self) -> Vec<RouteParentStatus> {
                self.status
                    .iter()
                    .flat_map(|s| s.parents.iter())
                    .map(|p| RouteParentStatus {
                        conditions: p.conditions.clone(),
                        controller_name: p.controller_name.clone(),
                        parent_ref: RouteParentRef {
                            group: p.parent_ref.group.clone(),
                            kind: p.parent_ref.kind.clone(),
                            name: p.parent_ref.name.clone(),
                            namespace: p.parent_ref.namespace.clone(),
                            port: p.parent_ref.port,
                            section_name: p.parent_ref.section_name.clone(),
                        },
                    })
                    .collect()
            }
        }
    };
}

impl_route_parents!(HttpRoute);
impl_route_parents!(TcpRoute);
impl_route_parents!(UdpRoute);

/// The `status.parents[]` we want on a route: every entry owned by another
/// controller or Gateway instance kept verbatim, plus our resolved parents.
/// Our instances sort only their own controller's entries. Foreign controllers
/// retain their relative order, avoiding competing list-order conventions.
///
/// Pure, so the loop-safety property is testable without an apiserver: feeding
/// this function its own output must be a fixed point, or the controller
/// re-patches on every reconcile.
fn route_parents(
    controller_name: &str,
    route: &RouteResult,
    current: &[RouteParentStatus],
    generation: Option<i64>,
    scope: &GatewayScope,
) -> Vec<RouteParentStatus> {
    let mut parents: Vec<RouteParentStatus> = current
        .iter()
        .filter(|p| {
            p.controller_name != controller_name || !owned_parent(scope, &route.namespace, p)
        })
        .cloned()
        .collect();

    for parent in route
        .parents
        .iter()
        .filter(|p| scope.owns_gateway(&p.gateway_namespace, &p.gateway_name))
    {
        let parent_ref = RouteParentRef {
            group: Some(GW_GROUP.to_string()),
            kind: Some("Gateway".to_string()),
            name: parent.gateway_name.clone(),
            namespace: Some(parent.gateway_namespace.clone()),
            port: parent.port,
            section_name: parent.section_name.clone(),
        };
        // Matched on the full reference. Keying on (name, namespace) made two
        // parentRefs to one Gateway that differ only by `sectionName` share a
        // single entry: each pass rebuilt the second one against the first
        // one's conditions, `lastTransitionTime` moved, the no-op guard never
        // held, and the controller re-patched forever.
        let existing = current
            .iter()
            .find(|p| p.controller_name == controller_name && p.parent_ref == parent_ref);
        let parent_problems: Vec<&Problem> = parent.problems.iter().collect();
        let conditions = build_conditions(
            &[
                Desired {
                    type_: "Accepted",
                    status: parent.accepted,
                    reason: parent.accepted_reason,
                    message: if parent.accepted {
                        "Route accepted by sozu-gateway".to_string()
                    } else {
                        problems_message(&parent_problems, "Route does not bind to this parent")
                    },
                },
                Desired {
                    type_: "ResolvedRefs",
                    status: parent.resolved_refs,
                    reason: parent.resolved_refs_reason,
                    message: if parent.resolved_refs {
                        "All backend references resolved".to_string()
                    } else {
                        problems_message(
                            &parent_problems,
                            "One or more backend references could not be resolved",
                        )
                    },
                },
            ],
            existing.map(|p| p.conditions.as_slice()),
            generation,
        );
        parents.push(RouteParentStatus {
            conditions,
            controller_name: controller_name.to_string(),
            parent_ref,
        });
    }
    let (mut foreign, mut ours): (Vec<_>, Vec<_>) = parents
        .into_iter()
        .partition(|p| p.controller_name != controller_name);
    ours.sort_by(|a, b| a.parent_ref.cmp(&b.parent_ref));
    foreign.extend(ours);
    foreign
}

fn owned_parent(scope: &GatewayScope, namespace: &str, parent: &RouteParentStatus) -> bool {
    let p = &parent.parent_ref;
    scope.owns_parent(
        namespace,
        p.group.as_deref(),
        p.kind.as_deref(),
        p.namespace.as_deref(),
        &p.name,
    )
}

/// A result computed from a specific cached generation. A status retry must
/// not label an older routing decision as observing a newer spec.
pub struct RouteStatusUpdate {
    route: RouteResult,
    generation: Option<i64>,
}

pub fn route_updates(
    results: &[RouteResult],
    inputs: &Inputs,
    controller_name: &str,
    scope: &GatewayScope,
) -> Vec<RouteStatusUpdate> {
    fn collect<K: RouteParents + Resource>(
        objects: &[Arc<K>],
        kind: RouteKind,
        results: &[RouteResult],
        controller_name: &str,
        scope: &GatewayScope,
        updates: &mut Vec<RouteStatusUpdate>,
    ) {
        for object in objects {
            let meta = object.meta();
            let namespace = meta.namespace.as_deref().unwrap_or("default");
            let name = meta.name.as_deref().unwrap_or_default();
            let result = results
                .iter()
                .find(|r| r.kind == kind && r.namespace == namespace && r.name == name);
            if result.is_none()
                && !object.route_parents().iter().any(|p| {
                    p.controller_name == controller_name && owned_parent(scope, namespace, p)
                })
            {
                continue;
            }
            // A route absent from build results may have lost its last parent
            // or its Gateway. Its old owned status still needs pruning.
            updates.push(RouteStatusUpdate {
                route: result.cloned().unwrap_or_else(|| RouteResult {
                    kind,
                    namespace: namespace.to_string(),
                    name: name.to_string(),
                    uid: meta.uid.clone(),
                    parents: vec![],
                }),
                generation: meta.generation,
            });
        }
    }
    let mut updates = Vec::new();
    collect(
        &inputs.http_routes,
        RouteKind::HttpRoute,
        results,
        controller_name,
        scope,
        &mut updates,
    );
    collect(
        &inputs.tcp_routes,
        RouteKind::TcpRoute,
        results,
        controller_name,
        scope,
        &mut updates,
    );
    collect(
        &inputs.udp_routes,
        RouteKind::UdpRoute,
        results,
        controller_name,
        scope,
        &mut updates,
    );
    updates
}

async fn write_route<K>(
    client: &Client,
    controller_name: &str,
    update: &RouteStatusUpdate,
    scope: &GatewayScope,
) -> Result<(), kube::Error>
where
    K: RouteParents + Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned,
    K: std::fmt::Debug,
    K::DynamicType: Default,
{
    let route = &update.route;
    let api: Api<K> = Api::namespaced(client.clone(), &route.namespace);
    let mut conflicts = 0;
    loop {
        let current = api.get(&route.name).await?;
        if current.meta().generation != update.generation || current.meta().uid != route.uid {
            // The watch will rebuild the changed or recreated route. Reusing
            // this stale result would claim to have observed an unseen spec.
            return Ok(());
        }
        let current_parents = current.route_parents();
        let parents = route_parents(
            controller_name,
            route,
            &current_parents,
            update.generation,
            scope,
        );
        if current_parents == parents {
            return Ok(());
        }
        let version = current.meta().resource_version.as_ref().ok_or_else(|| {
            kube::Error::Service(Box::new(std::io::Error::other(
                "route has no resourceVersion",
            )))
        })?;
        let patch =
            json!({ "metadata": { "resourceVersion": version }, "status": { "parents": parents } });
        match api
            .patch_status(&route.name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            Err(kube::Error::Api(error)) if error.code == 409 && conflicts < 2 => {
                conflicts += 1;
                // Another instance wrote between GET and PATCH. Re-read its
                // entries and merge again, with at most three total attempts.
            }
            Err(error) => return Err(error),
            Ok(_) => {
                debug!(kind = route.kind.as_str(), namespace = %route.namespace, name = %route.name, "route status updated");
                return Ok(());
            }
        }
    }
}

// ---- Ingress status (.status.loadBalancer.ingress) -------------------------

/// Map the publish Service's load-balancer address(es) into the shape an Ingress
/// status expects. Pure, so it is unit-tested without a cluster.
///
/// The result is sorted by `(ip, hostname)` so the order is independent of the
/// Service status's array order. The loop-safety guard in [`write_one_ingress`]
/// compares element-wise, so without this a provider that re-orders its
/// `loadBalancer.ingress` between reads would cause endless no-op re-patches.
pub(crate) fn lb_points(svc: &Service) -> Vec<IngressLoadBalancerIngress> {
    let mut points: Vec<IngressLoadBalancerIngress> = svc
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_ref())
        .map(|points| {
            points
                .iter()
                .map(|p| IngressLoadBalancerIngress {
                    hostname: p.hostname.clone(),
                    ip: p.ip.clone(),
                    ports: None,
                })
                .collect()
        })
        .unwrap_or_default();
    points.sort_by(|a, b| (&a.ip, &a.hostname).cmp(&(&b.ip, &b.hostname)));
    points
}

/// Publish the gateway's external address into each managed Ingress's
/// `.status.loadBalancer.ingress`. Loop-safe (skips no-op patches) and
/// best-effort. Does nothing when there is no address yet, so a still-pending
/// LoadBalancer never clears an Ingress's status.
pub async fn write_ingress_status(
    client: &Client,
    ingresses: &[IngressResult],
    points: &[IngressLoadBalancerIngress],
) {
    if points.is_empty() {
        return;
    }
    for r in ingresses {
        if let Err(e) = write_one_ingress(client, &r.namespace, &r.name, points).await {
            warn!(namespace = %r.namespace, name = %r.name, error = %e, "failed to write Ingress status");
        }
    }
}

async fn write_one_ingress(
    client: &Client,
    namespace: &str,
    name: &str,
    points: &[IngressLoadBalancerIngress],
) -> Result<(), kube::Error> {
    let api: Api<Ingress> = Api::namespaced(client.clone(), namespace);
    let current = api.get(name).await?;
    let cur = current
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_deref())
        .unwrap_or_default();
    if cur == points {
        return Ok(()); // already published — skip to stay loop-safe
    }
    let patch = json!({ "status": { "loadBalancer": { "ingress": points } } });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(namespace = %namespace, name = %name, "Ingress status updated");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sozu_gw_builder::RouteParentResult;

    fn parent(section: Option<&str>, accepted: bool) -> RouteParentResult {
        RouteParentResult {
            gateway_namespace: "sozu-system".to_string(),
            gateway_name: "gw".to_string(),
            section_name: section.map(str::to_string),
            port: None,
            accepted,
            accepted_reason: if accepted {
                "Accepted"
            } else {
                "NoMatchingParent"
            },
            resolved_refs: true,
            resolved_refs_reason: "ResolvedRefs",
            problems: vec![],
        }
    }

    fn route(parents: Vec<RouteParentResult>) -> RouteResult {
        RouteResult {
            kind: RouteKind::HttpRoute,
            namespace: "demo".to_string(),
            name: "web".to_string(),
            uid: Some("11111111-2222-3333-4444-555555555555".to_string()),
            parents,
        }
    }

    /// Two parentRefs to one Gateway that differ only by `sectionName` must
    /// produce two *distinguishable* status entries, or neither can be matched
    /// back to its own conditions on the next pass. This is the normal shape
    /// for a layer-4 route, where a Gateway may declare several listeners on
    /// one port and `sectionName` is the only way to pick one.
    #[test]
    fn parent_entries_carry_section_name_and_port() {
        let mut with_port = parent(Some("b"), true);
        with_port.port = Some(8443);
        let r = route(vec![parent(Some("a"), true), with_port]);
        let parents = route_parents(
            "sozu.io/gateway-controller",
            &r,
            &[],
            Some(3),
            &GatewayScope::default(),
        );

        assert_eq!(parents.len(), 2);
        assert_eq!(parents[0].parent_ref.section_name.as_deref(), Some("a"));
        assert_eq!(parents[1].parent_ref.section_name.as_deref(), Some("b"));
        assert_eq!(parents[1].parent_ref.port, Some(8443));
        assert_ne!(parents[0].parent_ref, parents[1].parent_ref);
    }

    /// The loop-safety contract, mechanically: feeding the writer its own
    /// output must change nothing, so the PATCH is skipped. With two parents
    /// differing only by `sectionName` and *different* conditions this used to
    /// fail — both matched the first stored entry, the second one's
    /// `lastTransitionTime` moved every pass, and the controller re-patched on
    /// every reconcile forever.
    #[test]
    fn rebuilding_from_our_own_status_is_a_fixed_point() {
        let controller = "sozu.io/gateway-controller";
        let r = route(vec![parent(Some("a"), true), parent(Some("b"), false)]);
        let first = route_parents(controller, &r, &[], Some(3), &GatewayScope::default());
        let second = route_parents(controller, &r, &first, Some(3), &GatewayScope::default());
        assert_eq!(first, second, "a second pass must be a no-op");
    }

    /// Entries written by another controller are carried through untouched:
    /// a route may be attached to somebody else's Gateway as well as ours.
    #[test]
    fn other_controllers_entries_are_preserved() {
        let controller = "sozu.io/gateway-controller";
        let theirs = RouteParentStatus {
            conditions: vec![],
            controller_name: "example.net/other".to_string(),
            parent_ref: RouteParentRef {
                group: Some(GW_GROUP.to_string()),
                kind: Some("Gateway".to_string()),
                name: "other-gw".to_string(),
                namespace: Some("other".to_string()),
                port: None,
                section_name: None,
            },
        };
        let parents = route_parents(
            controller,
            &route(vec![parent(None, true)]),
            std::slice::from_ref(&theirs),
            None,
            &GatewayScope::default(),
        );
        assert_eq!(parents.len(), 2);
        assert!(parents.contains(&theirs));
    }

    #[test]
    fn gateway_listener_acceptance_conditions_are_correct_and_stable() {
        for (protocols, parameters, accepted, reason) in [
            (vec!["HTTP"], false, "True", "Accepted"),
            (
                vec!["example.com/unsupported"],
                false,
                "False",
                "ListenersNotValid",
            ),
            (
                vec!["HTTP", "example.com/unsupported"],
                false,
                "True",
                "ListenersNotValid",
            ),
            (vec!["HTTP"], true, "False", "InvalidParameters"),
        ] {
            let listeners: Vec<_> = protocols
                .iter()
                .enumerate()
                .map(|(index, protocol)| {
                    json!({ "name": format!("listener-{index}"), "protocol": protocol, "port": 80 })
                })
                .collect();
            let infrastructure = parameters.then(|| json!({
                "parametersRef": { "group": "invalid.io", "kind": "InvalidParameters", "name": "invalid" }
            }));
            let mut current: Gateway = serde_json::from_value(json!({
                "metadata": { "name": "gw", "namespace": "demo", "generation": 3 },
                "spec": { "gatewayClassName": "sozu", "listeners": listeners, "infrastructure": infrastructure }
            }))
            .unwrap();
            let class: GatewayClass = serde_json::from_value(json!({
                "metadata": { "name": "sozu" },
                "spec": { "controllerName": "sozu.io/gateway-controller" }
            }))
            .unwrap();
            let inputs = sozu_gw_builder::Inputs {
                gateway_classes: vec![std::sync::Arc::new(class)],
                gateways: vec![std::sync::Arc::new(current.clone())],
                ..Default::default()
            };
            let result = sozu_gw_builder::build(&sozu_gw_builder::BuildConfig::default(), &inputs);
            let gateway = &result.gateways[0];
            let mut conditions = build_gateway_conditions(gateway, &current, false);
            assert_eq!(conditions[0].status, accepted);
            assert_eq!(conditions[0].reason, reason);
            assert_eq!(conditions[0].observed_generation, Some(3));
            if reason == "ListenersNotValid" {
                assert!(conditions[0].message.contains("example.com/unsupported"));
            }
            if parameters {
                assert_eq!(conditions[1].status, "False");
                assert!(conditions[0]
                    .message
                    .contains("invalid.io/InvalidParameters demo/invalid"));
                assert!(conditions[0].message.contains("not supported"));
            }

            let mut listeners = build_listeners_status(gateway, &current, Some(3));
            for (listener, protocol) in listeners.iter().zip(&protocols) {
                assert_eq!(
                    listener.conditions[0].reason,
                    if *protocol != "HTTP" {
                        "UnsupportedProtocol"
                    } else if parameters {
                        "Invalid"
                    } else {
                        "Accepted"
                    }
                );
            }

            // Existing timestamps make accidental renewal visible even when
            // two reconciles run within the same clock tick.
            let previous = Time("2000-01-01T00:00:00Z".parse().unwrap());
            for condition in &mut conditions {
                condition.last_transition_time = previous.clone();
            }
            for listener in &mut listeners {
                for condition in &mut listener.conditions {
                    condition.last_transition_time = previous.clone();
                }
            }
            current.status = Some(
                serde_json::from_value(json!({
                    "conditions": conditions,
                    "listeners": listeners
                }))
                .unwrap(),
            );
            let rebuilt = build_gateway_conditions(gateway, &current, false);
            assert!(
                conditions_equal(&conditions, &rebuilt),
                "reconciliation must be a no-op"
            );
            assert_eq!(
                serde_json::to_value(&listeners).unwrap(),
                serde_json::to_value(build_listeners_status(gateway, &current, Some(3))).unwrap()
            );

            current.metadata.generation = Some(4);
            let updated = build_gateway_conditions(gateway, &current, false);
            assert!(!conditions_equal(&conditions, &updated));
            for condition in updated {
                assert_eq!(condition.observed_generation, Some(4));
                assert_eq!(condition.last_transition_time, previous);
            }
        }
    }

    fn features(names: &[&str]) -> Vec<GatewayClassStatusSupportedFeatures> {
        names
            .iter()
            .map(|n| GatewayClassStatusSupportedFeatures {
                name: n.to_string(),
            })
            .collect()
    }

    /// The guard that decides whether to PATCH must look at the features, not
    /// only the conditions. A list changed by a Gateway API bump travels with
    /// conditions that did not move, so a conditions-only guard would compute
    /// the new list and then never write it.
    #[test]
    fn the_no_op_guard_notices_a_changed_feature_list() {
        assert!(features_equal(
            Some(&features(&["A", "B"])),
            &features(&["A", "B"])
        ));
        assert!(!features_equal(
            Some(&features(&["A"])),
            &features(&["A", "B"])
        ));
        assert!(!features_equal(
            Some(&features(&["A", "B"])),
            &features(&["A", "C"])
        ));
        // Never written before: `None` is not an empty list. Claiming nothing is
        // a statement, and it has to reach the object once.
        assert!(!features_equal(None, &features(&[])));
        assert!(features_equal(Some(&[]), &features(&[])));
    }

    /// The CRD keys `supportedFeatures` on `name` and requires ascending order.
    /// Enforced here because the constant is hand-maintained against the
    /// conformance run log, and a misordered patch is rejected by the apiserver.
    #[test]
    fn declared_features_are_sorted_and_unique() {
        let mut sorted = SUPPORTED_FEATURES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.as_slice(), SUPPORTED_FEATURES);
    }

    #[test]
    fn problems_message_is_deterministic_deduped_and_capped() {
        assert_eq!(problems_message(&[], "fallback"), "fallback");

        // Order-insensitive and deduped: the message participates in
        // lastTransitionTime reuse, so it must not flap across reconciles.
        let a = Problem::ServiceNotFound {
            service: "z".into(),
        };
        let b = Problem::ServiceNotFound {
            service: "a".into(),
        };
        let one = problems_message(&[&a, &b, &a], "");
        let two = problems_message(&[&b, &a, &b], "");
        assert_eq!(one, two);
        assert_eq!(one.matches("\"z\"").count(), 1, "duplicates collapse");

        let many: Vec<Problem> = (0..8)
            .map(|i| Problem::ServiceNotFound {
                service: format!("s{i}"),
            })
            .collect();
        let refs: Vec<&Problem> = many.iter().collect();
        assert!(problems_message(&refs, "").ends_with("(+3 more)"));
    }

    fn svc_with_ips(ips: &[&str]) -> Service {
        let ingress: Vec<_> = ips.iter().map(|ip| json!({ "ip": ip })).collect();
        serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" },
            "status": { "loadBalancer": { "ingress": ingress } }
        }))
        .unwrap()
    }

    #[test]
    fn lb_points_extracts_ip_and_hostname() {
        let svc: Service = serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" },
            "status": { "loadBalancer": { "ingress": [
                { "ip": "1.2.3.4" },
                { "hostname": "lb.example.com" }
            ] } }
        }))
        .unwrap();
        let pts = lb_points(&svc);
        assert_eq!(pts.len(), 2);
        assert!(pts.iter().any(|p| p.ip.as_deref() == Some("1.2.3.4")));
        assert!(pts
            .iter()
            .any(|p| p.hostname.as_deref() == Some("lb.example.com")));
    }

    #[test]
    fn lb_points_order_is_canonical() {
        // Same address set in two different Service orders must map to the same
        // (sorted) Vec, so the loop-safety comparison never flips on reorder.
        let a = lb_points(&svc_with_ips(&["10.0.0.2", "10.0.0.1"]));
        let b = lb_points(&svc_with_ips(&["10.0.0.1", "10.0.0.2"]));
        assert_eq!(a, b);
        assert_eq!(a[0].ip.as_deref(), Some("10.0.0.1"));
    }

    #[test]
    fn lb_points_empty_when_no_loadbalancer_status() {
        let svc: Service = serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" }
        }))
        .unwrap();
        assert!(lb_points(&svc).is_empty());
    }

    #[test]
    fn gateway_addresses_typed_from_lb() {
        let svc: Service = serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" },
            "status": { "loadBalancer": { "ingress": [
                { "ip": "1.2.3.4" },
                { "hostname": "lb.example.com" }
            ] } }
        }))
        .unwrap();
        let addrs = gateway_addresses(&svc);
        assert_eq!(addrs.len(), 2);
        assert!(addrs
            .iter()
            .any(|a| a.r#type.as_deref() == Some("IPAddress") && a.value == "1.2.3.4"));
        assert!(addrs
            .iter()
            .any(|a| a.r#type.as_deref() == Some("Hostname") && a.value == "lb.example.com"));
    }

    #[test]
    fn only_supported_service_types_wait_for_an_address() {
        for (kind, expected) in [
            ("LoadBalancer", Some(0)),
            ("ClusterIP", Some(0)),
            ("NodePort", None),
            ("ExternalName", None),
        ] {
            let service: Service = serde_json::from_value(json!({"spec": {"type": kind}})).unwrap();
            assert_eq!(
                published_gateway_addresses(&service).map(|a| a.len()),
                expected,
                "{kind}"
            );
        }
    }

    #[test]
    fn scopes_preserve_other_parents_and_remove_only_their_own() {
        let controller = "sozu.io/gateway-controller";
        let a = GatewayScope {
            gateway_scope: Some("sozu-system/gw".parse().unwrap()),
            ..Default::default()
        };
        let b = GatewayScope {
            exclude_gateway: vec!["sozu-system/gw".parse().unwrap()],
            ..Default::default()
        };
        let ours = route(vec![parent(Some("http"), true)]);
        let mut other_parent = parent(Some("https"), true);
        other_parent.gateway_name = "other".into();
        let theirs = route(vec![other_parent]);
        let first = route_parents(controller, &ours, &[], Some(1), &a);
        let combined = route_parents(controller, &theirs, &first, Some(1), &b);
        assert_eq!(combined.len(), 2);
        assert_eq!(
            combined,
            route_parents(controller, &ours, &combined, Some(1), &a)
        );
        assert_eq!(
            combined,
            route_parents(controller, &theirs, &combined, Some(1), &b)
        );
        let mut foreign = combined[0].clone();
        foreign.parent_ref.group = Some("example.net".into());
        foreign.parent_ref.kind = Some("CustomParent".into());
        foreign.parent_ref.port = Some(1234);
        let mut current = combined;
        current.push(foreign.clone());
        let removed = route_parents(controller, &route(vec![]), &current, Some(2), &a);
        assert_eq!(removed.len(), 2);
        assert!(
            removed.contains(&foreign),
            "unknown references must survive verbatim"
        );
        assert!(removed.iter().any(|p| p.parent_ref.name == "other"));
    }

    #[test]
    fn foreign_controller_order_is_preserved_across_instance_writers() {
        let controller = "sozu.io/gateway-controller";
        let a = GatewayScope {
            gateway_scope: Some("sozu-system/gw".parse().unwrap()),
            ..Default::default()
        };
        let b = GatewayScope {
            exclude_gateway: vec!["sozu-system/gw".parse().unwrap()],
            ..Default::default()
        };
        let ours = route(vec![parent(Some("http"), true)]);
        let mut other = parent(Some("https"), true);
        other.gateway_name = "other".into();
        let other = route(vec![other]);
        let first = route_parents(controller, &ours, &[], Some(1), &a);
        let mut foreign_z = first[0].clone();
        foreign_z.controller_name = "zzz.example/controller".into();
        let mut foreign_a = first[0].clone();
        foreign_a.controller_name = "aaa.example/controller".into();
        let mixed = vec![foreign_z.clone(), first[0].clone(), foreign_a.clone()];
        let updated = route_parents(controller, &other, &mixed, Some(1), &b);
        assert_eq!(&updated[..2], &[foreign_z, foreign_a]);
        assert_eq!(updated.len(), 4);
        assert_eq!(
            updated,
            route_parents(controller, &ours, &updated, Some(1), &a)
        );
        assert_eq!(
            updated,
            route_parents(controller, &other, &updated, Some(1), &b)
        );
    }

    #[test]
    fn routes_losing_their_last_parent_still_get_a_status_update() {
        let controller = "sozu.io/gateway-controller";
        let scope = GatewayScope {
            gateway_scope: Some("sozu-system/gw".parse().unwrap()),
            ..Default::default()
        };
        let result = route(vec![parent(None, true)]);
        let parents = route_parents(controller, &result, &[], Some(1), &scope);
        let mut inputs = Inputs::default();
        inputs.http_routes.push(Arc::new(
            serde_json::from_value(json!({
                "metadata":{"namespace":"demo","name":"web","uid":result.uid,"generation":2},
                "spec":{"parentRefs":[{"name":"other","namespace":"sozu-system"}]},
                "status":{"parents":parents}
            }))
            .unwrap(),
        ));
        let updates = route_updates(&[], &inputs, controller, &scope);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].route.parents.is_empty());
        assert_eq!(updates[0].generation, Some(2));
        assert!(route_parents(controller, &updates[0].route, &parents, Some(2), &scope).is_empty());
        let unowned = GatewayScope {
            exclude_gateway: vec!["sozu-system/gw".parse().unwrap()],
            ..Default::default()
        };
        assert!(route_updates(&[], &inputs, controller, &unowned).is_empty());
    }

    #[test]
    fn cluster_ip_addresses_are_internal_and_pending_load_balancers_stay_pending() {
        let mut service: Service = serde_json::from_value(json!({
            "spec":{"type":"ClusterIP","clusterIP":"10.0.0.10","clusterIPs":["10.0.0.10","fd00::10"]}
        })).unwrap();
        let addresses = gateway_addresses(&service);
        assert_eq!(addresses.len(), 2);
        assert!(addresses
            .iter()
            .all(|a| a.r#type.as_deref() == Some("IPAddress")));
        service.spec.as_mut().unwrap().type_ = Some("LoadBalancer".into());
        assert!(gateway_addresses(&service).is_empty());
        service.spec.as_mut().unwrap().type_ = Some("ClusterIP".into());
        service.spec.as_mut().unwrap().cluster_ips = Some(vec!["None".into()]);
        service.spec.as_mut().unwrap().cluster_ip = Some("None".into());
        assert!(gateway_addresses(&service).is_empty());
    }

    struct MockApi {
        object: serde_json::Value,
        gets: usize,
        patches: usize,
        conflicts: usize,
        reject_all: bool,
    }

    fn mock_client(
        object: serde_json::Value,
        rendezvous: bool,
        reject_all: bool,
    ) -> (Client, Arc<std::sync::Mutex<MockApi>>) {
        use http_body_util::BodyExt;
        use kube::client::Body;
        let state = Arc::new(std::sync::Mutex::new(MockApi {
            object,
            gets: 0,
            patches: 0,
            conflicts: 0,
            reject_all,
        }));
        let shared = state.clone();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let service = tower::service_fn(move |request: http::Request<Body>| {
            let shared = shared.clone();
            let barrier = barrier.clone();
            async move {
                let (status, body) = if request.method() == http::Method::GET {
                    let (object, initial) = {
                        let mut state = shared.lock().unwrap();
                        state.gets += 1;
                        (state.object.clone(), state.gets <= 2)
                    };
                    if rendezvous && initial {
                        barrier.wait().await;
                    }
                    (200, object)
                } else {
                    assert_eq!(request.method(), http::Method::PATCH);
                    let bytes = request.into_body().collect().await.unwrap().to_bytes();
                    let patch: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    let mut state = shared.lock().unwrap();
                    state.patches += 1;
                    let versioned = state.object["kind"] == "HTTPRoute";
                    if versioned {
                        assert!(patch["metadata"]["resourceVersion"].is_string());
                    }
                    if state.reject_all
                        || (versioned
                            && patch["metadata"]["resourceVersion"]
                                != state.object["metadata"]["resourceVersion"])
                    {
                        state.conflicts += 1;
                        (
                            409,
                            json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Conflict","message":"resourceVersion changed","code":409}),
                        )
                    } else {
                        let version: u64 = state.object["metadata"]["resourceVersion"]
                            .as_str()
                            .unwrap()
                            .parse()
                            .unwrap();
                        state.object["metadata"]["resourceVersion"] =
                            json!((version + 1).to_string());
                        if !state.object["status"].is_object() {
                            state.object["status"] = json!({});
                        }
                        for (key, value) in patch["status"].as_object().unwrap() {
                            state.object["status"][key] = value.clone();
                        }
                        (200, state.object.clone())
                    }
                };
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(status)
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
            }
        });
        (Client::new(service, "demo"), state)
    }

    fn current_route() -> serde_json::Value {
        json!({
            "apiVersion":"gateway.networking.k8s.io/v1","kind":"HTTPRoute",
            "metadata":{"namespace":"demo","name":"web","uid":"11111111-2222-3333-4444-555555555555","generation":1,"resourceVersion":"1"},
            "spec":{},"status":{"parents":[]}
        })
    }

    #[tokio::test]
    async fn concurrent_same_controller_writers_retry_without_losing_parents() {
        let controller = "sozu.io/gateway-controller";
        let (client, state) = mock_client(current_route(), true, false);
        let a = GatewayScope {
            gateway_scope: Some("sozu-system/gw".parse().unwrap()),
            ..Default::default()
        };
        let b = GatewayScope {
            exclude_gateway: vec!["sozu-system/gw".parse().unwrap()],
            ..Default::default()
        };
        let first = RouteStatusUpdate {
            route: route(vec![parent(Some("http"), true)]),
            generation: Some(1),
        };
        let mut other = parent(Some("https"), true);
        other.gateway_name = "other".into();
        let second = RouteStatusUpdate {
            route: route(vec![other]),
            generation: Some(1),
        };
        let (x, y) = tokio::join!(
            write_route::<HttpRoute>(&client, controller, &first, &a),
            write_route::<HttpRoute>(&client, controller, &second, &b),
        );
        x.unwrap();
        y.unwrap();
        {
            let state = state.lock().unwrap();
            assert_eq!(state.patches, 3);
            assert_eq!(state.conflicts, 1);
            assert_eq!(
                state.object["status"]["parents"].as_array().unwrap().len(),
                2
            );
        }
        write_route::<HttpRoute>(&client, controller, &first, &a)
            .await
            .unwrap();
        write_route::<HttpRoute>(&client, controller, &second, &b)
            .await
            .unwrap();
        assert_eq!(
            state.lock().unwrap().patches,
            3,
            "both writers settle on the same order"
        );
    }

    #[tokio::test]
    async fn route_status_conflicts_are_bounded_and_new_specs_are_not_misreported() {
        let update = RouteStatusUpdate {
            route: route(vec![parent(None, true)]),
            generation: Some(1),
        };
        let (client, state) = mock_client(current_route(), false, true);
        let result = write_route::<HttpRoute>(
            &client,
            "sozu.io/gateway-controller",
            &update,
            &GatewayScope::default(),
        )
        .await;
        assert!(matches!(result, Err(kube::Error::Api(error)) if error.code == 409));
        assert_eq!(state.lock().unwrap().patches, 3);
        let mut changed = current_route();
        changed["metadata"]["generation"] = json!(2);
        let (client, state) = mock_client(changed, false, false);
        write_route::<HttpRoute>(
            &client,
            "sozu.io/gateway-controller",
            &update,
            &GatewayScope::default(),
        )
        .await
        .unwrap();
        assert_eq!(state.lock().unwrap().patches, 0);
    }

    #[tokio::test]
    async fn pending_address_blocks_programmed_until_the_new_service_is_assigned() {
        let old = json!([{ "type": "IPAddress", "value": "198.51.100.10" }]);
        let current = json!({
            "apiVersion":"gateway.networking.k8s.io/v1", "kind":"Gateway",
            "metadata":{"namespace":"demo", "name":"gw", "generation":1, "resourceVersion":"1"},
            "spec":{"gatewayClassName":"sozu", "listeners":[]},
            "status":{"addresses":old}
        });
        let gateway = GatewayResult {
            namespace: "demo".into(),
            name: "gw".into(),
            uid: None,
            accepted: true,
            accepted_reason: "Accepted",
            programmed: true,
            problems: vec![],
            listeners: vec![],
        };
        let (client, state) = mock_client(current, false, false);
        write_gateway(&client, &gateway, Some(&[])).await.unwrap();
        {
            let state = state.lock().unwrap();
            let condition = state.object["status"]["conditions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["type"] == "Programmed")
                .unwrap();
            assert_eq!(condition["status"], "False");
            assert_eq!(condition["reason"], "AddressNotAssigned");
            assert_eq!(
                state.object["status"]["addresses"], old,
                "unattributed previous addresses are not blindly removed"
            );
            assert_eq!(state.patches, 1);
        }
        write_gateway(&client, &gateway, Some(&[])).await.unwrap();
        assert_eq!(state.lock().unwrap().patches, 1, "waiting is a fixed point");
        let assigned = vec![GatewayStatusAddresses {
            r#type: Some("IPAddress".into()),
            value: "198.51.100.20".into(),
        }];
        write_gateway(&client, &gateway, Some(&assigned))
            .await
            .unwrap();
        {
            let state = state.lock().unwrap();
            let condition = state.object["status"]["conditions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["type"] == "Programmed")
                .unwrap();
            assert_eq!(condition["status"], "True");
            assert_eq!(condition["reason"], "Programmed");
            assert_eq!(state.object["status"]["addresses"], json!(assigned));
            assert_eq!(state.patches, 2);
        }
        write_gateway(&client, &gateway, Some(&assigned))
            .await
            .unwrap();
        assert_eq!(
            state.lock().unwrap().patches,
            2,
            "assigned is a fixed point"
        );
        write_gateway(&client, &gateway, None).await.unwrap();
        assert_eq!(
            state.lock().unwrap().patches,
            2,
            "deployments without a publish Service retain the programming status"
        );
    }

    #[tokio::test]
    async fn only_the_default_instance_writes_gatewayclass_status() {
        let (client, state) = mock_client(
            json!({
                "apiVersion":"gateway.networking.k8s.io/v1","kind":"GatewayClass",
                "metadata":{"name":"sozu","generation":1,"resourceVersion":"1"},
                "spec":{"controllerName":"sozu.io/gateway-controller"}
            }),
            false,
            false,
        );
        let scoped = GatewayScope {
            gateway_scope: Some("demo/gw".parse().unwrap()),
            ..Default::default()
        };
        let classes = [GatewayClassResult {
            name: "sozu".into(),
            accepted: true,
        }];
        write_status(
            &client,
            "sozu.io/gateway-controller",
            &classes,
            &[],
            &[],
            None,
            &scoped,
        )
        .await;
        assert_eq!(state.lock().unwrap().gets, 0);
        write_status(
            &client,
            "sozu.io/gateway-controller",
            &classes,
            &[],
            &[],
            None,
            &GatewayScope::default(),
        )
        .await;
        assert_eq!(state.lock().unwrap().gets, 1);
        assert_eq!(state.lock().unwrap().patches, 1);
    }
}
