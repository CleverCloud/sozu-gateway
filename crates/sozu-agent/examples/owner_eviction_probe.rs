//! Does the agent converge when Sōzu stores a frontend under a cluster the
//! shadow never knew?
//!
//! That is the state a batch leaves behind when it applied but was never
//! recorded (a lost ack, a failure later in the batch, a controller restart
//! mid-apply) and the route then moves again. The probe stages it directly:
//! each key is installed for cluster `stale`, then the agent is handed the
//! request a reconcile from the stale shadow would send — an add for cluster
//! `wanted`, or a removal naming cluster `shadow` (or `ghost`, which Sōzu
//! never held) — and the main process's own
//! frontend list is printed after it, so the outcome is read from Sōzu rather
//! than from the agent's verdict.
//!
//! Env: SOZU_SOCK (command socket), PROBE_HTTP (an active HTTP listener),
//! PROBE_L4_BASE (first of three free ports for the TCP/UDP listeners it adds).

use std::net::SocketAddr;

use anyhow::{Context, Result};
use sozu_command_lib::channel::Channel;
use sozu_command_lib::proto::command::{
    request::RequestType, response_content::ContentType, ActivateListener, AddBackend, Cluster,
    FrontendFilters, ListenerType, PathRule, PathRuleKind, Request, RequestHttpFrontend,
    RequestTcpFrontend, RequestUdpFrontend, Response, ResponseContent, ResponseStatus,
    RulePosition, TcpListenerConfig, UdpListenerConfig,
};
use sozu_gw_agent::SozuAgent;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// What the main process lists for the probe's keys, by protocol.
fn listing(sock: &str) -> Result<String> {
    let mut channel: Channel<Request, Response> =
        Channel::from_path(sock, 1024 * 1024, 16 * 1024 * 1024)
            .with_context(|| format!("connect {sock}"))?;
    channel.blocking().context("blocking channel")?;
    let query: Request = RequestType::ListFrontends(FrontendFilters {
        http: false,
        https: false,
        tcp: false,
        domain: None,
        cluster_id: None,
    })
    .into();
    channel.write_message(&query).context("write")?;
    loop {
        let response = channel.read_message().context("read")?;
        if response.status == ResponseStatus::Processing as i32 {
            continue;
        }
        let Some(ResponseContent {
            content_type: Some(ContentType::FrontendList(listed)),
        }) = response.content
        else {
            return Ok(format!("no listing: {}", response.message));
        };
        let mut out = Vec::new();
        for front in &listed.http_frontends {
            out.push(format!(
                "http {}{} -> {}",
                front.hostname,
                front.path.value,
                front.cluster_id.as_deref().unwrap_or("-")
            ));
        }
        for front in &listed.tcp_frontends {
            out.push(format!(
                "tcp :{} -> {}",
                SocketAddr::from(front.address).port(),
                front.cluster_id
            ));
        }
        for front in &listed.udp_frontends {
            out.push(format!(
                "udp :{} -> {}",
                SocketAddr::from(front.address).port(),
                front.cluster_id
            ));
        }
        out.sort();
        return Ok(out.join(", "));
    }
}

fn http_front(http: SocketAddr, hostname: &str, cluster: &str) -> RequestHttpFrontend {
    RequestHttpFrontend {
        cluster_id: Some(cluster.to_string()),
        address: http.into(),
        hostname: hostname.to_string(),
        path: PathRule {
            kind: PathRuleKind::Prefix as i32,
            value: "/".to_string(),
        },
        position: RulePosition::Tree as i32,
        ..RequestHttpFrontend::default()
    }
}

fn tcp_front(port: u16, cluster: &str) -> RequestTcpFrontend {
    RequestTcpFrontend {
        cluster_id: cluster.to_string(),
        address: SocketAddr::from(([0, 0, 0, 0], port)).into(),
        ..RequestTcpFrontend::default()
    }
}

fn udp_front(port: u16, cluster: &str) -> RequestUdpFrontend {
    RequestUdpFrontend {
        cluster_id: cluster.to_string(),
        address: SocketAddr::from(([0, 0, 0, 0], port)).into(),
        ..RequestUdpFrontend::default()
    }
}

