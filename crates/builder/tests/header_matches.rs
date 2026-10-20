//! HTTPRoute header matches → Sōzu 2.3 header predicates: the mapping, the
//! refusals, and the lifting that rebuilds Gateway API precedence.

use std::collections::BTreeSet;
use std::sync::Arc;

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use serde_json::json;

use sozu_gw_builder::{
    build, BuildConfig, BuildOutput, Inputs, Problem, RouteParentResult, SozuRequestHeaders,
    MAX_HEADER_MATCHES_PER_HOSTNAME, MAX_LIFTED_PER_HOSTNAME,
};
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute};
use sozu_gw_ir as ir;

fn from_json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("valid k8s object json")
}

const SERVICES: [&str; 4] = ["v1", "v2", "v3", "other"];

fn inputs(routes: Vec<HttpRoute>) -> Inputs {
    inputs_with(routes, &SERVICES)
}

/// The Gateway, its class, these routes and one ready endpoint per Service.
fn inputs_with(routes: Vec<HttpRoute>, services: &[&str]) -> Inputs {
    let class: GatewayClass = from_json(json!({
        "metadata": { "name": "sozu" },
        "spec": { "controllerName": "sozu.io/gateway-controller" }
    }));
    let gateway: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu",
            "listeners": [{ "name": "http", "protocol": "HTTP", "port": 80 }] }
    }));
    let mut inputs = Inputs {
        gateway_classes: vec![Arc::new(class)],
        gateways: vec![Arc::new(gateway)],
        http_routes: routes.into_iter().map(Arc::new).collect(),
        ..Default::default()
    };
    for (i, name) in services.iter().enumerate() {
        let service: Service = from_json(json!({
            "metadata": { "name": name, "namespace": "demo" },
            "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
        }));
        let slice: EndpointSlice = from_json(json!({
            "metadata": { "name": format!("{name}-1"), "namespace": "demo",
                "labels": { "kubernetes.io/service-name": name } },
            "addressType": "IPv4",
            "ports": [{ "name": "http", "port": 8080 }],
            "endpoints": [{ "addresses": [format!("10.244.0.{}", i + 1)],
                "conditions": { "ready": true } }]
        }));
        inputs.services.push(Arc::new(service));
        inputs.endpointslices.push(Arc::new(slice));
    }
    inputs
}

/// An HTTPRoute on `app.example.com` with the given rules.
fn route(name: &str, created: &str, rules: serde_json::Value) -> HttpRoute {
    from_json(json!({
        "metadata": { "name": name, "namespace": "demo", "creationTimestamp": created },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": rules
        }
    }))
}

/// One rule to `service` with these matches.
fn rule(service: &str, matches: serde_json::Value) -> serde_json::Value {
    json!({ "matches": matches, "backendRefs": [{ "name": service, "port": 80 }] })
}

/// `f`'s result, and how long it kept this thread on a CPU. The bounded-work
/// tests time `build`, which runs on the calling thread, this way rather than
/// by the wall clock, which the tests the harness runs beside them inflate.
/// It does not make one machine's figure another's (see `build_bounded`).
/// Linux reports it in `/proc/thread-self/schedstat`; where it does not, this
/// falls back to the wall clock.
fn on_cpu<T>(f: impl FnOnce() -> T) -> (T, std::time::Duration) {
    fn ns() -> Option<u64> {
        std::fs::read_to_string("/proc/thread-self/schedstat")
            .ok()?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }
    let (before, started) = (ns(), std::time::Instant::now());
    let out = f();
    let spent = match (before, ns()) {
        (Some(before), Some(after)) => std::time::Duration::from_nanos(after - before),
        _ => started.elapsed(),
    };
    (out, spent)
}

/// The same routes with every header match removed.
fn without_header_matches(routes: &[HttpRoute]) -> Vec<HttpRoute> {
    routes
        .iter()
        .map(|route| {
            let mut value = serde_json::to_value(route).expect("serialisable");
            for rule in value
                .pointer_mut("/spec/rules")
                .and_then(|r| r.as_array_mut())
                .into_iter()
                .flatten()
            {
                for m in rule
                    .get_mut("matches")
                    .and_then(|m| m.as_array_mut())
                    .into_iter()
                    .flatten()
                {
                    if let Some(m) = m.as_object_mut() {
                        m.remove("headers");
                    }
                }
            }
            from_json(value)
        })
        .collect()
}

/// Builds `routes`, then `plain` (routes without header matches), and checks
/// the header matches cost at most `factor` times the CPU of the plain build
/// (plus 0.5 s for noise). The ratio is what the bounded-work tests
/// guard. An absolute bound is flaky on a slow runner and blind on a fast
/// one: the build of 8000 plain prefixes, each compiled as a regex, took
/// 18.8 s of CPU on a CI runner and 1.3 s on a workstation. The regressions
/// these tests exist for cost 30 times the plain build or more.
fn build_bounded(
    label: &str,
    routes: Vec<HttpRoute>,
    plain: Vec<HttpRoute>,
    factor: u32,
) -> BuildOutput {
    let plain = inputs(plain);
    let inputs = inputs(routes);
    let (out, with) = on_cpu(|| build(&BuildConfig::default(), &inputs));
    let (_, without) = on_cpu(|| build(&BuildConfig::default(), &plain));
    eprintln!("{label}: {with:?} with header matches, {without:?} without");
    assert!(
        with <= without * factor + std::time::Duration::from_millis(500),
        "{label}: {with:?} with header matches, {without:?} without"
    );
    out
}

fn build_routes(routes: Vec<HttpRoute>) -> BuildOutput {
    build(&BuildConfig::default(), &inputs(routes))
}

/// A frontend as one line: host, path (`P` prefix, `E` exact, `R` regex),
/// method, header matches (`=` exact, `~` regex) and target.
fn show(f: &ir::Frontend) -> String {
    let path = match &f.path {
        ir::PathMatch::Prefix(v) => format!("P{v}"),
        ir::PathMatch::Exact(v) => format!("E{v}"),
        ir::PathMatch::Regex(v) => format!("R{v}"),
    };
    let method = f
        .method
        .as_deref()
        .map(|m| format!(" {m}"))
        .unwrap_or_default();
    let headers = if f.header_matches.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = f
            .header_matches
            .iter()
            .map(|m| match &m.kind {
                ir::HeaderMatchKind::Exact(v) => format!("{}={v}", m.name),
                ir::HeaderMatchKind::Regex(v) => format!("{}~{v}", m.name),
            })
            .collect();
        format!(" [{}]", list.join(","))
    };
    let target = f.cluster_id.as_deref().unwrap_or("<redirect>");
    format!("{} {path}{method}{headers} -> {target}", f.hostname)
}

fn shown(out: &BuildOutput) -> Vec<String> {
    let mut lines: Vec<String> = out.ir.frontends.iter().map(show).collect();
    lines.sort();
    lines
}

fn sorted(lines: &[&str]) -> Vec<String> {
    let mut lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    lines.sort();
    lines
}

fn parent<'a>(out: &'a BuildOutput, name: &str) -> &'a RouteParentResult {
    &out.routes
        .iter()
        .find(|r| r.name == name)
        .expect("route result")
        .parents[0]
}

/// The IR must translate (one bad frontend fails every reconcile) and be a
/// fixed point of the diff.
fn assert_translates(out: &BuildOutput) {
    sozu_gw_translator::reconcile(&ir::Ir::default(), &out.ir).expect("the IR translates");
    assert!(sozu_gw_translator::reconcile(&out.ir, &out.ir)
        .expect("reconcile")
        .is_empty());
}

#[test]
fn exact_and_regex_header_matches_become_canonical_predicates() {
    // The Gateway API makes names case-insensitive and lets only the FIRST of
    // several entries naming one header count: the later `version` (here an
    // empty value that would otherwise be refused) is ignored.
    let out = build_routes(vec![route(
        "canary",
        "2026-01-01T00:00:00Z",
        json!([rule(
            "v2",
            json!([{ "headers": [
                { "name": "X-Env", "type": "RegularExpression", "value": "pre(prod|view)" },
                { "name": "Version", "value": "two" },
                { "name": "version", "type": "Exact", "value": "" }
            ] }])
        )]),
    )]);
    assert_eq!(
        shown(&out),
        sorted(&["app.example.com P/ [version=two,x-env~pre(prod|view)] -> demo.v2.80"])
    );
    let parent = parent(&out, "canary");
    assert!(parent.accepted, "{:?}", parent.problems);
    assert!(parent.problems.is_empty(), "{:?}", parent.problems);
    assert_translates(&out);
}

/// Each header match is the only condition of one match, next to a plain
/// `/ok` match in the same rule: a refusal must skip exactly that match,
/// report it, and leave the route Accepted.
fn refusal_of(config: &BuildConfig, header: serde_json::Value) -> (Vec<String>, Vec<Problem>) {
    let out = build(
        config,
        &inputs(vec![route(
            "r",
            "2026-01-01T00:00:00Z",
            json!([rule(
                "v1",
                json!([
                    { "path": { "type": "PathPrefix", "value": "/ok" } },
                    { "headers": [header] }
                ])
            )]),
        )]),
    );
    let parent = parent(&out, "r");
    assert!(parent.accepted, "{:?}", parent.problems);
    assert_eq!(parent.accepted_reason, "Accepted");
    assert_translates(&out);
    (shown(&out), parent.problems.clone())
}

#[test]
fn header_matches_sozu_or_the_api_cannot_honour_skip_the_match_not_the_route() {
    let refused = [
        // Sōzu's own `validate_header_predicate`: Host and Cookie are not
        // headers it keeps, a name must be a token, an exact value cannot
        // carry a control byte.
        json!({ "name": "Host", "value": "app.example.com" }),
        json!({ "name": "cookie", "value": "a=b" }),
        json!({ "name": "bad name", "value": "x" }),
        json!({ "name": ":method", "value": "GET" }),
        json!({ "name": "x-a", "value": "a\u{1}b" }),
        json!({ "name": "x-a", "value": "a\nb" }),
        // Values no request can carry: Sōzu trims the request value.
        json!({ "name": "x-a", "value": "" }),
        json!({ "name": "x-a", "value": " padded" }),
        json!({ "name": "x-a", "value": "padded\t" }),
        // Regexes Sōzu compiles bare and wrapped, refusing either failure.
        json!({ "name": "x-a", "type": "RegularExpression", "value": "a)(b" }),
        json!({ "name": "x-a", "type": "RegularExpression", "value": "(?x)a#" }),
        json!({ "name": "x-a", "type": "RegularExpression", "value": "(?=a)" }),
        // Headers Sōzu writes before it routes (default listener options).
        json!({ "name": "X-Forwarded-For", "value": "1.2.3.4" }),
        json!({ "name": "forwarded", "value": "for=1.2.3.4" }),
        json!({ "name": "X-Forwarded-Proto", "value": "https" }),
        json!({ "name": "x-forwarded-port", "value": "443" }),
        json!({ "name": "X-Request-Id", "type": "RegularExpression", "value": ".+" }),
        json!({ "name": "sozu-id", "type": "RegularExpression", "value": ".+" }),
    ];
    for header in refused {
        let (frontends, problems) = refusal_of(&BuildConfig::default(), header.clone());
        assert_eq!(
            frontends,
            sorted(&["app.example.com P/ok -> demo.v1.80"]),
            "{header}"
        );
        let name = header["name"].as_str().expect("name");
        assert!(
            matches!(&problems[..], [Problem::HeaderMatchRefused { header, .. }] if header == name),
            "{header}: {problems:?}"
        );
    }
}

