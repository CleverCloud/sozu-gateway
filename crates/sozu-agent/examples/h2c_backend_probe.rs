//! Does a backend that only speaks h2c get HTTP/2 from Sōzu, and only when
//! its cluster carries `http2`?
//!
//! The controller maps a Service port's `appProtocol: kubernetes.io/h2c` onto
//! `Cluster.http2` (Gateway API GEP-1911). This probe programs Sōzu through
//! the same IR → translator → agent path the controller uses, with `http2`
//! unset (every cluster before that change), then set, then unset again, and
//! sends these clients at the result:
//!
//!  - an HTTP/1.1 client and an HTTP/2 client (ALPN h2) through the HTTPS
//!    listener, and an HTTP/1.1 client through the HTTP listener — with
//!    `http2` all three should reach the backend as HTTP/2;
//!  - an HTTP/2 client with prior knowledge (cleartext h2c) through the HTTP
//!    listener, which Sōzu refuses whatever the cluster says;
//!  - an HTTP/1.1 client through the HTTP listener to a backend that speaks
//!    HTTP/1.1 only: what a port wrongly declaring `kubernetes.io/h2c` gets;
//!  - a cleartext HTTP/1.1 WebSocket upgrade to a backend that serves both
//!    HTTP/1.1 upgrades and h2c on one port, as Go h2c servers do: what a
//!    Service declaring `kubernetes.io/h2c` loses.
//!
//! Each step prints the requests the translator emitted, what each client got
//! (curl's `http_version` and status), and what the backend saw. The backends
//! run in-process and decode just enough HTTP/2 (frames, HPACK without
//! Huffman, which Sōzu 2.3.2 never sends) to report the method and path; the
//! h2c-only one closes any connection that does not open with the HTTP/2
//! preface, as grpc-go does.
//!
//! Runs against a local `sozu start` with HTTP and HTTPS listeners, and needs
//! a `curl` built with HTTP/2. Env: `SOZU_SOCK`, `PROBE_HTTP` / `PROBE_HTTPS`
//! (the listener addresses as Sōzu declares them), `PROBE_DIAL_HTTP` /
//! `PROBE_DIAL_HTTPS` (where clients connect), `PROBE_BACKEND_H2C` /
//! `PROBE_BACKEND_DUAL` / `PROBE_BACKEND_H1` (where the three backends
//! listen), `PROBE_CURL`, `PROBE_MAX_TIME` (curl's `--max-time`, seconds,
//! default 10).
//!
//! Example/probe code: `expect`/`anyhow` are fine here, this is not a
//! production path.

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use sozu_command_lib::proto::command::{request::RequestType, Request};
use sozu_gw_agent::SozuAgent;
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

/// The translator's test certificate; `app.example.com` is its only name.
const CERT: &str = include_str!("../../translator/tests/fixtures/cert_a.pem");
const KEY: &str = include_str!("../../translator/tests/fixtures/key_a.pem");
const APP_HOST: &str = "app.example.com";
const WS_HOST: &str = "ws.example.com";
const H1_HOST: &str = "h1.example.com";
const APP_CLUSTER: &str = "probe.app.80";
const WS_CLUSTER: &str = "probe.ws.80";
const H1_CLUSTER: &str = "probe.h1.80";
const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

type Log = Arc<Mutex<Vec<String>>>;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn addr(key: &str, default: &str) -> Result<SocketAddr> {
    let value = env_or(key, default);
    value
        .parse()
        .with_context(|| format!("{key}={value:?} is not an address"))
}

fn note(log: &Log, line: String) {
    log.lock().expect("log lock").push(line);
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Speaks {
    /// HTTP/2 with prior knowledge only (a grpc-go server).
    H2cOnly,
    /// h2c, plus HTTP/1.1 WebSocket upgrades on the same port.
    H2cAndUpgrade,
    /// HTTP/1.1 only: what a port wrongly declaring `kubernetes.io/h2c` has.
    Http1Only,
}

fn spawn_backend(at: SocketAddr, speaks: Speaks, name: &'static str, log: Log) -> Result<()> {
    let listener = TcpListener::bind(at).with_context(|| format!("bind {at}"))?;
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { continue };
            let log = log.clone();
            std::thread::spawn(move || {
                let _ = serve(stream, speaks, name, n + 1, &log);
            });
        }
    });
    Ok(())
}

