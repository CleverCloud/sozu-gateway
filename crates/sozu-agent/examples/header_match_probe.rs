//! Do HTTPRoute header matches route the way the Gateway API says, once the
//! builder has mapped them onto Sōzu 2.3 header predicates and lifted the
//! routes that outrank them?
//!
//! The probe stages the HTTPRoutes of four Gateway API v1.6.1 conformance
//! tests — HTTPRouteHeaderMatching, HTTPRouteMatching, HTTPRouteMethodMatching
//! and HTTPRouteMatchingAcrossRoutes — each on its own listener, and runs them
//! through the real pipeline: builder → IR → translator `reconcile` → agent.
//! It then sends each test's requests, `PROBE_REPEAT` times over fresh
//! connections so both workers answer, and tallies which backend answered.
//! The conformance suite itself is not run (it also checks status); this is
//! its traffic, against the frontends we would program.
//!
//! Then the precedence shapes the builder must get right or refuse, on
//! three listeners of their own: a header match on the root `/` under a plain
//! root `/` with a method, a plain Exact `/` (with and without a method) and a
//! longer prefix; a header match on `/api` under a longer plain prefix and
//! under a plain `/api` with a method; and a header match on a route without
//! hostnames under a longer plain prefix. Each shape exists twice, its plain
//! route's cluster id sorting before (`v1`) and after (`v3`) the header
//! match's (`v2`), because a fresh batch adds frontends in cluster-id order.
//! A case the builder refuses (`HeaderMatchOutranksRoute`, or
//! `HeaderMatchOverlap` between two header matches) is marked: the request
//! the skipped header match alone would serve then gets the plain answer, a
//! header match it outranked or a 404, and none may reach the skipped match's
//! backend over a route that outranks it.
//! The refused shapes are also programmed by hand, as the builder would have
//! programmed them with their copies, on `f…` hostnames and two more
//! hostname-less listeners: those cases show what the refusal prevents.
//!
//! Rounds: first the IR without its header-matched frontends — what a build
//! that skips header matches programs — then the diff that adds them (the
//! upgrade path, appended after the plain ones), then the same requests after
//! each worker has been replaced with `UpgradeWorker`: the fork path of an
//! automatic restart, which replays the main process's state in route-key
//! order rather than in the order the batches arrived, then once more after
//! the whole IR is removed and re-added in one batch (a fresh start). The
//! outcomes that hold through all three orders are the durable ones. The
//! header-matched frontends are then diffed away again, on the removal verbs,
//! and the main process's listing checked.
//!
//! Last, it asks which request headers Sōzu has already set when it routes: a
//! raw `regex .+` predicate per header, on its own hostname, and a request
//! that does not carry the header. The builder refuses header matches on the
//! ones that match.
//!
//! Env: SOZU_SOCK (command socket); PROBE_LISTEN_BASE (Sōzu already listens
//! for HTTP on 0.0.0.0:<base>..<base+3> and <base+5>..<base+9>);
//! PROBE_BACKENDS (`v1=127.0.0.1:p1,v2=…,v3=…`, each answering
//! `backend=<name>`); PROBE_REPEAT (requests per case, default 12);
//! PROBE_MANAGED_ONLY=1 runs the header section alone (for a Sōzu started
//! with other listener options, `send_x_real_ip` or `sozu_id_header`).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::json;
use sozu_command_lib::channel::Channel;
use sozu_command_lib::proto::command::{
    request::RequestType, response_content::ContentType, AddBackend, Cluster, FrontendFilters,
    HeaderMatchKind, HeaderPredicate, PathRule, PathRuleKind, Request, RequestHttpFrontend,
    Response, ResponseStatus, RulePosition, RunState, Status,
};
use sozu_gw_agent::SozuAgent;
use sozu_gw_builder::{build, BuildConfig, ExposedPort, ExposedProtocol, Inputs};
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

const NAMESPACE: &str = "gateway-conformance-infra";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn from_json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T> {
    serde_json::from_value(v).context("object json")
}

/// The test listeners: (section name, advertised port, offset). Offset 4 is
/// the HTTPS exposure the builder requires, which nothing here uses.
const LISTENERS: [(&str, u16, u16); 9] = [
    ("hm", 81, 0),
    ("m", 82, 1),
    ("mm", 83, 2),
    ("mar", 84, 3),
    ("pr", 85, 5),
    ("pla", 86, 6),
    ("plz", 87, 7),
    ("pfa", 88, 8),
    ("pfz", 89, 9),
];

