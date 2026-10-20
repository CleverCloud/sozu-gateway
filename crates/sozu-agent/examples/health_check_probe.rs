//! Does a TCP-connect health check eject a black-holed TCPRoute backend, how
//! fast, and what does the controller's own translator emit for one?
//!
//! On Sōzu 2.3.2 a TCP session whose backend connect *times out* is killed with
//! no failover, and the timeout charges nothing to the backend: the TCP proxy
//! records a failure and retries only on a refused or reset connect
//! (`lib/src/tcp.rs`, `fail_backend_connection` is called from the HUP path,
//! the backend timeout just closes). So about one new connection in N fails for
//! as long as a black-holed pod IP stays listed. The probe programs one TCP
//! cluster through the real pipeline (IR → `sozu_gw_translator::reconcile` →
//! agent), opens fresh connections at a fixed rate, open loop, and reads each
//! worker's `health_check.down` counter over the command socket:
//!
//!  1. `none`: a live echo server it runs itself and a black-holed address,
//!     no health check;
//!  2. `tcp`: the check turned on by a check-only diff (what the translator
//!     emits for it is printed), timed from the apply to each worker's first
//!     DOWN;
//!  3. `steady`: the same check, once every worker counted exactly one DOWN —
//!     the black hole's;
//!  4. `removed`: the check turned off again by a check-only diff;
//!  5. `dies`: the check on, the black hole replaced by a second echo server
//!     that works until the probe stops it answering mid-run — it stops
//!     accepting and the probe fills its accept queue, after which the kernel
//!     drops every new SYN, a black hole — timed from the queue filling to
//!     each worker's next DOWN.
//!
//! Each connection sends one line and must read it back; a session Sōzu closes
//! without an answer is a failure, and so is no answer within
//! `PROBE_READ_TIMEOUT_SECS`.
//!
//! Env: `SOZU_SOCK` (default `/run/sozu/sozu.sock`), `PROBE_TCP_BIND` (the
//! listener Sōzu opens, default `0.0.0.0:9300`), `PROBE_TCP_DIAL` (where the
//! probe reaches it, default `127.0.0.1:9300`), `PROBE_LIVE` and `PROBE_DYING`
//! (where the two echo backends bind — Sōzu must be able to dial them —
//! default `127.0.0.1:9301` and `127.0.0.1:9302`), `PROBE_DEAD` (a black hole,
//! default `10.255.255.1:9301`), `PROBE_CONNECTIONS` (per phase, default 120),
//! `PROBE_PERIOD_MS` (default 250), `PROBE_READ_TIMEOUT_SECS` (default 8),
//! `PROBE_DIES_AFTER_MS` (how long the check runs in phase 5 before the backend
//! stops answering, default 10000 — vary it to land at another point of the
//! probe cycle), and the check's timing `PROBE_HC` as
//! `interval/timeout/healthy/unhealthy` (default `3/2/2/2`, the chart's
//! documented default).
//!
//! Example/probe code: `expect`/`anyhow` are fine here, this is not a
//! production path.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sozu_command_lib::channel::Channel;
use sozu_command_lib::proto::command::{
    filtered_metrics::Inner, request::RequestType, response_content::ContentType,
    QueryHealthChecks, QueryMetricsOptions, Request, Response, ResponseContent, ResponseStatus,
};
use sozu_gw_agent::SozuAgent;
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// The probe's cluster id, `PROBE_CLUSTER` (default `probe.health-check.9301`).
/// It decides the jitter Sōzu adds to the probe interval.
fn cluster() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| env_or("PROBE_CLUSTER", "probe.health-check.9301"))
}

/// The jitter Sōzu 2.3.2 adds to a cluster's probe interval, computed the way
/// it does (`lib/src/health_check.rs`, `initiate_checks`): the std
/// `DefaultHasher` of the cluster id, modulo a fifth of the interval.
fn sozu_jitter_ms(cluster_id: &str, interval_secs: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let interval = Duration::from_secs(u64::from(interval_secs));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cluster_id.to_string().hash(&mut hasher);
    hasher.finish() % (interval.as_millis() as u64 / 5).max(1)
}

fn backend_id(name: &str) -> String {
    format!("{}#{name}", cluster())
}