fn serve(
    mut s: TcpStream,
    speaks: Speaks,
    name: &str,
    conn: usize,
    log: &Log,
) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut head = [0u8; 24];
    s.read_exact(&mut head)?;
    if &head == PREFACE {
        if speaks == Speaks::Http1Only {
            note(
                log,
                format!("{name} conn {conn}: HTTP/2 preface, closed (speaks HTTP/1.1 only)"),
            );
            return Ok(());
        }
        s.set_read_timeout(None)?;
        note(
            log,
            format!("{name} conn {conn}: HTTP/2 connection preface"),
        );
        return serve_h2(s, name, conn, log);
    }
    let mut buf = head.to_vec();
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
        let mut chunk = [0u8; 1024];
        let n = s.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let first = String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    let upgrade = text
        .lines()
        .any(|l| l.starts_with("upgrade:") && l.contains("websocket"));
    match speaks {
        Speaks::H2cAndUpgrade if upgrade => {
            note(
                log,
                format!(
                    "{name} conn {conn}: HTTP/1.1 {first:?} with Upgrade: websocket -> 101, echo"
                ),
            );
            s.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
            )?;
            let mut chunk = [0u8; 1024];
            loop {
                let n = s.read(&mut chunk)?;
                if n == 0 {
                    return Ok(());
                }
                s.write_all(&chunk[..n])?;
            }
        }
        Speaks::Http1Only => {
            note(
                log,
                format!("{name} conn {conn}: HTTP/1.1 {first:?} -> 200"),
            );
            let body = format!("{name} answered over HTTP/1.1\n");
            write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        _ => {
            note(
                log,
                format!("{name} conn {conn}: not HTTP/2, closed: {first:?}"),
            );
            Ok(())
        }
    }
}

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut out = Vec::with_capacity(9 + payload.len());
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

const END_STREAM: u8 = 0x1;
const ACK: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const PADDED: u8 = 0x8;
const PRIORITY: u8 = 0x20;

fn serve_h2(mut s: TcpStream, name: &str, conn: usize, log: &Log) -> std::io::Result<()> {
    s.write_all(&frame(4, 0, 0, &[]))?; // our SETTINGS: all defaults
    let mut hpack = Hpack::default();
    let mut awaiting_body: HashMap<u32, Vec<(String, String)>> = HashMap::new();
    let (mut block, mut block_stream, mut block_ends) = (Vec::new(), 0u32, false);
    loop {
        let mut h = [0u8; 9];
        if s.read_exact(&mut h).is_err() {
            return Ok(());
        }
        let len = u32::from_be_bytes([0, h[0], h[1], h[2]]) as usize;
        let (kind, flags) = (h[3], h[4]);
        let stream = u32::from_be_bytes([h[5], h[6], h[7], h[8]]) & 0x7fff_ffff;
        let mut payload = vec![0u8; len];
        s.read_exact(&mut payload)?;
        match kind {
            // DATA: a request body ends with END_STREAM.
            0 if flags & END_STREAM != 0 => {
                if let Some(headers) = awaiting_body.remove(&stream) {
                    respond(&mut s, name, conn, stream, &headers, log)?;
                }
            }
            // HEADERS, then CONTINUATION until END_HEADERS.
            1 | 9 => {
                let mut fragment = &payload[..];
                if kind == 1 {
                    (block_stream, block_ends) = (stream, flags & END_STREAM != 0);
                    block.clear();
                    if flags & PADDED != 0 && !fragment.is_empty() {
                        let pad = fragment[0] as usize;
                        fragment = &fragment[1..fragment.len().saturating_sub(pad)];
                    }
                    if flags & PRIORITY != 0 && fragment.len() >= 5 {
                        fragment = &fragment[5..];
                    }
                }
                block.extend_from_slice(fragment);
                if flags & END_HEADERS != 0 {
                    let headers = hpack.decode(&block);
                    if block_ends {
                        respond(&mut s, name, conn, block_stream, &headers, log)?;
                    } else {
                        awaiting_body.insert(block_stream, headers);
                    }
                }
            }
            4 if flags & ACK == 0 => s.write_all(&frame(4, ACK, 0, &[]))?,
            6 if flags & ACK == 0 => s.write_all(&frame(6, ACK, 0, &payload))?,
            7 => return Ok(()), // GOAWAY
            _ => {}
        }
    }
}

