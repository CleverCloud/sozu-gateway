//! Translator: pure IR → Sōzu protobuf commands.
//!
//! Side-effect free and golden-file tested. Two diff strategies, deliberately
//! split:
//!  - **Routing graph** (clusters / backends / frontends): we reuse Sōzu's own
//!    `ConfigState::diff`, so the semantics match the data plane exactly.
//!  - **Certificates**: we diff them ourselves, by fingerprint. This (a) lets us
//!    emit `ReplaceCertificate` for zero-gap rotation, where Sōzu 2.3's own
//!    certificate diff moves a certificate with a non-atomic Remove + Add (a
//!    gap), and turns an order-only or same-fingerprint change into
//!    Remove(X) + Add(X) — a pair `canonicalize` would reorder into a lost
//!    certificate, since adds tier before removes — and (b) kept
//!    clear of a debug-assert in sozu-command-lib 2.1.0–2.2.1 that fired when
//!    `ConfigState::diff` removed the last certificate at a listener address.
//!
//! Output is canonicalised into dependency-safe tiers (adds: listeners →
//! clusters → backends → certificates → frontends; removes in reverse, except
//! that cluster removals come last, after the listener teardown). Frontend *removes*
//! are ordered before frontend *adds*: Sōzu keys a route by host+path (not by
//! cluster_id), so re-pointing a route at another cluster is a Remove+Add on the
//! same key, and adding first would be rejected as a duplicate. Since Sōzu 2.3.0
//! a cluster removal also drops the cluster's frontends and backends, and the
//! diff leaves those out; they are spelled out again so they tier the same way
//! (see `expand_cluster_removals`). A new/replacement certificate lands before
//! the old one is removed → no TLS gap, and a name moving between two
//! certificates is given before it is taken (`CertRank`, `transfer_order`:
//! recipient before donor, in a deterministic topological order; only
//! certificates whose name transfers form a cycle cannot all go first). This
//! also makes the otherwise HashSet-ordered routing diff deterministic.
#![forbid(unsafe_code)]

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::net::SocketAddr;

use sozu_command_lib::certificate::{parse_pem, split_certificate_chain};
use sozu_command_lib::proto::command::{
    request::RequestType, ActivateListener, AddBackend, AddCertificate, CertificateAndKey, Cluster,
    Header, HeaderPosition, HealthCheckConfig, HealthCheckMode, ListenerType,
    LoadBalancingAlgorithms, LoadBalancingParams, PathRule, PathRuleKind, RedirectPolicy,
    RedirectScheme, RemoveCertificate, ReplaceCertificate, Request, RequestHttpFrontend,
    RequestTcpFrontend, RequestUdpFrontend, RulePosition, TcpListenerConfig, UdpAffinityKey,
    UdpClusterConfig, UdpListenerConfig,
};
use sozu_command_lib::state::ConfigState;
use sozu_gw_ir as ir;
use thiserror::Error;

mod replay;
pub use replay::{apply_acknowledged, replay_if_exact};