/// Answer one connection: one line in, the same line out, then close.
fn echo(stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) > 0 {
        let mut stream = stream;
        let _ = stream.write_all(line.as_bytes());
    }
}

/// An echo server on `addr` that answers until `stopped` is set, then stops
/// calling `accept` — so its accept queue fills and the kernel drops new SYNs.
fn spawn_echo(addr: SocketAddr, stopped: Arc<AtomicBool>) -> Result<()> {
    let listener = TcpListener::bind(addr).with_context(|| format!("bind echo {addr}"))?;
    listener.set_nonblocking(true)?;
    std::thread::spawn(move || loop {
        if stopped.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                std::thread::spawn(move || echo(stream));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    });
    Ok(())
}

/// Stop `addr` answering: no more `accept`, then connections of our own until
/// one times out — Linux drops a SYN outright while the listener's accept
/// queue is full (`tcp_conn_request`), so from the last completed filler on,
/// every new connection to `addr` is black-holed. Returns the fillers, which
/// must stay open, and that instant. Any other error, or a queue that never
/// fills, is an error: it would not be a black hole.
fn black_hole(addr: SocketAddr, stopped: &AtomicBool) -> Result<(Vec<TcpStream>, Instant)> {
    stopped.store(true, Ordering::SeqCst);
    // The accept loop polls every 2 ms; let it see the flag.
    std::thread::sleep(Duration::from_millis(20));
    let mut fillers = Vec::new();
    let mut full = Instant::now();
    loop {
        anyhow::ensure!(
            fillers.len() < 4096,
            "{addr} still completes connections after 4096 fillers: its accept queue never filled"
        );
        match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            Ok(stream) => {
                fillers.push(stream);
                full = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Ok((fillers, full)),
            Err(e) => {
                anyhow::bail!("filling {addr}'s accept queue failed with {e}, not a dropped SYN")
            }
        }
    }
}

/// How one connection through Sōzu ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Echoed,
    /// Sōzu closed the session without an answer.
    Closed,
    /// No answer within the read timeout.
    Timeout,
    /// The listener itself could not be reached.
    Connect,
}

struct Attempt {
    started: Duration,
    took: Duration,
    outcome: Outcome,
}

fn attempt(dial: SocketAddr, read_timeout: Duration) -> (Outcome, Duration) {
    let begin = Instant::now();
    let Ok(stream) = TcpStream::connect_timeout(&dial, Duration::from_secs(2)) else {
        return (Outcome::Connect, begin.elapsed());
    };
    let _ = stream.set_read_timeout(Some(read_timeout));
    let mut writer = stream.try_clone().expect("clone");
    if writer.write_all(b"ping\n").is_err() {
        return (Outcome::Closed, begin.elapsed());
    }
    let mut line = String::new();
    let outcome = match BufReader::new(stream).read_line(&mut line) {
        Ok(_) if line == "ping\n" => Outcome::Echoed,
        Ok(_) => Outcome::Closed,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Outcome::Timeout
        }
        Err(_) => Outcome::Closed,
    };
    (outcome, begin.elapsed())
}

/// Open `count` connections, one every `period` whatever the previous ones
/// are doing (open loop: a hanging session must not slow the schedule down).
fn load(
    dial: SocketAddr,
    count: usize,
    period: Duration,
    read_timeout: Duration,
    origin: Instant,
) -> Vec<Attempt> {
    let (tx, rx) = mpsc::channel();
    for i in 0..count {
        let due = origin + period * u32::try_from(i).expect("small count");
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let tx = tx.clone();
        std::thread::spawn(move || {
            let started = origin.elapsed();
            let (outcome, took) = attempt(dial, read_timeout);
            let _ = tx.send(Attempt {
                started,
                took,
                outcome,
            });
        });
    }
    drop(tx);
    let mut attempts: Vec<Attempt> = rx.iter().collect();
    attempts.sort_by_key(|a| a.started);
    attempts
}

