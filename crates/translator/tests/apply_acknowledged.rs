//! `apply_acknowledged` and `replay_if_exact`: the baseline after a batch
//! failed part-way, and whether the controller adopts it.
//!
//! Sōzu's main process is stood in for by `sozu-command-lib`'s own
//! `ConfigState` (the linked version, 2.3.2), fed the way the agent feeds
//! Sōzu: in order; a teardown of something already gone settled, a
//! frontend's after evicting whatever still holds its key; a duplicate
//! listener add settled; a duplicate frontend add repaired (evict, re-add);
//! a replace on a listener holding no certificate sent as an add; anything
//! else refused, which stops the batch. [`Sozu::V23`] layers on top the 2.3
//! behaviours a lagging baseline trips over (see [`dispatch`]). The linked
//! 2.3.2 state carries them itself, so every case run in both modes also
//! checks that the stand-in agrees with it.

use std::collections::BTreeSet;
use std::net::SocketAddr;

use sozu_command_lib::proto::command::{
    request::RequestType, AddCertificate, RemoveCertificate, Request, UdpAffinityKey,
};
use sozu_command_lib::state::{ConfigState, StateError};
use sozu_command_lib::ObjectKind;
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

const CERT_A: &str = include_str!("fixtures/cert_a.pem");
const KEY_A: &str = include_str!("fixtures/key_a.pem");
const CERT_B: &str = include_str!("fixtures/cert_b.pem");
const KEY_B: &str = include_str!("fixtures/key_b.pem");

const HTTP: &str = "0.0.0.0:8080";
const HTTPS: &str = "0.0.0.0:8443";
const OTHER_HTTPS: &str = "0.0.0.0:9443";

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("valid socket addr in test")
}

fn cluster(id: &str) -> ir::Cluster {
    ir::Cluster {
        id: id.to_string(),
        load_balancing: ir::LbAlgorithm::RoundRobin,
        sticky_session: false,
        https_redirect: false,
        max_connections_per_ip: None,
        retry_after: None,
        http2: false,
        health_check: None,
    }
}

fn backend(cluster_id: &str, address: &str) -> ir::Backend {
    ir::Backend {
        cluster_id: cluster_id.to_string(),
        backend_id: format!("{cluster_id}-{}", address.replace([':', '.'], "-")),
        address: addr(address),
        weight: None,
        sticky_id: None,
    }
}

fn route(host: &str, path: ir::PathMatch, cluster_id: Option<&str>, tls: bool) -> ir::Frontend {
    ir::Frontend {
        hostname: host.to_string(),
        path,
        method: None,
        cluster_id: cluster_id.map(str::to_string),
        tls,
        listener: addr(if tls { HTTPS } else { HTTP }),
        filters: ir::FrontendFilters::default(),
        header_matches: vec![],
    }
}

/// `frontend` guarded by one header match, which sends it on Sōzu 2.3's
/// header-predicate verbs and adds the predicate to its route key.
fn header_matched(frontend: ir::Frontend, name: &str, kind: ir::HeaderMatchKind) -> ir::Frontend {
    ir::Frontend {
        header_matches: vec![ir::HeaderMatch {
            name: name.to_string(),
            kind,
        }],
        ..frontend
    }
}

fn root() -> ir::PathMatch {
    ir::PathMatch::Prefix("/".into())
}

fn cert(certificate: &str, key: &str, listener: &str, names: &[&str]) -> ir::Certificate {
    ir::Certificate {
        listener: addr(listener),
        certificate: certificate.to_string(),
        chain: vec![],
        key: key.to_string(),
        names: names.iter().map(|n| n.to_string()).collect(),
    }
}

fn l4(protocol: ir::L4Protocol, port: u16, cluster_id: &str) -> ir::L4Frontend {
    ir::L4Frontend {
        protocol,
        listener: SocketAddr::from(([0, 0, 0, 0], port)),
        cluster_id: cluster_id.to_string(),
    }
}

/// Re-wrap a PEM body at another width: different text, same DER, so the same
/// fingerprint (Sōzu's certificate identity).
fn rewrap_pem(pem: &str) -> String {
    let mut header = "";
    let mut footer = "";
    let mut body = String::new();
    for line in pem.lines() {
        if line.starts_with("-----BEGIN") {
            header = line;
        } else if line.starts_with("-----END") {
            footer = line;
        } else {
            body.push_str(line.trim());
        }
    }
    let wrapped: Vec<&str> = body
        .as_bytes()
        .chunks(48)
        .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
        .collect();
    format!("{header}\n{}\n{footer}\n", wrapped.join("\n"))
}

/// Two HTTP clusters with their backends, a route on each listener for `x`,
/// an `Exact` route for `y`, a redirect-only route that names no cluster, and
/// one certificate.
fn applied() -> ir::Ir {
    ir::Ir {
        clusters: vec![cluster("x"), cluster("y")],
        backends: vec![backend("x", "10.0.0.1:80"), backend("y", "10.0.1.1:80")],
        frontends: vec![
            route("x.example.com", root(), Some("x"), false),
            route("x.example.com", root(), Some("x"), true),
            route(
                "y.example.com",
                ir::PathMatch::Exact("/v1".into()),
                Some("y"),
                false,
            ),
            ir::Frontend {
                filters: ir::FrontendFilters {
                    redirect: Some(ir::Redirect {
                        scheme: Some(ir::Scheme::Https),
                        status: ir::RedirectStatus::MovedPermanently,
                        hostname: None,
                        path: None,
                        port: None,
                    }),
                    ..ir::FrontendFilters::default()
                },
                ..route("r.example.com", root(), None, false)
            },
        ],
        certificates: vec![cert(CERT_A, KEY_A, HTTPS, &["x.example.com"])],
        l4_frontends: vec![],
    }
}

/// `ir` without cluster `id` and everything naming it.
fn without_cluster(ir: &ir::Ir, id: &str) -> ir::Ir {
    let mut out = ir.clone();
    out.clusters.retain(|c| c.id != id);
    out.backends.retain(|b| b.cluster_id != id);
    out.frontends
        .retain(|f| f.cluster_id.as_deref() != Some(id));
    out.l4_frontends.retain(|f| f.cluster_id != id);
    out
}

/// `ir` with every list in one order, to compare two IRs as sets.
fn sorted(ir: &ir::Ir) -> ir::Ir {
    let mut out = ir.clone();
    out.clusters.sort_by_cached_key(|c| format!("{c:?}"));
    out.backends.sort_by_cached_key(|b| format!("{b:?}"));
    out.frontends.sort_by_cached_key(|f| format!("{f:?}"));
    out.certificates.sort_by_cached_key(|c| format!("{c:?}"));
    out.l4_frontends.sort_by_cached_key(|f| format!("{f:?}"));
    out
}

// ----------------------------------------------------------------------------
// The stand-in Sōzu
// ----------------------------------------------------------------------------

/// Which data plane the stand-in behaves like.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sozu {
    Linked,
    V23,
}