/// The HTTPRoutes of the four conformance tests (v1.6.1 manifests), each
/// attached to its own listener of one Gateway.
fn routes() -> Result<Vec<serde_json::Value>> {
    let backend = |name: &str| json!([{ "name": format!("infra-backend-{name}"), "port": 8080 }]);
    let header = |name: &str, value: &str| json!({ "name": name, "value": value });
    let route = |name: &str, created: &str, section: &str, hostnames: serde_json::Value, rules| {
        json!({
            "metadata": { "name": name, "namespace": NAMESPACE, "creationTimestamp": created },
            "spec": {
                "parentRefs": [{ "name": "same-namespace", "sectionName": section }],
                "hostnames": hostnames,
                "rules": rules
            }
        })
    };
    let prefix = |value: &str| json!({ "type": "PathPrefix", "value": value });
    Ok(vec![
        route(
            "header-matching",
            "2026-10-19T10:00:00Z",
            "hm",
            json!(null),
            json!([
                { "matches": [{ "headers": [header("version", "one")] }], "backendRefs": backend("v1") },
                { "matches": [{ "headers": [header("version", "two")] }], "backendRefs": backend("v2") },
                { "matches": [{ "headers": [header("version", "two"), header("color", "orange")] }],
                  "backendRefs": backend("v1") },
                { "matches": [{ "headers": [header("color", "blue")] }, { "headers": [header("color", "green")] }],
                  "backendRefs": backend("v1") },
                { "matches": [{ "headers": [header("color", "red")] }, { "headers": [header("color", "yellow")] }],
                  "backendRefs": backend("v2") }
            ]),
        ),
        route(
            "matching",
            "2026-10-19T10:00:00Z",
            "m",
            json!(null),
            json!([
                { "matches": [{ "path": prefix("/") }, { "headers": [header("version", "one")] }],
                  "backendRefs": backend("v1") },
                { "matches": [{ "path": prefix("/v2") }, { "headers": [header("version", "two")] }],
                  "backendRefs": backend("v2") }
            ]),
        ),
        route(
            "method-matching",
            "2026-10-19T10:00:00Z",
            "mm",
            json!(null),
            json!([
                { "matches": [{ "method": "POST" }], "backendRefs": backend("v1") },
                { "matches": [{ "method": "GET" }], "backendRefs": backend("v2") },
                { "matches": [{ "path": prefix("/path1"), "method": "GET" }], "backendRefs": backend("v1") },
                { "matches": [{ "headers": [header("version", "one")], "method": "PUT" }],
                  "backendRefs": backend("v2") },
                { "matches": [{ "path": prefix("/path2"), "headers": [header("version", "two")], "method": "POST" }],
                  "backendRefs": backend("v3") },
                { "matches": [
                    { "path": prefix("/path3"), "method": "PATCH" },
                    { "path": prefix("/path4"), "headers": [header("version", "three")], "method": "DELETE" }
                  ], "backendRefs": backend("v1") },
                { "matches": [{ "path": prefix("/path5") }], "backendRefs": backend("v1") },
                { "matches": [{ "method": "PATCH" }], "backendRefs": backend("v2") },
                { "matches": [{ "headers": [header("version", "four")] }], "backendRefs": backend("v3") }
            ]),
        ),
        route(
            "matching-part1",
            "2026-10-19T10:00:00Z",
            "mar",
            json!(["example.com", "example.net"]),
            json!([
                { "matches": [{ "path": prefix("/") }, { "headers": [header("version", "one")] }],
                  "backendRefs": backend("v1") }
            ]),
        ),
        route(
            "matching-part2",
            "2026-10-19T10:00:01Z",
            "mar",
            json!(["example.com"]),
            json!([
                { "matches": [{ "path": prefix("/v2") }, { "headers": [header("version", "two")] }],
                  "backendRefs": backend("v2") }
            ]),
        ),
    ]
    .into_iter()
    .chain(precedence_routes())
    .collect())
}

