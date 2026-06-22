//! Throwaway protocol probe (Étape 1).
//!
//! Connects to a live Sōzu command socket and exercises the exact Phase-1 call
//! sequence against the REAL `sozu-command-lib` v2.1.0 API, so we can confirm
//! the wire protocol empirically before writing the Translator:
//!
//!   1. open the UNIX command socket, send `Status`, read the ack
//!   2. AddCluster + AddBackend + AddHttpFrontend  -> route host/path over HTTP
//!   3. AddCertificate + AddHttpsFrontend          -> terminate TLS for the host
//!
//! The functional proof (curl/openssl through Sōzu) is done by the surrounding
//! harness script; this binary just applies config and reports every response.
//!
//! Configuration via env vars (all optional, with dev defaults):
//!   SOZU_SOCK      path to the command socket           (default ./sozu.sock)
//!   PROBE_HTTP     HTTP listener address                (default 0.0.0.0:8080)
//!   PROBE_HTTPS    HTTPS listener address               (default 0.0.0.0:8443)
//!   PROBE_BACKEND  backend (pod-equivalent) address     (default 127.0.0.1:9000)
//!   PROBE_HOST     hostname to route                    (default app.example.com)
//!   PROBE_CERT     path to PEM certificate (tls.crt)    (default ./app.crt)
//!   PROBE_KEY      path to PEM private key  (tls.key)   (default ./app.key)
//!   PROBE_TRANSPORT  "request" | "worker"               (default request)
//!
//! This is example/test code: `?`/`anyhow` and a couple of `expect`s on
//! env parsing are fine here (not a production path).

use std::net::SocketAddr;

use anyhow::{anyhow, Context, Result};
use sozu_command_lib::certificate::split_certificate_chain;
use sozu_command_lib::channel::Channel;
use sozu_command_lib::proto::command::{
    request::RequestType, AddBackend, AddCertificate, CertificateAndKey, Cluster,
    LoadBalancingAlgorithms, PathRule, PathRuleKind, Request, RequestHttpFrontend, Response,
    ResponseStatus, RulePosition, SocketAddress, Status,
};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn sock_addr(s: &str) -> Result<SocketAddress> {
    let parsed: SocketAddr = s
        .parse()
        .with_context(|| format!("invalid socket address {s:?}"))?;
    // NEVER hand-pack the protobuf address: rely on the crate's conversion.
    Ok(parsed.into())
}

/// Send one `Request` and await the TERMINAL response, skipping any interim
/// `Processing`. Returns the terminal `Response` (or an error on `Failure`).
fn apply(channel: &mut Channel<Request, Response>, label: &str, req: Request) -> Result<Response> {
    channel
        .write_message(&req)
        .with_context(|| format!("write_message failed for {label}"))?;

    loop {
        let resp: Response = channel
            .read_message()
            .with_context(|| format!("read_message failed for {label}"))?;

        let status = resp.status;
        if status == ResponseStatus::Processing as i32 {
            println!("    [{label}] .. PROCESSING: {}", resp.message);
            continue;
        }
        if status == ResponseStatus::Ok as i32 {
            println!("    [{label}] OK: {}", resp.message);
            if let Some(content) = &resp.content {
                println!("    [{label}]    content: {content:?}");
            }
            return Ok(resp);
        }
        // Failure
        return Err(anyhow!(
            "[{label}] sozu FAILURE (status={status}): {}",
            resp.message
        ));
    }
}