/// What the main process does with one request. `Linked` is the linked
/// `ConfigState` as is (2.3.2; it was 2.2.1 when these rules were written). `V23` adds what 2.3 changed and a lagging baseline
/// runs into: an `AddBackend` naming a cluster it does not hold is refused;
/// `RemoveCluster` takes the cluster's backends and frontends with it;
/// removing the last certificate at a listener drops the listener's entry,
/// so a replace there is refused; a UDP address held by one cluster is
/// refused to another. (2.3 also refuses an HTTP route removal naming
/// another cluster than the stored one, which the agent then evicts as
/// stored: the same end state as 2.2.1's removal by key alone.)
fn dispatch(sozu: Sozu, state: &mut ConfigState, request: &Request) -> Result<(), StateError> {
    if sozu == Sozu::V23 {
        match &request.request_type {
            Some(RequestType::AddBackend(b))
                if !state.clusters.contains_key(b.cluster_id.as_str()) =>
            {
                return Err(StateError::NotFound {
                    kind: ObjectKind::Cluster,
                    id: b.cluster_id.clone(),
                });
            }
            Some(RequestType::AddUdpFrontend(f)) => {
                let address: SocketAddr = f.address.into();
                let claimed = state.udp_fronts.iter().any(|(id, fronts)| {
                    &**id != f.cluster_id.as_str()
                        && fronts.iter().any(|held| held.address == address)
                });
                if claimed {
                    return Err(StateError::Exists {
                        kind: ObjectKind::UdpFrontend,
                        id: address.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    state.dispatch(request)?;
    if sozu == Sozu::V23 {
        match &request.request_type {
            Some(RequestType::RemoveCluster(id)) => {
                state.backends.remove(id.as_str());
                state.tcp_fronts.remove(id.as_str());
                state.udp_fronts.remove(id.as_str());
                state
                    .http_fronts
                    .retain(|_, f| f.cluster_id.as_deref() != Some(id));
                state
                    .https_fronts
                    .retain(|_, f| f.cluster_id.as_deref() != Some(id));
            }
            Some(RequestType::RemoveCertificate(c)) => {
                let address: SocketAddr = c.address.into();
                if state
                    .certificates
                    .get(&address)
                    .is_some_and(|held| held.is_empty())
                {
                    state.certificates.remove(&address);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_teardown(request: &Request) -> bool {
    matches!(
        request.request_type,
        Some(
            RequestType::RemoveCluster(_)
                | RequestType::RemoveBackend(_)
                | RequestType::RemoveHttpFrontend(_)
                | RequestType::RemoveHttpsFrontend(_)
                | RequestType::RemoveHttpFrontendWithHeaderPredicates(_)
                | RequestType::RemoveHttpsFrontendWithHeaderPredicates(_)
                | RequestType::RemoveTcpFrontend(_)
                | RequestType::RemoveUdpFrontend(_)
                | RequestType::RemoveListener(_)
                | RequestType::RemoveCertificate(_)
                | RequestType::DeactivateListener(_)
        )
    )
}

/// The agent's eviction: remove whatever the main process stores under this
/// frontend request's key, whichever cluster it names. An HTTP(S) key carries
/// the frontend's header predicates, on whichever verb it was sent.
fn evict_holder(state: &mut ConfigState, request: &Request) {
    match &request.request_type {
        Some(
            RequestType::AddHttpFrontend(f)
            | RequestType::RemoveHttpFrontend(f)
            | RequestType::AddHttpFrontendWithHeaderPredicates(f)
            | RequestType::RemoveHttpFrontendWithHeaderPredicates(f),
        ) => {
            state.http_fronts.remove(&f.to_string());
        }
        Some(
            RequestType::AddHttpsFrontend(f)
            | RequestType::RemoveHttpsFrontend(f)
            | RequestType::AddHttpsFrontendWithHeaderPredicates(f)
            | RequestType::RemoveHttpsFrontendWithHeaderPredicates(f),
        ) => {
            state.https_fronts.remove(&f.to_string());
        }
        Some(RequestType::AddTcpFrontend(f) | RequestType::RemoveTcpFrontend(f)) => {
            let address: SocketAddr = f.address.into();
            for fronts in state.tcp_fronts.values_mut() {
                fronts.retain(|held| held.address != address);
            }
        }
        Some(RequestType::AddUdpFrontend(f) | RequestType::RemoveUdpFrontend(f)) => {
            let address: SocketAddr = f.address.into();
            for fronts in state.udp_fronts.values_mut() {
                fronts.retain(|held| held.address != address);
            }
        }
        _ => {}
    }
}

/// Whether the agent gets `request` acknowledged by `state`.
fn settle(sozu: Sozu, state: &mut ConfigState, request: &Request) -> bool {
    match dispatch(sozu, state, request) {
        Ok(()) => true,
        Err(StateError::NotFound { .. } | StateError::NoChange) if is_teardown(request) => {
            evict_holder(state, request);
            true
        }
        Err(StateError::NotFound {
            kind: ObjectKind::Certificate,
            ..
        }) => match &request.request_type {
            Some(RequestType::ReplaceCertificate(r)) => {
                let add: Request = RequestType::AddCertificate(AddCertificate {
                    address: r.address,
                    certificate: r.new_certificate.clone(),
                    expired_at: r.new_expired_at,
                })
                .into();
                dispatch(sozu, state, &add).is_ok()
            }
            _ => false,
        },
        Err(StateError::Exists { .. }) => match &request.request_type {
            Some(RequestType::AddTcpListener(_) | RequestType::AddUdpListener(_)) => true,
            Some(
                RequestType::AddHttpFrontend(_)
                | RequestType::AddHttpsFrontend(_)
                | RequestType::AddHttpFrontendWithHeaderPredicates(_)
                | RequestType::AddHttpsFrontendWithHeaderPredicates(_)
                | RequestType::AddTcpFrontend(_)
                | RequestType::AddUdpFrontend(_),
            ) => {
                evict_holder(state, request);
                dispatch(sozu, state, request).is_ok()
            }
            _ => false,
        },
        // Sōzu 2.3.2 refuses a UDP address another cluster holds with an
        // error of its own; the agent repairs it as it does `Exists`.
        Err(StateError::UdpFrontendAddressTaken { .. })
            if matches!(request.request_type, Some(RequestType::AddUdpFrontend(_))) =>
        {
            evict_holder(state, request);
            dispatch(sozu, state, request).is_ok()
        }
        Err(_) => false,
    }
}

/// Feed `batch` to `state` the way the agent does, and return how many
/// requests it acknowledged before one was refused.
fn feed(sozu: Sozu, state: &mut ConfigState, batch: &[Request]) -> usize {
    batch
        .iter()
        .position(|request| !settle(sozu, state, request))
        .unwrap_or(batch.len())
}

/// What the main process holds once `ir` is applied from scratch.
fn state_of(sozu: Sozu, ir: &ir::Ir) -> ConfigState {
    let mut state = ConfigState::new();
    let requests = tr::ir_to_requests(ir);
    assert_eq!(feed(sozu, &mut state, &requests), requests.len());
    state
}

/// A stored object, every field of it (its `Debug` is redacted).
macro_rules! json {
    ($value:expr) => {
        serde_json::to_string($value).expect("serialisable")
    };
}

/// Everything `state` routes with, one line each, for comparing two states
/// and for a readable assertion message. A line about a layer-4 object
/// carries its address between spaces.
fn holds(state: &ConfigState) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (id, c) in &state.clusters {
        out.insert(format!("cluster {id} {}", json!(c)));
    }
    for (id, backends) in &state.backends {
        for b in backends {
            out.insert(format!("backend {id} {}", json!(b)));
        }
    }
    for (key, front) in &state.http_fronts {
        out.insert(format!("http {key} -> {}", json!(front)));
    }
    for (key, front) in &state.https_fronts {
        out.insert(format!("https {key} -> {}", json!(front)));
    }
    for (id, fronts) in &state.tcp_fronts {
        for f in fronts {
            out.insert(format!("tcp {} -> {id}", f.address));
        }
    }
    for (id, fronts) in &state.udp_fronts {
        for f in fronts {
            out.insert(format!("udp {} -> {id}", f.address));
        }
    }
    for (address, listener) in &state.tcp_listeners {
        out.insert(format!("tcp listener {address} active={}", listener.active));
    }
    for (address, listener) in &state.udp_listeners {
        out.insert(format!("udp listener {address} active={}", listener.active));
    }
    for (address, certificates) in &state.certificates {
        for (fingerprint, certificate) in certificates {
            out.insert(format!(
                "certificate {address} {:02x?} names={:?}",
                fingerprint.0, certificate.names
            ));
        }
    }
    out
}

/// A removal Sōzu refuses outright: the main process cannot decode the
/// fingerprint, so nothing is applied and the agent does not settle it.
fn refused() -> Request {
    RequestType::RemoveCertificate(RemoveCertificate {
        address: addr(HTTPS).into(),
        fingerprint: "zz".to_string(),
    })
    .into()
}

fn one_request(previous: &ir::Ir, desired: &ir::Ir) -> Request {
    let mut requests = tr::reconcile(previous, desired).expect("translate");
    assert_eq!(requests.len(), 1, "{requests:?}");
    requests.remove(0)
}

// ----------------------------------------------------------------------------
// The batches
// ----------------------------------------------------------------------------

/// A batch from `previous` to `desired`, and a `third` state reached after
/// it failed, which drops what the batch changed.
struct Case {
    name: &'static str,
    previous: ir::Ir,
    desired: ir::Ir,
    third: ir::Ir,
}

/// Route `k.example.com` re-pointed from cluster `a` to cluster `b` on both
/// listeners, while other routes keep both clusters alive. `third` deletes
/// it.
fn repoint() -> Case {
    let routes = |k: Option<&str>| {
        let mut frontends = vec![
            route("a.example.com", root(), Some("a"), false),
            route("b.example.com", root(), Some("b"), false),
        ];
        if let Some(k) = k {
            frontends.push(route("k.example.com", root(), Some(k), false));
            frontends.push(route("k.example.com", root(), Some(k), true));
        }
        ir::Ir {
            clusters: vec![cluster("a"), cluster("b")],
            backends: vec![backend("a", "10.0.5.1:80"), backend("b", "10.0.6.1:80")],
            frontends,
            certificates: vec![],
            l4_frontends: vec![],
        }
    };
    Case {
        name: "re-point",
        previous: routes(Some("a")),
        desired: routes(Some("b")),
        third: routes(None),
    }
}

/// Cluster `x` removed. `third` wants it back with a second backend: from a
/// baseline that still lists `x`, only that backend's `AddBackend` goes out.
fn cluster_removal() -> Case {
    let previous = applied();
    let mut third = previous.clone();
    third.backends.push(backend("x", "10.0.0.2:80"));
    Case {
        name: "cluster removal",
        desired: without_cluster(&previous, "x"),
        previous,
        third,
    }
}

/// A rotation on one listener (a replace), a certificate swapped for one
/// with other names on another (an add and a removal). `third` renames the
/// rotated certificate and drops the other.
fn certificate_rotation() -> Case {
    let mut previous = applied();
    previous
        .certificates
        .push(cert(CERT_B, KEY_B, OTHER_HTTPS, &["b.example.com"]));
    let mut desired = applied();
    desired.certificates = vec![
        cert(CERT_B, KEY_B, HTTPS, &["x.example.com"]),
        cert(CERT_A, KEY_A, OTHER_HTTPS, &["a.example.com"]),
    ];
    let mut third = desired.clone();
    third.certificates = vec![cert(
        CERT_B,
        KEY_B,
        HTTPS,
        &["x.example.com", "www.x.example.com"],
    )];
    Case {
        name: "certificate rotation",
        previous,
        desired,
        third,
    }
}

/// A TCP and a UDP route moved to other ports. `third` deletes both routes.
fn layer4_move() -> Case {
    let mut previous = applied();
    previous.clusters.extend([cluster("z"), cluster("u")]);
    previous
        .backends
        .extend([backend("z", "10.0.2.1:5432"), backend("u", "10.0.3.1:53")]);
    previous.l4_frontends = vec![
        l4(ir::L4Protocol::Tcp, 5432, "z"),
        l4(ir::L4Protocol::Udp, 5353, "u"),
    ];
    let mut desired = previous.clone();
    desired.l4_frontends = vec![
        l4(ir::L4Protocol::Tcp, 6432, "z"),
        l4(ir::L4Protocol::Udp, 6353, "u"),
    ];
    let mut third = previous.clone();
    third.l4_frontends.clear();
    Case {
        name: "layer-4 move",
        previous,
        desired,
        third,
    }
}

/// A changed route (removal and add on one key), a removed cluster, a new
/// cluster with a route, a moved backend and a rotated certificate. `third`
/// drops the new cluster and the certificate again.
fn mixed() -> Case {
    let previous = applied();
    let mut desired = without_cluster(&previous, "y");
    desired.frontends[0]
        .filters
        .header_mods
        .push(ir::HeaderMod {
            on: ir::HeaderTarget::Response,
            key: "x-served-by".to_string(),
            value: Some("sozu".to_string()),
        });
    desired.clusters.push(cluster("w"));
    desired.backends.push(backend("w", "10.0.4.1:80"));
    desired.backends[0] = backend("x", "10.0.0.9:80");
    desired.frontends.push(route(
        "w.example.com",
        ir::PathMatch::Prefix("/api".into()),
        Some("w"),
        false,
    ));
    desired.certificates = vec![cert(CERT_B, KEY_B, HTTPS, &["x.example.com"])];
    let mut third = without_cluster(&desired, "w");
    third.certificates.clear();
    Case {
        name: "mixed",
        previous,
        desired,
        third,
    }
}

/// Cluster `u` serving a UDP route switches to another algorithm while the
/// route goes; `third` keeps the new algorithm and wants the route back. The
/// translator derives a cluster's UDP affinity from its UDP routes, so the
/// batch's upsert of `u` drops the affinity and `third` spells it back.
fn udp_affinity() -> Case {
    let mut previous = applied();
    previous.clusters.push(cluster("u"));
    previous.backends.push(backend("u", "10.0.3.1:53"));
    previous.l4_frontends = vec![l4(ir::L4Protocol::Udp, 5353, "u")];
    let mut desired = previous.clone();
    desired.clusters[2].load_balancing = ir::LbAlgorithm::Random;
    desired.l4_frontends.clear();
    let mut third = desired.clone();
    third.l4_frontends = previous.l4_frontends.clone();
    Case {
        name: "udp affinity",
        previous,
        desired,
        third,
    }
}

/// Header-matched routes next to the plain ones on `x.example.com`, keyed
/// apart from them by their predicates and sent on Sōzu 2.3's predicate
/// verbs: one re-pointed from cluster `y` to `x` (removal and add on one
/// key), one removed and one added on the HTTPS listener. `third` drops them
/// all.
fn header_matches() -> Case {
    let canary = |cluster: &str, tls: bool, name: &str, kind: ir::HeaderMatchKind| {
        header_matched(
            route("x.example.com", root(), Some(cluster), tls),
            name,
            kind,
        )
    };
    let always = || ir::HeaderMatchKind::Exact("always".into());
    let mut previous = applied();
    previous
        .frontends
        .push(canary("y", false, "x-canary", always()));
    previous.frontends.push(canary(
        "y",
        true,
        "x-env",
        ir::HeaderMatchKind::Regex("pre(prod|view)".into()),
    ));
    let mut desired = applied();
    desired
        .frontends
        .push(canary("x", false, "x-canary", always()));
    desired.frontends.push(canary(
        "y",
        true,
        "x-canary",
        ir::HeaderMatchKind::Regex("al(ways|so)".into()),
    ));
    Case {
        name: "header matches",
        previous,
        desired,
        third: applied(),
    }
}

// ----------------------------------------------------------------------------
// Single requests
// ----------------------------------------------------------------------------

#[test]
fn nothing_acknowledged_changes_nothing() {
    let previous = applied();
    assert_eq!(
        tr::apply_acknowledged(&previous, &mixed().desired, &[]),
        previous
    );
    assert_eq!(
        tr::replay_if_exact(&previous, &mixed().desired, &[]),
        Some(previous)
    );
}

/// An add sets what the desired state holds under its key, replacing what
/// the baseline held there: the desired config for an upserted cluster or
/// backend, every desired frontend under the route key (so the diff sends
/// the same one first), the desired layer-4 frontend on the address. An add
/// whose key the desired state does not hold cannot come from the diff, and
/// changes nothing.
#[test]
fn an_add_takes_what_the_desired_state_holds_under_its_key() {
    let mut previous = applied();
    previous.clusters.push(cluster("w"));
    previous.l4_frontends = vec![l4(ir::L4Protocol::Tcp, 5432, "x")];
    let mut desired = previous.clone();
    desired.clusters[0].sticky_session = true;
    desired.clusters[0].retry_after = Some(5);
    desired.backends[0].weight = Some(5);
    desired.frontends[0].cluster_id = Some("y".to_string());
    // Under the same key as the re-pointed route, never sent: the first wins.
    desired
        .frontends
        .push(route("x.example.com", root(), Some("w"), false));
    desired.l4_frontends = vec![l4(ir::L4Protocol::Tcp, 5432, "w")];
    let batch = tr::reconcile(&previous, &desired).expect("translate");
    let replayed = tr::apply_acknowledged(&previous, &desired, &batch);
    assert_eq!(sorted(&replayed), sorted(&desired));
    assert!(tr::reconcile(&replayed, &desired)
        .expect("translate")
        .is_empty());
    assert_eq!(
        tr::replay_if_exact(&previous, &desired, &batch),
        Some(replayed)
    );

    // The layer-4 re-point acknowledged only as far as its removal: the old
    // frontend stays for the listener it still stands for, a state the IR
    // cannot spell, so the replay is not adopted.
    let removal = batch
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::RemoveTcpFrontend(_))))
        .expect("the layer-4 route is re-pointed");
    let replayed = tr::apply_acknowledged(&previous, &desired, &batch[..=removal]);
    assert_eq!(replayed.l4_frontends, previous.l4_frontends);
    assert_eq!(
        tr::replay_if_exact(&previous, &desired, &batch[..=removal]),
        None
    );

    // Every add again: upserts, and frontend and listener adds the agent
    // settles, so the main process holds what it held.
    let again = tr::ir_to_requests(&previous);
    assert_eq!(
        tr::apply_acknowledged(&previous, &ir::Ir::default(), &again),
        previous,
        "nothing desired to take"
    );
    assert_eq!(
        tr::replay_if_exact(&previous, &ir::Ir::default(), &again),
        Some(previous)
    );
}

/// Requests are replayed in order: a cluster's removal takes out what named
/// it before, not what an add put back after it. (A head the translator
/// never sends, the cluster removed before what names it: only the replay is
/// checked here, since what a main process then holds depends on its
/// version.)
#[test]
fn requests_are_replayed_in_order() {
    let previous = applied();
    let mut desired = previous.clone();
    desired.backends.push(backend("x", "10.0.0.2:80"));
    let adds = tr::ir_to_requests(&desired);
    let add = |wanted: fn(&RequestType) -> bool| {
        adds.iter()
            .find(|r| r.request_type.as_ref().is_some_and(wanted))
            .cloned()
            .expect("the add is there")
    };
    let acknowledged = [
        RequestType::RemoveCluster("x".to_string()).into(),
        add(|r| matches!(r, RequestType::AddCluster(c) if c.cluster_id == "x")),
        add(|r| matches!(r, RequestType::AddBackend(b) if b.backend_id == "x-10-0-0-2-80")),
    ];
    let mut expected = without_cluster(&previous, "x");
    expected.clusters.push(cluster("x"));
    expected.backends.push(backend("x", "10.0.0.2:80"));
    assert_eq!(
        sorted(&tr::apply_acknowledged(&previous, &desired, &acknowledged)),
        sorted(&expected)
    );
}

/// Sōzu 2.3 removes a cluster's backends and frontends with it, so they
/// leave the baseline with it even when the batch did not remove them one by
/// one. A route that names no cluster is not one of them, and a layer-4
/// frontend waits for its listener's removal. (Again a head the translator
/// never sends, so only the replay is checked.)
#[test]
fn a_cluster_removal_drops_what_names_the_cluster() {
    let mut previous = applied();
    previous.l4_frontends = vec![l4(ir::L4Protocol::Tcp, 5432, "x")];
    let desired = without_cluster(&previous, "x");
    let remove_cluster: Request = RequestType::RemoveCluster("x".to_string()).into();
    let replayed =
        tr::apply_acknowledged(&previous, &desired, std::slice::from_ref(&remove_cluster));
    let mut expected = desired.clone();
    expected.l4_frontends = previous.l4_frontends.clone();
    assert_eq!(replayed, expected);
    assert_eq!(
        replayed.frontends.len(),
        2,
        "y's route and the redirect stay"
    );

    let remove_listener = tr::reconcile(&previous, &desired)
        .expect("translate")
        .into_iter()
        .find(|r| matches!(r.request_type, Some(RequestType::RemoveListener(_))))
        .expect("the listener is removed");
    assert_eq!(
        tr::apply_acknowledged(&previous, &desired, &[remove_cluster, remove_listener]),
        desired
    );
}

#[test]
fn a_backend_removal_drops_that_backend_alone() {
    let mut previous = applied();
    previous.backends.push(backend("x", "10.0.0.2:80"));
    // The same endpoint in another cluster is another backend.
    previous.backends.push(ir::Backend {
        cluster_id: "y".to_string(),
        ..backend("x", "10.0.0.2:80")
    });
    let mut desired = previous.clone();
    desired.backends.remove(2);
    let removal = one_request(&previous, &desired);
    assert!(matches!(
        removal.request_type,
        Some(RequestType::RemoveBackend(_))
    ));
    assert_eq!(
        tr::replay_if_exact(&previous, &desired, &[removal]),
        Some(desired)
    );
}

/// Sōzu stores one route per key and the agent evicts a route that another
/// cluster holds under it, so an acknowledged removal leaves nothing under
/// that key: every entry of the baseline under it goes, whatever its
/// cluster, and the other protocol's map is another key.
#[test]
fn a_route_removal_drops_every_entry_under_its_key() {
    let mut previous = applied();
    // An entry under x's HTTPS key that the translator never sent (the first
    // one wins), naming another cluster.
    previous
        .frontends
        .push(route("x.example.com", root(), Some("y"), true));
    let mut desired = previous.clone();
    desired
        .frontends
        .retain(|f| !(f.tls && f.hostname == "x.example.com"));
    let removal = one_request(&previous, &desired);
    assert!(matches!(
        &removal.request_type,
        Some(RequestType::RemoveHttpsFrontend(f)) if f.cluster_id.as_deref() == Some("x")
    ));
    assert_eq!(
        tr::replay_if_exact(&previous, &desired, &[removal]),
        Some(desired)
    );

    // An `Exact` route is keyed on the regex it compiles to.
    let mut desired = applied();
    desired.frontends.remove(2);
    let removal = one_request(&applied(), &desired);
    assert_eq!(
        tr::replay_if_exact(&applied(), &desired, &[removal]),
        Some(desired)
    );
}

/// A header-matched route goes out on the predicate verbs, under a key that
/// carries its predicates: its removal drops it alone, the plain route on the
/// same hostname, path and listener staying, its add sets it back under that
/// key alone, and an add on that key held already is settled as the agent
/// repairs it.
#[test]
fn a_header_matched_route_is_replayed_under_its_own_key() {
    let mut previous = applied();
    previous.frontends.push(header_matched(
        route("x.example.com", root(), Some("y"), false),
        "x-canary",
        ir::HeaderMatchKind::Exact("always".into()),
    ));
    let desired = applied();
    let removal = one_request(&previous, &desired);
    assert!(matches!(
        &removal.request_type,
        Some(RequestType::RemoveHttpFrontendWithHeaderPredicates(f))
            if f.cluster_id.as_deref() == Some("y")
    ));
    assert_eq!(
        tr::replay_if_exact(&previous, &desired, &[removal]),
        Some(desired.clone())
    );
    let add = one_request(&desired, &previous);
    assert!(matches!(
        &add.request_type,
        Some(RequestType::AddHttpFrontendWithHeaderPredicates(f))
            if f.cluster_id.as_deref() == Some("y")
    ));
    assert_eq!(
        tr::replay_if_exact(&desired, &previous, &[add]),
        Some(previous.clone())
    );

    // Every add again, the header-matched one on its key included: the agent
    // repairs each, so the main process holds what it held.
    let again = tr::ir_to_requests(&previous);
    assert!(again.iter().any(|r| matches!(
        r.request_type,
        Some(RequestType::AddHttpFrontendWithHeaderPredicates(_))
    )));
    assert_eq!(
        tr::replay_if_exact(&previous, &ir::Ir::default(), &again),
        Some(previous)
    );

    // The case `wherever_a_batch_stops_the_adopted_baseline_converges` runs
    // sends every predicate verb.
    let case = header_matches();
    let batch = tr::reconcile(&case.previous, &case.desired).expect("translate");
    let verbs: BTreeSet<&str> = batch
        .iter()
        .filter_map(|r| match r.request_type {
            Some(RequestType::AddHttpFrontendWithHeaderPredicates(_)) => Some("add http"),
            Some(RequestType::AddHttpsFrontendWithHeaderPredicates(_)) => Some("add https"),
            Some(RequestType::RemoveHttpFrontendWithHeaderPredicates(_)) => Some("remove http"),
            Some(RequestType::RemoveHttpsFrontendWithHeaderPredicates(_)) => Some("remove https"),
            _ => None,
        })
        .collect();
    assert_eq!(
        verbs,
        BTreeSet::from(["add http", "add https", "remove http", "remove https"]),
        "{batch:?}"
    );
}

#[test]
fn a_certificate_removal_drops_every_entry_of_that_identity() {
    let mut previous = applied();
    // The same certificate twice at one listener (another PEM spelling, more
    // names), once at another listener, and another certificate.
    previous.certificates.push(cert(
        &rewrap_pem(CERT_A),
        KEY_A,
        HTTPS,
        &["www.x.example.com"],
    ));
    previous
        .certificates
        .push(cert(CERT_A, KEY_A, OTHER_HTTPS, &["x.example.com"]));
    previous
        .certificates
        .push(cert(CERT_B, KEY_B, HTTPS, &["b.example.com"]));
    let mut desired = previous.clone();
    desired.certificates.drain(..2);
    let removal = one_request(&previous, &desired);
    assert!(matches!(
        removal.request_type,
        Some(RequestType::RemoveCertificate(_))
    ));
    assert_eq!(
        tr::replay_if_exact(&previous, &desired, &[removal]),
        Some(desired)
    );
}

/// A rotation swaps the old certificate for the desired new one; renaming a
/// certificate in place is a replace with its own fingerprint, which keeps
/// it, under the desired names.
#[test]
fn a_rotation_swaps_the_certificate_and_a_rename_renames_it() {
    let previous = applied();
    let mut rotated = previous.clone();
    rotated.certificates = vec![cert(CERT_B, KEY_B, HTTPS, &["x.example.com"])];
    let rotation = one_request(&previous, &rotated);
    assert!(matches!(
        rotation.request_type,
        Some(RequestType::ReplaceCertificate(_))
    ));
    assert_eq!(
        tr::replay_if_exact(&previous, &rotated, &[rotation]),
        Some(rotated)
    );

    let mut renamed = previous.clone();
    renamed.certificates[0]
        .names
        .push("www.x.example.com".to_string());
    let rename = one_request(&previous, &renamed);
    assert!(matches!(
        rename.request_type,
        Some(RequestType::ReplaceCertificate(_))
    ));
    assert_eq!(
        tr::replay_if_exact(&previous, &renamed, &[rename]),
        Some(renamed)
    );
}

/// A chain change on an unchanged leaf is a replace with its own
/// fingerprint as well: acknowledged, the replay records the desired chain,
/// so the next diff sends nothing for it; not acknowledged, the baseline
/// keeps the old chain and the next diff sends the same replace again.
#[test]
fn a_chain_change_is_recorded_once_acknowledged() {
    let previous = applied();
    let mut chained = previous.clone();
    chained.certificates[0].chain = vec![CERT_B.to_string()];
    let replace = one_request(&previous, &chained);
    assert!(matches!(
        replace.request_type,
        Some(RequestType::ReplaceCertificate(_))
    ));
    let adopted = tr::replay_if_exact(&previous, &chained, std::slice::from_ref(&replace))
        .expect("a certificate replace leaves the routing state as it was");
    assert_eq!(adopted, chained);
    assert!(tr::reconcile(&adopted, &chained)
        .expect("translate")
        .is_empty());
    let kept = tr::replay_if_exact(&previous, &chained, &[]).expect("nothing acknowledged");
    assert_eq!(one_request(&kept, &chained), replace);
}

/// A layer-4 frontend also stands for its listener. Dropped while Sōzu
/// still holds the listener, it would leave the listener out of the
/// baseline, and the retry would never take it down: it stays until the
/// listener's removal is acknowledged too. Until then the replayed IR does
/// not describe Sōzu, and the controller keeps the previous shadow instead.
#[test]
fn a_layer4_frontend_is_dropped_with_its_listener_only() {
    let mut previous = applied();
    previous.clusters.extend([cluster("z"), cluster("u")]);
    previous
        .backends
        .extend([backend("z", "10.0.2.1:5432"), backend("u", "10.0.3.1:53")]);
    previous.l4_frontends = vec![
        l4(ir::L4Protocol::Tcp, 5432, "z"),
        l4(ir::L4Protocol::Udp, 5353, "u"),
    ];
    let desired = without_cluster(&without_cluster(&previous, "z"), "u");
    let batch = tr::reconcile(&previous, &desired).expect("translate");
    let listener_teardown = batch
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::DeactivateListener(_))))
        .expect("the listeners are taken down");

    // Everything before the listener teardown acknowledged (the clusters go
    // after it), then a refusal.
    let mut failed = batch[..listener_teardown].to_vec();
    failed.push(refused());
    let mut sozu = state_of(Sozu::Linked, &previous);
    let acknowledged = feed(Sozu::Linked, &mut sozu, &failed);
    assert_eq!(acknowledged, listener_teardown);
    let replayed = tr::apply_acknowledged(&previous, &desired, &failed[..acknowledged]);
    assert_eq!(replayed.l4_frontends, previous.l4_frontends);
    assert_eq!(replayed.clusters, previous.clusters);
    assert!(replayed
        .backends
        .iter()
        .all(|b| b.cluster_id != "z" && b.cluster_id != "u"));

    // The retry takes the listeners down, from the replay as from the
    // previous shadow the controller keeps: the frontend removals are
    // already done, which the agent settles.
    assert_eq!(
        tr::replay_if_exact(&previous, &desired, &failed[..acknowledged]),
        None
    );
    for baseline in [&replayed, &previous] {
        let retry = tr::reconcile(baseline, &desired).expect("translate");
        let mut after = sozu.clone();
        assert_eq!(feed(Sozu::Linked, &mut after, &retry), retry.len());
        assert_eq!(holds(&after), holds(&state_of(Sozu::Linked, &desired)));
    }

    // Once a listener's removal is acknowledged, its frontend goes too.
    let (first_removed, removed) = batch
        .iter()
        .enumerate()
        .find_map(|(index, r)| match &r.request_type {
            Some(RequestType::RemoveListener(l)) => Some((index, SocketAddr::from(l.address))),
            _ => None,
        })
        .expect("the listeners are removed");
    let replayed = tr::apply_acknowledged(&previous, &desired, &batch[..=first_removed]);
    let still_held: Vec<ir::L4Frontend> = previous
        .l4_frontends
        .iter()
        .filter(|f| f.listener != removed)
        .cloned()
        .collect();
    assert_eq!(still_held.len(), 1);
    assert_eq!(
        replayed.l4_frontends, still_held,
        "only one listener's removal was acknowledged"
    );
    assert_eq!(
        tr::replay_if_exact(&previous, &desired, &batch[..=first_removed]),
        None,
        "the other listener is still held without its frontend"
    );
    assert_eq!(
        tr::replay_if_exact(&previous, &desired, &batch),
        Some(desired)
    );
}

/// The two layer-4 listener states the IR cannot spell: a listener whose
/// add was acknowledged without its frontend's, and one still held after its
/// frontend's removal. The replay is not adopted; the controller keeps the
/// previous shadow, which converges towards the desired state and towards
/// any other that keeps the layer-4 route as the batch left it. A target
/// that wants back what the head undid gets nothing for it, as before the
/// replay existed: the listener stays bound, or the frontend stays missing.
#[test]
fn a_layer4_listener_the_ir_cannot_spell_keeps_the_previous_shadow() {
    let previous = applied();
    let mut with_z = previous.clone();
    with_z.clusters.push(cluster("z"));
    with_z.backends.push(backend("z", "10.0.2.1:5432"));
    with_z.l4_frontends = vec![l4(ir::L4Protocol::Tcp, 5432, "z")];
    let listener = |state: &ConfigState| {
        holds(state)
            .iter()
            .any(|line| line.starts_with("tcp listener 0.0.0.0:5432 "))
    };
    let frontend = |state: &ConfigState| holds(state).contains("tcp 0.0.0.0:5432 -> z");

    for sozu in [Sozu::Linked, Sozu::V23] {
        // The listener added (and activated), its frontend not yet.
        let batch = tr::reconcile(&previous, &with_z).expect("translate");
        let stop = batch
            .iter()
            .position(|r| matches!(r.request_type, Some(RequestType::AddCluster(_))))
            .expect("the cluster is added after the listener");
        assert!(matches!(
            batch[0].request_type,
            Some(RequestType::AddTcpListener(_))
        ));
        let mut held = state_of(sozu, &previous);
        let mut failed = batch[..stop].to_vec();
        failed.push(refused());
        assert_eq!(feed(sozu, &mut held, &failed), stop);
        assert!(listener(&held) && !frontend(&held));
        assert_eq!(
            tr::replay_if_exact(&previous, &with_z, &batch[..stop]),
            None
        );
        let (acknowledged, misses) = retry(sozu, &held, &previous, &with_z);
        assert!(acknowledged && misses.is_empty(), "{sozu:?}: {misses:#?}");
        let (acknowledged, misses) = retry(sozu, &held, &previous, &previous);
        assert!(acknowledged);
        assert!(
            misses.iter().all(|line| line.contains(" 0.0.0.0:5432 ")) && !misses.is_empty(),
            "{sozu:?}: the listener stays bound: {misses:#?}"
        );

        // The frontend removed, its listener not yet.
        let batch = tr::reconcile(&with_z, &previous).expect("translate");
        let stop = batch
            .iter()
            .position(|r| !matches!(r.request_type, Some(RequestType::RemoveTcpFrontend(_))))
            .expect("more than the frontend's removal");
        assert_eq!(stop, 1, "{batch:?}");
        let mut held = state_of(sozu, &with_z);
        let mut failed = batch[..stop].to_vec();
        failed.push(refused());
        assert_eq!(feed(sozu, &mut held, &failed), stop);
        assert!(listener(&held) && !frontend(&held));
        assert_eq!(
            tr::replay_if_exact(&with_z, &previous, &batch[..stop]),
            None
        );
        let (acknowledged, misses) = retry(sozu, &held, &with_z, &previous);
        assert!(acknowledged && misses.is_empty(), "{sozu:?}: {misses:#?}");
        let (acknowledged, misses) = retry(sozu, &held, &with_z, &with_z);
        assert!(acknowledged);
        assert_eq!(
            misses,
            BTreeSet::from(["tcp 0.0.0.0:5432 -> z".to_string()]),
            "{sozu:?}: the frontend stays missing"
        );
    }
}

// ----------------------------------------------------------------------------
// Whole sequences
// ----------------------------------------------------------------------------

/// The sequence that wedges Sōzu 2.3: a batch removes cluster `x`, Sōzu
/// acknowledges that, a later request is refused, and the desired state then
/// wants `x` back with a second backend. The replay, adopted, sends the
/// cluster again before its backends.
#[test]
fn a_cluster_a_failed_batch_removed_is_added_back_before_its_backends() {
    let case = cluster_removal();
    let batch = tr::reconcile(&case.previous, &case.desired).expect("translate");
    let mut failed = batch.clone();
    failed.push(refused());
    for sozu in [Sozu::Linked, Sozu::V23] {
        let mut held = state_of(sozu, &case.previous);
        assert_eq!(feed(sozu, &mut held, &failed), batch.len(), "{sozu:?}");

        // From the unchanged baseline `x` itself did not change, so the new
        // backend goes out alone. 2.3 refuses it on every reconcile (2.2.1
        // took it and kept serving no route for `x`).
        let stale = tr::reconcile(&case.previous, &case.third).expect("translate");
        assert!(
            matches!(stale.as_slice(), [only] if matches!(only.request_type, Some(RequestType::AddBackend(_)))),
            "{stale:?}"
        );
        let mut stale_held = held.clone();
        let acknowledged = feed(sozu, &mut stale_held, &stale);
        assert_eq!(acknowledged, 0, "{sozu:?}");
        assert_ne!(holds(&stale_held), holds(&state_of(sozu, &case.third)));

        // The replay describes what Sōzu holds, so the controller adopts
        // it. From it the cluster goes out first, then both backends and
        // both routes, and Sōzu ends up holding the third state.
        let replayed = tr::apply_acknowledged(&case.previous, &case.desired, &batch);
        assert_eq!(sorted(&replayed), sorted(&case.desired));
        assert_eq!(
            tr::replay_if_exact(&case.previous, &case.desired, &batch).as_ref(),
            Some(&replayed),
            "{sozu:?}"
        );
        let requests = tr::reconcile(&replayed, &case.third).expect("translate");
        let position = |wanted: fn(&RequestType) -> bool| {
            requests
                .iter()
                .position(|r| r.request_type.as_ref().is_some_and(wanted))
        };
        let add_cluster =
            position(|r| matches!(r, RequestType::AddCluster(c) if c.cluster_id == "x"))
                .expect("the cluster is added again");
        let first_backend =
            position(|r| matches!(r, RequestType::AddBackend(b) if b.cluster_id == "x"))
                .expect("its backends are added");
        assert!(add_cluster < first_backend, "{requests:?}");
        let mut converged = held.clone();
        assert_eq!(feed(sozu, &mut converged, &requests), requests.len());
        assert_eq!(holds(&converged), holds(&state_of(sozu, &case.third)));
    }
}

/// A route re-pointed from one live cluster to another, its removal and its
/// add both acknowledged before a later request is refused, then deleted.
/// The replayed baseline, adopted, routes it to the new cluster, so the
/// deletion removes it. Replaying the removals alone, as a baseline that ignores
/// acknowledged adds would, holds no route under that key: the deletion
/// diffs to nothing and Sōzu keeps routing it to the new cluster. The
/// unchanged baseline still listed the old route, whose removal the agent
/// turns into evicting the new one.
#[test]
fn a_route_a_failed_batch_re_pointed_is_removed_when_deleted() {
    let case = repoint();
    let batch = tr::reconcile(&case.previous, &case.desired).expect("translate");
    assert_eq!(batch.len(), 4, "{batch:?}");
    let mut failed = batch.clone();
    failed.push(refused());
    let routed_to_b = |state: &ConfigState| {
        holds(state)
            .iter()
            .filter(|line| line.contains(";k.example.com;") && line.contains(r#""cluster_id":"b""#))
            .count()
    };
    for sozu in [Sozu::Linked, Sozu::V23] {
        let mut held = state_of(sozu, &case.previous);
        assert_eq!(feed(sozu, &mut held, &failed), batch.len(), "{sozu:?}");
        assert_eq!(
            routed_to_b(&held),
            2,
            "Sōzu routes K to b on both listeners"
        );

        let replayed = tr::apply_acknowledged(&case.previous, &case.desired, &batch);
        assert_eq!(
            tr::replay_if_exact(&case.previous, &case.desired, &batch).as_ref(),
            Some(&replayed),
            "{sozu:?}: the replay describes what Sōzu holds, and is adopted"
        );
        let deletion = tr::reconcile(&replayed, &case.third).expect("translate");
        assert_eq!(deletion.len(), 2, "{deletion:?}");
        let mut after = held.clone();
        assert_eq!(feed(sozu, &mut after, &deletion), deletion.len());
        assert_eq!(holds(&after), holds(&state_of(sozu, &case.third)));

        let removals: Vec<Request> = batch.iter().filter(|r| is_teardown(r)).cloned().collect();
        let removals_only = tr::apply_acknowledged(&case.previous, &case.desired, &removals);
        assert!(
            tr::reconcile(&removals_only, &case.third)
                .expect("translate")
                .is_empty(),
            "a baseline without the acknowledged adds sends nothing for K, \
             and Sōzu keeps routing it to b"
        );

        let unchanged = tr::reconcile(&case.previous, &case.third).expect("translate");
        let mut evicted = held.clone();
        assert_eq!(feed(sozu, &mut evicted, &unchanged), unchanged.len());
        assert_eq!(holds(&evicted), holds(&state_of(sozu, &case.third)));
    }
}

/// A cluster removed with its TCP route by a batch that fails before the
/// cluster's own removal lands, then wanted back with another backend.
/// Cluster removals come last in a batch, after the listener teardown, so
/// wherever the batch stopped Sōzu still holds the cluster, and so does the
/// baseline the controller adopts: the replay once the listener is gone,
/// the previous shadow while the listener is held without its frontend. The
/// new backend goes to a cluster Sōzu holds, which 2.2.1 did not require
/// either. (With the
/// cluster removed before the listener teardown, the kept shadow sent the
/// backend alone, and 2.3 refused it on every reconcile.) What the retry
/// leaves out of place is the documented listener state alone.
#[test]
fn a_cluster_whose_removal_did_not_land_takes_its_new_backend() {
    let mut previous = applied();
    previous.clusters.push(cluster("z"));
    previous.backends.push(backend("z", "10.0.2.1:5432"));
    previous.l4_frontends = vec![l4(ir::L4Protocol::Tcp, 5432, "z")];
    let desired = without_cluster(&previous, "z");
    let mut back = previous.clone();
    back.backends.retain(|b| b.cluster_id != "z");
    back.backends.push(backend("z", "10.0.2.2:5432"));

    let batch = tr::reconcile(&previous, &desired).expect("translate");
    assert!(
        matches!(batch.last().and_then(|r| r.request_type.as_ref()), Some(RequestType::RemoveCluster(id)) if id == "z"),
        "{batch:?}"
    );
    let frontend_removal = batch
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::RemoveTcpFrontend(_))))
        .expect("the route is removed");
    for sozu in [Sozu::Linked, Sozu::V23] {
        for stop in frontend_removal + 1..batch.len() {
            let context = format!("{sozu:?}, stopped at {stop}");
            let mut held = state_of(sozu, &previous);
            let mut failed = batch[..stop].to_vec();
            failed.push(refused());
            assert_eq!(feed(sozu, &mut held, &failed), stop, "{context}");
            assert!(
                holds(&held)
                    .iter()
                    .any(|line| line.starts_with("cluster z ")),
                "{context}"
            );
            let adopted = tr::replay_if_exact(&previous, &desired, &batch[..stop]);
            let baseline = adopted.clone().unwrap_or_else(|| previous.clone());
            assert!(baseline.clusters.iter().any(|c| c.id == "z"), "{context}");

            let (acknowledged, misses) = retry(sozu, &held, &baseline, &back);
            assert!(acknowledged, "{context}: the retry is refused");
            assert!(
                misses.iter().all(|line| line.contains(" 0.0.0.0:5432 ")),
                "{context}: {misses:#?}"
            );
            if adopted.is_some() {
                assert!(misses.is_empty(), "{context}: {misses:#?}");
            }
        }
    }
}

/// Certificates are outside the guard, and an add of a fingerprint Sōzu
/// already holds is a no-op there, names included (2.3.2, read from its
/// source; 2.2.1 behaved the same).
/// Over a drift the shadow never knew of, the certificate installed with
/// other names, the replay records the desired names: after two failed
/// batches the shadow lists them while Sōzu keeps the old ones, and nothing
/// sends them again. Keeping the shadow does no better: each retry sends
/// the same add, which changes nothing either. Pinned as it stands.
#[test]
fn a_certificate_add_over_a_drift_records_names_sozu_does_not_hold() {
    let mut previous = applied();
    previous.certificates.clear();
    let drift = cert(CERT_A, KEY_A, HTTPS, &["a.example.com"]);
    let mut first = previous.clone();
    first.certificates = vec![cert(CERT_A, KEY_A, HTTPS, &["x.example.com"])];
    let mut second = first.clone();
    second.backends.push(backend("x", "10.0.0.2:80"));
    let names = |state: &ConfigState| -> Vec<String> {
        holds(state)
            .into_iter()
            .filter(|line| line.starts_with("certificate "))
            .collect()
    };

    for sozu in [Sozu::Linked, Sozu::V23] {
        let mut held = state_of(sozu, &previous);
        let install = tr::ir_to_requests(&ir::Ir {
            certificates: vec![drift.clone()],
            ..ir::Ir::default()
        });
        assert_eq!(feed(sozu, &mut held, &install), install.len());
        let drifted = names(&held);
        assert!(drifted[0].contains("a.example.com"), "{drifted:?}");

        let batch = tr::reconcile(&previous, &first).expect("translate");
        assert!(
            matches!(batch.as_slice(), [only] if matches!(only.request_type, Some(RequestType::AddCertificate(_)))),
            "{batch:?}"
        );
        assert_eq!(feed(sozu, &mut held, &[batch[0].clone(), refused()]), 1);
        let shadow = tr::replay_if_exact(&previous, &first, &batch).expect("adopted");
        assert_eq!(shadow, first);

        let batch = tr::reconcile(&shadow, &second).expect("translate");
        assert!(
            matches!(batch.as_slice(), [only] if matches!(only.request_type, Some(RequestType::AddBackend(_)))),
            "{batch:?}"
        );
        assert_eq!(feed(sozu, &mut held, &[batch[0].clone(), refused()]), 1);
        let shadow = tr::replay_if_exact(&shadow, &second, &batch).expect("adopted");
        assert_eq!(shadow, second);

        assert_eq!(names(&held), drifted, "{sozu:?}: Sōzu keeps the old names");
        assert!(tr::reconcile(&shadow, &second)
            .expect("translate")
            .is_empty());
        let (acknowledged, misses) = retry(sozu, &held, &previous, &second);
        assert!(acknowledged);
        assert!(
            !misses.is_empty() && misses.iter().all(|line| line.starts_with("certificate ")),
            "{sozu:?}: the kept shadow's add changes nothing either: {misses:#?}"
        );
    }
}

/// The sequence a replay misdescribes: cluster `u` is upserted with another
/// algorithm and, since the batch also drops its UDP route, without UDP
/// affinity, and the batch stops before that route's removal. Sōzu holds the
/// new `u` without affinity next to the route; the replayed IR, holding both,
/// spells affinity back in. Adopted, it would make `third` (the new
/// algorithm, the route back) diff to nothing and leave the affinity lost.
/// The guard keeps the previous shadow, from which `third` re-sends the
/// upsert with affinity, and the desired state converges as before.
///
/// Going back to the previous state, no baseline the IR can spell does it
/// all: the previous shadow equals it and sends nothing, so Sōzu keeps the
/// cluster as the head upserted it, exactly as before the replay existed.
/// The replay would re-send it, and lose the affinity towards `third`.
#[test]
fn a_cluster_upsert_ahead_of_its_udp_route_removal_keeps_the_previous_shadow() {
    let case = udp_affinity();
    let batch = tr::reconcile(&case.previous, &case.desired).expect("translate");
    let upserts_u = |request: &Request, affinity: Option<UdpAffinityKey>| {
        matches!(&request.request_type, Some(RequestType::AddCluster(c))
            if c.cluster_id == "u"
                && c.udp.as_ref().and_then(|udp| udp.affinity_key) == affinity.map(|k| k as i32))
    };
    assert!(upserts_u(&batch[0], None), "{batch:?}");
    assert!(
        matches!(
            batch[1].request_type,
            Some(RequestType::RemoveUdpFrontend(_))
        ),
        "{batch:?}"
    );
    let head = &batch[..1];
    for sozu in [Sozu::Linked, Sozu::V23] {
        let mut held = state_of(sozu, &case.previous);
        assert_eq!(feed(sozu, &mut held, &[batch[0].clone(), refused()]), 1);

        // The baseline the controller adopts: the replay if exact, else the
        // previous shadow. From it, `third` re-sends `u` with affinity, and
        // the desired state converges.
        let baseline = tr::replay_if_exact(&case.previous, &case.desired, head)
            .unwrap_or_else(|| case.previous.clone());
        let requests = tr::reconcile(&baseline, &case.third).expect("translate");
        assert!(
            requests
                .iter()
                .any(|r| upserts_u(r, Some(UdpAffinityKey::SourceIpPort))),
            "{sozu:?}: {requests:?}"
        );
        for (name, target) in [("third", &case.third), ("desired", &case.desired)] {
            let (acknowledged, misses) = retry(sozu, &held, &baseline, target);
            assert!(
                acknowledged && misses.is_empty(),
                "{sozu:?}, to {name}: {misses:#?}"
            );
        }
        assert_eq!(baseline, case.previous, "{sozu:?}");

        // The replay itself equals `third`, so nothing would re-send the
        // affinity.
        let replayed = tr::apply_acknowledged(&case.previous, &case.desired, head);
        assert!(tr::reconcile(&replayed, &case.third)
            .expect("translate")
            .is_empty());
        let (_, lost) = retry(sozu, &held, &replayed, &case.third);
        assert!(
            !lost.is_empty() && lost.iter().all(|line| line.starts_with("cluster u ")),
            "{sozu:?}: {lost:#?}"
        );

        // Back to the previous state: the previous shadow sends nothing and
        // `u` stays as the head upserted it, while the replay would converge.
        let (acknowledged, misses) = retry(sozu, &held, &baseline, &case.previous);
        assert!(acknowledged);
        assert!(
            !misses.is_empty() && misses.iter().all(|line| line.starts_with("cluster u ")),
            "{sozu:?}: {misses:#?}"
        );
        let (_, from_replay) = retry(sozu, &held, &replayed, &case.previous);
        assert!(from_replay.is_empty(), "{sozu:?}: {from_replay:#?}");
    }
}

/// Retry from `baseline` towards `target` on a copy of `held`: whether the
/// retry was acknowledged in full, and what the stand-in then holds that a
/// fresh apply of `target` does not, or the other way round.
fn retry(
    sozu: Sozu,
    held: &ConfigState,
    baseline: &ir::Ir,
    target: &ir::Ir,
) -> (bool, BTreeSet<String>) {
    let requests = tr::reconcile(baseline, target).expect("translate");
    let mut after = held.clone();
    let acknowledged = feed(sozu, &mut after, &requests);
    let misses = holds(&after)
        .symmetric_difference(&holds(&state_of(sozu, target)))
        .cloned()
        .collect();
    (acknowledged == requests.len(), misses)
}

/// What the stand-in holds once `ir` is applied from scratch, or `None` when
/// it refuses part of it.
fn try_state_of(sozu: Sozu, ir: &ir::Ir) -> Option<ConfigState> {
    let mut state = ConfigState::new();
    let requests = tr::ir_to_requests(ir);
    (feed(sozu, &mut state, &requests) == requests.len()).then_some(state)
}

/// Stop each batch after each of its requests in turn with a refusal, and
/// take the baseline the controller adopts: the replay when
/// `replay_if_exact` returns it, the previous shadow otherwise. The replay
/// is adopted exactly when it describes what the stand-in holds, which every
/// head of a batch without layer-4 routes does. From the adopted replay,
/// every retry (towards the desired state, back towards the previous one,
/// towards the third) is acknowledged in full and leaves the stand-in
/// holding exactly its target, in both stand-in modes.
///
/// When the replay is not adopted, the baseline is the previous shadow, the
/// one the controller diffed from before it replayed anything. The retry
/// towards the desired state still converges; another target may not, and
/// what it leaves out of place is at most what the acknowledged head
/// changed (the layer-4 listener states and the UDP affinity the IR cannot
/// spell, see `a_layer4_listener_the_ir_cannot_spell_keeps_the_previous_shadow`
/// and `a_cluster_upsert_ahead_of_its_udp_route_removal_keeps_the_previous_shadow`).
#[test]
fn wherever_a_batch_stops_the_adopted_baseline_converges() {
    for case in [
        repoint(),
        cluster_removal(),
        certificate_rotation(),
        layer4_move(),
        mixed(),
        udp_affinity(),
        header_matches(),
    ] {
        let batch = tr::reconcile(&case.previous, &case.desired).expect("translate");
        assert!(batch.iter().any(is_teardown), "{}: {batch:?}", case.name);
        let layer4 =
            !case.previous.l4_frontends.is_empty() || !case.desired.l4_frontends.is_empty();
        let from_either = |ir: &ir::Ir| {
            let (p, d) = (&case.previous, &case.desired);
            ir.clusters
                .iter()
                .all(|c| p.clusters.contains(c) || d.clusters.contains(c))
                && ir
                    .backends
                    .iter()
                    .all(|b| p.backends.contains(b) || d.backends.contains(b))
                && ir
                    .frontends
                    .iter()
                    .all(|f| p.frontends.contains(f) || d.frontends.contains(f))
                && ir
                    .certificates
                    .iter()
                    .all(|c| p.certificates.contains(c) || d.certificates.contains(c))
                && ir
                    .l4_frontends
                    .iter()
                    .all(|f| p.l4_frontends.contains(f) || d.l4_frontends.contains(f))
        };
        let mut rejected = 0;
        for sozu in [Sozu::Linked, Sozu::V23] {
            let before = holds(&state_of(sozu, &case.previous));
            for stop in 0..=batch.len() {
                let context = format!("{} on {sozu:?}, stopped at {stop}", case.name);
                let mut held = state_of(sozu, &case.previous);
                let mut failed = batch[..stop].to_vec();
                failed.push(refused());
                assert_eq!(feed(sozu, &mut held, &failed), stop, "{context}");
                let head = &batch[..stop];
                let replayed = tr::apply_acknowledged(&case.previous, &case.desired, head);
                assert!(from_either(&replayed), "{context}: {replayed:?}");

                let exact = try_state_of(sozu, &replayed)
                    .is_some_and(|described| holds(&described) == holds(&held));
                let adopted = tr::replay_if_exact(&case.previous, &case.desired, head);
                assert_eq!(adopted.is_some(), exact, "{context}: {replayed:?}");
                if let Some(adopted) = &adopted {
                    assert_eq!(adopted, &replayed, "{context}");
                }
                assert!(exact || layer4, "{context}");
                if stop == 0 || stop == batch.len() {
                    assert!(exact, "{context}");
                }
                if stop == batch.len() {
                    assert_eq!(sorted(&replayed), sorted(&case.desired), "{context}");
                }
                rejected += usize::from(!exact);

                let baseline = adopted.as_ref().unwrap_or(&case.previous);
                let changed: BTreeSet<String> = holds(&held)
                    .symmetric_difference(&before)
                    .cloned()
                    .collect();
                for (name, target) in [
                    ("desired", &case.desired),
                    ("previous", &case.previous),
                    ("third", &case.third),
                ] {
                    let (acknowledged, misses) = retry(sozu, &held, baseline, target);
                    assert!(acknowledged, "{context}: the retry to {name} is refused");
                    if adopted.is_some() || name == "desired" {
                        assert!(misses.is_empty(), "{context}, to {name}: {misses:#?}");
                    } else {
                        assert!(
                            misses.is_subset(&changed),
                            "{context}, to {name}: {misses:#?} against {changed:#?}"
                        );
                    }
                }
            }
        }
        assert_eq!(rejected > 0, layer4, "{}", case.name);
    }
}

/// A UDP address another cluster holds, a drift the shadow missed. The
/// linked 2.3.2 state refuses the add with an error of its own
/// (`UdpFrontendAddressTaken`), not `Exists`; the agent repairs both alike
/// (evict the holder, add again), and so does each stand-in mode, leaving
/// the address to the desired cluster alone.
#[test]
fn a_udp_address_another_cluster_holds_is_repaired_in_both_modes() {
    let mut drifted = applied();
    drifted.clusters.push(cluster("u"));
    drifted.clusters.push(cluster("v"));
    drifted.backends.push(backend("u", "10.0.3.1:53"));
    drifted.backends.push(backend("v", "10.0.3.2:53"));
    drifted.l4_frontends = vec![l4(ir::L4Protocol::Udp, 5353, "u")];
    let mut desired = drifted.clone();
    desired.l4_frontends = vec![l4(ir::L4Protocol::Udp, 5353, "v")];
    let add = tr::ir_to_requests(&desired)
        .into_iter()
        .find(|r| matches!(r.request_type, Some(RequestType::AddUdpFrontend(_))))
        .expect("the desired UDP route");
    let udp_routes = |state: &ConfigState| -> BTreeSet<String> {
        holds(state)
            .into_iter()
            .filter(|line| line.starts_with("udp ") && !line.starts_with("udp listener"))
            .collect()
    };
    assert!(matches!(
        dispatch(Sozu::Linked, &mut state_of(Sozu::Linked, &drifted), &add),
        Err(StateError::UdpFrontendAddressTaken { .. })
    ));
    for sozu in [Sozu::Linked, Sozu::V23] {
        let mut held = state_of(sozu, &drifted);
        assert_eq!(
            feed(sozu, &mut held, std::slice::from_ref(&add)),
            1,
            "{sozu:?}"
        );
        assert_eq!(
            udp_routes(&held),
            udp_routes(&state_of(sozu, &desired)),
            "{sozu:?}"
        );
    }
}