/// The precedence shapes: (name, plain match, header-matched match). Each
/// becomes two routes, `<name>a.test` with its plain route on `v1` and
/// `<name>z.test` on `v3`, the header match on `v2`.
fn precedence_routes() -> Vec<serde_json::Value> {
    let canary = json!([{ "name": "X-Canary", "value": "1" }]);
    let prefix = |value: &str| json!({ "type": "PathPrefix", "value": value });
    let exact = |value: &str| json!({ "type": "Exact", "value": value });
    let shapes = [
        // refused: two root prefixes, the method unread
        (
            "rm",
            json!({ "path": prefix("/"), "method": "GET" }),
            json!({ "headers": canary }),
        ),
        // refused: on the target `/` the root meets the Exact `/`
        (
            "re",
            json!({ "path": exact("/") }),
            json!({ "headers": canary }),
        ),
        // kept: an Exact `/` naming the method decides at once
        (
            "rg",
            json!({ "path": exact("/"), "method": "GET" }),
            json!({ "headers": canary }),
        ),
        // kept: a regex longer than `/` above the root
        (
            "rl",
            json!({ "path": prefix("/api") }),
            json!({ "headers": canary }),
        ),
        // refused: two method-less regexes
        (
            "np",
            json!({ "path": prefix("/api/v2") }),
            json!({ "path": prefix("/api"), "headers": canary }),
        ),
        // kept: the same path with the method decides at once
        (
            "nm",
            json!({ "path": prefix("/api"), "method": "GET" }),
            json!({ "path": prefix("/api"), "headers": canary }),
        ),
    ];
    let mut routes = Vec::new();
    for (name, plain, header_matched) in shapes {
        for (suffix, service) in [("a", "v1"), ("z", "v3")] {
            routes.push(json!({
                "metadata": { "name": format!("{name}{suffix}"), "namespace": NAMESPACE,
                    "creationTimestamp": "2026-10-19T10:00:00Z" },
                "spec": {
                    "parentRefs": [{ "name": "same-namespace", "sectionName": "pr" }],
                    "hostnames": [format!("{name}{suffix}.test")],
                    "rules": [
                        { "matches": [plain.clone()],
                          "backendRefs": [{ "name": format!("infra-backend-{service}"), "port": 8080 }] },
                        { "matches": [header_matched.clone()],
                          "backendRefs": [{ "name": "infra-backend-v2", "port": 8080 }] }
                    ]
                }
            }));
        }
    }
    // refused: a route without hostnames, in Sōzu's first-match POST list
    for (section, service) in [("pla", "v1"), ("plz", "v3")] {
        routes.push(json!({
            "metadata": { "name": format!("hostless-{section}"), "namespace": NAMESPACE,
                "creationTimestamp": "2026-10-19T10:00:00Z" },
            "spec": {
                "parentRefs": [{ "name": "same-namespace", "sectionName": section }],
                "rules": [
                    { "matches": [{ "path": prefix("/api") }],
                      "backendRefs": [{ "name": format!("infra-backend-{service}"), "port": 8080 }] },
                    { "matches": [{ "headers": canary }],
                      "backendRefs": [{ "name": "infra-backend-v2", "port": 8080 }] }
                ]
            }
        }));
    }
    routes
}

/// One request of a test and the backend the Gateway API sends it to (`404`
/// for none). `tie` marks a case decided between frontends of the same Sōzu
/// class, which only insertion order settles (#80); `refused` one only a
/// header match the builder skips (`HeaderMatchOutranksRoute`,
/// `HeaderMatchOverlap`) could serve;
/// `forced` one on the frontends the builder refuses, programmed anyway.
struct Case {
    test: &'static str,
    method: &'static str,
    host: &'static str,
    path: &'static str,
    headers: &'static [(&'static str, &'static str)],
    want: &'static str,
    tie: bool,
    refused: bool,
    forced: bool,
}

const fn case(
    test: &'static str,
    method: &'static str,
    host: &'static str,
    path: &'static str,
    headers: &'static [(&'static str, &'static str)],
    want: &'static str,
) -> Case {
    Case {
        test,
        method,
        host,
        path,
        headers,
        want,
        tie: false,
        refused: false,
        forced: false,
    }
}

const fn tie(c: Case) -> Case {
    Case { tie: true, ..c }
}

const fn refused(c: Case) -> Case {
    Case { refused: true, ..c }
}

const fn forced(c: Case) -> Case {
    Case { forced: true, ..c }
}

const CANARY: &[(&str, &str)] = &[("X-Canary", "1")];

