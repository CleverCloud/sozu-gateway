//! Measurement probe: what a `QueryMetrics` returns with and without
//! `no_clusters`, and what the per-cluster query does to the workers.
//!
//! The controller's `/metrics` sends one `QueryMetrics` per scrape. Without
//! `no_clusters`, every worker answers with all of its cluster and backend
//! metrics in a single message; Sōzu 2.2.1's worker requeues a message larger
//! than `max_command_buffer_size` forever, so that worker stops serving. This
//! probe records, against a live Sōzu:
//!
//!  1. which metric names a `no_clusters` query returns (proxy + main only?);
//!  2. which names only the per-cluster query adds (the series a default
//!     scrape no longer exports);
//!  3. which error signals survive `no_clusters`: it also sends one request to
//!     an unknown host (Sōzu's 404) and one to a cluster whose only backend
//!     refuses connections (Sōzu's 503);
//!  4. with `PROBE_FULL_QUERY=1`, whether that query still answers and whether
//!     traffic still flows afterwards. Pair it with a lowered
//!     `max_command_buffer_size` to see the wedge on a laptop-sized config —
//!     **the workers do not recover; kill that Sōzu afterwards.**
//!
//! **Self-contained**, like `rewrite_redirect_probe`: it runs its own backend,
//! programs `PROBE_CLUSTERS` clusters (one backend and one HTTP frontend each),
//! sends `PROBE_REQUESTS` requests per host so each cluster holds metrics in
//! every worker, then queries.
//!
//! Env: `SOZU_SOCK` (default `/run/sozu/sozu.sock`), `PROBE_HTTP_BIND`
//! (default `0.0.0.0:8080`), `PROBE_HTTP_DIAL` (default `127.0.0.1:8080`),
//! `PROBE_BACKEND` (default `127.0.0.1:9098`), `PROBE_CLUSTERS` (default 20),
//! `PROBE_REQUESTS` (default 6), `PROBE_FULL_QUERY` (default off).
//!
//! Example/probe code: `expect`/`anyhow` are fine here, this is not a
//! production path.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sozu_command_lib::proto::command::{
    request::RequestType, AddBackend, AggregatedMetrics, Cluster, PathRule, PathRuleKind,
    QueryMetricsOptions, Request, RequestHttpFrontend, RulePosition,
};
use sozu_gw_agent::SozuAgent;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// A backend that answers every request `200 ok` and closes.
fn spawn_backend(addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr).with_context(|| format!("bind backend {addr}"))?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let mut stream = stream;
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\nconnection: close\r\n\r\nok\n",
                );
            });
        }
    });
    Ok(())
}

/// One HTTP/1.1 GET through Sōzu; `true` on a `200`.
fn get(dial: SocketAddr, host: &str) -> bool {
    status(dial, host).as_deref() == Some("200")
}

/// One HTTP/1.1 GET through Sōzu; the status code it answered with.
fn status(dial: SocketAddr, host: &str) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(&dial, Duration::from_secs(2)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut head = [0u8; 12];
    stream.read_exact(&mut head).ok()?;
    Some(String::from_utf8_lossy(&head[9..12]).into_owned())
}

/// Metric names by scope, in the Prometheus spelling the controller renders.
fn families(m: &AggregatedMetrics) -> [BTreeSet<String>; 4] {
    let prom = |raw: &str| {
        let body: String = raw
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        format!("sozu_{body}")
    };
    let proxy = m.proxying.keys().map(|k| prom(k)).collect();
    let main = m.main.keys().map(|k| prom(k)).collect();
    let cluster = m
        .clusters
        .values()
        .flat_map(|c| c.cluster.keys())
        .map(|k| prom(k))
        .collect();
    let backend = m
        .clusters
        .values()
        .flat_map(|c| c.backends.iter())
        .flat_map(|b| b.metrics.keys())
        .map(|k| prom(k))
        .collect();
    [proxy, main, cluster, backend]
}

fn report(label: &str, m: &AggregatedMetrics) {
    let [proxy, main, cluster, backend] = families(m);
    let backends: usize = m.clusters.values().map(|c| c.backends.len()).sum();
    println!(
        "[{label}] clusters={} backends={backends} workers={} json_bytes={}",
        m.clusters.len(),
        m.workers.len(),
        serde_json::to_string(m).map(|s| s.len()).unwrap_or(0)
    );
    for (scope, names) in [
        ("proxy", proxy),
        ("main", main),
        ("cluster", cluster),
        ("backend", backend),
    ] {
        println!("[{label}]   {scope} ({}): {:?}", names.len(), names);
    }
}

fn traffic(dial: SocketAddr, hosts: &[String], per_host: usize) -> (usize, usize) {
    let mut ok = 0;
    let mut total = 0;
    for host in hosts {
        for _ in 0..per_host {
            total += 1;
            ok += usize::from(get(dial, host));
        }
    }
    (ok, total)
}