fn main() -> Result<()> {
    let sock = env_or("SOZU_SOCK", "./sozu.sock");
    let listen_http = env_or("PROBE_HTTP", "0.0.0.0:8080");
    let listen_https = env_or("PROBE_HTTPS", "0.0.0.0:8443");
    let backend = env_or("PROBE_BACKEND", "127.0.0.1:9000");
    let host = env_or("PROBE_HOST", "app.example.com");
    let cert_path = env_or("PROBE_CERT", "./app.crt");
    let key_path = env_or("PROBE_KEY", "./app.key");
    let transport = env_or("PROBE_TRANSPORT", "request");

    let cluster_id = host.replace('.', "-");

    println!("== sozu protocol probe ==");
    println!("  socket       : {sock}");
    println!("  http listener: {listen_http}");
    println!("  https list.  : {listen_https}");
    println!("  backend      : {backend}");
    println!("  hostname     : {host}");
    println!("  cluster_id   : {cluster_id}");
    println!("  transport    : {transport}");
    println!();

    if transport == "worker" {
        // Open Question #1 probe: try the Sōzu->Sōzu envelope instead, to see
        // whether the external command socket rejects bare `Request`.
        return probe_worker_transport(&sock);
    }

    // ---- (a) connect + Status handshake ----------------------------------
    // from_path() returns a NON-blocking channel; switch to blocking for a
    // simple synchronous send/recv loop.
    let mut channel: Channel<Request, Response> =
        Channel::from_path(&sock, 1024 * 1024, 16 * 1024 * 1024)
            .with_context(|| format!("failed to connect to sozu socket at {sock}"))?;
    channel
        .blocking()
        .context("failed to set channel to blocking mode")?;
    println!("[1] connected, sending Status (bare Request transport)");
    apply(
        &mut channel,
        "Status",
        RequestType::Status(Status {}).into(),
    )?;

    // ---- (b) HTTP routing: AddCluster + AddBackend + AddHttpFrontend ------
    println!("\n[2] HTTP routing for {host} -> {backend}");

    let cluster = Cluster {
        cluster_id: cluster_id.clone(),
        sticky_session: false,
        https_redirect: false,
        load_balancing: LoadBalancingAlgorithms::RoundRobin as i32,
        ..Default::default()
    };
    apply(
        &mut channel,
        "AddCluster",
        RequestType::AddCluster(cluster).into(),
    )?;

    let add_backend = AddBackend {
        cluster_id: cluster_id.clone(),
        backend_id: format!("{cluster_id}-0"),
        address: sock_addr(&backend)?,
        sticky_id: None,
        load_balancing_parameters: None,
        backup: None,
    };
    apply(
        &mut channel,
        "AddBackend",
        RequestType::AddBackend(add_backend).into(),
    )?;

    let http_front = RequestHttpFrontend {
        cluster_id: Some(cluster_id.clone()),
        address: sock_addr(&listen_http)?,
        hostname: host.clone(),
        path: PathRule {
            kind: PathRuleKind::Prefix as i32,
            value: "/".to_string(),
        },
        method: None,
        position: RulePosition::Tree as i32,
        ..Default::default()
    };
    apply(
        &mut channel,
        "AddHttpFrontend",
        RequestType::AddHttpFrontend(http_front).into(),
    )?;

    // ---- (c) TLS: AddCertificate + AddHttpsFrontend ----------------------
    println!("\n[3] TLS termination for {host}");

    let cert_pem =
        std::fs::read_to_string(&cert_path).with_context(|| format!("reading cert {cert_path}"))?;
    let key_pem =
        std::fs::read_to_string(&key_path).with_context(|| format!("reading key {key_path}"))?;
    // split_certificate_chain() splits a concatenated PEM into individual certs;
    // the leaf is element 0, the rest is the chain.
    let mut chain = split_certificate_chain(cert_pem);
    let leaf = if chain.is_empty() {
        return Err(anyhow!("no PEM blocks found in {cert_path}"));
    } else {
        chain.remove(0)
    };

    let add_cert = AddCertificate {
        address: sock_addr(&listen_https)?,
        certificate: CertificateAndKey {
            certificate: leaf,
            certificate_chain: chain,
            key: key_pem,
            versions: vec![],          // empty => server default (TLS 1.2 + 1.3)
            names: vec![host.clone()], // explicit SNI name (avoids name-derivation ambiguity)
        },
        expired_at: None,
    };
    apply(
        &mut channel,
        "AddCertificate",
        RequestType::AddCertificate(add_cert).into(),
    )?;

    let https_front = RequestHttpFrontend {
        cluster_id: Some(cluster_id.clone()),
        address: sock_addr(&listen_https)?,
        hostname: host.clone(),
        path: PathRule {
            kind: PathRuleKind::Prefix as i32,
            value: "/".to_string(),
        },
        method: None,
        position: RulePosition::Tree as i32,
        ..Default::default()
    };
    apply(
        &mut channel,
        "AddHttpsFrontend",
        RequestType::AddHttpsFrontend(https_front).into(),
    )?;

    // ---- (d) idempotency smoke check: re-send Status ---------------------
    println!("\n[4] re-send Status (sanity)");
    apply(
        &mut channel,
        "Status#2",
        RequestType::Status(Status {}).into(),
    )?;

    println!("\n== probe applied successfully. Run curl/openssl checks now. ==");
    Ok(())
}

/// Minimal alternate-transport handshake to answer Open Question #1: does the
/// external command socket expect a `WorkerRequest { id, content }` envelope?
fn probe_worker_transport(sock: &str) -> Result<()> {
    use sozu_command_lib::proto::command::{WorkerRequest, WorkerResponse};

    let mut channel: Channel<WorkerRequest, WorkerResponse> =
        Channel::from_path(sock, 1024 * 1024, 16 * 1024 * 1024)
            .with_context(|| format!("failed to connect to sozu socket at {sock}"))?;
    channel
        .blocking()
        .context("failed to set channel to blocking mode")?;

    println!("[worker-transport] sending WorkerRequest{{id, Status}}");
    let req = WorkerRequest {
        id: "probe-status".to_string(),
        content: RequestType::Status(Status {}).into(),
    };
    channel
        .write_message(&req)
        .context("write_message (WorkerRequest) failed")?;
    let resp: WorkerResponse = channel
        .read_message()
        .context("read_message (WorkerResponse) failed")?;
    println!("[worker-transport] got WorkerResponse: {resp:?}");
    Ok(())
}