/// The conformance requests (v1.6.1), plus two of ours marked `+`.
const CASES: &[Case] = &[
    case("hm", "GET", "gw.test", "/", &[("Version", "one")], "v1"),
    // On a route without hostnames, `version: two` loses to `version: two` +
    // `color: orange` (more header matches) and `color: red`/`yellow` to
    // `version: one` (rule order), each to another backend: refused
    // (`HeaderMatchOverlap`). `version: two` + `color: blue` then reaches the
    // `color: blue` match it outranked.
    refused(case(
        "hm",
        "GET",
        "gw.test",
        "/",
        &[("Version", "two")],
        "v2",
    )),
    case(
        "hm",
        "GET",
        "gw.test",
        "/",
        &[("Version", "two"), ("Color", "orange")],
        "v1",
    ),
    refused(case(
        "hm",
        "GET",
        "gw.test",
        "/",
        &[("Version", "two"), ("Color", "blue")],
        "v2",
    )),
    case("hm", "GET", "gw.test", "/", &[("Color", "orange")], "404"),
    case(
        "hm",
        "GET",
        "gw.test",
        "/",
        &[("Some-Other-Header", "one")],
        "404",
    ),
    case("hm", "GET", "gw.test", "/", &[("Color", "blue")], "v1"),
    case("hm", "GET", "gw.test", "/", &[("Color", "green")], "v1"),
    refused(case("hm", "GET", "gw.test", "/", &[("Color", "red")], "v2")),
    refused(case(
        "hm",
        "GET",
        "gw.test",
        "/",
        &[("Color", "yellow")],
        "v2",
    )),
    case("hm", "GET", "gw.test", "/", &[("Color", "purple")], "404"),
    case("m", "GET", "gw.test", "/", &[], "v1"),
    case("m", "GET", "gw.test", "/example", &[], "v1"),
    case("m", "GET", "gw.test", "/", &[("Version", "one")], "v1"),
    tie(case("m", "GET", "gw.test", "/v2", &[], "v2")),
    tie(case("m", "GET", "gw.test", "/v2/example", &[], "v2")),
    // Both header matches sit under the plain `/v2` on a route without
    // hostnames: `version: one`'s copy of it goes to another backend, refused;
    // `version: two`'s to its own, so it is kept.
    case("m", "GET", "gw.test", "/", &[("Version", "two")], "v2"),
    tie(case("m", "GET", "gw.test", "/v2/", &[], "v2")),
    case("m", "GET", "gw.test", "/v2example", &[], "v1"),
    case("m", "GET", "gw.test", "/foo/v2/example", &[], "v1"),
    tie(case(
        "m+",
        "GET",
        "gw.test",
        "/v2",
        &[("Version", "one")],
        "v2",
    )),
    case("mm", "POST", "gw.test", "/", &[], "v1"),
    case("mm", "GET", "gw.test", "/", &[], "v2"),
    case("mm", "HEAD", "gw.test", "/", &[], "404"),
    tie(case("mm", "GET", "gw.test", "/path1", &[], "v1")),
    // Under the plain `/path5` on a route without hostnames: refused, like
    // `version: four` under the root routes with a method.
    refused(case(
        "mm",
        "PUT",
        "gw.test",
        "/",
        &[("version", "one")],
        "v2",
    )),
    case(
        "mm",
        "POST",
        "gw.test",
        "/path2",
        &[("version", "two")],
        "v3",
    ),
    tie(case("mm", "PATCH", "gw.test", "/path3", &[], "v1")),
    case(
        "mm",
        "DELETE",
        "gw.test",
        "/path4",
        &[("version", "three")],
        "v1",
    ),
    case("mm", "PUT", "gw.test", "/", &[], "404"),
    case("mm", "DELETE", "gw.test", "/path4", &[], "404"),
    tie(case("mm", "PATCH", "gw.test", "/path5", &[], "v1")),
    case("mm", "PATCH", "gw.test", "/", &[("version", "four")], "v2"),
    case("mar", "GET", "example.com", "/", &[], "v1"),
    case("mar", "GET", "example.com", "/example", &[], "v1"),
    case("mar", "GET", "example.net", "/example", &[], "v1"),
    case(
        "mar",
        "GET",
        "example.com",
        "/example",
        &[("Version", "one")],
        "v1",
    ),
    case("mar", "GET", "example.com", "/v2", &[], "v2"),
    case("mar", "GET", "example.net", "/v2", &[], "v1"),
    case("mar", "GET", "example.com", "/v2/example", &[], "v2"),
    case(
        "mar",
        "GET",
        "example.com",
        "/",
        &[("Version", "two")],
        "v2",
    ),
    case(
        "mar+",
        "GET",
        "example.com",
        "/v2",
        &[("Version", "one")],
        "v2",
    ),
    // The precedence shapes; `a` hosts' plain route is v1, `z` hosts' v3.
    case("pr", "GET", "rma.test", "/", CANARY, "v1"),
    case("pr", "GET", "rmz.test", "/", CANARY, "v3"),
    refused(case("pr", "POST", "rma.test", "/", CANARY, "v2")),
    case("pr", "GET", "rea.test", "/", CANARY, "v1"),
    case("pr", "GET", "rez.test", "/", CANARY, "v3"),
    case("pr", "GET", "rea.test", "/?q=1", CANARY, "v1"),
    refused(case("pr", "GET", "rea.test", "/x", CANARY, "v2")),
    case("pr", "GET", "rga.test", "/", CANARY, "v1"),
    case("pr", "GET", "rgz.test", "/", CANARY, "v3"),
    case("pr", "POST", "rga.test", "/", CANARY, "v2"),
    case("pr", "GET", "rgz.test", "/x", CANARY, "v2"),
    case("pr", "GET", "rga.test", "/", &[], "v1"),
    case("pr", "GET", "rla.test", "/api", CANARY, "v1"),
    case("pr", "GET", "rlz.test", "/api/x", CANARY, "v3"),
    case("pr", "GET", "rla.test", "/", CANARY, "v2"),
    case("pr", "GET", "rlz.test", "/apix", CANARY, "v2"),
    case("pr", "GET", "npa.test", "/api/v2", CANARY, "v1"),
    case("pr", "GET", "npz.test", "/api/v2", CANARY, "v3"),
    refused(case("pr", "GET", "npa.test", "/api", CANARY, "v2")),
    case("pr", "GET", "nma.test", "/api", CANARY, "v1"),
    case("pr", "GET", "nmz.test", "/api", CANARY, "v3"),
    case("pr", "POST", "nma.test", "/api", CANARY, "v2"),
    case("pla", "GET", "any.test", "/api", CANARY, "v1"),
    case("plz", "GET", "any.test", "/api", CANARY, "v3"),
    refused(case("pla", "GET", "any.test", "/", CANARY, "v2")),
    // The refused shapes with the header match and its copy programmed anyway
    // (see `forced_frontends`): the plain route must win each of these.
    forced(case("pr", "GET", "frma.test", "/", CANARY, "v1")),
    forced(case("pr", "GET", "frmz.test", "/", CANARY, "v3")),
    forced(case("pr", "GET", "frea.test", "/", CANARY, "v1")),
    forced(case("pr", "GET", "frez.test", "/", CANARY, "v3")),
    forced(case("pr", "GET", "fnpa.test", "/api/v2", CANARY, "v1")),
    forced(case("pr", "GET", "fnpz.test", "/api/v2", CANARY, "v3")),
    forced(case("pfa", "GET", "any.test", "/api", CANARY, "v1")),
    forced(case("pfz", "GET", "any.test", "/api", CANARY, "v3")),
];

