//! Does the controller's baseline converge after a batch Sōzu applied only in
//! part?
//!
//! The probe runs the controller's own loop — translate the shadow against the
//! desired IR, apply through the agent, advance the shadow on success — and
//! varies one step only: what the shadow becomes when a batch fails.
//! (a) keeps it unchanged, as the controller did before replaying;
//! (b) replays onto it only the removals among the requests Sōzu acknowledged;
//! (c) replays every request Sōzu acknowledged, adds included
//! (`apply_acknowledged`), whatever it leaves;
//! (d) adopts that replay only when it describes exactly the routing state
//! the acknowledged requests left (`replay_if_exact`), and otherwise keeps
//! the shadow unchanged (this change).
//!
//! Four sequences, each run once per mode on objects of its own:
//! - `back`: apply S (cluster x with a backend and an HTTP and an HTTPS
//!   route, cluster y with a route, a certificate), then the batch that
//!   removes x, failed by one more request Sōzu refuses at its end (a
//!   `RemoveCertificate` whose fingerprint is not hex), then x wanted back
//!   with a second backend, then an unrelated route added on y.
//! - `repoint`: apply S (clusters a and b, each with a route and a backend,
//!   and route k to a on both listeners), then the batch that re-points k to
//!   b, failed the same way, then k deleted.
//! - `udp`: apply S (cluster u, round robin, with a backend and a UDP route,
//!   which gives it UDP affinity), then the batch that switches u to random
//!   and drops the route, failed by the same refusal placed right after u's
//!   upsert, then the route wanted back.
//! - `udp-revert`: the same, then S wanted back.
//!
//! The main process's frontend list (and, for the UDP sequences, cluster u
//! as the main process and each worker hold it) is printed after each step,
//! so the outcome is read from Sōzu rather than from the agent's verdict.
//!
//! Env: SOZU_SOCK (command socket), PROBE_HTTP and PROBE_HTTPS (active
//! listeners), PROBE_UDP_PORT (the first of the eight UDP ports the
//! sequences bind, default 15301).

use std::net::SocketAddr;

use anyhow::{Context, Result};
use sozu_command_lib::channel::Channel;
use sozu_command_lib::proto::command::{
    request::RequestType, response_content::ContentType, Cluster, FrontendFilters,
    LoadBalancingAlgorithms, RemoveCertificate, Request, RequestHttpFrontend, Response,
    ResponseContent, ResponseStatus, UdpAffinityKey,
};
use sozu_gw_agent::SozuAgent;
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

/// A test certificate and its key (the translator's fixtures).
const CERT: &str = include_str!("../../translator/tests/fixtures/cert_a.pem");
const KEY: &str = include_str!("../../translator/tests/fixtures/key_a.pem");

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Send `request` on a fresh connection and return its terminal response.
fn query(sock: &str, request: RequestType) -> Result<Response> {
    let mut channel: Channel<Request, Response> =
        Channel::from_path(sock, 1024 * 1024, 16 * 1024 * 1024)
            .with_context(|| format!("connect {sock}"))?;
    channel.blocking().context("blocking channel")?;
    channel.write_message(&request.into()).context("write")?;
    loop {
        let response = channel.read_message().context("read")?;
        if response.status != ResponseStatus::Processing as i32 {
            return Ok(response);
        }
    }
}