#[derive(Debug, Error)]
pub enum TranslatorError {
    #[error("failed to fold request into ConfigState: {0}")]
    Dispatch(String),
    #[error("invalid certificate (cannot compute fingerprint): {0}")]
    Certificate(String),
    #[error("conflicting L4 frontends: {0}")]
    L4Conflict(String),
    #[error("{0} names cluster {1:?}, which the desired state does not declare")]
    UnknownCluster(&'static str, String),
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

fn rule(kind: PathRuleKind, value: String) -> PathRule {
    PathRule {
        kind: kind as i32,
        value,
    }
}

/// The Sōzu path rule one IR path match compiles to — [`ir::PathMatch::sozu_rule`]
/// mapped onto the wire type. The compilation itself lives in the IR so the
/// builder keys its collision arbitration on exactly what is emitted here: two
/// matches that compile to one rule are one route to Sōzu, whatever their IR
/// spelling.
fn path_rule(path: &ir::PathMatch) -> PathRule {
    match path.sozu_rule() {
        ir::SozuPathRule::Prefix(v) => rule(PathRuleKind::Prefix, v),
        ir::SozuPathRule::Regex(v) => rule(PathRuleKind::Regex, v),
    }
}

fn cluster_request(c: &ir::Cluster, udp: bool) -> Request {
    RequestType::AddCluster(Cluster {
        cluster_id: c.id.clone(),
        sticky_session: c.sticky_session,
        https_redirect: c.https_redirect,
        load_balancing: lb_algorithm(c.load_balancing),
        // Sōzu 2.3 applies a UDP cluster's per-IP cap to new flows per source
        // (99e6aee2), where 2.2 ignored it, and counts every client source
        // port as a flow. The `sozu.io/max-connections-per-ip` annotation was
        // never meant for datagrams — clients behind a NAT or a node SNAT
        // would lose new flows silently — so a UDP cluster keeps none.
        max_connections_per_ip: if udp { None } else { c.max_connections_per_ip },
        retry_after: c.retry_after,
        // Distinct sockets behind one source IP are distinct UDP clients. Since
        // Sōzu 2.3.0 a flow is always keyed on the source IP and port
        // (fcdec35e) and this only feeds backend selection; on 2.2 the
        // source-IP-only default reused the first socket's return address for
        // later datagrams from other source ports.
        udp: udp.then(|| UdpClusterConfig {
            affinity_key: Some(UdpAffinityKey::SourceIpPort as i32),
            ..Default::default()
        }),
        // HTTP/2 with prior knowledge to the backends. `Some(true)` or nothing:
        // the worker reads the field as `http2.unwrap_or(false)` (Sōzu 2.3.2
        // lib/src/server.rs:3289, lib/src/protocol/mux/router.rs:746), and an
        // `AddCluster` replaces the whole cluster (lib/src/http.rs:1270,
        // lib/src/https.rs:2760), so an upsert carrying `None` turns HTTP/2
        // back off: measured on new connections,
        // docs/probes/h2c-backend_sozu-2.3.2_2026-10-19.txt ("http2 unset
        // again"). An open keep-alive connection follows on its next request,
        // measured with `sozu cluster h2 disable` — an `AddCluster` carrying
        // `Some(false)`, not `None` —
        // docs/probes/grpc-http2-flip_sozu-2.3.2_2026-10-19.txt. A cluster
        // that never set it emits exactly the request it did before, and
        // `ConfigState::diff` carries a flip either way as one `AddCluster`
        // (command/src/state.rs, `DiffResult::Changed`). The field is ignored
        // by the TCP and UDP proxies, so a cluster an L4 route shares is
        // unaffected.
        http2: c.http2.then_some(true),
        // A TCP-connect check means nothing to datagrams, and Sōzu 2.3.2 does
        // not refuse one on a UDP cluster (`command/src/state.rs`), so it is
        // left out here; the builder has already dropped and reported it.
        health_check: if udp {
            None
        } else {
            c.health_check.as_ref().map(health_check_config)
        },
        ..Default::default()
    })
    .into()
}

/// The wire form of a TCP-connect health check — what the builder validates
/// with `validate_health_check_config` before the check enters the IR, so the
/// two must stay field for field the same (a builder test checks that the
/// builder admits exactly the checks `ConfigState` accepts from here).
///
/// Every field is spelled out rather than taken from `Default`: the proto
/// default is an HTTP probe with an empty `uri`, which Sōzu refuses ("health
/// check URI must start with '/'"), failing the whole `AddCluster` and, since
/// translation is all-or-nothing, every reconcile. `uri` is `/` although the
/// TCP mode ignores it: a Sōzu older than 2.3.0 has no `mode` field, runs
/// `GET <uri>` instead, and refuses an empty one the same way.
pub fn health_check_config(check: &ir::HealthCheck) -> HealthCheckConfig {
    HealthCheckConfig {
        uri: "/".to_string(),
        interval: check.interval_secs,
        timeout: check.timeout_secs,
        healthy_threshold: check.healthy_threshold,
        unhealthy_threshold: check.unhealthy_threshold,
        expected_status: 0,
        mode: HealthCheckMode::Tcp as i32,
        accepted_statuses: vec![],
        port: check.port.map(u32::from),
        // Protected probe headers (2.3.2) need a preflight, a two-phase
        // exchange and re-injection after every Sōzu restart; a TCP probe
        // sends nothing, and Sōzu refuses headers in TCP mode anyway.
        headers: vec![],
    }
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
        // A sticky cluster's cookie selects a backend only through this id; see
        // `ir::Backend::sticky_id`. A change is an in-place upsert in Sōzu, like
        // a weight change.
        sticky_id: b.sticky_id.clone(),
        load_balancing_parameters: b.weight.map(|weight| LoadBalancingParams { weight }),
        backup: None,
        unix_path: None,
    })
    .into()
}

fn frontend_payload(f: &ir::Frontend) -> RequestHttpFrontend {
    // A bare "*" catch-all goes in POST so it is a fallback and never shadows
    // specific-host (TREE) frontends. Sōzu parses "*" as DomainRule::Any.
    let position = if f.hostname == "*" {
        RulePosition::Post
    } else {
        RulePosition::Tree
    } as i32;
    let mut payload = RequestHttpFrontend {
        cluster_id: f.cluster_id.clone(),
        address: f.listener.into(),
        hostname: f.hostname.clone(),
        path: path_rule(&f.path),
        method: f.method.clone(),
        position,
        ..Default::default()
    };
    apply_filters(&mut payload, &f.filters);
    payload
}