/// Send one HTTP/1.1 request on a fresh connection; `backend=<name>` → name,
/// anything else → the status code.
fn send(port: u16, method: &str, host: &str, path: &str, headers: &[(&str, &str)]) -> String {
    let attempt = || -> Result<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes())?;
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let response = String::from_utf8_lossy(&response);
        let status = response
            .split_whitespace()
            .nth(1)
            .unwrap_or("none")
            .to_string();
        Ok(match response.split("\r\n\r\n").nth(1) {
            Some(body) if status == "200" && body.starts_with("backend=") => {
                body.trim_start_matches("backend=").trim().to_string()
            }
            _ => status,
        })
    };
    attempt().unwrap_or_else(|e| format!("error({e})"))
}

/// Run every case `repeat` times; print one line per case and the totals.
fn round(label: &str, base: u16, repeat: usize) {
    println!("== {label}");
    let (mut pass, mut split, mut fail) = (0, 0, 0);
    for c in CASES {
        let offset = LISTENERS
            .iter()
            .find(|(section, _, _)| c.test.trim_end_matches('+') == *section)
            .map(|(_, _, offset)| *offset)
            .expect("listener");
        let mut tally: BTreeMap<String, usize> = BTreeMap::new();
        for _ in 0..repeat {
            *tally
                .entry(send(base + offset, c.method, c.host, c.path, c.headers))
                .or_default() += 1;
        }
        let verdict = match (tally.get(c.want), tally.len()) {
            (Some(_), 1) => {
                pass += 1;
                "pass"
            }
            (Some(_), _) => {
                split += 1;
                "SPLIT"
            }
            _ => {
                fail += 1;
                "FAIL"
            }
        };
        let headers: Vec<String> = c.headers.iter().map(|(n, v)| format!("{n}: {v}")).collect();
        let got: Vec<String> = tally.iter().map(|(k, v)| format!("{k}={v}")).collect();
        println!(
            "{:<5} {:<6} {:<12} {:<16} {:<28} want={:<4} got {:<14} {}{}",
            c.test,
            c.method,
            c.host,
            c.path,
            headers.join(" + "),
            c.want,
            got.join(","),
            verdict,
            if c.tie {
                "  (same-class tie, #80)"
            } else if c.refused {
                "  (header match skipped by the builder)"
            } else if c.forced {
                "  (refused by the builder, programmed anyway)"
            } else {
                ""
            }
        );
    }
    println!(
        "-- {pass} pass, {split} split between workers, {fail} fail of {}",
        CASES.len()
    );
}

