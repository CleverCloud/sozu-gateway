//! Measurement probe: what Sōzu 2.2.0 actually does with `rewrite_host` /
//! `rewrite_path`, on the forwarding path and under each redirect policy.
//!
//! Two questions the project has been answering from the proto's doc comments
//! rather than from the wire, and both gate a user-visible feature:
//!
//!  1. **URLRewrite, path only.** `URLRewrite` is reported unsupported because a
//!     literal Gateway rewrite made the route 408 — but that was measured with
//!     `rewrite_host`, whose doc says it rewrites *the backend authority*, so
//!     the proxy dialled the rewritten host and timed out. `rewrite_path` alone
//!     (Gateway's `ReplaceFullPath`) is a different field and was never probed.
//!
//!  2. **RequestRedirect with a host/path target.** The proto documents building
//!     `Location` from `redirect_scheme` plus the `rewrite_*` fields — but only
//!     under `PERMANENT`. `FOUND` and `PERMANENT_REDIRECT` say nothing, and
//!     Gateway API's *default* `statusCode` is 302 → `FOUND`. If `FOUND` ignores
//!     `rewrite_host`, a redirect would answer with a `Location` pointing at the
//!     request itself: the self-redirect loop the current fail-closed branch
//!     exists to prevent. All three policies are therefore probed.
//!
//! It also checks the template grammar (`$HOST[n]` / `$PATH[n]`): a literal
//! value containing `$` may be reinterpreted, which would need escaping or a
//! Problem before any of this could be wired.
//!
//! **Self-contained.** It runs its own echo backend in-process, programs its own
//! cluster/backends/frontends over the command socket, and drives raw HTTP/1.1
//! over TCP — so it needs nothing but a live Sōzu with an HTTP listener. Run it
//! from a container that mounts the command socket:
//!
//! ```sh
//! kubectl exec -n sozu-system deploy/sozu-gateway -c controller -- \
//!   /usr/local/bin/rewrite_redirect_probe
//! ```
//!
//! Env: `SOZU_SOCK` (default `/run/sozu/sozu.sock`), `PROBE_HTTP_BIND` (the
//! address Sōzu listens on, default `0.0.0.0:8080`), `PROBE_HTTP_DIAL` (where to
//! send the test requests, default `127.0.0.1:8080`), `PROBE_BACKEND` (the echo
//! backend this binary starts, default `127.0.0.1:9099`).
//!
//! Example/probe code: `expect`/`anyhow` are fine here, this is not a
//! production path.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result};
use sozu_command_lib::channel::Channel;
use sozu_command_lib::proto::command::{
    request::RequestType, AddBackend, Cluster, Header, HeaderPosition, PathRule, PathRuleKind,
    RedirectPolicy, RedirectScheme, Request, RequestHttpFrontend, Response, ResponseStatus,
    RulePosition,
};

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// One scenario: a hostname, the frontend fields under test, and what we want
/// to learn from the answer.
struct Case {
    host: &'static str,
    question: &'static str,
    redirect: RedirectPolicy,
    scheme: RedirectScheme,
    rewrite_host: Option<&'static str>,
    rewrite_path: Option<&'static str>,
    rewrite_port: Option<u32>,
    /// The frontend's path rule. Defaults to `Prefix "/"`; a case probing what
    /// `$PATH[n]` captures needs the *regex* rule a Kubernetes prefix compiles
    /// to, which is the only place capture groups come from.
    path: Option<(PathRuleKind, &'static str)>,
    /// Request target to send. Defaults to `/original`.
    target: Option<&'static str>,
    /// Header mutations on the frontend: (position, key, val). An empty `val`
    /// is Sōzu's documented delete.
    headers: &'static [(HeaderPosition, &'static str, &'static str)],
    /// Extra request headers to send, so a *pre-existing* header can be
    /// distinguished from one the proxy added.
    send: &'static [(&'static str, &'static str)],
}

const CASES: &[Case] = &[
    Case {
        host: "baseline.probe",
        question: "control: plain proxying, no rewrite — the backend must see /original",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: None,
        rewrite_path: None,
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[],
        send: &[],
    },
    Case {
        host: "path-only.probe",
        question: "URLRewrite ReplaceFullPath: rewrite_path alone, no rewrite_host — \
                   does the backend see /rewritten, and does the request complete?",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: None,
        rewrite_path: Some("/rewritten"),
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[],
        send: &[],
    },
    Case {
        host: "host-and-path.probe",
        question: "the known-bad shape, re-measured: rewrite_host rewrites the backend \
                   authority, so the proxy dials elsewhere (expected 408/504)",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: Some("elsewhere.probe"),
        rewrite_path: Some("/rewritten"),
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[],
        send: &[],
    },
    Case {
        host: "dollar-literal.probe",
        question: "template grammar: is a literal `$` in rewrite_path reinterpreted?",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: None,
        rewrite_path: Some("/lit$PATH[0]eral"),
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[],
        send: &[],
    },
    Case {
        host: "redirect-301.probe",
        question: "PERMANENT + rewrite_host/path: the documented case — what Location?",
        redirect: RedirectPolicy::Permanent,
        scheme: RedirectScheme::UseHttps,
        rewrite_host: Some("new.probe"),
        rewrite_path: Some("/new"),
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[],
        send: &[],
    },
    Case {
        host: "redirect-302.probe",
        question: "FOUND + rewrite_host/path: UNDOCUMENTED, and Gateway API's default \
                   statusCode. If Location echoes the request, the feature dies here",
        redirect: RedirectPolicy::Found,
        scheme: RedirectScheme::UseHttps,
        rewrite_host: Some("new.probe"),
        rewrite_path: Some("/new"),
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[],
        send: &[],
    },
    Case {
        host: "redirect-308.probe",
        question: "PERMANENT_REDIRECT + rewrite_host/path: UNDOCUMENTED — what Location?",
        redirect: RedirectPolicy::PermanentRedirect,
        scheme: RedirectScheme::UseHttps,
        rewrite_host: Some("new.probe"),
        rewrite_path: Some("/new"),
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[],
        send: &[],
    },
    Case {
        host: "dollar-bare.probe",
        question: "a literal `$` that starts no known token — passed through, or eaten?",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: None,
        rewrite_path: Some("/price$100"),
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[],
        send: &[],
    },
    Case {
        host: "query.probe",
        question: "does rewrite_path keep the query string? Gateway's ReplaceFullPath \
                   replaces the path and leaves the query alone",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: None,
        rewrite_path: Some("/rewritten"),
        rewrite_port: None,
        path: None,
        target: Some("/original?q=1&x=2"),
        headers: &[],
        send: &[],
    },
    Case {
        host: "prefix-capture.probe",
        question: "what does $PATH[1] capture on the regex a Kubernetes prefix compiles to? \
                   Its only group is the boundary `(/|?|$)`, so ReplacePrefixMatch has no \
                   remainder to graft on",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: None,
        rewrite_path: Some("/pfx[$PATH[1]]"),
        rewrite_port: None,
        path: Some((PathRuleKind::Regex, "^/foo(/|\\?|$)")),
        target: Some("/foo/bar/baz"),
        headers: &[],
        send: &[],
    },
    Case {
        host: "hdr-set-request.probe",
        question: "Gateway `set` must OVERWRITE. The proto says HeaderPosition mirrors HAProxy \
                   set-header, which replaces. Client sends X-Env: staging, frontend sets \
                   X-Env: prod — does the backend see one value or two?",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: None,
        rewrite_path: None,
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[(HeaderPosition::Request, "X-Env", "prod")],
        send: &[("X-Env", "staging")],
    },
    Case {
        host: "hdr-set-request-absent.probe",
        question: "same set, but the client sends no X-Env — the header must appear once",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: None,
        rewrite_path: None,
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[(HeaderPosition::Request, "X-Env", "prod")],
        send: &[],
    },
    Case {
        host: "hdr-delete-request.probe",
        question: "empty val is documented as delete-by-name (HAProxy del-header parity)",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: None,
        rewrite_path: None,
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[(HeaderPosition::Request, "X-Env", "")],
        send: &[("X-Env", "staging")],
    },
    Case {
        host: "hdr-set-response.probe",
        question: "the response side of the same question: the backend answers \
                   X-Served-By: backend and the frontend sets X-Served-By: sozu",
        redirect: RedirectPolicy::Forward,
        scheme: RedirectScheme::UseSame,
        rewrite_host: None,
        rewrite_path: None,
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[(HeaderPosition::Response, "X-Served-By", "sozu")],
        send: &[],
    },
    Case {
        host: "redirect-port.probe",
        question: "rewrite_port on a redirect: does Location carry an explicit port? \
                   Gateway's RequestRedirect has a `port` field",
        redirect: RedirectPolicy::Found,
        scheme: RedirectScheme::UseHttps,
        rewrite_host: Some("new.probe"),
        rewrite_path: Some("/new"),
        rewrite_port: Some(8443),
        path: None,
        target: None,
        headers: &[],
        send: &[],
    },
    Case {
        host: "redirect-302-scheme-only.probe",
        question: "FOUND with no rewrite: the shape we ship today, as a comparison point",
        redirect: RedirectPolicy::Found,
        scheme: RedirectScheme::UseHttps,
        rewrite_host: None,
        rewrite_path: None,
        rewrite_port: None,
        path: None,
        target: None,
        headers: &[],
        send: &[],
    },
];

/// A minimal HTTP/1.1 echo: answers 200 with the request line and Host header
/// in the body, so a rewritten path or authority is visible from the outside.
fn spawn_echo_backend(addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr).with_context(|| format!("bind echo backend {addr}"))?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut request_line = String::new();
                let mut host = String::new();
                // Every occurrence, in order: one X-Env means replaced, two
                // means appended, and that is the whole question.
                let mut seen: Vec<String> = Vec::new();
                let _ = reader.read_line(&mut request_line);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("host:") {
                        host = v.trim().to_string();
                    }
                    if lower.starts_with("x-env:") {
                        seen.push(line.trim().to_string());
                    }
                }
                let body = format!(
                    "request-line={} backend-host={} x-env=[{}]\n",
                    request_line.trim(),
                    host,
                    seen.join(" | ")
                );
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Served-By: backend\r\n\
                     Connection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            });
        }
    });
    Ok(())
}