fn respond(
    s: &mut TcpStream,
    name: &str,
    conn: usize,
    stream: u32,
    headers: &[(String, String)],
    log: &Log,
) -> std::io::Result<()> {
    let get = |key: &str| {
        headers
            .iter()
            .find(|(n, _)| n == key)
            .map_or("-", |(_, v)| v.as_str())
    };
    let hop: Vec<&str> = headers
        .iter()
        .filter(|(n, _)| n == "upgrade" || n == "connection")
        .map(|(n, _)| n.as_str())
        .collect();
    note(
        log,
        format!(
            "{name} conn {conn} stream {stream}: HTTP/2.0 {} {} :scheme={} :authority={} \
             upgrade/connection headers: {}",
            get(":method"),
            get(":path"),
            get(":scheme"),
            get(":authority"),
            if hop.is_empty() {
                "none".to_string()
            } else {
                hop.join(",")
            },
        ),
    );
    let body = format!(
        "{name} answered over HTTP/2.0: {} {}\n",
        get(":method"),
        get(":path")
    );
    // `:status: 200` is static-table entry 8.
    s.write_all(&frame(1, END_HEADERS, stream, &[0x88]))?;
    s.write_all(&frame(0, END_STREAM, stream, body.as_bytes()))
}

/// RFC 7541 Appendix A.
const STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

/// An HPACK decoder for what Sōzu sends: every representation, the dynamic
/// table, no Huffman (Sōzu 2.3.2 `lib/src/protocol/mux/hpack/encoder.rs`:
/// "Strings are sent without Huffman coding"; one would print `<huffman>`).
#[derive(Default)]
struct Hpack {
    dynamic: VecDeque<(String, String)>,
    size: usize,
}

impl Hpack {
    fn entry(&self, index: usize) -> (String, String) {
        match index {
            1..=61 => {
                let (n, v) = STATIC_TABLE[index - 1];
                (n.to_string(), v.to_string())
            }
            _ => self
                .dynamic
                .get(index.wrapping_sub(62))
                .cloned()
                .unwrap_or_else(|| (format!("<bad index {index}>"), String::new())),
        }
    }

    fn insert(&mut self, field: (String, String)) {
        self.size += 32 + field.0.len() + field.1.len();
        self.dynamic.push_front(field);
        while self.size > 4096 {
            let Some((n, v)) = self.dynamic.pop_back() else {
                break;
            };
            self.size -= 32 + n.len() + v.len();
        }
    }

    fn decode(&mut self, block: &[u8]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < block.len() {
            let b = block[pos];
            if b & 0x80 != 0 {
                let index = integer(block, &mut pos, 7);
                out.push(self.entry(index));
            } else if b & 0xe0 == 0x20 {
                integer(block, &mut pos, 5); // dynamic table size update
            } else {
                let incremental = b & 0x40 != 0;
                let index = integer(block, &mut pos, if incremental { 6 } else { 4 });
                let name = if index == 0 {
                    string(block, &mut pos)
                } else {
                    self.entry(index).0
                };
                let field = (name, string(block, &mut pos));
                if incremental {
                    self.insert(field.clone());
                }
                out.push(field);
            }
        }
        out
    }
}

fn integer(buf: &[u8], pos: &mut usize, prefix: u32) -> usize {
    let mask = (1usize << prefix) - 1;
    let mut value = buf[*pos] as usize & mask;
    *pos += 1;
    if value < mask {
        return value;
    }
    let mut shift = 0;
    while *pos < buf.len() {
        let b = buf[*pos];
        *pos += 1;
        value += ((b & 0x7f) as usize) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            break;
        }
    }
    value
}

fn string(buf: &[u8], pos: &mut usize) -> String {
    if *pos >= buf.len() {
        return String::new();
    }
    let huffman = buf[*pos] & 0x80 != 0;
    let len = integer(buf, pos, 7);
    let end = (*pos + len).min(buf.len());
    let s = if huffman {
        "<huffman>".to_string()
    } else {
        String::from_utf8_lossy(&buf[*pos..end]).into_owned()
    };
    *pos = end;
    s
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

/// A response body on one line: Sōzu's own answers by their `<h1>`.
fn condense(body: &str) -> String {
    if let Some(start) = body.find("<h1>") {
        let rest = &body[start + 4..];
        return rest[..rest.find("</h1>").unwrap_or(rest.len())].to_string();
    }
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(120).collect()
}

fn curl(bin: &str, max_time: &str, label: &str, args: &[String]) {
    let output = Command::new(bin)
        .args(["-sS", "-k", "--max-time", max_time])
        .args(["-w", "\n%{http_version} %{http_code}"])
        .args(args)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(e) => {
            println!("  {label}: cannot run {bin}: {e}");
            return;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (body, meta) = stdout.rsplit_once('\n').unwrap_or(("", &stdout));
    let mut meta = meta.split_whitespace();
    let (version, status) = (meta.next().unwrap_or("?"), meta.next().unwrap_or("?"));
    let mut line = format!("  {label}: http_version={version} status={status}");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        line.push_str(&format!(
            " curl_exit={} ({})",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ));
    }
    let body = condense(body);
    if !body.is_empty() {
        line.push_str(&format!(" | {body}"));
    }
    println!("{line}");
}

/// A cleartext HTTP/1.1 WebSocket upgrade; on `101`, one message through the
/// tunnel.
fn websocket(dial: SocketAddr) -> Result<String> {
    let mut s = TcpStream::connect(dial).with_context(|| format!("connect {dial}"))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        s,
        "GET /ws HTTP/1.1\r\nHost: {WS_HOST}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = match s.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Ok("no response within 5 s".to_string());
            }
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let status = String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or("<no response>")
        .to_string();
    if !status.contains(" 101 ") {
        return Ok(format!("{status:?}, no tunnel"));
    }
    s.write_all(b"ping-through-tunnel")?;
    let n = s.read(&mut chunk)?;
    Ok(format!(
        "{status:?}, tunnel echoed {:?}",
        String::from_utf8_lossy(&chunk[..n])
    ))
}