/// One request/response exchange on the raw command channel, for the verbs
/// the agent does not expose (Status with worker ids, UpgradeWorker).
fn raw(sock: &str, request: Request) -> Result<Response> {
    let mut channel: Channel<Request, Response> =
        Channel::from_path(sock, 1024 * 1024, 16 * 1024 * 1024)
            .with_context(|| format!("connect {sock}"))?;
    channel.blocking().context("blocking channel")?;
    channel.write_message(&request).context("write")?;
    loop {
        let response = channel.read_message().context("read")?;
        if response.status != ResponseStatus::Processing as i32 {
            return Ok(response);
        }
    }
}

/// Diff `from` → `to`, print the verbs of the batch, and apply it.
fn apply(agent: &mut SozuAgent, label: &str, from: &ir::Ir, to: &ir::Ir) -> Result<()> {
    let requests = tr::reconcile(from, to).context("translate")?;
    let mut verbs: BTreeMap<String, usize> = BTreeMap::new();
    for request in &requests {
        let verb = serde_json::to_value(request)?
            .get("request_type")
            .and_then(|t| t.as_object())
            .and_then(|t| t.keys().next().cloned())
            .unwrap_or_default();
        *verbs.entry(verb).or_default() += 1;
    }
    agent.apply(&requests).context("apply the batch")?;
    println!("applied {label}: {} requests {verbs:?}", requests.len());
    Ok(())
}

fn worker_ids(sock: &str) -> Result<Vec<u32>> {
    let response = raw(sock, RequestType::Status(Status {}).into())?;
    match response.content.and_then(|c| c.content_type) {
        Some(ContentType::Workers(infos)) => Ok(infos
            .vec
            .iter()
            .filter(|w| w.run_state == RunState::Running as i32)
            .map(|w| w.id)
            .collect()),
        _ => bail!("no worker list: {}", response.message),
    }
}

fn main() -> Result<()> {
    let sock = env_or("SOZU_SOCK", "./sozu.sock");
    let base: u16 = env_or("PROBE_LISTEN_BASE", "28781").parse()?;
    let repeat: usize = env_or("PROBE_REPEAT", "12").parse()?;
    let backends: BTreeMap<String, SocketAddr> = env_or(
        "PROBE_BACKENDS",
        "v1=127.0.0.1:28791,v2=127.0.0.1:28792,v3=127.0.0.1:28793",
    )
    .split(',')
    .map(|pair| {
        let (name, addr) = pair.split_once('=').context("name=addr")?;
        Ok((name.to_string(), addr.parse()?))
    })
    .collect::<Result<_>>()?;

    // ---- the cluster objects, as the controller would hand them over ----
    let mut inputs = Inputs {
        gateway_classes: vec![Arc::new(from_json(json!({
            "metadata": { "name": "gateway-conformance" },
            "spec": { "controllerName": "sozu.io/gateway-controller" }
        }))?)],
        gateways: vec![Arc::new(from_json(json!({
            "metadata": { "name": "same-namespace", "namespace": NAMESPACE },
            "spec": { "gatewayClassName": "gateway-conformance", "listeners":
                LISTENERS.iter().map(|(name, port, _)|
                    json!({ "name": name, "protocol": "HTTP", "port": port })).collect::<Vec<_>>() }
        }))?)],
        http_routes: routes()?
            .into_iter()
            .map(|r| from_json(r).map(Arc::new))
            .collect::<Result<_>>()?,
        ..Default::default()
    };
    for (name, addr) in &backends {
        let service = format!("infra-backend-{name}");
        inputs.services.push(Arc::new(from_json(json!({
            "metadata": { "name": service, "namespace": NAMESPACE },
            "spec": { "ports": [{ "name": "http", "port": 8080 }] }
        }))?));
        inputs.endpointslices.push(Arc::new(from_json(json!({
            "metadata": { "name": format!("{service}-1"), "namespace": NAMESPACE,
                "labels": { "kubernetes.io/service-name": service } },
            "addressType": "IPv4",
            "ports": [{ "name": "http", "port": addr.port() }],
            "endpoints": [{ "addresses": [addr.ip().to_string()], "conditions": { "ready": true } }]
        }))?));
    }
    let mut exposure: Vec<ExposedPort> = LISTENERS
        .iter()
        .map(|(name, port, offset)| ExposedPort {
            name: name.to_string(),
            port: *port,
            bind: base + offset,
            protocol: ExposedProtocol::Http,
            transport: None,
            owner: None,
        })
        .collect();
    exposure.push(ExposedPort {
        name: "https".to_string(),
        port: 443,
        bind: base + 4,
        protocol: ExposedProtocol::Https,
        transport: None,
        owner: None,
    });
    let config = BuildConfig {
        controller_name: "sozu.io/gateway-controller".to_string(),
        exposure,
        ..Default::default()
    };

    let mut agent = SozuAgent::new(sock.clone());
    if env_or("PROBE_MANAGED_ONLY", "") != "1" {
        conformance(&mut agent, &sock, &config, &inputs, base, repeat)?;
    }

    // ---- which headers has Sōzu already set when it routes? ----
    managed_headers(&mut agent, &backends, base)?;
    println!("== done");
    Ok(())
}