#[test]
fn header_matches_sozu_leaves_alone_are_programmed() {
    let accepted = [
        // A tab inside the value is a legal field value; so is UTF-8.
        json!({ "name": "x-a", "value": "a\tb" }),
        json!({ "name": "x-a", "value": "é" }),
        // With Sōzu's default options X-Real-IP and X-Forwarded-Host reach
        // the router as the client sent them (measured).
        json!({ "name": "X-Real-IP", "value": "1.2.3.4" }),
        json!({ "name": "X-Forwarded-Host", "value": "app.example.com" }),
        json!({ "name": "x-a", "type": "RegularExpression", "value": "^a$" }),
    ];
    for header in accepted {
        let (frontends, problems) = refusal_of(&BuildConfig::default(), header.clone());
        assert!(problems.is_empty(), "{header}: {problems:?}");
        // `/ok`, the header-matched root, and `/ok` lifted above it.
        assert_eq!(frontends.len(), 3, "{header}: {frontends:?}");
    }
}

#[test]
fn the_configured_correlation_header_and_x_real_ip_follow_sozus_options() {
    let config = BuildConfig {
        sozu_request_headers: SozuRequestHeaders {
            correlation_header: "X-Trace-Id".to_string(),
            x_real_ip: true,
        },
        ..Default::default()
    };
    for (name, refused) in [
        ("x-trace-id", true),
        ("x-real-ip", true),
        ("Sozu-Id", false),
    ] {
        let (_, problems) = refusal_of(&config, json!({ "name": name, "value": "x" }));
        assert_eq!(!problems.is_empty(), refused, "{name}: {problems:?}");
    }
}

#[test]
fn an_unknown_header_match_type_reads_unsupported_value() {
    let out = build_routes(vec![route(
        "r",
        "2026-01-01T00:00:00Z",
        json!([rule(
            "v1",
            json!([
                { "path": { "type": "PathPrefix", "value": "/ok" } },
                { "headers": [{ "name": "x-a", "type": "Contains", "value": "a" }] }
            ])
        )]),
    )]);
    assert_eq!(shown(&out), sorted(&["app.example.com P/ok -> demo.v1.80"]));
    let parent = parent(&out, "r");
    assert!(!parent.accepted);
    assert_eq!(parent.accepted_reason, "UnsupportedValue");
    assert!(matches!(
        &parent.problems[..],
        [Problem::HeaderMatchRefused { .. }]
    ));
}

#[test]
fn query_matches_stay_unsupported_and_are_reported_apart() {
    let out = build_routes(vec![route(
        "r",
        "2026-01-01T00:00:00Z",
        json!([rule(
            "v1",
            json!([
                { "path": { "type": "PathPrefix", "value": "/ok" } },
                { "queryParams": [{ "name": "a", "value": "b" }] },
                { "headers": [{ "name": "x-a", "value": "a" }],
                  "queryParams": [{ "name": "a", "value": "b" }] }
            ])
        )]),
    )]);
    assert_eq!(shown(&out), sorted(&["app.example.com P/ok -> demo.v1.80"]));
    let parent = parent(&out, "r");
    assert!(parent.accepted);
    assert_eq!(parent.problems, vec![Problem::QueryMatchUnsupported]);
    assert!(Problem::QueryMatchUnsupported
        .to_string()
        .contains("query parameter"));
}

/// HTTPRouteMatching's shape (Gateway API v1.6.1 conformance): a header
/// match on the root must not take `/v2` from the plain `/v2` route.
#[test]
fn a_longer_prefix_is_lifted_above_a_header_matched_root() {
    let out = build_routes(vec![route(
        "matching",
        "2026-01-01T00:00:00Z",
        json!([
            rule(
                "v1",
                json!([
                    { "path": { "type": "PathPrefix", "value": "/" } },
                    { "headers": [{ "name": "version", "value": "one" }] }
                ])
            ),
            rule(
                "v2",
                json!([
                    { "path": { "type": "PathPrefix", "value": "/v2" } },
                    { "headers": [{ "name": "version", "value": "two" }] }
                ])
            )
        ]),
    )]);
    assert_eq!(
        shown(&out),
        sorted(&[
            "app.example.com P/ -> demo.v1.80",
            "app.example.com P/ [version=one] -> demo.v1.80",
            "app.example.com P/ [version=two] -> demo.v2.80",
            "app.example.com P/v2 -> demo.v2.80",
            // lifted: `/v2` outranks both header-matched roots
            "app.example.com P/v2 [version=one] -> demo.v2.80",
            "app.example.com P/v2 [version=two] -> demo.v2.80",
        ])
    );
    assert!(parent(&out, "matching").problems.is_empty());
    assert_translates(&out);
}

/// HTTPRouteMatchingAcrossRoutes: the same, across two routes and two
/// hostnames; `/v2` exists on example.com only, so nothing is lifted on
/// example.net.
#[test]
fn lifting_stays_on_the_hostname_and_crosses_routes() {
    let part1: HttpRoute = from_json(json!({
        "metadata": { "name": "part1", "namespace": "demo",
            "creationTimestamp": "2026-01-01T00:00:00Z" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["example.com", "example.net"],
            "rules": [rule("v1", json!([
                { "path": { "type": "PathPrefix", "value": "/" } },
                { "headers": [{ "name": "version", "value": "one" }] }
            ]))]
        }
    }));
    let part2: HttpRoute = from_json(json!({
        "metadata": { "name": "part2", "namespace": "demo",
            "creationTimestamp": "2026-01-01T00:00:01Z" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["example.com"],
            "rules": [rule("v2", json!([
                { "path": { "type": "PathPrefix", "value": "/v2" } },
                { "headers": [{ "name": "version", "value": "two" }] }
            ]))]
        }
    }));
    let out = build_routes(vec![part2, part1]);
    assert_eq!(
        shown(&out),
        sorted(&[
            "example.com P/ -> demo.v1.80",
            "example.com P/ [version=one] -> demo.v1.80",
            "example.com P/ [version=two] -> demo.v2.80",
            "example.com P/v2 -> demo.v2.80",
            "example.com P/v2 [version=one] -> demo.v2.80",
            "example.com P/v2 [version=two] -> demo.v2.80",
            "example.net P/ -> demo.v1.80",
            "example.net P/ [version=one] -> demo.v1.80",
        ])
    );
    assert_translates(&out);
}

#[test]
fn an_exact_path_and_a_method_are_lifted_and_nothing_less_specific_is() {
    // Gateway API: Exact path > longest prefix > method > most header
    // matches. Every plain route here outranks the header-matched root (a
    // longer path, or the Exact `/` with a method), and plain `/api GET`
    // outranks the header match on `/api` (same path, a method). Nothing
    // outranks the other two header matches: `E/bar` is an Exact path above
    // the plain prefix `P/bar`; `P/bar/baz POST` is longer than every prefix
    // covering it. Every copy here wins in any order Sōzu may hold it: a
    // regex longer than `/` against the root prefix, or a regex naming the
    // method against a header match without one.
    let out = build_routes(vec![route(
        "r",
        "2026-01-01T00:00:00Z",
        json!([
            rule(
                "v1",
                json!([{ "path": { "type": "Exact", "value": "/foo" } }])
            ),
            rule(
                "v1",
                json!([{ "path": { "type": "Exact", "value": "/" }, "method": "GET" }])
            ),
            rule(
                "v1",
                json!([{ "path": { "type": "PathPrefix", "value": "/bar" } }])
            ),
            rule(
                "v1",
                json!([{ "path": { "type": "PathPrefix", "value": "/api" }, "method": "GET" }])
            ),
            rule(
                "v1",
                json!([{ "path": { "type": "PathPrefix", "value": "/elsewhere" } }])
            ),
            rule(
                "v3",
                json!([{ "headers": [{ "name": "version", "value": "four" }] }])
            ),
            rule(
                "v3",
                json!([{
                    "path": { "type": "Exact", "value": "/bar" },
                    "headers": [{ "name": "version", "value": "five" }]
                }])
            ),
            rule(
                "v3",
                json!([{
                    "path": { "type": "PathPrefix", "value": "/bar/baz" },
                    "method": "POST",
                    "headers": [{ "name": "version", "value": "six" }]
                }])
            ),
            rule(
                "v3",
                json!([{
                    "path": { "type": "PathPrefix", "value": "/api" },
                    "headers": [{ "name": "version", "value": "seven" }]
                }])
            ),
        ]),
    )]);
    assert_eq!(
        shown(&out),
        sorted(&[
            "app.example.com E/foo -> demo.v1.80",
            "app.example.com E/ GET -> demo.v1.80",
            "app.example.com P/bar -> demo.v1.80",
            "app.example.com P/api GET -> demo.v1.80",
            "app.example.com P/elsewhere -> demo.v1.80",
            "app.example.com P/ [version=four] -> demo.v3.80",
            "app.example.com E/bar [version=five] -> demo.v3.80",
            "app.example.com P/bar/baz POST [version=six] -> demo.v3.80",
            "app.example.com P/api [version=seven] -> demo.v3.80",
            // lifted above the header-matched root, which every plain route
            // here outranks on its path or method
            "app.example.com E/foo [version=four] -> demo.v1.80",
            "app.example.com E/ GET [version=four] -> demo.v1.80",
            "app.example.com P/bar [version=four] -> demo.v1.80",
            "app.example.com P/api GET [version=four] -> demo.v1.80",
            "app.example.com P/elsewhere [version=four] -> demo.v1.80",
            // lifted above the header match on `/api`
            "app.example.com P/api GET [version=seven] -> demo.v1.80",
        ])
    );
    assert!(parent(&out, "r").problems.is_empty());
    assert_translates(&out);
}

/// A route with these rules on `hostname`, or on none when it is `None`.
fn route_on(name: &str, hostname: Option<&str>, rules: serde_json::Value) -> HttpRoute {
    from_json(json!({
        "metadata": { "name": name, "namespace": "demo",
            "creationTimestamp": "2026-01-01T00:00:00Z" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": hostname.map(|h| vec![h]),
            "rules": rules
        }
    }))
}