fn main() -> Result<()> {
    let sock = env_or("SOZU_SOCK", "/run/sozu/sozu.sock");
    let bind: SocketAddr = env_or("PROBE_HTTP_BIND", "0.0.0.0:8080").parse()?;
    let dial: SocketAddr = env_or("PROBE_HTTP_DIAL", "127.0.0.1:8080").parse()?;
    let backend: SocketAddr = env_or("PROBE_BACKEND", "127.0.0.1:9098").parse()?;
    let clusters: usize = env_or("PROBE_CLUSTERS", "20").parse()?;
    let per_host: usize = env_or("PROBE_REQUESTS", "6").parse()?;
    let full = env_or("PROBE_FULL_QUERY", "") == "1";

    spawn_backend(backend)?;
    let mut agent = SozuAgent::new(sock);

    let mut requests: Vec<Request> = Vec::new();
    let mut hosts = Vec::new();
    for i in 0..clusters {
        let cluster_id = format!("probe.metrics-{i}.80");
        let host = format!("metrics-{i}.probe.test");
        requests.push(
            RequestType::AddCluster(Cluster {
                cluster_id: cluster_id.clone(),
                ..Default::default()
            })
            .into(),
        );
        requests.push(
            RequestType::AddBackend(AddBackend {
                cluster_id: cluster_id.clone(),
                backend_id: format!("{cluster_id}-0"),
                address: backend.into(),
                ..Default::default()
            })
            .into(),
        );
        requests.push(
            RequestType::AddHttpFrontend(RequestHttpFrontend {
                cluster_id: Some(cluster_id),
                address: bind.into(),
                hostname: host.clone(),
                path: PathRule {
                    kind: PathRuleKind::Prefix as i32,
                    value: "/".to_string(),
                },
                position: RulePosition::Tree as i32,
                ..Default::default()
            })
            .into(),
        );
        hosts.push(host);
    }
    // A cluster whose only backend refuses connections: Sōzu answers 503.
    let dead = "probe.metrics-dead.80";
    requests.push(
        RequestType::AddCluster(Cluster {
            cluster_id: dead.to_string(),
            ..Default::default()
        })
        .into(),
    );
    requests.push(
        RequestType::AddBackend(AddBackend {
            cluster_id: dead.to_string(),
            backend_id: format!("{dead}-0"),
            address: SocketAddr::from(([127, 0, 0, 1], 1)).into(),
            ..Default::default()
        })
        .into(),
    );
    requests.push(
        RequestType::AddHttpFrontend(RequestHttpFrontend {
            cluster_id: Some(dead.to_string()),
            address: bind.into(),
            hostname: "dead.probe.test".to_string(),
            path: PathRule {
                kind: PathRuleKind::Prefix as i32,
                value: "/".to_string(),
            },
            position: RulePosition::Tree as i32,
            ..Default::default()
        })
        .into(),
    );
    agent.apply(&requests).context("program clusters")?;
    println!("[setup] programmed {clusters} clusters + 1 with a refusing backend");

    let (ok, total) = traffic(dial, &hosts, per_host);
    println!("[traffic] before queries: {ok}/{total} x 200");
    println!(
        "[traffic] unknown host -> {:?}, refusing backend -> {:?}",
        status(dial, "unknown.probe.test"),
        status(dial, "dead.probe.test")
    );

    let started = Instant::now();
    let proxy_only = agent
        .query_metrics(QueryMetricsOptions {
            no_clusters: true,
            ..Default::default()
        })
        .context("no_clusters query")?;
    println!("[no_clusters] answered in {:?}", started.elapsed());
    report("no_clusters", &proxy_only);
    let (ok, total) = traffic(dial, &hosts[..1], 10);
    println!("[traffic] after no_clusters query: {ok}/{total} x 200");

    if full {
        let started = Instant::now();
        match agent.query_metrics(QueryMetricsOptions::default()) {
            Ok(all) => {
                println!("[full] answered in {:?}", started.elapsed());
                report("full", &all);
                let [p0, m0, ..] = families(&proxy_only);
                let [p1, m1, c1, b1] = families(&all);
                println!(
                    "[full] names cluster-labelled or proxy-level only in full: {:?}",
                    p1.difference(&p0)
                        .chain(m1.difference(&m0))
                        .chain(c1.iter())
                        .chain(b1.iter())
                        .collect::<BTreeSet<_>>()
                );
            }
            Err(e) => println!("[full] failed after {:?}: {e}", started.elapsed()),
        }
        let (ok, total) = traffic(dial, &hosts[..1], 10);
        println!("[traffic] after full query: {ok}/{total} x 200");
    }
    Ok(())
}