/// The HTTP and HTTPS frontends the main process lists whose hostname ends
/// with `suffix`, and the UDP frontends of a cluster whose id ends with it.
fn listing(sock: &str, suffix: &str) -> Result<String> {
    let response = query(
        sock,
        RequestType::ListFrontends(FrontendFilters {
            http: false,
            https: false,
            tcp: false,
            domain: None,
            cluster_id: None,
        }),
    )?;
    let Some(ResponseContent {
        content_type: Some(ContentType::FrontendList(listed)),
    }) = response.content
    else {
        return Ok(format!("no listing: {}", response.message));
    };
    let mut out = Vec::new();
    for (scheme, fronts) in [
        ("http", &listed.http_frontends),
        ("https", &listed.https_frontends),
    ] {
        for front in fronts.iter().filter(|f| f.hostname.ends_with(suffix)) {
            out.push(format!(
                "{scheme} {}/ -> {}",
                front.hostname,
                front.cluster_id.as_deref().unwrap_or("-")
            ));
        }
    }
    for front in listed
        .udp_frontends
        .iter()
        .filter(|f| f.cluster_id.ends_with(suffix))
    {
        out.push(format!(
            "udp {} -> {}",
            SocketAddr::from(front.address),
            front.cluster_id
        ));
    }
    out.sort();
    Ok(format!("[{}]", out.join(", ")))
}

/// A cluster's load balancing and UDP affinity, as the request or state
/// carrying it spells them.
fn cluster_config(c: &Cluster) -> String {
    let algorithm = LoadBalancingAlgorithms::try_from(c.load_balancing)
        .map(|a| a.as_str_name().to_string())
        .unwrap_or_else(|_| c.load_balancing.to_string());
    let affinity = match c.udp.as_ref().map(|udp| udp.affinity_key) {
        None => "no udp config".to_string(),
        Some(None) => "udp affinity unset".to_string(),
        Some(Some(key)) => format!(
            "udp affinity {}",
            UdpAffinityKey::try_from(key)
                .map(|k| k.as_str_name().to_string())
                .unwrap_or_else(|_| key.to_string())
        ),
    };
    format!("{algorithm}, {affinity}")
}

/// Cluster `id` as the main process and each worker hold it.
fn cluster_view(sock: &str, id: &str) -> Result<String> {
    let response = query(sock, RequestType::QueryClusterById(id.to_string()))?;
    let Some(ResponseContent {
        content_type: Some(ContentType::WorkerResponses(responses)),
    }) = response.content
    else {
        return Ok(format!("no answer: {}", response.message));
    };
    let mut out = Vec::new();
    for (who, content) in responses.map {
        let held = match content.content_type {
            Some(ContentType::Clusters(clusters)) => clusters
                .vec
                .iter()
                .find_map(|c| c.configuration.as_ref())
                .map(cluster_config)
                .unwrap_or_else(|| "absent".to_string()),
            _ => "no cluster list".to_string(),
        };
        out.push(format!("{who}: {held}"));
    }
    Ok(format!("[{}]", out.join("; ")))
}

/// A request named by its verb and the object it names.
fn label(request: &Request) -> String {
    let route = |verb: &str, f: &RequestHttpFrontend| {
        format!(
            "{verb}({}->{})",
            f.hostname,
            f.cluster_id.as_deref().unwrap_or("-")
        )
    };
    let address = |a: &sozu_command_lib::proto::command::SocketAddress| SocketAddr::from(*a);
    match &request.request_type {
        Some(RequestType::AddCluster(c))
            if c.udp.is_some()
                || c.load_balancing != LoadBalancingAlgorithms::RoundRobin as i32 =>
        {
            format!("AddCluster({}, {})", c.cluster_id, cluster_config(c))
        }
        Some(RequestType::AddCluster(c)) => format!("AddCluster({})", c.cluster_id),
        Some(RequestType::RemoveCluster(id)) => format!("RemoveCluster({id})"),
        Some(RequestType::AddBackend(b)) => format!("AddBackend({})", b.backend_id),
        Some(RequestType::RemoveBackend(b)) => format!("RemoveBackend({})", b.backend_id),
        Some(RequestType::AddHttpFrontend(f)) => route("AddHttpFrontend", f),
        Some(RequestType::RemoveHttpFrontend(f)) => route("RemoveHttpFrontend", f),
        Some(RequestType::AddHttpsFrontend(f)) => route("AddHttpsFrontend", f),
        Some(RequestType::RemoveHttpsFrontend(f)) => route("RemoveHttpsFrontend", f),
        Some(RequestType::AddCertificate(_)) => "AddCertificate".to_string(),
        Some(RequestType::RemoveCertificate(c)) => format!("RemoveCertificate({})", c.fingerprint),
        Some(RequestType::AddUdpListener(l)) => format!("AddUdpListener({})", address(&l.address)),
        Some(RequestType::ActivateListener(l)) => {
            format!("ActivateListener({})", address(&l.address))
        }
        Some(RequestType::DeactivateListener(l)) => {
            format!("DeactivateListener({})", address(&l.address))
        }
        Some(RequestType::RemoveListener(l)) => format!("RemoveListener({})", address(&l.address)),
        Some(RequestType::AddUdpFrontend(f)) => {
            format!("AddUdpFrontend({}->{})", address(&f.address), f.cluster_id)
        }
        Some(RequestType::RemoveUdpFrontend(f)) => {
            format!(
                "RemoveUdpFrontend({}->{})",
                address(&f.address),
                f.cluster_id
            )
        }
        _ => "other".to_string(),
    }
}