fn apply(channel: &mut Channel<Request, Response>, label: &str, req: Request) -> Result<()> {
    channel.write_message(&req).context("write request")?;
    loop {
        let response: Response = channel.read_message().context("read response")?;
        match response.status() {
            ResponseStatus::Processing => continue,
            ResponseStatus::Ok => return Ok(()),
            ResponseStatus::Failure => {
                anyhow::bail!("{label} rejected by Sōzu: {}", response.message)
            }
        }
    }
}

/// Send one request through Sōzu and return the raw response (headers + body).
fn probe_request(dial: SocketAddr, host: &str, target: &str, send: &[(&str, &str)]) -> String {
    let mut stream = match TcpStream::connect(dial) {
        Ok(s) => s,
        Err(e) => return format!("<connect failed: {e}>"),
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let extra: String = send.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\
         User-Agent: sozu-probe\r\n{extra}\r\n"
    );
    if let Err(e) = stream.write_all(request.as_bytes()) {
        return format!("<write failed: {e}>");
    }
    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Ok(_) => String::from_utf8_lossy(&raw).to_string(),
        Err(e) => format!("<read failed: {e} (partial: {})>", raw.len()),
    }
}

/// The status line and, when present, the Location header and the body's first
/// line — the three things every question above is answered by.
fn digest(raw: &str) -> String {
    let mut status = "<no status line>".to_string();
    let mut location = None;
    let mut served_by: Vec<String> = Vec::new();
    let mut body = None;
    let mut in_body = false;
    for (i, line) in raw.lines().enumerate() {
        if i == 0 {
            status = line.trim().to_string();
            continue;
        }
        if in_body {
            if !line.trim().is_empty() && body.is_none() {
                body = Some(line.trim().to_string());
            }
            continue;
        }
        if line.trim().is_empty() {
            in_body = true;
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("location:") {
            location = Some(v.trim().to_string());
        }
        if lower.starts_with("x-served-by:") {
            served_by.push(line.trim().to_string());
        }
    }
    let mut out = status;
    if let Some(l) = location {
        out.push_str(&format!(" | Location: {l}"));
    }
    if !served_by.is_empty() {
        out.push_str(&format!(" | resp X-Served-By=[{}]", served_by.join(" ; ")));
    }
    if let Some(b) = body {
        out.push_str(&format!(" | body: {b}"));
    }
    out
}

fn main() -> Result<()> {
    let sock = env_or("SOZU_SOCK", "/run/sozu/sozu.sock");
    let http_bind: SocketAddr = env_or("PROBE_HTTP_BIND", "0.0.0.0:8080").parse()?;
    let http_dial: SocketAddr = env_or("PROBE_HTTP_DIAL", "127.0.0.1:8080").parse()?;
    let backend: SocketAddr = env_or("PROBE_BACKEND", "127.0.0.1:9099").parse()?;

    println!("== sozu rewrite/redirect probe ==");
    println!("socket={sock} listener={http_bind} dial={http_dial} backend={backend}\n");

    spawn_echo_backend(backend)?;

    let mut channel: Channel<Request, Response> =
        Channel::from_path(&sock, 16_384, 163_840).context("open command socket")?;
    channel.blocking().context("set blocking")?;

    let cluster_id = "probe-rewrite";
    apply(
        &mut channel,
        "AddCluster",
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_string(),
            ..Default::default()
        })
        .into(),
    )?;
    apply(
        &mut channel,
        "AddBackend",
        RequestType::AddBackend(AddBackend {
            cluster_id: cluster_id.to_string(),
            backend_id: format!("{cluster_id}-1"),
            address: backend.into(),
            ..Default::default()
        })
        .into(),
    )?;

    // A rejected frontend is an answer, not an abort: Sōzu validates the
    // rewrite templates at `AddHttpFrontend` time, so "the request never even
    // reaches the data plane" is one of the outcomes worth measuring — and, for
    // a controller applying a whole batch, the most dangerous one.
    let mut rejected: Vec<(&str, String)> = Vec::new();
    for case in CASES {
        let front = RequestHttpFrontend {
            cluster_id: Some(cluster_id.to_string()),
            address: http_bind.into(),
            hostname: case.host.to_string(),
            path: match case.path {
                Some((kind, value)) => PathRule {
                    kind: kind as i32,
                    value: value.to_string(),
                },
                None => PathRule {
                    kind: PathRuleKind::Prefix as i32,
                    value: "/".to_string(),
                },
            },
            position: RulePosition::Tree as i32,
            redirect: Some(case.redirect as i32),
            redirect_scheme: Some(case.scheme as i32),
            rewrite_host: case.rewrite_host.map(str::to_string),
            rewrite_path: case.rewrite_path.map(str::to_string),
            rewrite_port: case.rewrite_port,
            headers: case
                .headers
                .iter()
                .map(|(position, key, val)| Header {
                    position: *position as i32,
                    key: key.to_string(),
                    val: val.to_string(),
                })
                .collect(),
            ..Default::default()
        };
        if let Err(e) = apply(
            &mut channel,
            &format!("AddHttpFrontend({})", case.host),
            RequestType::AddHttpFrontend(front).into(),
        ) {
            rejected.push((case.host, e.to_string()));
        }
    }

    println!("frontends programmed; probing\n");
    for case in CASES {
        println!("--- {} ---", case.host);
        println!("Q: {}", case.question);
        println!(
            "   redirect={:?} scheme={:?} rewrite_host={:?} rewrite_path={:?} rewrite_port={:?}",
            case.redirect, case.scheme, case.rewrite_host, case.rewrite_path, case.rewrite_port
        );
        let target = case.target.unwrap_or("/original");
        if let Some((kind, value)) = case.path {
            println!("   path rule: {kind:?} {value:?}  request: {target}");
        }
        if let Some((_, why)) = rejected.iter().find(|(h, _)| *h == case.host) {
            println!("A: FRONTEND REJECTED — {why}\n");
            continue;
        }
        let raw = probe_request(http_dial, case.host, target, case.send);
        println!("A: {}\n", digest(&raw));
    }

    println!("== done ==");
    Ok(())
}
