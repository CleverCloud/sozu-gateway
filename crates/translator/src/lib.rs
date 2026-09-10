//! Translator: pure IR → Sōzu protobuf commands.
//!
//! Side-effect free and golden-file tested. Routing changes reuse Sōzu's own
//! `ConfigState::diff`, with two deliberately separate paths:
//!  - **HTTP/HTTPS frontends**: preserve path precedence in Sōzu's ordered
//!    lists when adding, removing or repointing routes.
//!  - **Certificates**: we diff them ourselves, by fingerprint. This (a) lets us
//!    emit `ReplaceCertificate` for zero-gap rotation, and (b) avoids a
//!    debug-assert in sozu-command-lib 2.1.0 that fires when `ConfigState::diff`
//!    removes the last certificate at a listener address (an empty cert bucket
//!    is left behind and the replay check is not normalised for it).
//!
//! Output is canonicalised into dependency-safe tiers (adds: clusters →
//! backends → certificates → frontends; removes in reverse). Frontend *removes*
//! are ordered before frontend *adds*: Sōzu keys a route by host+path (not by
//! cluster_id), so re-pointing a route at another cluster is a Remove+Add on the
//! same key, and adding first would be rejected as a duplicate. A new/replacement
//! certificate lands before the old one is removed. A name-only update of the
//! same certificate needs removal before addition, with a brief TLS gap: Sōzu
//! ignores ReplaceCertificate when the fingerprint is unchanged. Ordering also
//! makes the otherwise HashSet-ordered routing diff deterministic.
#![forbid(unsafe_code)]

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;

use sozu_command_lib::proto::command::{
    request::RequestType, ActivateListener, AddBackend, AddCertificate, CertificateAndKey, Cluster,
    Header, HeaderPosition, ListenerType, LoadBalancingAlgorithms, LoadBalancingParams, PathRule,
    PathRuleKind, RedirectPolicy, RedirectScheme, RemoveCertificate, ReplaceCertificate, Request,
    RequestHttpFrontend, RequestTcpFrontend, RequestUdpFrontend, RulePosition, TcpListenerConfig,
    UdpAffinityKey, UdpClusterConfig, UdpListenerConfig,
};
use sozu_command_lib::state::ConfigState;
use sozu_gw_ir as ir;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TranslatorError {
    #[error("failed to fold request into ConfigState: {0}")]
    Dispatch(String),
    #[error("invalid certificate (cannot compute fingerprint): {0}")]
    Certificate(String),
    #[error("conflicting L4 frontends: {0}")]
    L4Conflict(String),
}

// ----------------------------------------------------------------------------
// IR element -> request payloads
// ----------------------------------------------------------------------------

fn lb_algorithm(algo: ir::LbAlgorithm) -> i32 {
    let v = match algo {
        ir::LbAlgorithm::RoundRobin => LoadBalancingAlgorithms::RoundRobin,
        ir::LbAlgorithm::Random => LoadBalancingAlgorithms::Random,
        ir::LbAlgorithm::LeastLoaded => LoadBalancingAlgorithms::LeastLoaded,
        ir::LbAlgorithm::PowerOfTwo => LoadBalancingAlgorithms::PowerOfTwo,
    };
    v as i32
}