/// Where a plain route outranks a header match and no copy of it can win in
/// every order Sōzu may hold the two, the header match is skipped on that
/// hostname and reported; the plain route and the route's status stay.
#[test]
fn a_header_match_no_copy_can_rank_below_is_skipped_and_reported() {
    let canary = json!([{ "name": "x-canary", "value": "1" }]);
    let prefix = |path: &str| json!({ "type": "PathPrefix", "value": path });
    let exact = |path: &str| json!({ "type": "Exact", "value": path });
    // (hostname, plain match, header-matched match, the plain route as
    // reported, a word of the reason)
    let cases = [
        // Two root prefixes: Sōzu ranks prefixes by length, not method, and
        // the later of two equal ones wins.
        (
            Some("app.example.com"),
            json!({ "path": prefix("/"), "method": "GET" }),
            json!({ "headers": canary }),
            "GET prefix /",
            "ignoring the method",
        ),
        // On the target `/` the root prefix meets the bar the Exact `/` sets.
        (
            Some("app.example.com"),
            json!({ "path": exact("/") }),
            json!({ "headers": canary }),
            "exact /",
            "target `/`",
        ),
        // Two method-less regexes: the later wins.
        (
            Some("app.example.com"),
            json!({ "path": prefix("/api/v2") }),
            json!({ "path": prefix("/api"), "headers": canary }),
            "prefix /api/v2",
            "method-less path regexes",
        ),
        (
            Some("app.example.com"),
            json!({ "path": exact("/api") }),
            json!({ "path": prefix("/api"), "headers": canary }),
            "exact /api",
            "method-less path regexes",
        ),
        // A header match naming a method lends it to its copies, and a regex
        // naming the method decides at once: two decide by the order.
        (
            Some("app.example.com"),
            json!({ "path": prefix("/api/v2") }),
            json!({ "path": prefix("/api"), "headers": canary, "method": "POST" }),
            "prefix /api/v2",
            "both name the method",
        ),
        (
            Some("app.example.com"),
            json!({ "path": prefix("/api/v2"), "method": "POST" }),
            json!({ "path": prefix("/api"), "headers": canary, "method": "POST" }),
            "POST prefix /api/v2",
            "both name the method",
        ),
        // A hostname-less route is in Sōzu's first-match POST list.
        (
            None,
            json!({ "path": prefix("/api") }),
            json!({ "headers": canary }),
            "prefix /api",
            "without hostnames",
        ),
        (
            None,
            json!({ "path": prefix("/api"), "method": "GET" }),
            json!({ "path": prefix("/api"), "headers": canary }),
            "GET prefix /api",
            "without hostnames",
        ),
    ];
    for (hostname, plain, header_matched, reported, reason) in cases {
        let out = build_routes(vec![route_on(
            "r",
            hostname,
            json!([
                rule("v1", json!([plain])),
                rule("v2", json!([header_matched]))
            ]),
        )]);
        let case = format!("{hostname:?}: {plain} under {header_matched}");
        let host = hostname.unwrap_or("*");
        assert!(
            out.ir.frontends.iter().all(|f| f.header_matches.is_empty()),
            "{case}: {:?}",
            shown(&out)
        );
        assert_eq!(out.ir.frontends.len(), 1, "{case}: the plain route stays");
        let parent = parent(&out, "r");
        assert!(
            parent.accepted,
            "{case}: a skipped match leaves the route Accepted"
        );
        match &parent.problems[..] {
            [problem @ Problem::HeaderMatchOutranksRoute {
                hostname,
                header_match,
                route,
                copy_of: None,
                reason: why,
            }] => {
                assert_eq!(hostname, host, "{case}");
                assert!(
                    header_match.contains("x-canary=\"1\""),
                    "{case}: {header_match}"
                );
                assert_eq!(route, reported, "{case}");
                assert!(why.contains(reason), "{case}: {why}");
                assert_eq!(problem.reason(), "HeaderMatchOutranksRoute");
                assert!(problem.to_string().contains(reported), "{case}: {problem}");
            }
            other => panic!("{case}: {other:?}"),
        }
        assert_translates(&out);
    }
}

/// Two routes carrying the same header match to the same backend are one
/// frontend in Sōzu, folded into the older route's. When that header match
/// is skipped, the newer route's is skipped with it, and both are told.
#[test]
fn a_skipped_header_match_is_reported_to_every_route_that_carries_it() {
    let header = json!([{ "headers": [{ "name": "version", "value": "one" }] }]);
    let out = build_routes(vec![
        route(
            "plain",
            "2026-01-01T00:00:00Z",
            json!([rule(
                "v1",
                json!([{ "path": { "type": "Exact", "value": "/" } }])
            )]),
        ),
        route(
            "first",
            "2026-01-02T00:00:00Z",
            json!([rule("v2", header.clone())]),
        ),
        route("twin", "2026-01-03T00:00:00Z", json!([rule("v2", header)])),
    ]);
    assert_eq!(shown(&out), sorted(&["app.example.com E/ -> demo.v1.80"]));
    for name in ["first", "twin"] {
        assert!(
            matches!(
                &parent(&out, name).problems[..],
                [Problem::HeaderMatchOutranksRoute { .. }]
            ),
            "{name}: {:?}",
            parent(&out, name).problems
        );
    }
    assert!(parent(&out, "plain").problems.is_empty());
    assert_translates(&out);
}

/// The refusal is per header-matched frontend: the route's other header
/// matches still program, and so does one on a route without hostnames that
/// no plain route outranks (HTTPRouteHeaderMatching's shape).
#[test]
fn only_the_header_match_no_copy_can_rank_below_is_skipped() {
    let out = build_routes(vec![
        route_on(
            "hosted",
            Some("app.example.com"),
            json!([
                rule("v1", json!([{ "path": { "type": "Exact", "value": "/" } }])),
                rule(
                    "v2",
                    json!([{ "headers": [{ "name": "version", "value": "one" }] }])
                ),
                rule(
                    "v3",
                    json!([{
                        "path": { "type": "PathPrefix", "value": "/v3" },
                        "headers": [{ "name": "version", "value": "one" }]
                    }])
                ),
            ]),
        ),
        route_on(
            "hostless",
            None,
            json!([
                rule(
                    "v1",
                    json!([{ "path": { "type": "PathPrefix", "value": "/" } }])
                ),
                rule(
                    "v2",
                    json!([{ "headers": [{ "name": "version", "value": "two" }] }])
                ),
            ]),
        ),
    ]);
    assert_eq!(
        shown(&out),
        sorted(&[
            "app.example.com E/ -> demo.v1.80",
            "app.example.com P/v3 [version=one] -> demo.v3.80",
            "* P/ -> demo.v1.80",
            "* P/ [version=two] -> demo.v2.80",
        ])
    );
    assert!(matches!(
        &parent(&out, "hosted").problems[..],
        [Problem::HeaderMatchOutranksRoute { .. }]
    ));
    assert!(parent(&out, "hostless").problems.is_empty());
    assert_translates(&out);
}

#[test]
fn regex_paths_are_never_lifted_nor_lifted_above() {
    let out = build_routes(vec![route(
        "r",
        "2026-01-01T00:00:00Z",
        json!([
            rule(
                "v1",
                json!([{ "path": { "type": "RegularExpression", "value": "/re/.*" } }])
            ),
            rule(
                "v1",
                json!([{ "path": { "type": "Exact", "value": "/exact" } }])
            ),
            rule(
                "v3",
                json!([{
                    "path": { "type": "RegularExpression", "value": "/.*" },
                    "headers": [{ "name": "version", "value": "one" }]
                }])
            ),
            rule(
                "v2",
                json!([{ "headers": [{ "name": "version", "value": "two" }] }])
            ),
        ]),
    )]);
    assert_eq!(
        shown(&out),
        sorted(&[
            "app.example.com R/re/.* -> demo.v1.80",
            "app.example.com E/exact -> demo.v1.80",
            "app.example.com R/.* [version=one] -> demo.v3.80",
            "app.example.com P/ [version=two] -> demo.v2.80",
            // only the Exact route is lifted, and only above the prefix root
            "app.example.com E/exact [version=two] -> demo.v1.80",
        ])
    );
}

#[test]
fn a_copy_whose_key_a_real_route_holds_is_dropped() {
    // `P/v2 [version=one]` is a real route of its own: it outranks plain
    // `/v2` for those requests (same path, more header matches), so the copy
    // that lifting `/v2` above `P/ [version=one]` would produce is dropped.
    let out = build_routes(vec![route(
        "r",
        "2026-01-01T00:00:00Z",
        json!([
            rule(
                "v1",
                json!([{ "headers": [{ "name": "version", "value": "one" }] }])
            ),
            rule(
                "v2",
                json!([{ "path": { "type": "PathPrefix", "value": "/v2" } }])
            ),
            rule(
                "v3",
                json!([{
                    "path": { "type": "PathPrefix", "value": "/v2" },
                    "headers": [{ "name": "version", "value": "one" }]
                }])
            ),
        ]),
    )]);
    assert_eq!(
        shown(&out),
        sorted(&[
            "app.example.com P/ [version=one] -> demo.v1.80",
            "app.example.com P/v2 -> demo.v2.80",
            "app.example.com P/v2 [version=one] -> demo.v3.80",
        ])
    );
    assert_translates(&out);
}

#[test]
fn an_ingress_route_is_lifted_too() {
    // Sōzu's route table does not know where a frontend came from: a plain
    // Ingress path on the hostname is outranked by a header-matched route
    // exactly like a plain HTTPRoute path.
    let ingress: Ingress = from_json(json!({
        "metadata": { "name": "ing", "namespace": "demo" },
        "spec": { "ingressClassName": "sozu", "rules": [{
            "host": "app.example.com",
            "http": { "paths": [{ "path": "/api", "pathType": "Prefix",
                "backend": { "service": { "name": "other", "port": { "number": 80 } } } }] }
        }]}
    }));
    let mut inputs = inputs(vec![route(
        "r",
        "2026-01-01T00:00:00Z",
        json!([rule(
            "v1",
            json!([{ "headers": [{ "name": "version", "value": "one" }] }])
        )]),
    )]);
    inputs.ingresses = vec![Arc::new(ingress)];
    let out = build(&BuildConfig::default(), &inputs);
    assert!(
        shown(&out).contains(&"app.example.com P/api [version=one] -> demo.other.80".to_string())
    );
}

/// `count` plain prefixes under the header-matched root, on one hostname.
fn crowded(count: usize) -> BuildOutput {
    let mut rules: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            rule(
                "v1",
                json!([{ "path": { "type": "PathPrefix", "value": format!("/p{i}") } }]),
            )
        })
        .collect();
    rules.push(rule(
        "v2",
        json!([{ "headers": [{ "name": "version", "value": "two" }] }]),
    ));
    let other: HttpRoute = from_json(json!({
        "metadata": { "name": "other-host", "namespace": "demo",
            "creationTimestamp": "2026-01-01T00:00:01Z" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["other.example.com"],
            "rules": [
                rule("v1", json!([{ "path": { "type": "PathPrefix", "value": "/x" } }])),
                rule("v3", json!([{ "headers": [{ "name": "version", "value": "two" }] }]))
            ]
        }
    }));
    build_routes(vec![
        route("crowded", "2026-01-01T00:00:00Z", json!(rules)),
        other,
    ])
}