/// The conformance-shaped routes, through the pipeline, in rounds.
fn conformance(
    agent: &mut SozuAgent,
    sock: &str,
    config: &BuildConfig,
    inputs: &Inputs,
    base: u16,
    repeat: usize,
) -> Result<()> {
    println!("== header match probe: HTTPRoutes -> builder -> translator -> agent -> Sōzu");
    let mut out = build(config, inputs);
    for r in &out.routes {
        for p in &r.parents {
            println!(
                "route {}: accepted={} ({}) problems={:?}",
                r.name, p.accepted, p.accepted_reason, p.problems
            );
        }
    }
    let predicated = out
        .ir
        .frontends
        .iter()
        .filter(|f| !f.header_matches.is_empty())
        .count();
    println!(
        "ir: {} frontends, {} with header matches (lifted copies included)",
        out.ir.frontends.len(),
        predicated
    );
    let section_of = |f: &ir::Frontend| {
        LISTENERS
            .iter()
            .find(|(_, _, offset)| f.listener.port() == base + offset)
            .map_or("?", |(section, _, _)| *section)
    };
    out.ir.frontends.extend(forced_frontends(base));
    for f in &out.ir.frontends {
        if matches!(section_of(f), "pr" | "pla" | "plz" | "pfa" | "pfz") {
            println!(
                "  {:<4} {:<44} {}",
                section_of(f),
                f.cluster_id.as_deref().unwrap_or("-"),
                f.sozu_route_key().key
            );
        }
    }
    // What a build that skips header matches programs: the same IR without
    // its header-matched frontends (lifted copies included).
    let without = ir::Ir {
        frontends: out
            .ir
            .frontends
            .iter()
            .filter(|f| f.header_matches.is_empty())
            .cloned()
            .collect(),
        ..out.ir.clone()
    };
    apply(
        agent,
        "without header matches",
        &ir::Ir::default(),
        &without,
    )?;
    println!("workers: {:?}", worker_ids(sock)?);
    round(
        "round 0: header matches skipped (what the builder did before)",
        base,
        repeat,
    );

    apply(agent, "header matches added", &without, &out.ir)?;
    round(
        "round 1: header matches added by a diff, both workers fed in batch order",
        base,
        repeat,
    );

    for (i, id) in worker_ids(sock)?.into_iter().enumerate() {
        let response = raw(sock, RequestType::UpgradeWorker(id).into())?;
        println!(
            "UpgradeWorker {id}: status={} {}",
            response.status, response.message
        );
        std::thread::sleep(Duration::from_secs(3));
        println!("workers: {:?}", worker_ids(sock)?);
        round(
            &format!(
                "round {}: {} worker(s) replaced, forked from the main process state",
                i + 2,
                i + 1
            ),
            base,
            repeat,
        );
    }

    apply(agent, "everything removed", &out.ir, &ir::Ir::default())?;
    apply(
        agent,
        "everything re-added in one batch",
        &ir::Ir::default(),
        &out.ir,
    )?;
    round(
        "round 4: the whole IR added in one batch, both workers fed in batch order",
        base,
        repeat,
    );

    apply(agent, "header matches removed", &out.ir, &without)?;
    let listed = raw(
        sock,
        RequestType::ListFrontends(FrontendFilters {
            http: true,
            ..FrontendFilters::default()
        })
        .into(),
    )?;
    if let Some(ContentType::FrontendList(listed)) = listed.content.and_then(|c| c.content_type) {
        let predicated = listed
            .http_frontends
            .iter()
            .filter(|f| !f.header_predicates.is_empty())
            .count();
        println!(
            "main process lists {} HTTP frontends, {predicated} with header predicates",
            listed.http_frontends.len()
        );
    }
    Ok(())
}