/// What the shadow becomes when a batch fails.
#[derive(Clone, Copy)]
enum Mode {
    /// (a) Unchanged.
    Unchanged,
    /// (b) The acknowledged removals replayed, the adds left out.
    Removals,
    /// (c) Every acknowledged request replayed, whatever it leaves.
    Replay,
    /// (d) The replay adopted only when exact, the shadow kept otherwise.
    Exact,
}

impl Mode {
    fn tag(self) -> &'static str {
        match self {
            Mode::Unchanged => "a",
            Mode::Removals => "b",
            Mode::Replay => "c",
            Mode::Exact => "d",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Mode::Unchanged => "unchanged (the controller before replaying)",
            Mode::Removals => "the acknowledged removals replayed, the adds left out",
            Mode::Replay => "every acknowledged request replayed, whatever it leaves",
            Mode::Exact => {
                "every acknowledged request replayed when that is exact, else unchanged \
                 (this change)"
            }
        }
    }
}

fn is_removal(request: &Request) -> bool {
    matches!(
        request.request_type,
        Some(
            RequestType::RemoveCluster(_)
                | RequestType::RemoveBackend(_)
                | RequestType::RemoveHttpFrontend(_)
                | RequestType::RemoveHttpsFrontend(_)
                | RequestType::RemoveTcpFrontend(_)
                | RequestType::RemoveUdpFrontend(_)
                | RequestType::RemoveListener(_)
                | RequestType::RemoveCertificate(_)
                | RequestType::DeactivateListener(_)
        )
    )
}

/// The controller's loop, with one choice: what the shadow becomes when a
/// batch fails.
struct Controller {
    agent: SozuAgent,
    shadow: ir::Ir,
    mode: Mode,
}

/// Where a batch is made to fail: the request Sōzu refuses, inserted after
/// the first `after` requests (at the end when the batch is shorter).
struct Fail {
    refused: Request,
    after: usize,
}

impl Controller {
    /// One reconcile towards `desired`, failed part-way by `fail` when given.
    fn reconcile(&mut self, desired: &ir::Ir, fail: Option<Fail>) -> Result<String> {
        let mut requests = tr::reconcile(&self.shadow, desired)?;
        if let Some(Fail { refused, after }) = fail {
            requests.insert(after.min(requests.len()), refused);
        }
        if requests.is_empty() {
            return Ok("no requests".to_string());
        }
        let labels: Vec<String> = requests.iter().map(label).collect();
        let outcome = match self.agent.apply_counted(&requests) {
            Ok(()) => {
                self.shadow = desired.clone();
                "applied".to_string()
            }
            Err(failure) => {
                let acknowledged = &requests[..failure.applied];
                let mut verdict = String::new();
                match self.mode {
                    Mode::Unchanged => {}
                    Mode::Removals => {
                        let removals: Vec<Request> = acknowledged
                            .iter()
                            .filter(|r| is_removal(r))
                            .cloned()
                            .collect();
                        self.shadow = tr::apply_acknowledged(&self.shadow, desired, &removals);
                    }
                    Mode::Replay => {
                        self.shadow = tr::apply_acknowledged(&self.shadow, desired, acknowledged);
                    }
                    // This change replays only the head whose effect on the
                    // main process the agent established.
                    Mode::Exact => match tr::replay_if_exact(
                        &self.shadow,
                        desired,
                        &requests[..failure.replayable],
                    ) {
                        Some(replayed) => {
                            self.shadow = replayed;
                            verdict = " [replay exact: adopted]".to_string();
                        }
                        None => verdict = " [replay not exact: shadow kept]".to_string(),
                    },
                }
                format!(
                    "FAILED, {} of {} acknowledged: {}{verdict}",
                    failure.applied,
                    requests.len(),
                    failure.cause
                )
            }
        };
        Ok(format!("[{}] -> {outcome}", labels.join(", ")))
    }