#[test]
fn lifting_stops_at_a_per_hostname_limit_and_says_so() {
    let at_limit = crowded(MAX_LIFTED_PER_HOSTNAME);
    let lifted = |out: &BuildOutput, host: &str| {
        out.ir
            .frontends
            .iter()
            .filter(|f| f.hostname == host && !f.header_matches.is_empty())
            .count()
    };
    // The header-matched root plus one copy per plain prefix.
    assert_eq!(
        lifted(&at_limit, "app.example.com"),
        MAX_LIFTED_PER_HOSTNAME + 1
    );
    assert!(parent(&at_limit, "crowded").problems.is_empty());

    // One more and the header match is skipped on that hostname — never
    // programmed without the copies that keep it below the plain routes.
    let over = crowded(MAX_LIFTED_PER_HOSTNAME + 1);
    assert_eq!(lifted(&over, "app.example.com"), 0);
    assert_eq!(
        over.ir
            .frontends
            .iter()
            .filter(|f| f.hostname == "app.example.com")
            .count(),
        MAX_LIFTED_PER_HOSTNAME + 1,
        "the plain routes stay"
    );
    let parent = parent(&over, "crowded");
    assert!(parent.accepted, "a skipped match leaves the route Accepted");
    assert_eq!(
        parent.problems,
        vec![Problem::HeaderMatchPrecedenceLimit {
            hostname: "app.example.com".to_string(),
            limit: MAX_LIFTED_PER_HOSTNAME,
        }]
    );
    // Another hostname has its own budget.
    assert_eq!(lifted(&over, "other.example.com"), 2);
    assert_translates(&over);
}

#[test]
fn a_regex_path_spelling_a_predicate_suffix_is_arbitrated_as_a_collision() {
    // Sōzu does not quote the path in its route key, so this regex takes the
    // key of the older route's header match; the newer one loses, reported as
    // a key-only collision instead of failing the translation.
    let older = route(
        "older",
        "2026-01-01T00:00:00Z",
        json!([rule(
            "v1",
            json!([{
                "path": { "type": "RegularExpression", "value": "/x" },
                "headers": [{ "name": "X-Canary", "value": "always" }]
            }])
        )]),
    );
    let newer = route(
        "newer",
        "2026-01-01T00:00:01Z",
        json!([rule(
            "v2",
            json!([{ "path": { "type": "RegularExpression",
                "value": "/x;H0:\"x-canary\"=\"always\"" } }])
        )]),
    );
    let out = build_routes(vec![newer, older]);
    assert_eq!(
        shown(&out),
        sorted(&["app.example.com R/x [x-canary=always] -> demo.v1.80"])
    );
    let loser = parent(&out, "newer");
    assert_eq!(loser.accepted_reason, "RouteCollision");
    assert!(matches!(
        &loser.problems[..],
        [Problem::RouteCollision { key_only: true, .. }]
    ));
    assert_translates(&out);
}

// ---------------------------------------------------------------------------
// A model of Sōzu 2.3.2's route selection, to check what the built IR routes
// in every order Sōzu may hold it in: a fresh batch, incremental adds and a
// re-forked worker's route-key replay all differ (#80).

/// One request: method, target (the path with its query string, which is what
/// Sōzu matches path rules against) and headers, first occurrence first.
struct Request<'a> {
    method: &'a str,
    target: &'a str,
    headers: &'a [(&'a str, &'a str)],
}

/// A frontend as a worker's route table holds it: its path rule and header
/// predicates compiled the way Sōzu compiles them (`anchored_regex`).
struct Held<'a> {
    frontend: &'a ir::Frontend,
    path: Rule,
    headers: Vec<(String, Value)>,
}

enum Rule {
    Prefix(String),
    Regex(regex::bytes::Regex),
}

enum Value {
    Exact(String),
    Regex(regex::bytes::Regex),
}

fn anchored(pattern: &str) -> regex::bytes::Regex {
    regex::bytes::Regex::new(&format!("\\A(?:{pattern})\\z")).expect("a regex Sōzu compiles")
}

fn held(frontend: &ir::Frontend) -> Held<'_> {
    Held {
        frontend,
        path: match frontend.path.sozu_rule() {
            ir::SozuPathRule::Prefix(prefix) => Rule::Prefix(prefix),
            ir::SozuPathRule::Regex(pattern) => Rule::Regex(anchored(&pattern)),
        },
        headers: frontend
            .header_matches
            .iter()
            .map(|m| {
                let value = match &m.kind {
                    ir::HeaderMatchKind::Exact(value) => Value::Exact(value.clone()),
                    ir::HeaderMatchKind::Regex(pattern) => Value::Regex(anchored(pattern)),
                };
                (m.name.clone(), value)
            })
            .collect(),
    }
}

/// `PathRule::matches`: a prefix reports its length, a regex only that it
/// matched (`None`).
fn path_result(h: &Held, target: &str) -> Option<Option<usize>> {
    match &h.path {
        Rule::Prefix(prefix) => target
            .starts_with(prefix.as_str())
            .then_some(Some(prefix.len())),
        Rule::Regex(regex) => regex.is_match(target.as_bytes()).then_some(None),
    }
}

/// `MethodRule::matches`: `None` when the method excludes the rule, else
/// whether it names the method (`Equals`) rather than none (`All`).
fn method_result(h: &Held, method: &str) -> Option<bool> {
    match &h.frontend.method {
        None => Some(false),
        Some(m) => (m == method).then_some(true),
    }
}

/// `HeaderRules::matches`: every predicate holds on the first occurrence of
/// its header, names compared ASCII-case-insensitively.
fn headers_hold(h: &Held, headers: &[(&str, &str)]) -> bool {
    h.headers.iter().all(|(name, expected)| {
        let Some((_, value)) = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) else {
            return false;
        };
        match expected {
            Value::Exact(expected) => value == expected,
            Value::Regex(regex) => regex.is_match(value.as_bytes()),
        }
    })
}

/// One class of `select_tree_rule`: `TreeSelection::offer`.
#[derive(Default)]
struct TreeSelection<'a> {
    prefix_length: usize,
    matched: Option<&'a ir::Frontend>,
    exact: bool,
}

impl<'a> TreeSelection<'a> {
    fn offer(
        &mut self,
        prefix: Option<usize>,
        method_equals: bool,
        len: usize,
        f: &'a ir::Frontend,
    ) {
        match prefix {
            None if method_equals => {
                self.matched = Some(f);
                self.exact = true;
            }
            None => {
                self.prefix_length = len;
                self.matched = Some(f);
            }
            Some(size) => {
                if size >= self.prefix_length {
                    self.prefix_length = size;
                    self.matched = Some(f);
                }
            }
        }
    }
}

/// What Sōzu serves `request` with among the frontends of one hostname held
/// in `order`: `select_tree_rule` for a hostname leaf, `select_listed_rule`
/// for the hostname-less POST list (`lib/src/router/mod.rs`).
fn sozu_selects<'a>(order: &[&Held<'a>], request: &Request) -> Option<&'a ir::Frontend> {
    if order.first().is_some_and(|h| h.frontend.hostname == "*") {
        let mut unpredicated = None;
        for h in order {
            let plain = h.headers.is_empty();
            if unpredicated.is_some() && plain {
                continue;
            }
            if path_result(h, request.target).is_none()
                || method_result(h, request.method).is_none()
            {
                continue;
            }
            if plain {
                unpredicated = Some(h.frontend);
            } else if headers_hold(h, request.headers) {
                return Some(h.frontend);
            }
        }
        return unpredicated;
    }
    let mut predicated = TreeSelection::default();
    let mut unpredicated = TreeSelection::default();
    for h in order {
        let class = if h.headers.is_empty() {
            &mut unpredicated
        } else {
            &mut predicated
        };
        if class.exact {
            continue;
        }
        let Some(prefix) = path_result(h, request.target) else {
            continue;
        };
        let Some(method_equals) = method_result(h, request.method) else {
            continue;
        };
        if !h.headers.is_empty() && !headers_hold(h, request.headers) {
            continue;
        }
        class.offer(prefix, method_equals, request.target.len(), h.frontend);
        if predicated.exact {
            break;
        }
    }
    predicated.matched.or(unpredicated.matched)
}

/// Every order of `items` (Heap's algorithm).
fn permutations<T: Copy>(items: &[T]) -> Vec<Vec<T>> {
    fn heap<T: Copy>(k: usize, items: &mut Vec<T>, out: &mut Vec<Vec<T>>) {
        if k <= 1 {
            out.push(items.clone());
            return;
        }
        for i in 0..k {
            heap(k - 1, items, out);
            let j = if k.is_multiple_of(2) { i } else { 0 };
            items.swap(j, k - 1);
        }
    }
    let mut items = items.to_vec();
    let mut out = Vec::new();
    heap(items.len(), &mut items, &mut out);
    out
}

/// The backend a selection serves (`404` for none).
fn backend(f: Option<&ir::Frontend>) -> String {
    f.map(|f| {
        f.cluster_id
            .clone()
            .unwrap_or_else(|| "<redirect>".to_string())
    })
    .unwrap_or_else(|| "404".to_string())
}

/// The HTTP frontends on `host`, as a worker holds them.
fn held_on<'a>(out: &'a BuildOutput, host: &str) -> Vec<Held<'a>> {
    out.ir
        .frontends
        .iter()
        .filter(|f| f.hostname == host && !f.tls)
        .map(held)
        .collect()
}

/// Whether `request` matches `h` at all. Sōzu skips a rule that does not
/// before it weighs anything, so only the order of those that do matters.
fn matches(h: &Held, request: &Request) -> bool {
    path_result(h, request.target).is_some()
        && method_result(h, request.method).is_some()
        && headers_hold(h, request.headers)
}

/// The backends Sōzu serves `request` with across every order it may hold
/// `frontends` in.
fn served_by(frontends: &[Held], request: &Request) -> BTreeSet<String> {
    let matching: Vec<&Held> = frontends.iter().filter(|h| matches(h, request)).collect();
    assert!(matching.len() <= 8, "{} matching frontends", matching.len());
    permutations(&matching)
        .iter()
        .map(|order| backend(sozu_selects(order, request)))
        .collect()
}

/// The backends Sōzu serves `request` on `host` with across every order it
/// may hold that hostname's HTTP frontends in.
fn served(out: &BuildOutput, host: &str, request: &Request) -> BTreeSet<String> {
    served_by(&held_on(out, host), request)
}

fn only(backend: &str) -> BTreeSet<String> {
    BTreeSet::from([backend.to_string()])
}

/// A route on `hostname` (none when `None`) with this creation time and rules.
fn route_at(
    name: &str,
    created: &str,
    hostname: Option<&str>,
    rules: serde_json::Value,
) -> HttpRoute {
    from_json(json!({
        "metadata": { "name": name, "namespace": "demo", "creationTimestamp": created },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": hostname.map(|h| vec![h]),
            "rules": rules
        }
    }))
}