// ---------------------------------------------------------------------------
// The model and the run
// ---------------------------------------------------------------------------

struct Addrs {
    http: SocketAddr,
    https: SocketAddr,
    h2c_backend: SocketAddr,
    dual_backend: SocketAddr,
    h1_backend: SocketAddr,
}

fn model(http2: bool, a: &Addrs) -> ir::Ir {
    let cluster = |id: &str| ir::Cluster {
        id: id.to_string(),
        load_balancing: ir::LbAlgorithm::RoundRobin,
        sticky_session: false,
        https_redirect: false,
        max_connections_per_ip: None,
        retry_after: None,
        http2,
    };
    let backend = |cluster_id: &str, address: SocketAddr| ir::Backend {
        cluster_id: cluster_id.to_string(),
        backend_id: format!("{cluster_id}#{address}"),
        address,
        weight: None,
        sticky_id: None,
    };
    let frontend = |hostname: &str, cluster_id: &str, tls: bool| ir::Frontend {
        hostname: hostname.to_string(),
        path: ir::PathMatch::Prefix("/".into()),
        method: None,
        cluster_id: Some(cluster_id.to_string()),
        tls,
        listener: if tls { a.https } else { a.http },
        filters: ir::FrontendFilters::default(),
    };
    ir::Ir {
        clusters: vec![
            cluster(APP_CLUSTER),
            cluster(WS_CLUSTER),
            cluster(H1_CLUSTER),
        ],
        backends: vec![
            backend(APP_CLUSTER, a.h2c_backend),
            backend(WS_CLUSTER, a.dual_backend),
            backend(H1_CLUSTER, a.h1_backend),
        ],
        frontends: vec![
            frontend(APP_HOST, APP_CLUSTER, false),
            frontend(APP_HOST, APP_CLUSTER, true),
            frontend(WS_HOST, WS_CLUSTER, false),
            frontend(H1_HOST, H1_CLUSTER, false),
        ],
        certificates: vec![ir::Certificate {
            listener: a.https,
            certificate: CERT.to_string(),
            chain: vec![],
            key: KEY.to_string(),
            names: vec![APP_HOST.to_string()],
        }],
        l4_frontends: vec![],
    }
}

/// One line per emitted request; an `AddCluster` with its `http2`.
fn describe(requests: &[Request]) -> Vec<String> {
    requests
        .iter()
        .map(|r| match &r.request_type {
            Some(RequestType::AddCluster(c)) => {
                format!("ADD_CLUSTER {} http2={:?}", c.cluster_id, c.http2)
            }
            _ => serde_json::to_value(r)
                .ok()
                .and_then(|v| {
                    v["request_type"]
                        .as_object()
                        .and_then(|o| o.keys().next().cloned())
                })
                .unwrap_or_else(|| "<unknown>".to_string()),
        })
        .collect()
}