fn summarise(phase: &str, attempts: &[Attempt]) {
    let mut by_outcome: BTreeMap<Outcome, usize> = BTreeMap::new();
    let mut windows: BTreeMap<u64, usize> = BTreeMap::new();
    let mut failed_took: Vec<f64> = Vec::new();
    let mut last_failure: Option<Duration> = None;
    let span = attempts.last().map_or(0, |a| a.started.as_secs() / 10);
    for w in 0..=span {
        windows.insert(w, 0);
    }
    for a in attempts {
        *by_outcome.entry(a.outcome).or_default() += 1;
        if a.outcome != Outcome::Echoed {
            *windows.entry(a.started.as_secs() / 10).or_default() += 1;
            failed_took.push(a.took.as_secs_f64());
            last_failure = Some(a.started);
        }
    }
    let failed = failed_took.len();
    println!(
        "[{phase}] {} connections: {} echoed, {failed} failed {:?}",
        attempts.len(),
        by_outcome.get(&Outcome::Echoed).copied().unwrap_or(0),
        by_outcome
            .iter()
            .filter(|(o, _)| **o != Outcome::Echoed)
            .collect::<Vec<_>>()
    );
    println!(
        "[{phase}]   failed per 10 s window: {:?}",
        windows.values().collect::<Vec<_>>()
    );
    if failed > 0 {
        let min = failed_took.iter().copied().fold(f64::INFINITY, f64::min);
        let max = failed_took.iter().copied().fold(0.0, f64::max);
        println!(
            "[{phase}]   each failure took {min:.2}-{max:.2} s; the last one started at +{:.2} s",
            last_failure.unwrap_or_default().as_secs_f64()
        );
    }
}

/// `health_check.down` per worker, as counted so far.
///
/// A proxy-wide counter: with Sōzu 2.3.2's default metrics detail level the
/// per-backend `backend.available` gauge loses its cluster and backend labels
/// before the local drain stores it (`lib/src/metrics/mod.rs`,
/// `filter_labels_for_detail`), so which backend went down cannot be read.
/// The phases tell it apart instead: each has exactly one backend that should
/// go down, a worker counting more is reported, and `steady` runs only when
/// every worker counted exactly one.
fn downs(agent: &mut SozuAgent) -> Result<BTreeMap<String, i64>> {
    let metrics = agent.query_metrics(QueryMetricsOptions {
        no_clusters: true,
        workers: true,
        metric_names: vec!["health_check.down".into()],
        ..Default::default()
    })?;
    Ok(metrics
        .workers
        .iter()
        .map(|(worker, w)| {
            let count = match w
                .proxy
                .get("health_check.down")
                .and_then(|m| m.inner.as_ref())
            {
                Some(Inner::Count(n)) => *n,
                _ => 0,
            };
            (worker.clone(), count)
        })
        .collect())
}