#[test]
fn the_model_reproduces_sozus_measured_precedence() {
    // Measured on 2.3.2 (`header_match_probe`): a header match on the root
    // takes a plain Exact `/foo`'s request when nothing is lifted; two
    // method-less path regexes go to the one held last; a regex naming the
    // method decides wherever it stands.
    let host = "app.example.com";
    let frontend = |path: ir::PathMatch, method: Option<&str>, cluster: &str| ir::Frontend {
        hostname: host.into(),
        path,
        method: method.map(str::to_string),
        cluster_id: Some(cluster.into()),
        tls: false,
        listener: "0.0.0.0:8080".parse().unwrap(),
        filters: Default::default(),
        header_matches: vec![],
    };
    let canary = vec![ir::HeaderMatch {
        name: "x-canary".into(),
        kind: ir::HeaderMatchKind::Exact("1".into()),
    }];
    let exact = frontend(ir::PathMatch::Exact("/foo".into()), None, "plain");
    let root = ir::Frontend {
        header_matches: canary.clone(),
        ..frontend(ir::PathMatch::Prefix("/".into()), None, "canary")
    };
    let get = Request {
        method: "GET",
        target: "/foo",
        headers: &[("X-Canary", "1")],
    };
    let (exact, root) = (held(&exact), held(&root));
    for order in permutations(&[&exact, &root]) {
        assert_eq!(backend(sozu_selects(&order, &get)), "canary");
    }
    let api = frontend(ir::PathMatch::Prefix("/api".into()), None, "api");
    let v2 = frontend(ir::PathMatch::Prefix("/api/v2".into()), None, "v2");
    let api_get = frontend(ir::PathMatch::Prefix("/api".into()), Some("GET"), "get");
    let (api, v2, api_get) = (held(&api), held(&v2), held(&api_get));
    let deep = Request {
        method: "GET",
        target: "/api/v2/x",
        headers: &[],
    };
    assert_eq!(backend(sozu_selects(&[&v2, &api], &deep)), "api");
    assert_eq!(backend(sozu_selects(&[&api, &v2], &deep)), "v2");
    assert_eq!(backend(sozu_selects(&[&v2, &api_get], &deep)), "get");
    assert_eq!(backend(sozu_selects(&[&api_get, &v2], &deep)), "get");
}

/// Review item 1(a): an Exact path outranks a prefix with a method, but both
/// compile to regexes and Sōzu serves a regex naming the method first,
/// whatever the order. The lower header match is skipped and names the
/// higher one; the requests both match keep reaching the Exact route.
#[test]
fn a_header_match_naming_a_method_never_takes_an_exact_header_match() {
    let out = build_routes(vec![
        route_at(
            "exact",
            "2026-01-01T00:00:01Z",
            Some("app.example.com"),
            json!([rule(
                "v1",
                json!([{
                    "path": { "type": "Exact", "value": "/foo" },
                    "headers": [{ "name": "a", "value": "1" }]
                }])
            )]),
        ),
        route_at(
            "method",
            "2026-01-01T00:00:00Z",
            Some("app.example.com"),
            json!([rule(
                "v2",
                json!([{
                    "path": { "type": "PathPrefix", "value": "/foo" },
                    "method": "GET",
                    "headers": [{ "name": "b", "value": "1" }]
                }])
            )]),
        ),
    ]);
    let both = Request {
        method: "GET",
        target: "/foo",
        headers: &[("a", "1"), ("b", "1")],
    };
    assert_eq!(served(&out, "app.example.com", &both), only("demo.v1.80"));
    assert_eq!(
        shown(&out),
        sorted(&["app.example.com E/foo [a=1] -> demo.v1.80"])
    );
    let skipped = parent(&out, "method");
    assert!(
        skipped.accepted,
        "a skipped match leaves the route Accepted"
    );
    match &skipped.problems[..] {
        [problem @ Problem::HeaderMatchOverlap {
            hostname,
            header_match,
            route,
            ..
        }] => {
            assert_eq!(hostname, "app.example.com");
            assert_eq!(header_match, "GET prefix /foo with b=\"1\"");
            assert!(route.contains("demo/exact"), "{route}");
            assert_eq!(problem.reason(), "HeaderMatchOverlap");
        }
        other => panic!("{other:?}"),
    }
    assert!(parent(&out, "exact").problems.is_empty());
    assert_translates(&out);
}

/// Review item 1(b): with every condition met the match with more header
/// matches wins, but two root prefixes go to the one Sōzu holds last. The
/// one-header match is skipped even though its route is older.
#[test]
fn the_match_with_more_header_matches_wins_in_every_order() {
    let out = build_routes(vec![
        route_at(
            "two",
            "2026-01-01T00:00:01Z",
            Some("app.example.com"),
            json!([rule(
                "v1",
                json!([{ "headers": [
                    { "name": "a", "value": "1" },
                    { "name": "z", "value": "1" }
                ] }])
            )]),
        ),
        route_at(
            "one",
            "2026-01-01T00:00:00Z",
            Some("app.example.com"),
            json!([rule(
                "v2",
                json!([{ "headers": [{ "name": "b", "value": "1" }] }])
            )]),
        ),
    ]);
    let all = Request {
        method: "GET",
        target: "/",
        headers: &[("a", "1"), ("z", "1"), ("b", "1")],
    };
    assert_eq!(served(&out, "app.example.com", &all), only("demo.v1.80"));
    // The cost: the skipped match's own requests reach no header match.
    let own = Request {
        method: "GET",
        target: "/",
        headers: &[("b", "1")],
    };
    assert_eq!(served(&out, "app.example.com", &own), only("404"));
    assert!(matches!(
        &parent(&out, "one").problems[..],
        [Problem::HeaderMatchOverlap { route, .. }] if route.contains("demo/two")
    ));
    assert!(parent(&out, "two").problems.is_empty());
    assert_translates(&out);
}

/// Header matches that cannot hold together never compete: different exact
/// values of one header, or a regex that cannot match the other's value.
/// Neither do header matches Sōzu orders by its own rules alone (a longer
/// path regex over the root prefix), nor two to one backend (`version: one`
/// with and without `color: red`).
#[test]
fn header_matches_that_cannot_compete_or_that_sozu_orders_are_kept() {
    let header = |name: &str, value: &str| json!({ "name": name, "value": value });
    let out = build_routes(vec![route_at(
        "r",
        "2026-01-01T00:00:00Z",
        Some("app.example.com"),
        json!([
            rule("v1", json!([{ "headers": [header("version", "one")] }])),
            rule("v2", json!([{ "headers": [header("version", "two")] }])),
            rule(
                "v3",
                json!([{ "headers": [
                    { "name": "version", "type": "RegularExpression", "value": "th.*" }
                ] }])
            ),
            rule(
                "other",
                json!([{
                    "path": { "type": "PathPrefix", "value": "/api" },
                    "headers": [header("color", "blue")]
                }])
            ),
            rule(
                "v1",
                json!([{ "headers": [header("version", "one"), header("color", "red")] }])
            ),
        ]),
    )]);
    assert!(
        parent(&out, "r").problems.is_empty(),
        "{:?}",
        parent(&out, "r").problems
    );
    assert_eq!(out.ir.frontends.len(), 5, "{:?}", shown(&out));
    for (headers, path, want) in [
        (&[("version", "one")][..], "/", "demo.v1.80"),
        (&[("version", "two")][..], "/", "demo.v2.80"),
        (&[("version", "three")][..], "/", "demo.v3.80"),
        (
            &[("version", "two"), ("color", "blue")][..],
            "/api/x",
            "demo.other.80",
        ),
        (
            &[("version", "one"), ("color", "red")][..],
            "/",
            "demo.v1.80",
        ),
    ] {
        let request = Request {
            method: "GET",
            target: path,
            headers,
        };
        assert_eq!(
            served(&out, "app.example.com", &request),
            only(want),
            "{headers:?} {path}"
        );
    }
    assert_translates(&out);
}

/// A copy lifted above one header match competes with every other header
/// match its requests can meet. Here the copy of a plain `GET /foo` prefix,
/// made for the header match on the root, would be served ahead of an Exact
/// `/foo` header match that outranks the plain route; the root header match,
/// whose copy it is, is skipped.
#[test]
fn a_copy_never_takes_a_header_match_that_outranks_its_route() {
    let out = build_routes(vec![route_at(
        "r",
        "2026-01-01T00:00:00Z",
        Some("app.example.com"),
        json!([
            rule(
                "v1",
                json!([{ "path": { "type": "PathPrefix", "value": "/foo" }, "method": "GET" }])
            ),
            rule(
                "v3",
                json!([{
                    "path": { "type": "Exact", "value": "/foo" },
                    "headers": [{ "name": "b", "value": "1" }]
                }])
            ),
            rule(
                "v2",
                json!([{ "headers": [{ "name": "a", "value": "1" }] }])
            ),
        ]),
    )]);
    let both = Request {
        method: "GET",
        target: "/foo",
        headers: &[("a", "1"), ("b", "1")],
    };
    assert_eq!(served(&out, "app.example.com", &both), only("demo.v3.80"));
    assert_eq!(
        shown(&out),
        sorted(&[
            "app.example.com P/foo GET -> demo.v1.80",
            "app.example.com E/foo [b=1] -> demo.v3.80",
        ])
    );
    assert!(matches!(
        &parent(&out, "r").problems[..],
        [Problem::HeaderMatchOverlap { copy_of: Some(copied), route, .. }]
            if &**copied == "GET prefix /foo" && route.contains("exact /foo")
    ));
    assert_translates(&out);
}

/// On a route without hostnames Sōzu serves the first header match it holds
/// that matches: two that can meet one request, to different backends, are
/// never both programmed. The lower is skipped (rule order here).
#[test]
fn hostname_less_header_matches_that_can_meet_one_request_are_not_both_programmed() {
    let out = build_routes(vec![route_at(
        "hostless",
        "2026-01-01T00:00:00Z",
        None,
        json!([
            rule(
                "v1",
                json!([{ "headers": [{ "name": "version", "value": "one" }] }])
            ),
            rule(
                "v2",
                json!([{ "headers": [{ "name": "color", "value": "red" }] }])
            ),
            rule(
                "v1",
                json!([{ "headers": [{ "name": "color", "value": "blue" }] }])
            ),
        ]),
    )]);
    assert_eq!(
        shown(&out),
        sorted(&[
            "* P/ [version=one] -> demo.v1.80",
            "* P/ [color=blue] -> demo.v1.80",
        ])
    );
    let both = Request {
        method: "GET",
        target: "/",
        headers: &[("version", "one"), ("color", "red")],
    };
    assert_eq!(served(&out, "*", &both), only("demo.v1.80"));
    assert!(matches!(
        &parent(&out, "hostless").problems[..],
        [Problem::HeaderMatchOverlap { reason, .. }] if reason.contains("without hostnames")
    ));
    assert_translates(&out);
}

/// Review item 2: Sōzu does not quote the path in its route key, so a plain
/// regex path can spell the key of a copy lifted above a header match. That
/// frontend is not the copy — it matches nothing the copy matches — so the
/// copy is not dropped in its favour: the header match that needed it is
/// skipped and the plain Exact `/foo` keeps its request.
#[test]
fn a_route_spelling_a_copys_key_is_a_collision_not_a_substitute() {
    let out = build_routes(vec![
        route_at(
            "u",
            "2026-01-01T00:00:00Z",
            Some("app.example.com"),
            json!([rule(
                "v1",
                json!([{ "path": { "type": "Exact", "value": "/foo" } }])
            )]),
        ),
        route_at(
            "p",
            "2026-01-01T00:00:00Z",
            Some("app.example.com"),
            json!([rule(
                "v2",
                json!([{ "headers": [{ "name": "x", "value": "1" }] }])
            )]),
        ),
        route_at(
            "f",
            "2026-01-01T00:00:00Z",
            Some("app.example.com"),
            json!([rule(
                "v3",
                json!([{ "path": { "type": "RegularExpression",
                    "value": "^/foo(?:\\?(?-u:.*))?$;H0:\"x\"=\"1\"" } }])
            )]),
        ),
    ]);
    let request = Request {
        method: "GET",
        target: "/foo",
        headers: &[("x", "1")],
    };
    assert_eq!(
        served(&out, "app.example.com", &request),
        only("demo.v1.80")
    );
    assert!(out.ir.frontends.iter().all(|f| f.header_matches.is_empty()));
    let skipped = parent(&out, "p");
    assert!(skipped.accepted);
    match &skipped.problems[..] {
        [problem @ Problem::HeaderMatchCopyKeyTaken {
            header_match,
            route,
            holder,
            ..
        }] => {
            assert_eq!(header_match, "prefix / with x=\"1\"");
            assert_eq!(route, "exact /foo");
            assert!(holder.contains("demo/f"), "{holder}");
            assert_eq!(problem.reason(), "HeaderMatchCopyKeyTaken");
        }
        other => panic!("{other:?}"),
    }
    assert!(parent(&out, "f").problems.is_empty());
    assert_translates(&out);
}