    /// The shadow's routes for `host`, as `scheme -> cluster`.
    fn routes(&self, host: &str) -> String {
        let routes: Vec<String> = self
            .shadow
            .frontends
            .iter()
            .filter(|f| f.hostname == host)
            .map(|f| {
                format!(
                    "{} -> {}",
                    if f.tls { "https" } else { "http" },
                    f.cluster_id.as_deref().unwrap_or("-")
                )
            })
            .collect();
        format!("[{}]", routes.join(", "))
    }

    /// The shadow's cluster `id` and its UDP routes, as the translator would
    /// send them.
    fn udp_shadow(&self, id: &str) -> String {
        let requests = tr::ir_to_requests(&self.shadow);
        let cluster = requests
            .iter()
            .find_map(|r| match &r.request_type {
                Some(RequestType::AddCluster(c)) if c.cluster_id == id => Some(cluster_config(c)),
                _ => None,
            })
            .unwrap_or_else(|| "absent".to_string());
        let routes: Vec<String> = self
            .shadow
            .l4_frontends
            .iter()
            .filter(|f| f.cluster_id == id)
            .map(|f| format!("udp {}", f.listener))
            .collect();
        format!("{cluster}; routes [{}]", routes.join(", "))
    }
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
    }
}

fn backend(cluster_id: &str, n: u8) -> ir::Backend {
    ir::Backend {
        cluster_id: cluster_id.to_string(),
        backend_id: format!("{cluster_id}-{n}"),
        address: SocketAddr::from(([127, 0, 0, n], 9)),
        weight: None,
        sticky_id: None,
    }
}

fn route(listener: SocketAddr, host: &str, cluster_id: &str, tls: bool) -> ir::Frontend {
    ir::Frontend {
        hostname: host.to_string(),
        path: ir::PathMatch::Prefix("/".to_string()),
        method: None,
        cluster_id: Some(cluster_id.to_string()),
        tls,
        listener,
        filters: ir::FrontendFilters::default(),
    }
}

/// The request Sōzu refuses part-way through a batch: a certificate removal
/// whose fingerprint is not hex, which the main process refuses before any
/// worker sees it.
fn refused(https: SocketAddr) -> Request {
    RequestType::RemoveCertificate(RemoveCertificate {
        address: https.into(),
        fingerprint: "zz".to_string(),
    })
    .into()
}

struct Probe {
    sock: String,
    http: SocketAddr,
    https: SocketAddr,
    udp_port: u16,
}

impl Probe {
    /// The refusal appended at the end of a batch.
    fn fail_at_end(&self) -> Fail {
        Fail {
            refused: refused(self.https),
            after: usize::MAX,
        }
    }

    /// A controller starting from an empty shadow, and the name and host
    /// suffix of the objects it owns.
    fn controller(&self, mode: Mode, sequence: &str) -> (Controller, String, String) {
        let name = format!("{sequence}-{}", mode.tag());
        println!(
            "== ({}) {sequence}: shadow after a failed batch: {} ==",
            mode.tag(),
            mode.describe()
        );
        let controller = Controller {
            agent: SozuAgent::new(&self.sock),
            shadow: ir::Ir::default(),
            mode,
        };
        let suffix = format!(".{name}.probe");
        (controller, name, suffix)
    }