/// Poll every 100 ms, up to `deadline` past `origin`, until every worker has
/// counted a DOWN past `baseline`; return when each did.
fn time_down(
    poller: &mut SozuAgent,
    origin: Instant,
    baseline: &BTreeMap<String, i64>,
    deadline: Duration,
) -> Result<BTreeMap<String, Duration>> {
    let mut at: BTreeMap<String, Duration> = BTreeMap::new();
    while origin.elapsed() < deadline && at.len() < baseline.len() {
        for (worker, count) in downs(poller)? {
            if count > baseline.get(&worker).copied().unwrap_or(0) {
                at.entry(worker).or_insert_with(|| origin.elapsed());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(at)
}

/// Print when each worker counted its first DOWN of the phase and how many it
/// counted in all; `true` when every worker counted exactly one.
fn report_down(
    phase: &str,
    target: &str,
    at: &BTreeMap<String, Duration>,
    baseline: &BTreeMap<String, i64>,
    now: &BTreeMap<String, i64>,
) -> bool {
    let mut exactly_one = !baseline.is_empty();
    for (worker, before) in baseline {
        let counted = now.get(worker).copied().unwrap_or(0) - before;
        exactly_one &= counted == 1;
        match at.get(worker) {
            Some(when) => println!(
                "[{phase}]   worker {worker}: DOWN at +{:.1} s (polled every 0.1 s), {counted} \
                 DOWN(s) in the phase, where only {target} should go down",
                when.as_secs_f64()
            ),
            None => println!("[{phase}]   worker {worker}: no DOWN within the deadline"),
        }
    }
    exactly_one
}

/// What the main process holds as the cluster's health check, read back.
fn held_check(sock: &str) -> Result<String> {
    let mut channel: Channel<Request, Response> =
        Channel::from_path(sock, 1024 * 1024, 16 * 1024 * 1024)
            .with_context(|| format!("connect {sock}"))?;
    channel.blocking().context("blocking channel")?;
    let query: Request = RequestType::QueryHealthChecks(QueryHealthChecks {
        cluster_id: Some(cluster().to_string()),
    })
    .into();
    channel.write_message(&query).context("write")?;
    loop {
        let response = channel.read_message().context("read")?;
        if response.status == ResponseStatus::Processing as i32 {
            continue;
        }
        return Ok(match response.content {
            Some(ResponseContent {
                content_type: Some(ContentType::HealthChecksList(list)),
            }) => serde_json::to_string(&list.map)?,
            other => format!("{} / {other:?}", response.message),
        });
    }
}

fn graph(
    bind: SocketAddr,
    backends: &[(&str, SocketAddr)],
    check: Option<ir::HealthCheck>,
) -> ir::Ir {
    ir::Ir {
        clusters: vec![ir::Cluster {
            id: cluster().into(),
            load_balancing: ir::LbAlgorithm::RoundRobin,
            sticky_session: false,
            https_redirect: false,
            max_connections_per_ip: None,
            retry_after: None,
            http2: false,
            health_check: check,
        }],
        backends: backends
            .iter()
            .map(|(name, address)| ir::Backend {
                cluster_id: cluster().into(),
                backend_id: backend_id(name),
                address: *address,
                weight: None,
                sticky_id: None,
            })
            .collect(),
        l4_frontends: vec![ir::L4Frontend {
            protocol: ir::L4Protocol::Tcp,
            listener: bind,
            cluster_id: cluster().into(),
        }],
        ..Default::default()
    }
}

/// The requests a diff emits, one line each, with the cluster's check spelled
/// out (`Debug` on a request prints only its variant name).
fn show(phase: &str, requests: &[Request]) {
    println!(
        "[{phase}] translator emitted {} request(s):",
        requests.len()
    );
    for request in requests {
        match &request.request_type {
            Some(RequestType::AddCluster(cluster)) => println!(
                "[{phase}]   AddCluster {} health_check={}",
                cluster.cluster_id,
                serde_json::to_string(&cluster.health_check).unwrap_or_default()
            ),
            Some(other) => println!(
                "[{phase}]   {}",
                serde_json::to_string(other)
                    .unwrap_or_default()
                    .chars()
                    .take(160)
                    .collect::<String>()
            ),
            None => println!("[{phase}]   <empty>"),
        }
    }
}

fn main() -> Result<()> {
    let sock = env_or("SOZU_SOCK", "/run/sozu/sozu.sock");
    let bind: SocketAddr = env_or("PROBE_TCP_BIND", "0.0.0.0:9300").parse()?;
    let dial: SocketAddr = env_or("PROBE_TCP_DIAL", "127.0.0.1:9300").parse()?;
    let live: SocketAddr = env_or("PROBE_LIVE", "127.0.0.1:9301").parse()?;
    let dying: SocketAddr = env_or("PROBE_DYING", "127.0.0.1:9302").parse()?;
    let dead: SocketAddr = env_or("PROBE_DEAD", "10.255.255.1:9301").parse()?;
    let count: usize = env_or("PROBE_CONNECTIONS", "120").parse()?;
    let period = Duration::from_millis(env_or("PROBE_PERIOD_MS", "250").parse()?);
    let read_timeout = Duration::from_secs(env_or("PROBE_READ_TIMEOUT_SECS", "8").parse()?);
    let timing: Vec<u32> = env_or("PROBE_HC", "3/2/2/2")
        .split('/')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .context("PROBE_HC is interval/timeout/healthy/unhealthy")?;
    let [interval_secs, timeout_secs, healthy_threshold, unhealthy_threshold] = timing[..] else {
        anyhow::bail!("PROBE_HC is interval/timeout/healthy/unhealthy");
    };
    let check = ir::HealthCheck {
        interval_secs,
        timeout_secs,
        healthy_threshold,
        unhealthy_threshold,
        port: None,
    };
    let dies_after = Duration::from_millis(env_or("PROBE_DIES_AFTER_MS", "10000").parse()?);
    let deadline = Duration::from_secs(30);

    spawn_echo(live, Arc::new(AtomicBool::new(false)))?;
    let dying_stopped = Arc::new(AtomicBool::new(false));
    spawn_echo(dying, dying_stopped.clone())?;
    let mut agent = SozuAgent::new(sock.clone());
    let mut poller = SozuAgent::new(sock.clone());
    println!("== health check probe ==");
    println!(
        "cluster {} (Sōzu's interval jitter for it: {} ms): live {live} (echo), dead {dead} \
         (black hole), dying {dying} (echo until phase 5 stops it); round robin; {count} \
         connections per phase, one every {} ms; check {interval_secs}/{timeout_secs}/\
         {healthy_threshold}/{unhealthy_threshold} (interval/timeout/healthy/unhealthy)",
        cluster(),
        sozu_jitter_ms(cluster(), interval_secs),
        period.as_millis()
    );

    // 1. No check.
    let none = graph(bind, &[("live", live), ("dead", dead)], None);
    let requests = tr::reconcile(&ir::Ir::default(), &none)?;
    show("none", &requests);
    agent.apply(&requests).context("apply none")?;
    std::thread::sleep(Duration::from_secs(1));
    let attempts = load(dial, count, period, read_timeout, Instant::now());
    summarise("none", &attempts);
    println!(
        "[none]   health_check.down per worker: {:?}",
        downs(&mut poller)?
    );

    // 2. The check turned on by a check-only diff, timed to DOWN per worker.
    let checked = graph(bind, &[("live", live), ("dead", dead)], Some(check.clone()));
    let requests = tr::reconcile(&none, &checked)?;
    show("tcp", &requests);
    let baseline = downs(&mut poller)?;
    let origin = Instant::now();
    agent.apply(&requests).context("apply tcp")?;
    println!("[tcp] main process holds: {}", held_check(&sock)?);
    let load_thread = std::thread::spawn(move || load(dial, count, period, read_timeout, origin));
    let at = time_down(&mut poller, origin, &baseline, deadline)?;
    let attempts = load_thread.join().expect("load thread");
    let only_dead = report_down("tcp", "dead", &at, &baseline, &downs(&mut poller)?);
    summarise("tcp", &attempts);

    // 3. Same check, steady state: only once every worker counted exactly one
    // DOWN — a second one would be the live backend.
    if only_dead {
        let attempts = load(dial, count, period, read_timeout, Instant::now());
        summarise("steady", &attempts);
    } else {
        println!("[steady] skipped: not every worker counted exactly one DOWN");
    }

    // 4. The check turned off by a check-only diff.
    let requests = tr::reconcile(&checked, &none)?;
    show("removed", &requests);
    agent.apply(&requests).context("apply removed")?;
    println!("[removed] main process holds: {}", held_check(&sock)?);
    let attempts = load(dial, count, period, read_timeout, Instant::now());
    summarise("removed", &attempts);

    // 5. A backend that stops answering under a running check.
    let healthy = graph(bind, &[("live", live), ("dying", dying)], Some(check));
    let requests = tr::reconcile(&none, &healthy)?;
    show("dies", &requests);
    agent.apply(&requests).context("apply dies")?;
    std::thread::sleep(dies_after);
    let baseline = downs(&mut poller)?;
    println!(
        "[dies] after {} ms of checks, health_check.down per worker: {baseline:?}",
        dies_after.as_millis()
    );
    let (fillers, origin) = black_hole(dying, &dying_stopped)?;
    let traffic = Instant::now();
    println!(
        "[dies] {dying} stopped answering: {} connection(s) of the probe's own filled its accept \
         queue and the next one timed out; times are from the last completed filler (+0.0 s), \
         after which every SYN to it is dropped, and the connections start at +{:.1} s",
        fillers.len(),
        traffic.duration_since(origin).as_secs_f64()
    );
    let load_thread = std::thread::spawn(move || load(dial, count, period, read_timeout, traffic));
    let at = time_down(&mut poller, origin, &baseline, deadline)?;
    let attempts = load_thread.join().expect("load thread");
    report_down("dies", "dying", &at, &baseline, &downs(&mut poller)?);
    summarise("dies", &attempts);
    drop(fillers);
    Ok(())
}