/// Review item 3: the per-hostname budget counts the copies actually added.
/// Each of the first prefixes already has a real header match with the copy's
/// very match, so lifting it above the header-matched root adds nothing; the
/// rest take the whole budget. Counting every candidate would exceed it.
#[test]
fn the_lifting_budget_counts_only_copies_actually_added() {
    // Every header match on the hostname within its own cap: these, the root.
    let stood_in = MAX_HEADER_MATCHES_PER_HOSTNAME - 1;
    let mut rules: Vec<serde_json::Value> = (0..stood_in)
        .flat_map(|i| {
            let path = json!({ "type": "PathPrefix", "value": format!("/p{i}") });
            [
                rule("v1", json!([{ "path": path.clone() }])),
                rule(
                    "v1",
                    json!([{ "path": path, "headers": [{ "name": "x", "value": "1" }] }]),
                ),
            ]
        })
        .collect();
    rules.extend((0..MAX_LIFTED_PER_HOSTNAME).map(|i| {
        rule(
            "v1",
            json!([{ "path": { "type": "PathPrefix", "value": format!("/q{i}") } }]),
        )
    }));
    rules.push(rule(
        "v2",
        json!([{ "headers": [{ "name": "x", "value": "1" }] }]),
    ));
    let out = build_routes(vec![route("crowded", "2026-01-01T00:00:00Z", json!(rules))]);
    let parent = parent(&out, "crowded");
    assert!(parent.problems.is_empty(), "{:?}", parent.problems);
    assert_eq!(
        out.ir.frontends.len(),
        2 * stood_in + 2 * MAX_LIFTED_PER_HOSTNAME + 1
    );
    assert!(out
        .ir
        .frontends
        .iter()
        .any(|f| f.path == ir::PathMatch::Prefix("/".into()) && !f.header_matches.is_empty()));
    assert_translates(&out);
}

/// Two copies Sōzu cannot order between themselves skip the header match
/// they were made for. A header match on the root needs copies of a plain
/// Exact `/foo` (v1) and a plain prefix `/foo` (v2), and each copy outranks it
/// in every order; but between themselves they are two method-less regexes,
/// so with the header `GET /foo` would reach whichever copy Sōzu holds last,
/// in an insertion order of the copies' own, where the Gateway API wants v1.
/// Skipping the header match leaves those requests to the plain routes, in
/// the order they have without any header match (#80).
#[test]
fn two_copies_sozu_cannot_order_skip_the_header_match_that_needs_them() {
    let host = "app.example.com";
    let out = build_routes(vec![
        route_at(
            "exact",
            "2026-01-01T00:00:00Z",
            Some(host),
            json!([rule(
                "v1",
                json!([{ "path": { "type": "Exact", "value": "/foo" } }])
            )]),
        ),
        route_at(
            "prefix",
            "2026-01-01T00:00:01Z",
            Some(host),
            json!([rule(
                "v2",
                json!([{ "path": { "type": "PathPrefix", "value": "/foo" } }])
            )]),
        ),
        route_at(
            "canary",
            "2026-01-01T00:00:02Z",
            Some(host),
            json!([rule(
                "v3",
                json!([{ "headers": [{ "name": "x", "value": "1" }] }])
            )]),
        ),
    ]);
    assert_eq!(
        shown(&out),
        sorted(&[
            "app.example.com E/foo -> demo.v1.80",
            "app.example.com P/foo -> demo.v2.80",
        ])
    );
    let skipped = parent(&out, "canary");
    assert!(
        skipped.accepted,
        "a skipped match leaves the route Accepted"
    );
    match &skipped.problems[..] {
        [problem @ Problem::HeaderMatchOutranksRoute {
            hostname,
            header_match,
            route,
            copy_of: Some(copied),
            reason,
        }] => {
            assert_eq!(hostname, host);
            assert_eq!(header_match, "prefix / with x=\"1\"");
            assert_eq!(route, "exact /foo");
            assert_eq!(&**copied, "prefix /foo");
            assert!(reason.contains("method-less path regexes"), "{reason}");
            assert_eq!(problem.reason(), "HeaderMatchOutranksRoute");
            let message = problem.to_string();
            assert!(message.contains("exact /foo") && message.contains("prefix /foo"));
        }
        other => panic!("{other:?}"),
    }
    assert!(parent(&out, "exact").problems.is_empty());
    assert!(parent(&out, "prefix").problems.is_empty());
    // The header changes nothing: the plain routes' own order decides.
    for target in ["/foo", "/foo/bar", "/"] {
        let with = Request {
            method: "GET",
            target,
            headers: &[("x", "1")],
        };
        let without = Request {
            method: "GET",
            target,
            headers: &[],
        };
        assert_eq!(
            served(&out, host, &with),
            served(&out, host, &without),
            "{target}"
        );
    }
    assert_translates(&out);
}

/// A copy names the method its header match names when its plain route names
/// none, so it takes no request the header match cannot. Here plain `PUT`
/// Exact `/a` (v2) outranks plain Exact `/a` (v1) for `PUT /a`, and neither
/// header match below matches `PUT`: a method-less copy of `/a` would take
/// `PUT /a` with the header from v2, which is never copied. It also keeps a
/// header match naming a method under a plain Exact `/`: the copy names the
/// method too, and Sōzu serves it ahead of the root in every order.
#[test]
fn a_copy_takes_no_request_its_header_match_cannot() {
    let host = "app.example.com";
    let out = build_routes(vec![route_at(
        "r",
        "2026-01-01T00:00:00Z",
        Some(host),
        json!([
            rule(
                "v1",
                json!([{ "path": { "type": "Exact", "value": "/a" } }])
            ),
            rule(
                "v2",
                json!([{ "path": { "type": "Exact", "value": "/a" }, "method": "PUT" }])
            ),
            rule(
                "v3",
                json!([{ "method": "GET", "headers": [{ "name": "x", "value": "1" }] }])
            ),
        ]),
    )]);
    assert_eq!(
        shown(&out),
        sorted(&[
            "app.example.com E/a -> demo.v1.80",
            "app.example.com E/a PUT -> demo.v2.80",
            "app.example.com P/ GET [x=1] -> demo.v3.80",
            "app.example.com E/a GET [x=1] -> demo.v1.80",
        ])
    );
    assert!(parent(&out, "r").problems.is_empty());
    for (method, target, want) in [
        ("PUT", "/a", "demo.v2.80"),
        ("GET", "/a", "demo.v1.80"),
        ("POST", "/a", "demo.v1.80"),
        ("GET", "/b", "demo.v3.80"),
        ("PUT", "/b", "404"),
    ] {
        let request = Request {
            method,
            target,
            headers: &[("x", "1")],
        };
        assert_eq!(
            served(&out, host, &request),
            only(want),
            "{method} {target}"
        );
    }
    assert_translates(&out);

    let out = build_routes(vec![route_at(
        "r",
        "2026-01-01T00:00:00Z",
        Some(host),
        json!([
            rule("v1", json!([{ "path": { "type": "Exact", "value": "/" } }])),
            rule(
                "v2",
                json!([{ "method": "POST", "headers": [{ "name": "x", "value": "1" }] }])
            ),
        ]),
    )]);
    assert!(parent(&out, "r").problems.is_empty());
    assert_eq!(
        shown(&out),
        sorted(&[
            "app.example.com E/ -> demo.v1.80",
            "app.example.com P/ POST [x=1] -> demo.v2.80",
            "app.example.com E/ POST [x=1] -> demo.v1.80",
        ])
    );
    for (method, target, want) in [
        ("POST", "/", "demo.v1.80"),
        ("POST", "/?q=1", "demo.v1.80"),
        ("POST", "/x", "demo.v2.80"),
        ("GET", "/", "demo.v1.80"),
        ("GET", "/x", "404"),
    ] {
        let request = Request {
            method,
            target,
            headers: &[("x", "1")],
        };
        assert_eq!(
            served(&out, host, &request),
            only(want),
            "{method} {target}"
        );
    }
    assert_translates(&out);
}

/// `count` header matches on the root of `hostname`, one per rule, every one
/// on a header of its own and with a response header filter of its own: any
/// two can meet one request, serve it differently, and Sōzu orders them by
/// insertion alone, so the admission must skip all but one. Rules are spread
/// over routes of `per_route` rules, the routes ordered by age.
fn overlapping_roots(hostname: &str, count: usize, per_route: usize) -> Vec<HttpRoute> {
    let rules: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            json!({
                "matches": [{ "headers": [{ "name": format!("x-{i}"), "value": "1" }] }],
                "filters": [{ "type": "ResponseHeaderModifier",
                    "responseHeaderModifier": { "set": [{ "name": "x-variant",
                        "value": format!("{i}") }] } }],
                "backendRefs": [{ "name": "v1", "port": 80 }]
            })
        })
        .collect();
    rules
        .chunks(per_route)
        .enumerate()
        .map(|(i, rules)| {
            route_at(
                &format!("{hostname}-{i:04}"),
                &format!(
                    "2026-01-01T{:02}:{:02}:{:02}Z",
                    i / 3600,
                    i / 60 % 60,
                    i % 60
                ),
                Some(hostname),
                json!(rules),
            )
        })
        .collect()
}

/// The admission skips one header match at a time; 4000 on one hostname that
/// all overlap unsettled used to take about 10¹⁰ pair checks. Past the cap,
/// header matches are skipped and reported before any planning: the build is
/// quick, the 64 oldest are weighed (the oldest wins, the other 63 overlap
/// it), and every route past them carries `HeaderMatchLimit`.
#[test]
fn thousands_of_overlapping_header_matches_on_one_hostname_stay_bounded() {
    const COUNT: usize = 4000;
    const PER_ROUTE: usize = 16;
    let host = "app.example.com";
    let routes = overlapping_roots(host, COUNT, PER_ROUTE);
    let plain = without_header_matches(&routes);
    let out = build_bounded(
        &format!("{COUNT} header matches on one hostname"),
        routes,
        plain,
        8,
    );
    let programmed: Vec<&ir::Frontend> = out
        .ir
        .frontends
        .iter()
        .filter(|f| !f.header_matches.is_empty())
        .collect();
    assert_eq!(programmed.len(), 1, "{:?}", shown(&out));
    assert_eq!(programmed[0].header_matches[0].name, "x-0");
    let weighed = MAX_HEADER_MATCHES_PER_HOSTNAME / PER_ROUTE;
    let limit = Problem::HeaderMatchLimit {
        hostname: host.to_string(),
        limit: MAX_HEADER_MATCHES_PER_HOSTNAME,
    };
    for (i, route) in out.routes.iter().enumerate() {
        assert_eq!(route.name, format!("{host}-{i:04}"));
        let problems = &route.parents[0].problems;
        if i < weighed {
            let overlaps = problems
                .iter()
                .filter(|p| matches!(p, Problem::HeaderMatchOverlap { .. }))
                .count();
            assert_eq!(overlaps, PER_ROUTE - usize::from(i == 0), "{}", route.name);
            assert!(!problems.contains(&limit), "{}", route.name);
        } else {
            assert_eq!(problems, &vec![limit.clone()], "{}", route.name);
        }
    }
    assert!(limit.to_string().contains("more than 64 header matches"));
    // Uncapped, the admission alone would make about 10¹⁰ pair checks here;
    // capped, the whole build took 55 ms in a debug build and 17 ms in a
    // release one when written.
    assert_translates(&out);
}