    /// Cluster x removed by a failed batch, then wanted back with a second
    /// backend.
    fn back(&self, mode: Mode) -> Result<()> {
        let (mut controller, name, suffix) = self.controller(mode, "back");
        let (http, https) = (self.http, self.https);
        let (x, y) = (format!("x-{name}"), format!("y-{name}"));
        let (x_host, y_host) = (format!("x{suffix}"), format!("y{suffix}"));
        let s = ir::Ir {
            clusters: vec![cluster(&x), cluster(&y)],
            backends: vec![backend(&x, 1), backend(&y, 3)],
            frontends: vec![
                route(http, &x_host, &x, false),
                route(https, &x_host, &x, true),
                route(http, &y_host, &y, false),
            ],
            certificates: vec![ir::Certificate {
                listener: https,
                certificate: CERT.to_string(),
                chain: vec![],
                key: KEY.to_string(),
                names: vec!["app.example.com".to_string()],
            }],
            l4_frontends: vec![],
        };
        let mut without_x = s.clone();
        without_x.clusters.retain(|c| c.id != x);
        without_x.backends.retain(|b| b.cluster_id != x);
        without_x
            .frontends
            .retain(|f| f.cluster_id.as_deref() != Some(x.as_str()));
        let mut x_back = s.clone();
        x_back.backends.push(backend(&x, 2));
        let mut new_route = x_back.clone();
        new_route
            .frontends
            .push(route(http, &format!("y2{suffix}"), &y, false));

        let sock = &self.sock;
        println!("1. apply S: {}", controller.reconcile(&s, None)?);
        println!("   frontends: {}", listing(sock, &suffix)?);
        println!(
            "2. remove {x}, a later request refused: {}",
            controller.reconcile(&without_x, Some(self.fail_at_end()))?
        );
        println!("   frontends: {}", listing(sock, &suffix)?);
        println!(
            "   shadow lists cluster {x}: {}",
            controller.shadow.clusters.iter().any(|c| c.id == x)
        );
        println!(
            "3. {x} back with a second backend: {}",
            controller.reconcile(&x_back, None)?
        );
        println!("   frontends: {}", listing(sock, &suffix)?);
        println!(
            "4. next reconcile, a new route on {y}: {}",
            controller.reconcile(&new_route, None)?
        );
        println!("   frontends: {}", listing(sock, &suffix)?);
        println!(
            "5. next reconcile, unchanged: {}",
            controller.reconcile(&new_route, None)?
        );
        println!("   frontends: {}", listing(sock, &suffix)?);
        Ok(())
    }

    /// Route k re-pointed from cluster a to cluster b by a failed batch whose
    /// removal and add Sōzu both acknowledged, then deleted.
    fn repoint(&self, mode: Mode) -> Result<()> {
        let (mut controller, name, suffix) = self.controller(mode, "repoint");
        let (http, https) = (self.http, self.https);
        let (a, b) = (format!("a-{name}"), format!("b-{name}"));
        let k_host = format!("k{suffix}");
        let with_k = |target: Option<&str>| {
            let mut frontends = vec![
                route(http, &format!("a{suffix}"), &a, false),
                route(http, &format!("b{suffix}"), &b, false),
            ];
            if let Some(target) = target {
                frontends.push(route(http, &k_host, target, false));
                frontends.push(route(https, &k_host, target, true));
            }
            ir::Ir {
                clusters: vec![cluster(&a), cluster(&b)],
                backends: vec![backend(&a, 1), backend(&b, 3)],
                frontends,
                certificates: vec![],
                l4_frontends: vec![],
            }
        };

        let sock = &self.sock;
        println!(
            "1. apply S: {}",
            controller.reconcile(&with_k(Some(&a)), None)?
        );
        println!("   frontends: {}", listing(sock, &suffix)?);
        println!(
            "2. re-point {k_host} to {b}, a later request refused: {}",
            controller.reconcile(&with_k(Some(&b)), Some(self.fail_at_end()))?
        );
        println!("   frontends: {}", listing(sock, &suffix)?);
        println!("   shadow routes {k_host}: {}", controller.routes(&k_host));
        println!(
            "3. delete {k_host}: {}",
            controller.reconcile(&with_k(None), None)?
        );
        println!("   frontends: {}", listing(sock, &suffix)?);
        println!(
            "4. next reconcile, unchanged: {}",
            controller.reconcile(&with_k(None), None)?
        );
        println!("   frontends: {}", listing(sock, &suffix)?);
        Ok(())
    }
}