/// The shapes the builder refuses (`HeaderMatchOutranksRoute`), programmed
/// as it would have programmed them without the refusal: the plain route, the
/// header match on `v2`, and the copy of the plain route carrying the header
/// match's conditions. `a` hosts' plain route is on `v1`, `z` hosts' on `v3`.
fn forced_frontends(base: u16) -> Vec<ir::Frontend> {
    let listener = |offset: u16| SocketAddr::from(([0, 0, 0, 0], base + offset));
    let cluster = |service: &str| Some(format!("{NAMESPACE}.infra-backend-{service}.8080"));
    let canary = vec![ir::HeaderMatch {
        name: "x-canary".to_string(),
        kind: ir::HeaderMatchKind::Exact("1".to_string()),
    }];
    let prefix = |v: &str| ir::PathMatch::Prefix(v.to_string());
    let exact = |v: &str| ir::PathMatch::Exact(v.to_string());
    // (hostname stem, offset, plain path, plain method, header-matched path)
    let shapes = [
        ("frm", 5, prefix("/"), Some("GET"), prefix("/"), true),
        ("fre", 5, exact("/"), None, prefix("/"), true),
        ("fnp", 5, prefix("/api/v2"), None, prefix("/api"), true),
        ("", 8, prefix("/api"), None, prefix("/"), false),
    ];
    let mut frontends = Vec::new();
    for (stem, offset, plain_path, plain_method, header_path, hosted) in shapes {
        for (suffix, service, extra) in [("a", "v1", 0), ("z", "v3", 1)] {
            let (hostname, offset) = if hosted {
                (format!("{stem}{suffix}.test"), offset)
            } else {
                ("*".to_string(), offset + extra)
            };
            let plain = ir::Frontend {
                hostname: hostname.clone(),
                path: plain_path.clone(),
                method: plain_method.map(str::to_string),
                cluster_id: cluster(service),
                tls: false,
                listener: listener(offset),
                filters: ir::FrontendFilters::default(),
                header_matches: vec![],
            };
            frontends.push(ir::Frontend {
                header_matches: canary.clone(),
                ..plain.clone()
            });
            frontends.push(ir::Frontend {
                path: header_path.clone(),
                method: None,
                cluster_id: cluster("v2"),
                header_matches: canary.clone(),
                ..plain.clone()
            });
            frontends.push(plain);
        }
    }
    frontends
}

/// Which request headers Sōzu has set by the time it routes: one raw
/// `regex .+` predicate per header, each on its own hostname, and a request
/// that carries none of them.
fn managed_headers(
    agent: &mut SozuAgent,
    backends: &BTreeMap<String, SocketAddr>,
    base: u16,
) -> Result<()> {
    println!("== headers seen by the router when the client sends none (regex `.+` predicate)");
    let probe_cluster = "managed-probe";
    let (_, first) = backends.iter().next().context("a backend")?;
    let mut setup: Vec<Request> = vec![
        RequestType::AddCluster(Cluster {
            cluster_id: probe_cluster.to_string(),
            ..Cluster::default()
        })
        .into(),
        RequestType::AddBackend(AddBackend {
            cluster_id: probe_cluster.to_string(),
            backend_id: format!("{probe_cluster}-0"),
            address: (*first).into(),
            ..AddBackend::default()
        })
        .into(),
    ];
    let managed = [
        "x-forwarded-for",
        "x-forwarded-proto",
        "x-forwarded-port",
        "x-forwarded-host",
        "forwarded",
        "x-real-ip",
        "x-request-id",
        "sozu-id",
        "x-trace-id",
        "traceparent",
        "user-agent",
    ];
    for name in managed {
        let front = RequestHttpFrontend {
            cluster_id: Some(probe_cluster.to_string()),
            address: SocketAddr::from(([0, 0, 0, 0], base)).into(),
            hostname: format!("{name}.managed.test"),
            path: PathRule {
                kind: PathRuleKind::Prefix as i32,
                value: "/".to_string(),
            },
            position: RulePosition::Tree as i32,
            header_predicates: vec![HeaderPredicate {
                name: name.to_string(),
                kind: HeaderMatchKind::Regex as i32,
                value: Some(".+".to_string()),
            }],
            ..RequestHttpFrontend::default()
        };
        setup.push(RequestType::add_http_frontend(front).into());
    }
    agent.apply(&setup).context("managed-header frontends")?;
    for name in managed {
        let host = format!("{name}.managed.test");
        // `send` adds only Host and Connection; curl's User-Agent is absent.
        let got = send(base, "GET", &host, "/", &[]);
        let verdict = if got.starts_with('v') {
            "set by Sōzu before routing"
        } else {
            "absent at routing"
        };
        println!("{name:<18} no client header -> {got:<5} {verdict}");
    }
    Ok(())
}
