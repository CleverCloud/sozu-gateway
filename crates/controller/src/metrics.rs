//! Minimal HTTP `/metrics` endpoint exposing Sōzu's data-plane metrics.
//!
//! Sōzu has no native Prometheus endpoint; on each scrape this handler pulls its
//! aggregated metrics over the command socket (a `QueryMetrics` request) and
//! renders them with the pure `sozu-gw-prometheus` crate. Best-effort: a socket
//! hiccup yields `503`, never a panic, and a bind failure simply disables the
//! endpoint — routing is never affected.
//!
//! Hand-rolled on a `TcpListener` for the same reason as the health server, and
//! it reuses that module's request-line parser.

use std::net::SocketAddr;

use sozu_gw_agent::{QueryMetricsOptions, SozuAgentHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, warn};

/// Prometheus text exposition content type (legacy `0.0.4`).
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Spawn the metrics server as a background task. On a bind failure it logs and
/// gives up (metrics are an operability aid, never a reason to kill routing).
pub fn spawn(addr: SocketAddr, agent: SozuAgentHandle) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                error!(error = %e, %addr, "failed to bind metrics endpoint");
                return;
            }
        };
        debug!(%addr, "metrics endpoint listening (/metrics)");
        loop {
            match listener.accept().await {
                Ok((mut sock, _)) => {
                    let agent = agent.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_one(&mut sock, &agent).await {
                            debug!(error = %e, "metrics connection error");
                        }
                    });
                }
                Err(e) => warn!(error = %e, "metrics accept error"),
            }
        }
    });
}

async fn serve_one(sock: &mut TcpStream, agent: &SozuAgentHandle) -> std::io::Result<()> {
    let mut buf = [0u8; 256];
    let n = sock.read(&mut buf).await?;
    let (status, content_type, body): (&str, &str, String) =
        match crate::health::request_path(&buf[..n]) {
            Some("/metrics") => match agent.query_metrics(QueryMetricsOptions::default()).await {
                Ok(metrics) => ("200 OK", CONTENT_TYPE, sozu_gw_prometheus::render(&metrics)),
                Err(e) => {
                    warn!(error = %e, "failed to query sozu metrics");
                    (
                        "503 Service Unavailable",
                        "text/plain",
                        "metrics unavailable\n".to_string(),
                    )
                }
            },
            _ => ("404 Not Found", "text/plain", "not found\n".to_string()),
        };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(response.as_bytes()).await?;
    sock.flush().await
}