fn main() -> Result<()> {
    let sock = env_or("SOZU_SOCK", "./sozu.sock");
    let http: SocketAddr = env_or("PROBE_HTTP", "0.0.0.0:8080").parse()?;
    let base: u16 = env_or("PROBE_L4_BASE", "9101").parse()?;
    let (tcp_port, udp_port, tcp_orphan_port) = (base, base + 1, base + 2);
    let mut agent = SozuAgent::new(sock.clone());

    println!("== owner eviction probe ==");
    let mut setup: Vec<Request> = Vec::new();
    for cluster in ["stale", "wanted", "shadow"] {
        setup.push(
            RequestType::AddCluster(Cluster {
                cluster_id: cluster.to_string(),
                ..Cluster::default()
            })
            .into(),
        );
        setup.push(
            RequestType::AddBackend(AddBackend {
                cluster_id: cluster.to_string(),
                backend_id: format!("{cluster}-0"),
                address: SocketAddr::from(([127, 0, 0, 1], 9)).into(),
                ..AddBackend::default()
            })
            .into(),
        );
    }
    for port in [tcp_port, tcp_orphan_port] {
        let address = SocketAddr::from(([0, 0, 0, 0], port));
        setup.push(
            RequestType::AddTcpListener(TcpListenerConfig {
                address: address.into(),
                ..TcpListenerConfig::default()
            })
            .into(),
        );
        setup.push(
            RequestType::ActivateListener(ActivateListener {
                address: address.into(),
                proxy: ListenerType::Tcp as i32,
                from_scm: false,
                interface: None,
            })
            .into(),
        );
    }
    let udp_address = SocketAddr::from(([0, 0, 0, 0], udp_port));
    setup.push(
        RequestType::AddUdpListener(UdpListenerConfig {
            address: udp_address.into(),
            ..UdpListenerConfig::default()
        })
        .into(),
    );
    setup.push(
        RequestType::ActivateListener(ActivateListener {
            address: udp_address.into(),
            proxy: ListenerType::Udp as i32,
            from_scm: false,
            interface: None,
        })
        .into(),
    );
    // The state a stale shadow does not know about: every key held by `stale`.
    setup.push(RequestType::AddHttpFrontend(http_front(http, "add.probe", "stale")).into());
    setup.push(RequestType::AddHttpFrontend(http_front(http, "remove.probe", "stale")).into());
    setup.push(RequestType::AddHttpFrontend(http_front(http, "ghost.probe", "stale")).into());
    setup.push(RequestType::AddTcpFrontend(tcp_front(tcp_port, "stale")).into());
    setup.push(RequestType::AddUdpFrontend(udp_front(udp_port, "stale")).into());
    setup.push(RequestType::AddTcpFrontend(tcp_front(tcp_orphan_port, "stale")).into());
    agent.apply(&setup).context("setup")?;
    println!("before: {}", listing(&sock)?);

    let cases: Vec<(&str, Request)> = vec![
        (
            "http add for another cluster",
            RequestType::AddHttpFrontend(http_front(http, "add.probe", "wanted")).into(),
        ),
        (
            "http removal naming the shadow's cluster",
            RequestType::RemoveHttpFrontend(http_front(http, "remove.probe", "shadow")).into(),
        ),
        (
            "http removal naming a cluster sozu never held",
            RequestType::RemoveHttpFrontend(http_front(http, "ghost.probe", "ghost")).into(),
        ),
        (
            "tcp add for another cluster",
            RequestType::AddTcpFrontend(tcp_front(tcp_port, "wanted")).into(),
        ),
        (
            "udp add for another cluster",
            RequestType::AddUdpFrontend(udp_front(udp_port, "wanted")).into(),
        ),
        (
            "tcp removal naming the shadow's cluster",
            RequestType::RemoveTcpFrontend(tcp_front(tcp_orphan_port, "shadow")).into(),
        ),
    ];
    for (name, request) in cases {
        let outcome = match agent.apply(std::slice::from_ref(&request)) {
            Ok(()) => "applied".to_string(),
            Err(error) => format!("FAILED: {error}"),
        };
        println!("{name}: {outcome}");
    }
    println!("after: {}", listing(&sock)?);
    Ok(())
}