fn main() -> Result<()> {
    let sock = env_or("SOZU_SOCK", "./sozu.sock");
    let a = Addrs {
        http: addr("PROBE_HTTP", "0.0.0.0:8080")?,
        https: addr("PROBE_HTTPS", "0.0.0.0:8443")?,
        h2c_backend: addr("PROBE_BACKEND_H2C", "127.0.0.1:9097")?,
        dual_backend: addr("PROBE_BACKEND_DUAL", "127.0.0.1:9098")?,
        h1_backend: addr("PROBE_BACKEND_H1", "127.0.0.1:9099")?,
    };
    let dial_http = addr("PROBE_DIAL_HTTP", "127.0.0.1:8080")?;
    let dial_https = addr("PROBE_DIAL_HTTPS", "127.0.0.1:8443")?;
    let curl_bin = env_or("PROBE_CURL", "curl");
    let max_time = env_or("PROBE_MAX_TIME", "10");

    let log: Log = Arc::new(Mutex::new(Vec::new()));
    spawn_backend(
        a.h2c_backend,
        Speaks::H2cOnly,
        "h2c-only backend",
        log.clone(),
    )?;
    spawn_backend(
        a.dual_backend,
        Speaks::H2cAndUpgrade,
        "h2c+upgrade backend",
        log.clone(),
    )?;
    spawn_backend(
        a.h1_backend,
        Speaks::Http1Only,
        "http1-only backend",
        log.clone(),
    )?;

    let resolve = |host: &str, dial: SocketAddr| format!("{host}:{}:{}", dial.port(), dial.ip());
    let https_url = |path: &str| format!("https://{APP_HOST}:{}{path}", dial_https.port());
    let http_url = |host: &str, path: &str| format!("http://{host}:{}{path}", dial_http.port());
    let flush_backend = || {
        std::thread::sleep(Duration::from_millis(200));
        for line in log.lock().expect("log lock").drain(..) {
            println!("    backend: {line}");
        }
    };
    let clients = |step: &str| {
        let cases: [(&str, Vec<String>); 5] = [
            (
                "HTTPS listener, HTTP/1.1 client",
                vec![
                    "--http1.1".into(),
                    "--resolve".into(),
                    resolve(APP_HOST, dial_https),
                    https_url(&format!("/{step}/https-h1")),
                ],
            ),
            (
                "HTTPS listener, HTTP/2 client (ALPN h2)",
                vec![
                    "--http2".into(),
                    "--resolve".into(),
                    resolve(APP_HOST, dial_https),
                    https_url(&format!("/{step}/https-h2")),
                ],
            ),
            (
                "HTTP listener, HTTP/1.1 client",
                vec![
                    "--http1.1".into(),
                    "--resolve".into(),
                    resolve(APP_HOST, dial_http),
                    http_url(APP_HOST, &format!("/{step}/http-h1")),
                ],
            ),
            (
                "HTTP listener, HTTP/2 prior knowledge (h2c)",
                vec![
                    "--http2-prior-knowledge".into(),
                    "--resolve".into(),
                    resolve(APP_HOST, dial_http),
                    http_url(APP_HOST, &format!("/{step}/http-h2c")),
                ],
            ),
            (
                "HTTP listener, HTTP/1.1 client to an HTTP/1.1-only backend",
                vec![
                    "--http1.1".into(),
                    "--resolve".into(),
                    resolve(H1_HOST, dial_http),
                    http_url(H1_HOST, &format!("/{step}/h1-only")),
                ],
            ),
        ];
        for (label, args) in cases {
            curl(&curl_bin, &max_time, label, &args);
            flush_backend();
        }
        match websocket(dial_http) {
            Ok(outcome) => println!("  HTTP listener, WebSocket upgrade ({WS_HOST}): {outcome}"),
            Err(e) => println!("  HTTP listener, WebSocket upgrade ({WS_HOST}): error {e:#}"),
        }
        flush_backend();
    };

    let mut agent = SozuAgent::new(sock);
    let off = model(false, &a);
    let on = model(true, &a);
    let steps: [(&str, &str, &ir::Ir, &ir::Ir); 3] = [
        (
            "unset",
            "http2 unset: what every cluster got before appProtocol was read",
            &ir::Ir::default(),
            &off,
        ),
        (
            "set",
            "http2 = true: a Service port declaring kubernetes.io/h2c",
            &off,
            &on,
        ),
        (
            "unset-again",
            "http2 unset again: the appProtocol removed from the Service port",
            &on,
            &off,
        ),
    ];
    println!("== h2c backend probe ==");
    for (step, title, from, to) in steps {
        let requests = tr::reconcile(from, to).context("translate")?;
        println!("\n## {title}");
        println!("emitted: {}", describe(&requests).join(", "));
        agent
            .apply(&requests)
            .with_context(|| format!("apply {step}"))?;
        clients(step);
    }
    let cleanup = tr::reconcile(&off, &ir::Ir::default()).context("translate cleanup")?;
    agent.apply(&cleanup).context("cleanup")?;
    println!("\ncleanup: {} requests applied", cleanup.len());
    Ok(())
}