fn frontend_request(tls: bool, payload: RequestHttpFrontend) -> Request {
    if tls {
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

/// Frontends deduplicated by the key Sōzu stores them under
/// ([`ir::Frontend::sozu_route_key`]): the frontend map (HTTP or HTTPS) and the
/// `;`-joined string of listener, hostname, *compiled* path rule and method.
/// Sōzu rejects an add on an occupied key, so a duplicate produced by
/// overlapping objects must not become a hard reconcile failure. Comparing the
/// compiled rule makes `Exact("/foo")` and a user regex that spells the same
/// anchored pattern one route here; comparing the joined string makes a regex
/// `/x;GET` and a regex `/x` with `method: GET` one route, exactly as they are
/// to Sōzu. It is the key the builder arbitrates on, so the loser is the one it
/// named. First occurrence wins, as everywhere else.
fn unique_frontends(ir: &ir::Ir) -> Vec<&ir::Frontend> {
    let mut seen: BTreeSet<ir::SozuRouteKey> = BTreeSet::new();
    ir.frontends
        .iter()
        .filter(|f| seen.insert(f.sozu_route_key()))
        .collect()
}

/// HTTP/HTTPS frontend requests, deduplicated a second time on the key Sōzu
/// derives from the *emitted* payload (`RequestHttpFrontend`'s `Display`) — a
/// backstop for [`unique_frontends`], which computes the same key one step
/// earlier: Sōzu holds a route key once, and because translation is
/// all-or-nothing a single clash would fail *every* reconcile, taking unrelated
/// routes down with it. Never let an IR, however it was produced, be able to do
/// that. First occurrence wins.
fn http_frontend_requests(ir: &ir::Ir) -> Vec<Request> {
    let mut seen: BTreeSet<(bool, String)> = BTreeSet::new();
    unique_frontends(ir)
        .into_iter()
        .map(|f| (f.tls, frontend_payload(f)))
        .filter(|(tls, payload)| seen.insert((*tls, payload.to_string())))
        .map(|(tls, payload)| frontend_request(tls, payload))
        .collect()
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
    Ok(hex(&bytes))
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
    /// The intermediates as Sōzu serves them: DER, in order, without a copy
    /// of the leaf (Sōzu drops those). Compared rather than the PEM, so a
    /// re-encoded Secret is not a change.
    chain: Vec<Vec<u8>>,
    cert: ir::Certificate,
}

/// The certificate chain `c` makes Sōzu serve after its leaf: every PEM block
/// of every chain entry, decoded, minus copies of the leaf — the same split,
/// decode and deduplication Sōzu applies when it loads the certificate.
fn served_chain(c: &ir::Certificate) -> Result<Vec<Vec<u8>>, TranslatorError> {
    let decode = |pem: &str| {
        parse_pem(pem.as_bytes())
            .map(|pem| pem.contents)
            .map_err(|e| TranslatorError::Certificate(format!("{e:?}")))
    };
    let leaf = decode(&c.certificate)?;
    let mut chain = Vec::new();
    for entry in &c.chain {
        for pem in split_certificate_chain(entry.clone()) {
            let link = decode(&pem)?;
            if link != leaf {
                chain.push(link);
            }
        }
    }
    Ok(chain)
}

/// Group the certs by (listener, fingerprint), unioning the SNI names within
/// each group. The fingerprint is computed over the parsed DER, so two
/// byte-different PEM encodings of the *same* certificate share one identity in
/// Sōzu; kept as separate entries they would make the diff compare the single
/// loaded cert against whichever duplicate it pairs with — re-emitting a
/// ReplaceCertificate on every cycle and clamping SNI coverage to that entry's
/// names. Grouping first keeps `reconcile(&ir, &ir)` empty and every hostname
/// covered. The first occurrence fixes the group's position; the material Sōzu
/// is sent (chain, key, PEM) is the smallest of the group's entries, so which
/// Secret is read first never changes what is served — two Secrets sharing a
/// leaf with different chains would otherwise trade places across reconciles
/// and replace the certificate each time. The merged name set is sorted.
fn keyed_certs(certs: &[ir::Certificate]) -> Result<Vec<KeyedCert>, TranslatorError> {
    let mut out: Vec<KeyedCert> = Vec::new();
    let mut index: HashMap<(SocketAddr, String), usize> = HashMap::new();
    for c in certs {
        let fp = fingerprint(c)?;
        let chain = served_chain(c)?;
        match index.get(&(c.listener, fp.clone())) {
            Some(&i) => {
                let existing = &mut out[i];
                let mut names: BTreeSet<String> = existing.cert.names.drain(..).collect();
                names.extend(c.names.iter().cloned());
                if (
                    std::cmp::Reverse(chain.len()),
                    &chain,
                    &c.key,
                    &c.certificate,
                ) < (
                    std::cmp::Reverse(existing.chain.len()),
                    &existing.chain,
                    &existing.cert.key,
                    &existing.cert.certificate,
                ) {
                    existing.chain = chain;
                    existing.cert = c.clone();
                }
                existing.cert.names = names.into_iter().collect();
            }
            None => {
                index.insert((c.listener, fp.clone()), out.len());
                out.push(KeyedCert {
                    listener: c.listener,
                    fingerprint: fp,
                    chain,
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
        interface: None,
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
        interface: None,
    })
    .into()
}

fn activate_listener(addr: SocketAddr, proxy: ListenerType) -> Request {
    RequestType::ActivateListener(ActivateListener {
        address: addr.into(),
        proxy: proxy as i32,
        from_scm: false,
        interface: None,
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
        Some(RequestType::AddBackend(_)) | Some(RequestType::AddUnixBackend(_)) => 3,
        // Within the tier, a request that takes a name off a certificate goes
        // after the ones that give it to another (`CertRank`).
        Some(RequestType::AddCertificate(_)) | Some(RequestType::ReplaceCertificate(_)) => 4,
        // Frontend removes precede frontend adds. Sōzu keys a route by
        // `address;hostname;path[;method]` (cluster_id is NOT part of the key),
        // so re-pointing a host+path at a different cluster yields a Remove(old)
        // + Add(new) on the *same* key. Add-before-Remove would make the live
        // `add_http_frontend` hit an Occupied entry → `StateError::Exists`, and
        // the trailing Remove would then delete the route outright. Removing
        // first leaves the entry Vacant for the re-add (a tiny, unavoidable gap
        // since Sōzu, 2.3.2 included, has no atomic frontend replace).
        //
        // Sōzu 2.3 carries a frontend with header predicates or a
        // `silent_close` redirect on dedicated verbs (`RequestType::add_http_frontend`
        // and siblings pick them, `ConfigState::diff` included); they tier with
        // the plain ones so a route keeps its place whatever it carries.
        Some(RequestType::RemoveHttpFrontend(_))
        | Some(RequestType::RemoveHttpsFrontend(_))
        | Some(RequestType::RemoveHttpFrontendWithHeaderPredicates(_))
        | Some(RequestType::RemoveHttpsFrontendWithHeaderPredicates(_))
        | Some(RequestType::RemoveTcpFrontend(_))
        | Some(RequestType::RemoveUdpFrontend(_)) => 5,
        Some(RequestType::AddHttpFrontend(_))
        | Some(RequestType::AddHttpsFrontend(_))
        | Some(RequestType::AddHttpFrontendWithHeaderPredicates(_))
        | Some(RequestType::AddHttpsFrontendWithHeaderPredicates(_))
        | Some(RequestType::AddHttpFrontendWithSilentClose(_))
        | Some(RequestType::AddHttpsFrontendWithSilentClose(_))
        | Some(RequestType::AddTcpFrontend(_))
        | Some(RequestType::AddUdpFrontend(_)) => 6,
        Some(RequestType::RemoveBackend(_)) => 7,
        Some(RequestType::RemoveCertificate(_)) => 9,
        // Listener teardown: deactivate before remove — the order Sōzu itself
        // emits. Explicit consecutive tiers so the order can never silently
        // flip on the lexicographic accident of the serialized request names
        // (the within-tier sort key is the JSON encoding).
        Some(RequestType::DeactivateListener(_)) => 10,
        Some(RequestType::RemoveListener(_)) => 11,
        // Cluster removals come last. Nothing else in a batch depends on a
        // cluster going (its frontends and backends are removed before, and
        // a listener does not name one), and on 2.3, where the removal takes
        // the cluster's frontends and backends with it, they are gone by
        // then: the batch removes them explicitly (`expand_cluster_removals`). Last, a batch that fails before a cluster's removal lands
        // still holds the cluster, and one whose cluster removal landed has
        // finished its layer-4 listener teardown: a failure never leaves a
        // removed cluster next to a listener the IR cannot describe, which
        // would keep the stale baseline that sends the cluster's backends
        // without it, refused on 2.3 (`ClusterNotFound`).
        Some(RequestType::RemoveCluster(_)) => 12,
        _ => 100,
    }
}

/// Reorder into dependency-safe tiers with a deterministic secondary key.
fn canonicalize(requests: Vec<Request>) -> Vec<Request> {
    canonicalize_ranked(requests.into_iter().map(|r| (CertRank::Gains, r)).collect())
}

/// [`canonicalize`], with a certificate request's [`CertRank`] ordering it
/// within its tier ahead of the JSON encoding.
fn canonicalize_ranked(mut requests: Vec<(CertRank, Request)>) -> Vec<Request> {
    requests.sort_by_cached_key(|(rank, req)| {
        let key = serde_json::to_string(req).unwrap_or_default();
        (tier(req), *rank, key)
    });
    requests.into_iter().map(|(_, req)| req).collect()
}

/// Where a certificate request sits among the `AddCertificate` and
/// `ReplaceCertificate` of one batch, which share a tier.
///
/// The builder gives a contested SNI name to one certificate and strips it
/// from the others, so a name can move between two loaded certificates in
/// one batch: the winner gains it (an add, or a same-fingerprint replace),
/// the loser drops it (a same-fingerprint replace). Each is applied at once,
/// per worker, since Sōzu 2.3.2 (0bc160b7), so if the loser went first the
/// name would fall back to the default certificate until the winner's
/// request landed (measured: `docs/probes/sni-ownership-race_sozu-2.3.2_*`).
/// Ordered by the JSON encoding, an add happens to precede a
/// replace, but two replaces follow their PEM text. This makes the order
/// explicit: nothing that only takes names away runs before anything that
/// gives one, and among the replaces that do both, every name that moves is
/// given before it is taken ([`transfer_order`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CertRank {
    /// Takes no name away: an add, a rotation (same names), a chain update, or
    /// an in-place replace that only adds names. Requests outside the
    /// certificates rank here too; a rank only orders within a tier.
    Gains,
    /// Takes a name away and gives another, at its position in
    /// [`transfer_order`]: after every trade that gives it a name it takes,
    /// unless the two are on a cycle of transfers.
    Trades(usize),
    /// Only takes names away: nothing can be waiting for it.
    Withdraws,
}

/// An SNI name as Sōzu's resolver keys it: per listener, ASCII-lowercased.
type SniKey = (SocketAddr, String);

fn sni_keys(c: &ir::Certificate) -> BTreeSet<SniKey> {
    c.names
        .iter()
        .map(|n| (c.listener, n.to_ascii_lowercase()))
        .collect()
}

/// A same-fingerprint `ReplaceCertificate` that gives some names to its
/// certificate and takes others off it.
struct Trade {
    request: Request,
    gives: BTreeSet<SniKey>,
    takes: BTreeSet<SniKey>,
}

/// The order the trades of one batch run in, as each trade's position: for
/// every name one trade gives and another takes, the one giving it goes
/// first, so the name never falls back to the default certificate between
/// the two. A chain of transfers (`{x}` → `{y}` on one certificate while `{y}`
/// → `{z}` on another) runs recipient first whatever the PEM texts. What the
/// transfers leave unordered follows the JSON encoding, as everywhere in
/// [`canonicalize_ranked`]: of the orders that respect them, this is the
/// smallest by that key, so the batch is deterministic.
///
/// Transfers can form a cycle — certificates swapping names, each gaining
/// one another on the cycle drops — and then no order gives every name
/// first. The trades on a cycle keep the JSON order among themselves (they
/// still run after every trade that gives them a name from outside it, and
/// before every one they give a name to), so only a name on a cycle can
/// fall back to the default certificate for a moment.
///
/// Linear in the names moved: a name is one node between the trades giving
/// it and those taking it, not an edge per (giver, taker) pair, and cycles
/// are found as strongly connected components.
fn transfer_order(trades: &[Trade]) -> Vec<usize> {
    let count = trades.len();
    // Each trade's place in the JSON order: the tie-break.
    let mut by_key: Vec<usize> = (0..count).collect();
    let keys: Vec<String> = trades
        .iter()
        .map(|t| serde_json::to_string(&t.request).unwrap_or_default())
        .collect();
    by_key.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
    let mut seq = vec![0; count];
    for (place, &trade) in by_key.iter().enumerate() {
        seq[trade] = place;
    }

    // Nodes: the trades, then one per name both given and taken here.
    let mut movers: BTreeMap<&SniKey, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
    for (i, t) in trades.iter().enumerate() {
        for name in &t.gives {
            movers.entry(name).or_default().0.push(i);
        }
        for name in &t.takes {
            movers.entry(name).or_default().1.push(i);
        }
    }
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (givers, takers) in movers.into_values() {
        if givers.is_empty() || takers.is_empty() {
            continue;
        }
        let name = edges.len();
        for giver in givers {
            edges[giver].push(name);
        }
        edges.push(takers);
    }

    let (component, components) = strongly_connected(&edges);
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); components];
    for (node, &c) in component.iter().enumerate() {
        members[c].push(node);
    }
    let mut waiting = vec![0usize; components];
    for (node, targets) in edges.iter().enumerate() {
        for &target in targets {
            if component[target] != component[node] {
                waiting[component[target]] += 1;
            }
        }
    }
    // A component holding no trade emits nothing and runs as soon as it is
    // ready; one holding trades runs at its first trade's JSON place.
    let priority = |c: usize| {
        members[c]
            .iter()
            .filter(|&&node| node < count)
            .map(|&node| seq[node])
            .min()
            .map_or((false, 0), |first| (true, first))
    };
    let mut ready: BinaryHeap<Reverse<((bool, usize), usize)>> = (0..components)
        .filter(|&c| waiting[c] == 0)
        .map(|c| Reverse((priority(c), c)))
        .collect();
    let mut position = vec![0; count];
    let mut next = 0;
    while let Some(Reverse((_, c))) = ready.pop() {
        let mut on_it: Vec<usize> = members[c].iter().copied().filter(|&n| n < count).collect();
        on_it.sort_by_key(|&trade| seq[trade]);
        for trade in on_it {
            position[trade] = next;
            next += 1;
        }
        for &node in &members[c] {
            for &target in &edges[node] {
                let t = component[target];
                if t != c {
                    waiting[t] -= 1;
                    if waiting[t] == 0 {
                        ready.push(Reverse((priority(t), t)));
                    }
                }
            }
        }
    }
    position
}

/// The strongly connected components of a graph given as adjacency lists:
/// each node's component, and how many there are (Tarjan's algorithm, with
/// an explicit stack so a long chain cannot overflow the thread's).
fn strongly_connected(edges: &[Vec<usize>]) -> (Vec<usize>, usize) {
    const UNSEEN: usize = usize::MAX;
    let nodes = edges.len();
    let mut index = vec![UNSEEN; nodes];
    let mut low = vec![0; nodes];
    let mut on_stack = vec![false; nodes];
    let mut stack: Vec<usize> = Vec::new();
    let mut component = vec![UNSEEN; nodes];
    let (mut next_index, mut components) = (0, 0);
    // (node, next edge to follow)
    let mut calls: Vec<(usize, usize)> = Vec::new();
    for root in 0..nodes {
        if index[root] != UNSEEN {
            continue;
        }
        index[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;
        calls.push((root, 0));
        while let Some(&(node, edge)) = calls.last() {
            if let Some(&target) = edges[node].get(edge) {
                if let Some(call) = calls.last_mut() {
                    call.1 += 1;
                }
                if index[target] == UNSEEN {
                    index[target] = next_index;
                    low[target] = next_index;
                    next_index += 1;
                    stack.push(target);
                    on_stack[target] = true;
                    calls.push((target, 0));
                } else if on_stack[target] {
                    low[node] = low[node].min(index[target]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(parent, _)) = calls.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component[member] = components;
                    if member == node {
                        break;
                    }
                }
                components += 1;
            }
        }
    }
    (component, components)
}

/// Every backend and frontend of the desired state names a cluster it declares.
///
/// The builder only routes to a Service it resolved, and resolving one inserts
/// its cluster, so this never fails on its output. It guards the batch shape
/// [`expand_cluster_removals`] relies on: `ConfigState::diff` would re-add a
/// frontend still naming a cluster it removes *after* that cluster's
/// `RemoveCluster` (upstream's "add back" case), while the tiers place the add
/// first and the 2.3 cascade would then delete it — reported as applied.
/// Refusing the state loudly beats applying it wrong. The previous state, read
/// back from disk, is not checked: it describes what was applied.
fn check_cluster_references(desired: &ir::Ir) -> Result<(), TranslatorError> {
    let declared: BTreeSet<&str> = desired.clusters.iter().map(|c| c.id.as_str()).collect();
    let unknown = |id: &str| !declared.contains(id);
    if let Some(b) = desired.backends.iter().find(|b| unknown(&b.cluster_id)) {
        return Err(TranslatorError::UnknownCluster(
            "a backend",
            b.cluster_id.clone(),
        ));
    }
    if let Some(id) = desired
        .frontends
        .iter()
        .filter_map(|f| f.cluster_id.as_deref())
        .find(|id| unknown(id))
    {
        return Err(TranslatorError::UnknownCluster(
            "an HTTP frontend",
            id.to_string(),
        ));
    }
    if let Some(f) = desired.l4_frontends.iter().find(|f| unknown(&f.cluster_id)) {
        return Err(TranslatorError::UnknownCluster(
            "an L4 frontend",
            f.cluster_id.clone(),
        ));
    }
    Ok(())
}

/// Spell out what each `RemoveCluster` of a diff removes.
///
/// Since Sōzu 2.3.0 `RemoveCluster` also drops every frontend and backend
/// naming the cluster, and `ConfigState::diff` relies on it: it emits the
/// cluster removal first and no removal for what the cascade takes. That
/// order is lost to `canonicalize`, which tiers cluster removes after frontend
/// adds, so a route re-pointed from a removed cluster to another would be added
/// while the old one still holds its key (`StateError::Exists`). With the
/// explicit removes back in the batch they tier ahead of the adds again, and
/// the cascade finds nothing left to drop.
fn expand_cluster_removals(previous: &ConfigState, requests: Vec<Request>) -> Vec<Request> {
    let mut out = Vec::with_capacity(requests.len());
    for req in requests {
        if let Some(RequestType::RemoveCluster(cluster_id)) = &req.request_type {
            out.extend(previous.cluster_removal_requests(cluster_id));
        }
        out.push(req);
    }
    out
}

/// Drop every `RemoveBackend` whose (cluster_id, backend_id, address) triple
/// also appears as an `AddBackend` in the same batch.
///
/// `ConfigState::diff` emits a *changed* backend (same key, e.g. a new weight
/// or sticky id) as Remove-then-Add, but `canonicalize` reorders backend adds
/// (tier 3) before backend removes (tier 7), turning that pair into
/// Add-then-Remove. Sōzu's
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
    for req in canonicalize(requests) {
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

/// Minimal certificate requests to converge `previous` → `desired`. Identity is
/// (listener, fingerprint) — matching Sōzu's own cert store — so the same cert on
/// two listeners is tracked independently. Both sides are grouped by that
/// identity first (`keyed_certs`), so duplicate entries for one cert converge
/// to a single entry carrying the union of their SNI names. Handles:
///  - new cert at (listener, fp)        -> AddCertificate
///  - cert gone from (listener, fp)     -> RemoveCertificate
///  - same (listener, fp), names or served chain differ -> ReplaceCertificate
///    (same fp; Sōzu skips a plain AddCertificate whose fp already exists, so a
///    Replace is the only way to update a loaded certificate in place, and
///    since Sōzu 2.3.2 a same-fingerprint Replace whose names or material
///    differ is applied per worker without a gap — 2.2.1 ignored it). A key
///    that only changed encoding is no change: the builder admits only the
///    leaf's own key.
///  - rotation (a removed + an added at the same listener sharing the SNI name
///    set) -> a single ReplaceCertificate (zero-gap)
///
/// Each request carries its [`CertRank`]: an in-place replace that drops a
/// name ranks after the requests that may be giving that name to another
/// certificate, and one that also gains a name runs after every replace
/// giving it a name it takes ([`transfer_order`]).
fn certificate_requests(
    previous: &[ir::Certificate],
    desired: &[ir::Certificate],
) -> Result<Vec<(CertRank, Request)>, TranslatorError> {
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

    let mut out = Vec::new();
    let mut trades: Vec<Trade> = Vec::new();
    let mut truly_added: Vec<&KeyedCert> = Vec::new();

    for d in &des {
        match prev_by_key.get(&(d.listener, d.fingerprint.as_str())) {
            // Same (listener, fp): in place. A name or chain change needs a
            // request; the leaf, and with it the key, cannot differ.
            Some(p) => {
                if names_set(&p.cert) != names_set(&d.cert) || p.chain != d.chain {
                    let request = replace_certificate_request(&d.cert, d.fingerprint.clone());
                    let (old, new) = (sni_keys(&p.cert), sni_keys(&d.cert));
                    let gives: BTreeSet<SniKey> = new.difference(&old).cloned().collect();
                    let takes: BTreeSet<SniKey> = old.difference(&new).cloned().collect();
                    match (gives.is_empty(), takes.is_empty()) {
                        (_, true) => out.push((CertRank::Gains, request)),
                        (true, false) => out.push((CertRank::Withdraws, request)),
                        (false, false) => trades.push(Trade {
                            request,
                            gives,
                            takes,
                        }),
                    }
                }
            }
            None => truly_added.push(d),
        }
    }
    let order = transfer_order(&trades);
    for (trade, position) in trades.into_iter().zip(order) {
        out.push((CertRank::Trades(position), trade.request));
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
            out.push((
                CertRank::Gains,
                replace_certificate_request(&new.cert, truly_removed[idx].fingerprint.clone()),
            ));
        } else {
            out.push((CertRank::Gains, add_certificate_request(&new.cert)));
        }
    }
    // A removal takes names away too, but it already tiers after every add
    // and replace.
    for (i, old) in truly_removed.iter_mut().enumerate() {
        if !used[i] {
            out.push((
                CertRank::Withdraws,
                remove_certificate_request(old.listener, old.fingerprint.clone()),
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
    canonicalize(requests)
}

/// Minimal, dependency-safe requests to converge a `previous` applied IR towards
/// the `desired` IR. Idempotent: `reconcile(&ir, &ir)` is empty. The controller
/// keeps `previous` as its shadow and swaps it to `desired` only after a
/// successful apply.
pub fn reconcile(previous: &ir::Ir, desired: &ir::Ir) -> Result<Vec<Request>, TranslatorError> {
    check_cluster_references(desired)?;
    let previous_state = routing_state(previous)?;
    let routing = drop_superseded_backend_removes(expand_cluster_removals(
        &previous_state,
        previous_state.diff(&routing_state(desired)?),
    ));
    let mut requests: Vec<(CertRank, Request)> = routing
        .into_iter()
        .map(|req| (CertRank::Gains, req))
        .collect();
    requests.extend(certificate_requests(
        &previous.certificates,
        &desired.certificates,
    )?);
    let mut requests = canonicalize_ranked(requests);
    // `ConfigState::diff` emits the activation of a newly-added active TCP/UDP
    // listener twice (once inline, once in its trailing activation sweep).
    // After `canonicalize` the batch is fully sorted, so identical requests
    // are adjacent and the duplicate collapses here.
    requests.dedup();
    Ok(requests)
}

/// An address as Sōzu's wire type carries it: IP and port, without an IPv6
/// flow label or scope id, so a payload and an IR address compare equal.
fn plain(address: SocketAddr) -> SocketAddr {
    SocketAddr::new(address.ip(), address.port())
}

/// Lower-case hex, the spelling Sōzu and [`fingerprint`] use.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod transfer_order_tests {
    use super::{remove_certificate_request, transfer_order, SniKey, Trade};
    use std::collections::BTreeSet;

    /// A trade whose JSON encoding sorts by `id`, giving and taking `names`.
    fn trade(id: &str, gives: &[&str], takes: &[&str]) -> Trade {
        let listener = "0.0.0.0:443".parse().expect("address");
        let keys = |names: &[&str]| -> BTreeSet<SniKey> {
            names.iter().map(|n| (listener, n.to_string())).collect()
        };
        Trade {
            request: remove_certificate_request(listener, id.to_string()),
            gives: keys(gives),
            takes: keys(takes),
        }
    }

    #[test]
    fn unrelated_trades_keep_the_json_order() {
        let trades = [
            trade("c", &["c1"], &["c2"]),
            trade("a", &["a1"], &["a2"]),
            trade("b", &["b1"], &["b2"]),
        ];
        assert_eq!(transfer_order(&trades), [2, 0, 1]);
    }

    #[test]
    fn a_chain_runs_from_its_last_recipient_back_to_its_first_donor() {
        // `c` gives n1, which `b` takes; `b` gives n2, which `a` takes: the
        // reverse of the JSON order.
        let trades = [
            trade("a", &["a1"], &["n2"]),
            trade("b", &["n2"], &["n1"]),
            trade("c", &["n1"], &["c1"]),
        ];
        assert_eq!(transfer_order(&trades), [2, 1, 0]);
    }

    #[test]
    fn a_cycle_keeps_the_json_order_inside_and_its_edges_outside() {
        // `p` and `q` swap x and y. `e` gives `q` a name, so runs before the
        // cycle; `p` gives `d` one, so `d` runs after it, though `d` sorts
        // first.
        let trades = [
            trade("d", &["d1"], &["z"]),
            trade("e", &["w"], &["e1"]),
            trade("p", &["y", "z"], &["x"]),
            trade("q", &["x"], &["y", "w"]),
        ];
        assert_eq!(transfer_order(&trades), [3, 0, 1, 2]);
    }

    #[test]
    fn a_long_chain_is_ordered_without_deep_recursion() {
        const LENGTH: usize = 50_000;
        let names: Vec<String> = (0..=LENGTH).map(|i| format!("n{i}")).collect();
        // Trade i gives n{i+1}, which trade i+1 takes: the chain runs in the
        // JSON order, and the search for cycles walks it end to end from the
        // first trade, 100,000 nodes deep.
        let forwards: Vec<Trade> = (0..LENGTH)
            .map(|i| trade(&format!("{i:06}"), &[&names[i + 1]], &[&names[i]]))
            .collect();
        let order = transfer_order(&forwards);
        assert!(order.iter().enumerate().all(|(i, &p)| p == i));
        // Trade i gives n{i}, which trade i-1 takes: every donor sorts before
        // its recipient, so the chain runs backwards.
        let backwards: Vec<Trade> = (0..LENGTH)
            .map(|i| trade(&format!("{i:06}"), &[&names[i]], &[&names[i + 1]]))
            .collect();
        let order = transfer_order(&backwards);
        assert!(order.iter().enumerate().all(|(i, &p)| p == LENGTH - 1 - i));
    }
}