impl Probe {
    /// Cluster u, which a UDP route gives UDP affinity, switched to random
    /// while the route goes, by a batch that fails right after u's upsert;
    /// then, with `revert`, the first state wanted back, and otherwise the
    /// route wanted back on the random cluster.
    fn udp(&self, mode: Mode, revert: bool, port: u16) -> Result<()> {
        let sequence = if revert { "udp-revert" } else { "udp" };
        let (mut controller, name, _) = self.controller(mode, sequence);
        let u = format!("u-{name}");
        let listener = SocketAddr::from(([0, 0, 0, 0], port));
        let s = ir::Ir {
            clusters: vec![cluster(&u)],
            backends: vec![backend(&u, 5)],
            frontends: vec![],
            certificates: vec![],
            l4_frontends: vec![ir::L4Frontend {
                protocol: ir::L4Protocol::Udp,
                listener,
                cluster_id: u.clone(),
            }],
        };
        let mut random = s.clone();
        random.clusters[0].load_balancing = ir::LbAlgorithm::Random;
        random.l4_frontends.clear();
        let mut route_back = random.clone();
        route_back.l4_frontends = s.l4_frontends.clone();
        let third = if revert { &s } else { &route_back };

        let sock = &self.sock;
        let show = |controller: &Controller| -> Result<()> {
            println!("   sozu cluster {u}: {}", cluster_view(sock, &u)?);
            println!("   frontends: {}", listing(sock, &name)?);
            println!("   shadow: {}", controller.udp_shadow(&u));
            Ok(())
        };
        println!("1. apply S: {}", controller.reconcile(&s, None)?);
        show(&controller)?;
        let fail = Fail {
            refused: refused(self.https),
            after: 1,
        };
        println!(
            "2. {u} to random, its route dropped, refused after the upsert: {}",
            controller.reconcile(&random, Some(fail))?
        );
        show(&controller)?;
        let wanted = if revert {
            "S back"
        } else {
            "random, the route back"
        };
        println!("3. {wanted}: {}", controller.reconcile(third, None)?);
        show(&controller)?;
        println!(
            "4. next reconcile, unchanged: {}",
            controller.reconcile(third, None)?
        );
        show(&controller)?;
        Ok(())
    }
}

fn main() -> Result<()> {
    let probe = Probe {
        sock: env_or("SOZU_SOCK", "./sozu.sock"),
        http: env_or("PROBE_HTTP", "0.0.0.0:8080").parse()?,
        https: env_or("PROBE_HTTPS", "0.0.0.0:8443").parse()?,
        udp_port: env_or("PROBE_UDP_PORT", "15301").parse()?,
    };
    let modes = [Mode::Unchanged, Mode::Removals, Mode::Replay, Mode::Exact];
    println!("== partial apply probe ==");
    for mode in modes {
        probe.back(mode)?;
    }
    for mode in modes {
        probe.repoint(mode)?;
    }
    let mut port = probe.udp_port;
    for revert in [false, true] {
        for mode in modes {
            probe.udp(mode, revert, port)?;
            port += 1;
        }
    }
    Ok(())
}