/// Escape every regex metacharacter so a literal path is matched literally.
fn regex_escape(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    for c in literal.chars() {
        if r"\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn rule(kind: PathRuleKind, value: String) -> PathRule {
    PathRule {
        kind: kind as i32,
        value,
    }
}

/// The Sōzu path rule one IR path match compiles to.
///
/// `PathMatch::Prefix` carries **Kubernetes** prefix semantics: it matches on
/// path *element* boundaries, so `/foo` covers `/foo`, `/foo?page=2` and
/// `/foo/bar`, but never `/foobar`. Sōzu's `PathRuleKind::Prefix` is a plain
/// string prefix and matches all four, so a non-root prefix compiles to an
/// anchored regex instead.
///
/// The alternation is what makes it a *boundary*: after the literal the target
/// must end, continue with `/`, or start its query string. Sōzu matches path
/// rules against the request target with the query string still attached
/// (verified against a live Sōzu: without the `\?` branch, `/foo?page=2` 404s),
/// and it does **not** anchor regexes on its own — an unanchored `/foo(/|$)`
/// also matches `/xx/foo`, so the `^` is load-bearing. `PROTOCOL.md` left this
/// as an open question; both halves are now measured.
///
/// The root `/` stays a plain prefix. Exact paths use a regex too: matching
/// must ignore the query string, and Sōzu 2.2.1 cannot remove `Equals` rules
/// because its worker-side `PathRule::eq` has no `Equals` arm. The exact regex
/// deliberately differs from a prefix regex at the same path, keeping both
/// routes on separate Sōzu keys. Raw regexes pass through unchanged.
///
/// Kubernetes treats a trailing slash as insignificant (`/foo/` ≡ `/foo`), so it
/// is trimmed first — otherwise the two spellings of one route would diff
/// forever.
fn path_rule(path: &ir::PathMatch) -> PathRule {
    match path {
        ir::PathMatch::Exact(v) => {
            rule(PathRuleKind::Regex, format!("^{}(\\?|$)", regex_escape(v)))
        }
        ir::PathMatch::Regex(v) => rule(PathRuleKind::Regex, v.clone()),
        ir::PathMatch::Prefix(v) => {
            let trimmed = v.trim_end_matches('/');
            if trimmed.is_empty() {
                rule(PathRuleKind::Prefix, "/".to_string())
            } else {
                rule(
                    PathRuleKind::Regex,
                    format!("^{}(/|\\?|$)", regex_escape(trimmed)),
                )
            }
        }
    }
}

fn cluster_request(c: &ir::Cluster, udp: bool) -> Request {
    RequestType::AddCluster(Cluster {
        cluster_id: c.id.clone(),
        sticky_session: c.sticky_session,
        https_redirect: c.https_redirect,
        load_balancing: lb_algorithm(c.load_balancing),
        max_connections_per_ip: c.max_connections_per_ip,
        retry_after: c.retry_after,
        // Distinct sockets behind one source IP are distinct UDP clients.
        // Sōzu's source-IP-only default otherwise reuses the first socket's
        // return address for later datagrams from other source ports.
        udp: udp.then(|| UdpClusterConfig {
            affinity_key: Some(UdpAffinityKey::SourceIpPort as i32),
            ..Default::default()
        }),
        ..Default::default()
    })
    .into()
}

fn cluster_requests(ir: &ir::Ir) -> Vec<Request> {
    let udp_clusters: BTreeSet<&str> = ir
        .l4_frontends
        .iter()
        .filter(|frontend| frontend.protocol == ir::L4Protocol::Udp)
        .map(|frontend| frontend.cluster_id.as_str())
        .collect();
    ir.clusters
        .iter()
        .map(|cluster| cluster_request(cluster, udp_clusters.contains(cluster.id.as_str())))
        .collect()
}

fn backend_request(b: &ir::Backend) -> Request {
    RequestType::AddBackend(AddBackend {
        cluster_id: b.cluster_id.clone(),
        backend_id: b.backend_id.clone(),
        // Always use the crate's conversion — never hand-pack the address.
        address: b.address.into(),
        sticky_id: None,
        load_balancing_parameters: b.weight.map(|weight| LoadBalancingParams { weight }),
        backup: None,
    })
    .into()
}

fn frontend_request(f: &ir::Frontend) -> Request {
    // The trie wildcard covers one label only. Gateway wildcards use a full
    // DomainRule regex in POST, after exact-host TREE rules. TLS certificate
    // names never pass through this conversion.
    let hostname = if f.multi_label_wildcard {
        f.hostname
            .strip_prefix("*.")
            .map(|suffix| format!(r"/[^.]+(?:\.[^.]+)*\.{}/", regex_escape(suffix)))
            .unwrap_or_else(|| f.hostname.clone())
    } else {
        f.hostname.clone()
    };
    // A bare "*" catch-all is also a POST fallback.
    let position = if f.hostname == "*" || hostname != f.hostname {
        RulePosition::Post
    } else {
        RulePosition::Tree
    } as i32;
    let mut payload = RequestHttpFrontend {
        cluster_id: f.cluster_id.clone(),
        address: f.listener.into(),
        hostname,
        path: path_rule(&f.path),
        method: f.method.clone(),
        position,
        ..Default::default()
    };
    apply_filters(&mut payload, &f.filters);
    if f.tls {
        RequestType::AddHttpsFrontend(payload).into()
    } else {
        RequestType::AddHttpFrontend(payload).into()
    }
}

/// Map the IR's per-route filters onto Sōzu's frontend fields.
fn apply_filters(payload: &mut RequestHttpFrontend, filters: &ir::FrontendFilters) {
    payload.headers = filters
        .header_mods
        .iter()
        .map(|m| Header {
            position: match m.on {
                ir::HeaderTarget::Request => HeaderPosition::Request,
                ir::HeaderTarget::Response => HeaderPosition::Response,
            } as i32,
            key: m.key.clone(),
            // Empty value deletes the header by name (Sōzu semantics).
            val: m.value.clone().unwrap_or_default(),
        })
        .collect();

    if let Some(redirect) = &filters.redirect {
        payload.redirect = Some(match redirect.status {
            ir::RedirectStatus::MovedPermanently => RedirectPolicy::Permanent,
            ir::RedirectStatus::Found => RedirectPolicy::Found,
            ir::RedirectStatus::PermanentRedirect => RedirectPolicy::PermanentRedirect,
        } as i32);
        if let Some(scheme) = redirect.scheme {
            payload.redirect_scheme = Some(match scheme {
                ir::Scheme::Http => RedirectScheme::UseHttp,
                ir::Scheme::Https => RedirectScheme::UseHttps,
            } as i32);
        }
        // Sōzu builds the `Location` from the same `rewrite_*` fields it uses to
        // rewrite a forwarded request — which of the two it means is decided by
        // `redirect`, not by the fields. An unset one keeps the request's value.
        payload.rewrite_host = redirect.hostname.clone();
        payload.rewrite_path = redirect.path.clone();
        payload.rewrite_port = redirect.port.map(u32::from);
    }

    if let Some(rewrite) = &filters.rewrite {
        // Same three fields as a redirect target, so the two cannot both be
        // expressed on one frontend. The Gateway API keeps them apart anyway —
        // URLRewrite needs a backendRef and RequestRedirect forbids one — and
        // the builder refuses the combination rather than letting the later
        // assignment win silently.
        payload.rewrite_host = rewrite.hostname.clone();
        payload.rewrite_path = rewrite.path.clone();
    }
}

/// Frontends deduplicated by their IR route identity: tls, listener, hostname,
/// path and method. Sōzu rejects a duplicate AddHttpFrontend, so a benign
/// duplicate produced by overlapping Ingresses must not become a hard reconcile
/// failure. First occurrence wins, matching the builder's collision reporting.
fn unique_frontends(ir: &ir::Ir) -> Vec<&ir::Frontend> {
    let mut seen = BTreeSet::new();
    ir.frontends
        .iter()
        .filter(|f| {
            seen.insert((
                f.tls,
                f.listener,
                f.hostname.as_str(),
                f.multi_label_wildcard,
                &f.path,
                f.method.as_deref(),
            ))
        })
        .collect()
}

type FrontendGroup = (bool, SocketAddr, Option<String>);
type FrontendKey = (FrontendGroup, String, i32, String, Option<String>);

fn http_frontend(req: &Request) -> Option<(bool, &RequestHttpFrontend)> {
    match &req.request_type {
        Some(RequestType::AddHttpFrontend(f)) => Some((false, f)),
        Some(RequestType::AddHttpsFrontend(f)) => Some((true, f)),
        _ => None,
    }
}

fn frontend_group(tls: bool, f: &RequestHttpFrontend) -> FrontendGroup {
    // Match the existing HTTPS-before-HTTP command order across independent groups.
    // All POST hostnames share one append-only list. Keep their ordering
    // together so a wildcard added later can precede an existing catch-all.
    let host = (f.position == RulePosition::Tree as i32).then(|| f.hostname.clone());
    (!tls, f.address.into(), host)
}

fn frontend_key(tls: bool, f: &RequestHttpFrontend) -> FrontendKey {
    (
        frontend_group(tls, f),
        f.hostname.clone(),
        f.path.kind,
        f.path.value.clone(),
        f.method.clone(),
    )
}

/// HTTP/HTTPS frontend requests, deduplicated a second time on the *emitted*
/// route key.
///
/// [`unique_frontends`] compares IR path matches, but two different ones can
/// still compile to the same Sōzu rule — `Prefix("/foo")` and `Prefix("/foo/")`
/// are one path in Kubernetes. The builder canonicalises that spelling away, so
/// this pass is a backstop rather than the primary control: Sōzu holds a route
/// key once, and because translation is all-or-nothing a single clash would
/// fail *every* reconcile, taking unrelated routes down with it. Never let an
/// IR, however it was produced, be able to do that. First occurrence wins, as
/// everywhere else.
fn http_frontend_requests(ir: &ir::Ir) -> Vec<Request> {
    let mut seen = BTreeSet::new();
    let mut requests: Vec<_> = unique_frontends(ir)
        .into_iter()
        .map(frontend_request)
        .filter(|req| {
            let (tls, f) = http_frontend(req).expect("a frontend request");
            seen.insert(frontend_key(tls, f))
        })
        .collect();

    // TREE returns immediately for an explicit method on a regex/exact path,
    // but only remembers a methodless match. Give each methodless path a
    // variant for the methods used on this host: otherwise GET / can beat a
    // longer methodless /api even when /api was inserted first. Existing
    // explicit matches win over these variants on the same route key.
    let mut methods: BTreeMap<FrontendGroup, BTreeSet<String>> = BTreeMap::new();
    for req in &requests {
        let (tls, f) = http_frontend(req).expect("a frontend request");
        if f.position == RulePosition::Tree as i32 {
            if let Some(method) = &f.method {
                methods
                    .entry(frontend_group(tls, f))
                    .or_default()
                    .insert(method.clone());
            }
        }
    }
    let mut variants = Vec::new();
    for req in &requests {
        let (tls, f) = http_frontend(req).expect("a frontend request");
        if f.method.is_some() || f.position != RulePosition::Tree as i32 {
            continue;
        }
        for method in methods.get(&frontend_group(tls, f)).into_iter().flatten() {
            let mut variant = f.clone();
            variant.method = Some(method.clone());
            if seen.insert(frontend_key(tls, &variant)) {
                variants.push(if tls {
                    RequestType::AddHttpsFrontend(variant).into()
                } else {
                    RequestType::AddHttpFrontend(variant).into()
                });
            }
        }
    }
    requests.extend(variants);
    canonicalize(requests)
}

fn certificate_and_key(c: &ir::Certificate) -> CertificateAndKey {
    CertificateAndKey {
        certificate: c.certificate.clone(),
        certificate_chain: c.chain.clone(),
        key: c.key.clone(),
        versions: vec![], // empty => server default (TLS 1.2 + 1.3)
        names: c.names.clone(),
    }
}

fn add_certificate_request(c: &ir::Certificate) -> Request {
    RequestType::AddCertificate(AddCertificate {
        address: c.listener.into(),
        certificate: certificate_and_key(c),
        expired_at: None,
    })
    .into()
}

/// Lower-case hex fingerprint of a certificate's leaf, matching the form Sōzu
/// stores and `RemoveCertificate` expects.
fn fingerprint(c: &ir::Certificate) -> Result<String, TranslatorError> {
    let bytes = sozu_command_lib::certificate::calculate_fingerprint(c.certificate.as_bytes())
        .map_err(|e| TranslatorError::Certificate(format!("{e:?}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// SNI name set (order-insensitive) for rotation pairing.
fn names_set(c: &ir::Certificate) -> BTreeSet<&str> {
    c.names.iter().map(String::as_str).collect()
}

fn remove_certificate_request(listener: SocketAddr, fingerprint: String) -> Request {
    RequestType::RemoveCertificate(RemoveCertificate {
        address: listener.into(),
        fingerprint,
    })
    .into()
}

fn replace_certificate_request(new: &ir::Certificate, old_fingerprint: String) -> Request {
    RequestType::ReplaceCertificate(ReplaceCertificate {
        address: new.listener.into(),
        new_certificate: certificate_and_key(new),
        old_fingerprint,
        new_expired_at: None,
    })
    .into()
}

/// A certificate keyed by (listener, fingerprint) — Sōzu's own identity for a
/// loaded cert (`HashMap<SocketAddr, HashMap<Fingerprint, _>>`).
struct KeyedCert {
    listener: SocketAddr,
    fingerprint: String,
    cert: ir::Certificate,
}

/// Group the certs by (listener, fingerprint), unioning the SNI names within
/// each group. The fingerprint is computed over the parsed DER, so two
/// byte-different PEM encodings of the *same* certificate share one identity in
/// Sōzu; kept as separate entries they would make the diff compare the single
/// loaded cert against whichever duplicate it pairs with — re-emitting a
/// certificate update on every cycle and clamping SNI coverage to that entry's
/// names. Grouping first keeps `reconcile(&ir, &ir)` empty and every hostname
/// covered. The first occurrence fixes the group's position and PEM bytes
/// (the DER is identical anyway); the merged name set is sorted.
fn keyed_certs(certs: &[ir::Certificate]) -> Result<Vec<KeyedCert>, TranslatorError> {
    let mut out: Vec<KeyedCert> = Vec::new();
    let mut index: HashMap<(SocketAddr, String), usize> = HashMap::new();
    for c in certs {
        let fp = fingerprint(c)?;
        match index.get(&(c.listener, fp.clone())) {
            Some(&i) => {
                let existing = &mut out[i].cert;
                let mut names: BTreeSet<String> = existing.names.drain(..).collect();
                names.extend(c.names.iter().cloned());
                existing.names = names.into_iter().collect();
            }
            None => {
                index.insert((c.listener, fp.clone()), out.len());
                out.push(KeyedCert {
                    listener: c.listener,
                    fingerprint: fp,
                    cert: c.clone(),
                });
            }
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------------------
// Layer 4 (TCP/UDP)
// ----------------------------------------------------------------------------

fn tcp_listener_add(addr: SocketAddr) -> Request {
    RequestType::AddTcpListener(TcpListenerConfig {
        address: addr.into(),
        public_address: None,
        expect_proxy: false,
        front_timeout: 60,
        back_timeout: 30,
        connect_timeout: 3,
        active: true,
        // SNI preread knobs (sozu 2.2.0). Left unset: they are only consulted
        // when a frontend on this listener is SNI-scoped, and none of ours is —
        // one L4 listen address carries exactly one cluster. Unset also means
        // the fields are not encoded at all, so the request stays byte-identical
        // to what a pre-2.2.0 Sōzu already accepts.
        sni_preread_timeout: None,
        sni_preread_max_bytes: None,
    })
    .into()
}

fn udp_listener_add(addr: SocketAddr) -> Request {
    RequestType::AddUdpListener(UdpListenerConfig {
        address: addr.into(),
        public_address: None,
        front_timeout: 30,
        back_timeout: 30,
        max_rx_datagram_size: 1500,
        max_flows: 0,
        active: true,
    })
    .into()
}

fn activate_listener(addr: SocketAddr, proxy: ListenerType) -> Request {
    RequestType::ActivateListener(ActivateListener {
        address: addr.into(),
        proxy: proxy as i32,
        from_scm: false,
    })
    .into()
}

fn l4_frontend_request(f: &ir::L4Frontend) -> Request {
    match f.protocol {
        ir::L4Protocol::Tcp => RequestType::AddTcpFrontend(RequestTcpFrontend {
            cluster_id: f.cluster_id.clone(),
            address: f.listener.into(),
            tags: Default::default(),
            // SNI/ALPN routing (sozu 2.2.0) is not wired: TCPRoute v1 carries no
            // hostname to read it from — only TLSRoute does, and there
            // `hostnames` is required. Absent `sni` is Sōzu's raw-TCP fallback:
            // the frontend matches whatever the ClientHello says.
            sni: None,
            alpn: vec![],
        })
        .into(),
        ir::L4Protocol::Udp => RequestType::AddUdpFrontend(RequestUdpFrontend {
            cluster_id: f.cluster_id.clone(),
            address: f.listener.into(),
            tags: Default::default(),
        })
        .into(),
    }
}

/// L4 frontends deduplicated by exact identity (protocol + listener +
/// cluster). Like [`unique_frontends`], a benign duplicate — two sources
/// mapping the same port to the same cluster — must not hard-fail the whole
/// reconcile with `StateError::Exists`. First occurrence wins. Only *exact*
/// duplicates collapse; conflicting claims are [`check_l4_conflicts`]' job.
fn unique_l4_frontends(l4: &[ir::L4Frontend]) -> Vec<&ir::L4Frontend> {
    let mut seen: BTreeSet<(ir::L4Protocol, SocketAddr, &str)> = BTreeSet::new();
    l4.iter()
        .filter(|f| seen.insert((f.protocol, f.listener, f.cluster_id.as_str())))
        .collect()
}

/// Reject two L4 frontends claiming one listen address for *different*
/// clusters. At L4 there is no host multiplexing — one address routes to
/// exactly one cluster — and `ConfigState` buckets TCP/UDP frontends by
/// cluster, so the fold alone would accept both claims and silently program
/// an ambiguous route. Expects an exact-deduplicated slice: any repeated
/// (protocol, listener) key left is a conflict.
///
/// **This is a net, not the guard.** It fails the whole reconcile, HTTP
/// included, so it must never be what settles a dispute between two tenants'
/// routes. The builder resolves those per route, with a Problem on the loser's
/// own status; reaching this error means the builder let something through.
fn check_l4_conflicts(l4: &[&ir::L4Frontend]) -> Result<(), TranslatorError> {
    let mut claims: BTreeMap<(ir::L4Protocol, SocketAddr), &str> = BTreeMap::new();
    for f in l4 {
        if let Some(other) = claims.insert((f.protocol, f.listener), f.cluster_id.as_str()) {
            return Err(TranslatorError::L4Conflict(format!(
                "{} ({:?}) is claimed by both cluster {other:?} and cluster {:?}",
                f.listener, f.protocol, f.cluster_id
            )));
        }
    }
    Ok(())
}

/// `AddTcpListener`/`AddUdpListener` for each distinct L4 listen address (active,
/// so `ConfigState::diff` derives the matching `ActivateListener`).
fn l4_listener_adds(l4: &[ir::L4Frontend]) -> Vec<Request> {
    let mut seen: BTreeSet<(ir::L4Protocol, SocketAddr)> = BTreeSet::new();
    l4.iter()
        .filter(|f| seen.insert((f.protocol, f.listener)))
        .map(|f| match f.protocol {
            ir::L4Protocol::Tcp => tcp_listener_add(f.listener),
            ir::L4Protocol::Udp => udp_listener_add(f.listener),
        })
        .collect()
}

/// Explicit `ActivateListener` for each distinct L4 listen address — needed on
/// the full-apply path (no diff to derive activation from `active = true`).
fn l4_listener_activations(l4: &[ir::L4Frontend]) -> Vec<Request> {
    let mut seen: BTreeSet<(ir::L4Protocol, SocketAddr)> = BTreeSet::new();
    l4.iter()
        .filter(|f| seen.insert((f.protocol, f.listener)))
        .map(|f| {
            let proxy = match f.protocol {
                ir::L4Protocol::Tcp => ListenerType::Tcp,
                ir::L4Protocol::Udp => ListenerType::Udp,
            };
            activate_listener(f.listener, proxy)
        })
        .collect()
}

// ----------------------------------------------------------------------------
// Dependency-safe canonical ordering
// ----------------------------------------------------------------------------

fn tier(req: &Request) -> u8 {
    match &req.request_type {
        // Listeners must exist before they can be activated, and both before any
        // cluster/frontend can attach to them.
        Some(RequestType::AddHttpListener(_))
        | Some(RequestType::AddHttpsListener(_))
        | Some(RequestType::AddTcpListener(_))
        | Some(RequestType::AddUdpListener(_)) => 0,
        Some(RequestType::ActivateListener(_)) => 1,
        Some(RequestType::AddCluster(_)) => 2,
        Some(RequestType::AddBackend(_)) => 3,
        Some(RequestType::AddCertificate(_)) | Some(RequestType::ReplaceCertificate(_)) => 4,
        // Frontend removes precede frontend adds. Sōzu keys a route by
        // `address;hostname;path[;method]` (cluster_id is NOT part of the key),
        // so re-pointing a host+path at a different cluster yields a Remove(old)
        // + Add(new) on the *same* key. Add-before-Remove would make the live
        // `add_http_frontend` hit an Occupied entry → `StateError::Exists`, and
        // the trailing Remove would then delete the route outright. Removing
        // first leaves the entry Vacant for the re-add (a tiny, unavoidable gap
        // since Sōzu 2.1.0 has no atomic frontend replace).
        Some(RequestType::RemoveHttpFrontend(_))
        | Some(RequestType::RemoveHttpsFrontend(_))
        | Some(RequestType::RemoveTcpFrontend(_))
        | Some(RequestType::RemoveUdpFrontend(_)) => 5,
        Some(RequestType::AddHttpFrontend(_))
        | Some(RequestType::AddHttpsFrontend(_))
        | Some(RequestType::AddTcpFrontend(_))
        | Some(RequestType::AddUdpFrontend(_)) => 6,
        Some(RequestType::RemoveBackend(_)) => 7,
        Some(RequestType::RemoveCluster(_)) => 8,
        Some(RequestType::RemoveCertificate(_)) => 9,
        // Listener teardown: deactivate before remove — the order Sōzu itself
        // emits. Explicit consecutive tiers so the order can never silently
        // flip on the lexicographic accident of the serialized request names
        // (the within-tier sort key is the JSON encoding).
        Some(RequestType::DeactivateListener(_)) => 10,
        Some(RequestType::RemoveListener(_)) => 11,
        _ => 100,
    }
}

// A TREE methodless match is remembered until a later matching regex/exact
// rule replaces it. POST and explicit TREE methods take the first match.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum FrontendPriority {
    LastMatch((u8, usize)),
    FirstMatch(Reverse<(u8, usize)>),
}

type FrontendOrder = (FrontendGroup, Reverse<usize>, FrontendPriority, bool);

fn frontend_order(req: &Request) -> Option<FrontendOrder> {
    let (tls, f) = http_frontend(req)?;
    let kind = match f.path.kind() {
        PathRuleKind::Equals => 2,
        // Exact paths compile to a query-or-end boundary, whereas prefixes
        // also allow a slash. Preserve exact-before-prefix priority after
        // both have become regexes on the wire.
        PathRuleKind::Regex if f.path.value.ends_with(r"(\?|$)") => 2,
        PathRuleKind::Regex => 1,
        PathRuleKind::Prefix => 0,
    };
    // Non-root Kubernetes prefixes are anchored regexes. A nested literal
    // prefix always has a longer escaped expression, including metacharacters.
    // Arbitrary implementation-specific regexes get deterministic ordering;
    // there is no general notion of specificity between those expressions.
    let specificity = (kind, f.path.value.len());
    let priority = if f.position == RulePosition::Tree as i32 && f.method.is_none() {
        FrontendPriority::LastMatch(specificity)
    } else {
        FrontendPriority::FirstMatch(Reverse(specificity))
    };
    // Overlapping Gateway wildcards are nested suffixes: the longer suffix
    // wins before path or method specificity. All emitted wildcard patterns
    // have the same wrapper, and the bare catch-all is shorter than any.
    Some((
        frontend_group(tls, f),
        Reverse(f.hostname.len()),
        priority,
        f.method.is_none(),
    ))
}

/// Reorder into dependency-safe tiers with a deterministic secondary key.
/// Only certificate removals paired with a name update move ahead of the
/// certificate adds. An Add with an existing fingerprint is a worker no-op;
/// leaving its paired removal at the teardown tier would delete the cert.
fn canonicalize(
    mut requests: Vec<Request>,
    certificate_reloads: &BTreeSet<(SocketAddr, String)>,
) -> Vec<Request> {
    requests.sort_by_cached_key(|req| {
        let reload = match &req.request_type {
            Some(RequestType::RemoveCertificate(c)) => {
                certificate_reloads.contains(&(c.address.into(), c.fingerprint.clone()))
            }
            _ => false,
        };
        let key = serde_json::to_string(req).unwrap_or_default();
        (if reload { 4 } else { tier(req) }, !reload, frontend_order(req), key)
    });
    requests
}

/// Drop every `RemoveBackend` whose (cluster_id, backend_id, address) triple
/// also appears as an `AddBackend` in the same batch.
///
/// `ConfigState::diff` emits a *changed* backend (same key, e.g. a new weight)
/// as Remove-then-Add, but `canonicalize` reorders backend adds (tier 3) before
/// backend removes (tier 7), turning that pair into Add-then-Remove. Sōzu's
/// `add_backend` is an upsert and `remove_backend` matches on
/// (backend_id, address) only, so the trailing Remove would delete the backend
/// the Add just updated — leaving the cluster short one live backend. The Add
/// alone already converges, so the Remove is the stale half of the pair and is
/// dropped. A backend whose *address* changed diffs under two different triples
/// and keeps its Remove.
fn drop_superseded_backend_removes(requests: Vec<Request>) -> Vec<Request> {
    let added: BTreeSet<(String, String, SocketAddr)> = requests
        .iter()
        .filter_map(|req| match &req.request_type {
            Some(RequestType::AddBackend(b)) => {
                Some((b.cluster_id.clone(), b.backend_id.clone(), b.address.into()))
            }
            _ => None,
        })
        .collect();
    if added.is_empty() {
        return requests;
    }
    requests
        .into_iter()
        .filter(|req| match &req.request_type {
            Some(RequestType::RemoveBackend(b)) => {
                !added.contains(&(b.cluster_id.clone(), b.backend_id.clone(), b.address.into()))
            }
            _ => true,
        })
        .collect()
}

// ----------------------------------------------------------------------------
// Diff building blocks
// ----------------------------------------------------------------------------

/// Fold the IR's routing graph (clusters/backends/frontends, NO certificates)
/// into a `ConfigState`. Certificates are handled separately, so they never
/// enter the `ConfigState::diff` path.
fn routing_state(ir: &ir::Ir) -> Result<ConfigState, TranslatorError> {
    let mut requests: Vec<Request> = Vec::new();
    requests.extend(cluster_requests(ir));
    requests.extend(ir.backends.iter().map(backend_request));
    requests.extend(http_frontend_requests(ir));
    // L4: listeners (active=true, so diff derives ActivateListener) + frontends.
    // No explicit ActivateListener here — dispatching it would need the listener
    // to already exist in this transient state, and the diff handles activation.
    let l4 = unique_l4_frontends(&ir.l4_frontends);
    check_l4_conflicts(&l4)?;
    requests.extend(l4_listener_adds(&ir.l4_frontends));
    requests.extend(l4.into_iter().map(l4_frontend_request));
    let mut state = ConfigState::new();
    for req in canonicalize(requests, &BTreeSet::new()) {
        // The library's own message identifies the clashing object only by the
        // byte length of its id, so the request has to carry the diagnosis: on a
        // duplicate route key, this is the only thing that names the route. Safe
        // to inline — `Debug` on a request redacts certificate and key material.
        state
            .dispatch(&req)
            .map_err(|e| TranslatorError::Dispatch(format!("{e}; request was {req:?}")))?;
    }
    Ok(state)
}

/// Keep the longest desired prefix already present, in order, in each
/// routing-list group. Sōzu appends frontends, so the remaining suffix must be
/// removed and re-added: sorting a new /api alone cannot place it before an
/// existing catch-all /. A POST group includes every hostname on the bind,
/// whereas TREE keeps a separate list per hostname. Pure removals need no
/// re-adds; unchanged groups and backend-only updates stay put.
fn ordered_frontend_diff(previous: &ir::Ir, desired: &ir::Ir) -> Vec<Request> {
    fn groups(ir: &ir::Ir) -> BTreeMap<FrontendGroup, Vec<Request>> {
        let mut groups: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for req in http_frontend_requests(ir) {
            let (tls, f) = http_frontend(&req).expect("a frontend request");
            groups.entry(frontend_group(tls, f)).or_default().push(req);
        }
        groups
    }
    let previous = groups(previous);
    let mut desired = groups(desired);
    let mut requests = Vec::new();
    for (key, old) in previous {
        let new = desired.remove(&key).unwrap_or_default();
        if old == new {
            continue;
        }
        let mut retained = vec![false; old.len()];
        let mut cursor = 0;
        let mut append_from = 0;
        for wanted in &new {
            let Some(offset) = old[cursor..].iter().position(|req| req == wanted) else {
                break;
            };
            cursor += offset;
            retained[cursor] = true;
            cursor += 1;
            append_from += 1;
        }
        for (req, keep) in old.into_iter().zip(retained) {
            if keep {
                continue;
            }
            let remove = match req.request_type {
                Some(RequestType::AddHttpFrontend(f)) => RequestType::RemoveHttpFrontend(f),
                Some(RequestType::AddHttpsFrontend(f)) => RequestType::RemoveHttpsFrontend(f),
                _ => unreachable!("only HTTP/HTTPS frontend groups"),
            };
            requests.push(remove.into());
        }
        requests.extend(new.into_iter().skip(append_from));
    }
    requests.extend(desired.into_values().flatten());
    requests
}

#[derive(Default)]
struct CertificateRequests {
    requests: Vec<Request>,
    /// Same-fingerprint name updates need removal before their add. All other
    /// certificate removals keep the normal teardown order.
    reloads: BTreeSet<(SocketAddr, String)>,
}

/// Minimal certificate requests to converge `previous` → `desired`. Identity is
/// (listener, fingerprint) — matching Sōzu's own cert store — so the same cert on
/// two listeners is tracked independently. Both sides are grouped by that
/// identity first (`keyed_certs`), so duplicate entries for one cert converge
/// to a single entry carrying the union of their SNI names. Handles:
///  - new cert at (listener, fp)        -> AddCertificate
///  - cert gone from (listener, fp)     -> RemoveCertificate
///  - same (listener, fp), names differ -> RemoveCertificate + AddCertificate;
///    Sōzu 2.2.1 skips both Add and Replace when the fingerprint already exists.
///    Removal must precede the add, leaving a brief TLS availability gap.
///  - rotation (a removed + an added at the same listener sharing the SNI name
///    set) -> a single ReplaceCertificate (zero-gap)
fn certificate_requests(
    previous: &[ir::Certificate],
    desired: &[ir::Certificate],
) -> Result<CertificateRequests, TranslatorError> {
    let prev = keyed_certs(previous)?;
    let des = keyed_certs(desired)?;

    let prev_by_key: HashMap<(SocketAddr, &str), &KeyedCert> = prev
        .iter()
        .map(|k| ((k.listener, k.fingerprint.as_str()), k))
        .collect();
    let des_keys: BTreeSet<(SocketAddr, &str)> = des
        .iter()
        .map(|k| (k.listener, k.fingerprint.as_str()))
        .collect();

    let mut out = CertificateRequests::default();
    let mut truly_added: Vec<&KeyedCert> = Vec::new();

    for d in &des {
        match prev_by_key.get(&(d.listener, d.fingerprint.as_str())) {
            // Same (listener, fp): only a name change needs a reload.
            Some(p) => {
                if names_set(&p.cert) != names_set(&d.cert) {
                    out.reloads.insert((d.listener, d.fingerprint.clone()));
                    out.requests.push(remove_certificate_request(
                        d.listener,
                        d.fingerprint.clone(),
                    ));
                    out.requests.push(add_certificate_request(&d.cert));
                }
            }
            None => truly_added.push(d),
        }
    }

    let mut truly_removed: Vec<&KeyedCert> = prev
        .iter()
        .filter(|p| !des_keys.contains(&(p.listener, p.fingerprint.as_str())))
        .collect();

    // Pair an add with a removal at the same listener + same SNI names -> rotate.
    let mut used = vec![false; truly_removed.len()];
    for new in &truly_added {
        if let Some(idx) = truly_removed.iter().enumerate().position(|(i, old)| {
            !used[i] && old.listener == new.listener && names_set(&old.cert) == names_set(&new.cert)
        }) {
            used[idx] = true;
            out.requests.push(replace_certificate_request(
                &new.cert,
                truly_removed[idx].fingerprint.clone(),
            ));
        } else {
            out.requests.push(add_certificate_request(&new.cert));
        }
    }
    for (i, old) in truly_removed.iter_mut().enumerate() {
        if !used[i] {
            out.requests.push(remove_certificate_request(
                old.listener,
                old.fingerprint.clone(),
            ));
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------------------
// Public API
// ----------------------------------------------------------------------------

/// The full desired state expressed as `Add*` requests, in canonical order.
/// Pure mapping (no diff/replay) — handy for a fresh "apply everything" and for
/// golden snapshots of the IR → command mapping.
pub fn ir_to_requests(ir: &ir::Ir) -> Vec<Request> {
    let mut requests = Vec::new();
    requests.extend(cluster_requests(ir));
    requests.extend(ir.backends.iter().map(backend_request));
    requests.extend(http_frontend_requests(ir));
    requests.extend(ir.certificates.iter().map(add_certificate_request));
    // L4: add the listener, activate it, then attach the frontend (tiered).
    requests.extend(l4_listener_adds(&ir.l4_frontends));
    requests.extend(l4_listener_activations(&ir.l4_frontends));
    requests.extend(
        unique_l4_frontends(&ir.l4_frontends)
            .into_iter()
            .map(l4_frontend_request),
    );
    canonicalize(requests, &BTreeSet::new())
}

/// Dependency-safe requests to converge a `previous` applied IR towards
/// the `desired` IR. Idempotent: `reconcile(&ir, &ir)` is empty. The controller
/// keeps `previous` as its shadow and swaps it to `desired` only after a
/// successful apply.
pub fn reconcile(previous: &ir::Ir, desired: &ir::Ir) -> Result<Vec<Request>, TranslatorError> {
    let mut requests = routing_state(previous)?.diff(&routing_state(desired)?);
    requests.retain(|req| {
        !matches!(
            req.request_type,
            Some(RequestType::AddHttpFrontend(_))
                | Some(RequestType::AddHttpsFrontend(_))
                | Some(RequestType::RemoveHttpFrontend(_))
                | Some(RequestType::RemoveHttpsFrontend(_))
        )
    });
    requests.extend(ordered_frontend_diff(previous, desired));
    let certificates = certificate_requests(&previous.certificates, &desired.certificates)?;
    requests.extend(certificates.requests);
    let mut requests = canonicalize(
        drop_superseded_backend_removes(requests),
        &certificates.reloads,
    );
    // `ConfigState::diff` emits the activation of a newly-added active TCP/UDP
    // listener twice (once inline, once in its trailing activation sweep).
    // After `canonicalize` the batch is fully sorted, so identical requests
    // are adjacent and the duplicate collapses here.
    requests.dedup();
    Ok(requests)
}