/// Past the cap each skipped header match is reported on its own route, so a
/// tenant splitting its matches over thousands of HTTPRoutes makes thousands
/// of reports: delivering each must not cost a scan of every route.
#[test]
fn one_header_match_per_route_over_thousands_of_routes_stays_bounded() {
    const ROUTES: usize = 40_000;
    let host = "app.example.com";
    // Measured against as many plain routes on distinct hostnames: without
    // their header matches these would collide on one key, and collision
    // reports took the same scan per report.
    let plain = (0..ROUTES)
        .map(|i| {
            route_at(
                &format!("plain-{i:05}"),
                "2026-01-01T00:00:00Z",
                Some(&format!("h{i:05}.example.com")),
                json!([rule(
                    "v1",
                    json!([{ "path": { "type": "PathPrefix", "value": "/" } }])
                )]),
            )
        })
        .collect();
    let out = build_bounded(
        &format!("{ROUTES} routes with one header match each"),
        overlapping_roots(host, ROUTES, 1),
        plain,
        8,
    );
    let limit = Problem::HeaderMatchLimit {
        hostname: host.to_string(),
        limit: MAX_HEADER_MATCHES_PER_HOSTNAME,
    };
    let limited = out
        .routes
        .iter()
        .filter(|r| r.parents[0].problems.contains(&limit))
        .count();
    assert_eq!(limited, ROUTES - MAX_HEADER_MATCHES_PER_HOSTNAME);
    // Delivered by a scan of every route per report, half this count took
    // 6.6 s in a debug build when written, growing with its square; indexed,
    // 0.4 s.
}

/// Each hostname is planned alone, so a tenant spreading header matches over
/// many hostnames, each at the cap and each needing every skip the cap allows,
/// costs time linear in the hostnames.
#[test]
fn header_matches_at_the_cap_on_many_hostnames_stay_bounded() {
    const HOSTNAMES: usize = 63;
    let routes: Vec<HttpRoute> = (0..HOSTNAMES)
        .flat_map(|h| {
            overlapping_roots(
                &format!("h{h:02}.example.com"),
                MAX_HEADER_MATCHES_PER_HOSTNAME,
                16,
            )
        })
        .collect();
    let out = build_bounded(
        &format!(
            "{} header matches on {HOSTNAMES} hostnames",
            HOSTNAMES * MAX_HEADER_MATCHES_PER_HOSTNAME
        ),
        routes.clone(),
        without_header_matches(&routes),
        // Planning 63 groups of 64 is work the plain build, which only
        // settles colliding roots, does not do: 11 times it when written.
        // Replanning every hostname after each skip, as merged groups would,
        // multiplies that planning by the 63 hostnames.
        40,
    );
    for h in 0..HOSTNAMES {
        let host = format!("h{h:02}.example.com");
        let programmed: Vec<&ir::Frontend> = out
            .ir
            .frontends
            .iter()
            .filter(|f| f.hostname == host && !f.header_matches.is_empty())
            .collect();
        assert_eq!(programmed.len(), 1, "{host}");
        assert_eq!(programmed[0].header_matches[0].name, "x-0", "{host}");
    }
    assert!(out
        .routes
        .iter()
        .flat_map(|r| &r.parents[0].problems)
        .all(|p| matches!(p, Problem::HeaderMatchOverlap { .. })));
    // 330 ms in a debug build and 32 ms in a release one when written.
    assert_translates(&out);
}

/// A header match searches the plain routes beneath it only as far as its
/// copy budget reaches: here 8000 plain prefixes outrank each of 64 header
/// matches on the root, and every one of those needs more copies than the
/// budget allows, so each is skipped after reading a few of them. Copying
/// every plain route that outranks each header match in every round, as this
/// once did, took two minutes for 20000 in a debug build; it now adds nothing
/// measurable to what the plain routes alone cost.
#[test]
fn many_plain_routes_under_header_matches_stay_bounded() {
    const PLAIN: usize = 8000;
    let plain = (0..PLAIN / 16).map(|r| {
        let rules: Vec<serde_json::Value> = (0..16)
            .map(|i| {
                rule(
                    "v1",
                    json!([{ "path": { "type": "PathPrefix",
                        "value": format!("/p{}", r * 16 + i) } }]),
                )
            })
            .collect();
        route_at(
            &format!("plain-{r:04}"),
            "2026-01-01T00:00:00Z",
            Some("app.example.com"),
            json!(rules),
        )
    });
    let routes: Vec<HttpRoute> = plain
        .chain(overlapping_roots(
            "app.example.com",
            MAX_HEADER_MATCHES_PER_HOSTNAME,
            16,
        ))
        .collect();
    let plain = without_header_matches(&routes);
    let out = build_bounded(
        &format!("{PLAIN} plain routes under 64 header matches"),
        routes,
        plain,
        4,
    );
    assert_eq!(out.ir.frontends.len(), PLAIN, "only the plain routes");
    let limits = out
        .routes
        .iter()
        .flat_map(|r| &r.parents[0].problems)
        .filter(|p| matches!(p, Problem::HeaderMatchPrecedenceLimit { .. }))
        .count();
    // One per route: four routes of 16 header matches, deduplicated.
    assert_eq!(limits, MAX_HEADER_MATCHES_PER_HOSTNAME / 16);
    // 1.3 s in a debug build when written, nearly all of it the plain routes:
    // copying every outranking plain route in every round cost about 38 times
    // the plain build.
}

/// Which header matches the cap keeps follows the routes' age, then
/// `namespace/name`, never the order the routes arrive in: here 100 routes in
/// four creation seconds, named out of age order, each with one header match
/// on a header value of its own (no two can meet, so all within the cap are
/// programmed), built in several input orders.
#[test]
fn the_header_match_cap_keeps_the_oldest_routes_whatever_the_input_order() {
    let host = "app.example.com";
    let routes: Vec<HttpRoute> = (0..100usize)
        .map(|i| {
            let name = format!("r{:03}", (i * 37) % 100);
            route_at(
                &name,
                &format!("2026-01-01T00:00:0{}Z", i % 4),
                Some(host),
                json!([rule(
                    "v1",
                    json!([{ "headers": [{ "name": "x-tenant", "value": name }] }])
                )]),
            )
        })
        .collect();
    let mut by_claim: Vec<(String, String)> = routes
        .iter()
        .map(|r| {
            let created = r.metadata.creation_timestamp.as_ref().expect("a timestamp");
            (
                serde_json::to_string(created).expect("a timestamp"),
                r.metadata.name.clone().expect("a name"),
            )
        })
        .collect();
    by_claim.sort();
    let kept: BTreeSet<String> = by_claim
        .iter()
        .take(MAX_HEADER_MATCHES_PER_HOSTNAME)
        .map(|(_, name)| name.clone())
        .collect();
    let outcome = |routes: Vec<HttpRoute>| {
        let out = build_routes(routes);
        let programmed: BTreeSet<String> = out
            .ir
            .frontends
            .iter()
            .flat_map(|f| f.header_matches.iter())
            .map(|m| match &m.kind {
                ir::HeaderMatchKind::Exact(value) => value.clone(),
                ir::HeaderMatchKind::Regex(value) => value.clone(),
            })
            .collect();
        let mut problems: Vec<(String, Vec<Problem>)> = out
            .routes
            .iter()
            .map(|r| (r.name.clone(), r.parents[0].problems.clone()))
            .collect();
        problems.sort_by(|a, b| a.0.cmp(&b.0));
        (shown(&out), programmed, problems)
    };
    let (frontends, programmed, problems) = outcome(routes.clone());
    assert_eq!(programmed, kept);
    for (name, problems) in &problems {
        if kept.contains(name) {
            assert!(problems.is_empty(), "{name}: {problems:?}");
        } else {
            assert_eq!(
                problems,
                &vec![Problem::HeaderMatchLimit {
                    hostname: host.to_string(),
                    limit: MAX_HEADER_MATCHES_PER_HOSTNAME,
                }],
                "{name}"
            );
        }
    }
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut orders = vec![routes.iter().rev().cloned().collect::<Vec<_>>()];
    for _ in 0..4 {
        let mut shuffled = routes.clone();
        for i in (1..shuffled.len()).rev() {
            shuffled.swap(i, rng.below(i + 1));
        }
        orders.push(shuffled);
    }
    for order in orders {
        assert_eq!(
            outcome(order),
            (frontends.clone(), programmed.clone(), problems.clone())
        );
    }
}

/// A deterministic xorshift generator, so the property below replays the same
/// cases on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// One generated HTTPRoute match: a path type and path, a method and header
/// matches as (name, regex, value).
#[derive(Debug, Clone)]
struct Spec {
    path_type: &'static str,
    path: &'static str,
    method: Option<&'static str>,
    headers: Vec<(&'static str, bool, &'static str)>,
}

impl Spec {
    /// A match, with a `RegularExpression` path one time in four when
    /// `regex_paths`.
    fn generate(rng: &mut Rng, header_matched: bool, regex_paths: bool) -> Spec {
        const PATHS: [&str; 4] = ["/", "/a", "/a/b", "/b"];
        const REGEX_PATHS: [&str; 4] = ["/a.*", "/(a|b)(/.*)?", "/a/b(\\?.*)?", "/.*"];
        const METHODS: [Option<&str>; 3] = [None, Some("GET"), Some("POST")];
        const HEADERS: [(&str, bool, &str); 5] = [
            ("x", false, "1"),
            ("x", false, "2"),
            ("x", true, "1|3"),
            ("y", false, "1"),
            ("z", true, ".*"),
        ];
        let mut headers: Vec<(&str, bool, &str)> = Vec::new();
        if header_matched {
            for _ in 0..1 + rng.below(2) {
                let header = HEADERS[rng.below(HEADERS.len())];
                if headers.iter().all(|(name, ..)| *name != header.0) {
                    headers.push(header);
                }
            }
        }
        let (path_type, path) = match rng.below(if regex_paths { 4 } else { 3 }) {
            0 => ("Exact", PATHS[rng.below(PATHS.len())]),
            3 => (
                "RegularExpression",
                REGEX_PATHS[rng.below(REGEX_PATHS.len())],
            ),
            _ => ("PathPrefix", PATHS[rng.below(PATHS.len())]),
        };
        Spec {
            path_type,
            path,
            method: METHODS[rng.below(METHODS.len())],
            headers,
        }
    }

    fn to_match(&self) -> serde_json::Value {
        let mut m = json!({ "path": { "type": self.path_type, "value": self.path } });
        if let Some(method) = self.method {
            m["method"] = json!(method);
        }
        if !self.headers.is_empty() {
            m["headers"] = self
                .headers
                .iter()
                .map(|(name, regex, value)| {
                    json!({ "name": name, "value": value,
                        "type": if *regex { "RegularExpression" } else { "Exact" } })
                })
                .collect();
        }
        m
    }
}

/// Gateway API precedence of a programmed frontend, written from the spec
/// alone: an Exact path, then the longest prefix, then a method, then the most
/// header matches; the older route (`index`) breaks a tie.
fn gateway_api_rank(
    f: &ir::Frontend,
    index: usize,
) -> (bool, usize, bool, usize, std::cmp::Reverse<usize>) {
    let (exact, length) = match &f.path {
        ir::PathMatch::Exact(_) => (true, 0),
        ir::PathMatch::Prefix(prefix) => (false, prefix.trim_end_matches('/').len()),
        ir::PathMatch::Regex(_) => unreachable!("checked for determinism only"),
    };
    (
        exact,
        length,
        f.method.is_some(),
        f.header_matches.len(),
        std::cmp::Reverse(index),
    )
}

/// Introducing header matches never makes Sōzu serve a request the Gateway API
/// would not, beyond what the same plain routes already do without them, in
/// any order Sōzu may hold the frontends: a header match the builder skipped is
/// simply absent. Each generated match is its own route to its own backend,
/// routes ordered by age, and plain routes may overlap.
///
/// A request no header-matched frontend (a header match or a copy) can serve
/// is in Sōzu's plain class alone: it must be served exactly as a build of the
/// plain routes alone serves it. That answer may itself differ from the
/// Gateway API's, and is allowed to: two plain routes one request can match
/// are ordered by Sōzu's own rules and by insertion order, whatever this
/// builder does (#80), and the generator reaches that inherited limitation
/// (counted below). A request a header-matched frontend can serve must reach
/// the Gateway API's choice among the programmed routes in every order, even
/// where the plain routes alone would not: that class is entirely this
/// builder's doing. One case in three also draws `RegularExpression` paths,
/// whose precedence is the implementation's: there such a request must still
/// reach one backend whatever the order.
#[test]
fn programmed_header_matches_route_by_gateway_api_precedence_in_every_order() {
    const TARGETS: [&str; 10] = [
        "/", "/a", "/a/", "/a/b", "/a/b/c", "/a?q", "/ab", "/b", "/b/c", "/c",
    ];
    const METHODS: [&str; 3] = ["GET", "POST", "PUT"];
    const HEADER_SETS: [&[(&str, &str)]; 10] = [
        &[],
        &[("x", "1")],
        &[("x", "2")],
        &[("x", "3")],
        &[("y", "1")],
        &[("z", "q")],
        &[("x", "1"), ("y", "1")],
        &[("x", "2"), ("y", "1")],
        &[("x", "3"), ("z", "q")],
        &[("x", "1"), ("y", "1"), ("z", "q")],
    ];
    let services: Vec<String> = (0..8).map(|i| format!("s{i}")).collect();
    let services: Vec<&str> = services.iter().map(String::as_str).collect();
    let requests: Vec<Request> = TARGETS
        .iter()
        .flat_map(|target| {
            METHODS.iter().flat_map(move |method| {
                HEADER_SETS.iter().map(move |headers| Request {
                    method,
                    target,
                    headers,
                })
            })
        })
        .collect();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut checked, mut skipped_matches, mut copies, mut regex_cases) = (0, 0, 0, 0);
    let (mut overlapping, mut inherited) = (0, 0);
    for case in 0..900 {
        let hostname = (case % 5 != 4).then_some("app.example.com");
        let host = hostname.unwrap_or("*");
        let regex_paths = case % 3 == 2;
        let plain = rng.below(4);
        let specs: Vec<Spec> = (0..plain + 1 + rng.below(4))
            .map(|i| Spec::generate(&mut rng, i >= plain, regex_paths))
            .collect();
        let route_of = |i: usize, spec: &Spec| {
            route_at(
                &format!("r{i}"),
                &format!("2026-01-01T00:00:{i:02}Z"),
                hostname,
                json!([rule(&format!("s{i}"), json!([spec.to_match()]))]),
            )
        };
        let routes = specs
            .iter()
            .enumerate()
            .map(|(i, s)| route_of(i, s))
            .collect();
        let out = build(&BuildConfig::default(), &inputs_with(routes, &services));
        // The same plain routes, without a single header match.
        let plain_routes = specs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.headers.is_empty())
            .map(|(i, s)| route_of(i, s))
            .collect();
        let plain_out = build(
            &BuildConfig::default(),
            &inputs_with(plain_routes, &services),
        );
        let frontends = held_on(&out, host);
        let plain_frontends = held_on(&plain_out, host);
        // The route each frontend belongs to, and whether it is a copy lifted
        // above a header match (a plain route's backend with header matches).
        let origin = |h: &Held| -> (usize, bool) {
            let cluster = h.frontend.cluster_id.as_deref().expect("a backend");
            let index: usize = cluster
                .trim_start_matches("demo.s")
                .trim_end_matches(".80")
                .parse()
                .expect("one of ours");
            (
                index,
                specs[index].headers.is_empty() && !h.headers.is_empty(),
            )
        };
        let shown_case = |request: &Request| {
            format!(
                "case {case}: {} {}{:?} on {specs:#?}\nprogrammed {:#?}\nproblems {:#?}",
                request.method,
                request.target,
                request.headers,
                shown(&out),
                out.routes
                    .iter()
                    .flat_map(|r| r.parents.iter().map(move |p| (&r.name, &p.problems)))
                    .filter(|(_, problems)| !problems.is_empty())
                    .collect::<Vec<_>>()
            )
        };
        if plain_frontends.iter().enumerate().any(|(i, a)| {
            plain_frontends
                .iter()
                .skip(i + 1)
                .any(|b| requests.iter().any(|r| matches(a, r) && matches(b, r)))
        }) {
            overlapping += 1;
        }
        for request in &requests {
            let got = served_by(&frontends, request);
            if !frontends
                .iter()
                .any(|h| !h.headers.is_empty() && matches(h, request))
            {
                let alone = served_by(&plain_frontends, request);
                assert_eq!(got, alone, "{}", shown_case(request));
                if !regex_paths {
                    let want = plain_frontends
                        .iter()
                        .filter(|h| matches(h, request))
                        .max_by_key(|h| gateway_api_rank(h.frontend, origin(h).0))
                        .map(|h| h.frontend);
                    if alone != BTreeSet::from([backend(want)]) {
                        inherited += 1;
                    }
                }
                continue;
            }
            if regex_paths {
                assert_eq!(got.len(), 1, "{}", shown_case(request));
                continue;
            }
            let want = frontends
                .iter()
                .filter(|h| !origin(h).1 && matches(h, request))
                .max_by_key(|h| gateway_api_rank(h.frontend, origin(h).0))
                .map(|h| h.frontend);
            assert_eq!(
                got,
                BTreeSet::from([backend(want)]),
                "{}",
                shown_case(request)
            );
        }
        assert_translates(&out);
        if regex_paths {
            regex_cases += 1;
            continue;
        }
        checked += 1;
        copies += frontends.iter().filter(|h| origin(h).1).count();
        skipped_matches += specs.iter().filter(|s| !s.headers.is_empty()).count()
            - frontends
                .iter()
                .filter(|h| !h.headers.is_empty() && !origin(h).1)
                .count();
    }
    // The generator must reach kept and skipped header matches, copies,
    // regex paths, overlapping plain routes and the order they inherit (600
    // cases checked against the reference, 327 header matches skipped, 162
    // copies, 300 cases with regex paths, 277 cases with overlapping plain
    // routes and 2486 requests in their inherited order when written).
    assert!(checked > 400, "{checked} cases checked");
    assert!(
        skipped_matches > 200,
        "{skipped_matches} header matches skipped"
    );
    assert!(copies > 100, "{copies} copies lifted");
    assert!(regex_cases > 200, "{regex_cases} cases with regex paths");
    assert!(
        overlapping > 150,
        "{overlapping} cases with overlapping plain routes"
    );
    assert!(
        inherited > 1000,
        "{inherited} requests in the inherited plain order"
    );
}

/// HTTPRouteHeaderMatching (Gateway API v1.6.1 conformance), a route without
/// hostnames: Sōzu serves the first header match it holds, so every two that
/// one request can meet with different backends lose the lower one. What
/// remains routes every request the Gateway API's way in every order; the
/// three skipped matches' own requests (`version: two`, `color: red`/`yellow`)
/// go elsewhere, which the conformance test counts as failures.
#[test]
fn the_header_matching_conformance_shape_keeps_what_sozu_orders() {
    let header = |name: &str, value: &str| json!({ "name": name, "value": value });
    let out = build_routes(vec![route_at(
        "header-matching",
        "2026-01-01T00:00:00Z",
        None,
        json!([
            rule("v1", json!([{ "headers": [header("version", "one")] }])),
            rule("v2", json!([{ "headers": [header("version", "two")] }])),
            rule(
                "v1",
                json!([{ "headers": [header("version", "two"), header("color", "orange")] }])
            ),
            rule(
                "v1",
                json!([
                    { "headers": [header("color", "blue")] },
                    { "headers": [header("color", "green")] }
                ])
            ),
            rule(
                "v2",
                json!([
                    { "headers": [header("color", "red")] },
                    { "headers": [header("color", "yellow")] }
                ])
            ),
        ]),
    )]);
    assert_eq!(
        shown(&out),
        sorted(&[
            "* P/ [version=one] -> demo.v1.80",
            "* P/ [color=orange,version=two] -> demo.v1.80",
            "* P/ [color=blue] -> demo.v1.80",
            "* P/ [color=green] -> demo.v1.80",
        ])
    );
    let skipped: Vec<&str> = parent(&out, "header-matching")
        .problems
        .iter()
        .map(|problem| match problem {
            Problem::HeaderMatchOverlap { header_match, .. } => header_match.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        skipped,
        [
            "prefix / with version=\"two\"",
            "prefix / with color=\"red\"",
            "prefix / with color=\"yellow\"",
        ]
    );
    for (headers, want) in [
        (&[("Version", "one")][..], "demo.v1.80"),
        (&[("Version", "two"), ("Color", "orange")][..], "demo.v1.80"),
        (&[("Color", "blue")][..], "demo.v1.80"),
        (&[("Color", "green")][..], "demo.v1.80"),
        (&[("Color", "purple")][..], "404"),
        // The skipped matches' own requests: the Gateway API wants v2.
        (&[("Version", "two")][..], "404"),
        (&[("Version", "two"), ("Color", "blue")][..], "demo.v1.80"),
        (&[("Color", "red")][..], "404"),
    ] {
        let request = Request {
            method: "GET",
            target: "/",
            headers,
        };
        assert_eq!(served(&out, "*", &request), only(want), "{headers:?}");
    }
    assert_translates(&out);
}
