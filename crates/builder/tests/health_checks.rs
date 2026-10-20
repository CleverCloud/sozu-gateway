//! Active health checks: what a Service's `sozu.io/health-check*` annotations
//! and the controller-wide default compile to, what is refused and reported,
//! and why a cluster carrying UDP never gets one.

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use serde_json::json;
use sozu_command_lib::proto::command::request::RequestType;
use sozu_command_lib::state::ConfigState;

use sozu_gw_builder::{
    build, health_check_refusal, BuildConfig, BuildOutput, ExposedPort, ExposedProtocol,
    HealthCheckDefault, Inputs, Problem,
};
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute, TcpRoute, UdpRoute};
use sozu_gw_ir as ir;

fn from_json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("valid k8s object json")
}

fn arcs<T>(items: Vec<T>) -> Vec<Arc<T>> {
    items.into_iter().map(Arc::new).collect()
}

/// Service `web` in `demo` (port 80 → 8080) carrying `annotations`.
fn web_service(annotations: &[(&str, &str)]) -> Service {
    let annotations: BTreeMap<&str, &str> = annotations.iter().copied().collect();
    from_json(json!({
        "metadata": { "name": "web", "namespace": "demo", "annotations": annotations },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }))
}

fn web_slice() -> EndpointSlice {
    from_json(json!({
        "metadata": { "name": "web-1", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "IPv4",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [{ "addresses": ["10.244.0.5"], "conditions": { "ready": true } }]
    }))
}

fn ingress(name: &str, host: &str) -> Ingress {
    from_json(json!({
        "metadata": { "name": name, "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{ "host": host, "http": { "paths": [
                { "path": "/", "pathType": "Prefix",
                  "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
            ]}}]
        }
    }))
}

fn web_inputs(annotations: &[(&str, &str)]) -> Inputs {
    Inputs {
        ingresses: arcs(vec![ingress("web", "app.example.com")]),
        services: arcs(vec![web_service(annotations)]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    }
}

fn with_default_on() -> BuildConfig {
    BuildConfig {
        health_check: HealthCheckDefault {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The only cluster's check.
fn check_of(out: &BuildOutput) -> Option<ir::HealthCheck> {
    assert_eq!(out.ir.clusters.len(), 1, "{:?}", out.ir.clusters);
    out.ir.clusters[0].health_check.clone()
}

fn health_problems(problems: &[Problem]) -> Vec<&Problem> {
    problems
        .iter()
        .filter(|p| {
            matches!(
                p,
                Problem::InvalidHealthCheck { .. } | Problem::HealthCheckNotApplied { .. }
            )
        })
        .collect()
}

const DEFAULT: ir::HealthCheck = HealthCheckDefault::DEFAULT_CHECK;

#[test]
fn the_default_is_off_and_a_service_turns_a_check_on_with_tcp() {
    assert!(!BuildConfig::default().health_check.enabled);
    assert_eq!(
        DEFAULT,
        ir::HealthCheck {
            interval_secs: 3,
            timeout_secs: 2,
            healthy_threshold: 2,
            unhealthy_threshold: 2,
            port: None,
        },
        "the timing the chart and docs promise"
    );

    let out = build(&BuildConfig::default(), &web_inputs(&[]));
    assert_eq!(check_of(&out), None);

    for spelling in ["tcp", "TCP", " Tcp "] {
        let out = build(
            &BuildConfig::default(),
            &web_inputs(&[("sozu.io/health-check", spelling)]),
        );
        assert_eq!(check_of(&out), Some(DEFAULT), "{spelling:?}");
        assert!(out.results[0].problems.is_empty(), "{spelling:?}");
    }
}

#[test]
fn a_tcp_default_reaches_every_service_and_off_opts_one_out() {
    let out = build(&with_default_on(), &web_inputs(&[]));
    assert_eq!(check_of(&out), Some(DEFAULT));

    let out = build(
        &with_default_on(),
        &web_inputs(&[("sozu.io/health-check", "off")]),
    );
    assert_eq!(check_of(&out), None);
    assert!(out.results[0].problems.is_empty());
}

#[test]
fn tuning_annotations_override_one_field_each() {
    let out = build(
        &BuildConfig::default(),
        &web_inputs(&[
            ("sozu.io/health-check", "tcp"),
            ("sozu.io/health-check-interval", "10"),
            ("sozu.io/health-check-timeout", " 4 "),
            ("sozu.io/health-check-healthy-threshold", "3"),
            ("sozu.io/health-check-unhealthy-threshold", "5"),
            ("sozu.io/health-check-port", "9090"),
        ]),
    );
    assert_eq!(
        check_of(&out),
        Some(ir::HealthCheck {
            interval_secs: 10,
            timeout_secs: 4,
            healthy_threshold: 3,
            unhealthy_threshold: 5,
            port: Some(9090),
        })
    );
    assert!(out.results[0].problems.is_empty());

    // Anything left unset keeps the controller's own timing, not Sōzu's.
    let mut cfg = with_default_on();
    cfg.health_check.check.interval_secs = 7;
    let out = build(&cfg, &web_inputs(&[("sozu.io/health-check-timeout", "1")]));
    assert_eq!(
        check_of(&out),
        Some(ir::HealthCheck {
            interval_secs: 7,
            timeout_secs: 1,
            ..DEFAULT
        })
    );
}

#[test]
fn tuning_a_check_that_is_off_changes_nothing_and_reports_nothing() {
    for cfg in [BuildConfig::default(), with_default_on()] {
        let out = build(
            &cfg,
            &web_inputs(&[
                ("sozu.io/health-check", "off"),
                ("sozu.io/health-check-interval", "soon"),
                ("sozu.io/health-check-port", "0"),
            ]),
        );
        assert_eq!(check_of(&out), None);
        assert!(out.results[0].problems.is_empty());
    }
    // Off by default, tuned but never turned on.
    let out = build(
        &BuildConfig::default(),
        &web_inputs(&[("sozu.io/health-check-interval", "5")]),
    );
    assert_eq!(check_of(&out), None);
}

#[test]
fn an_unusable_check_is_reported_and_programs_no_check_at_all() {
    // Each would either never reach Sōzu as written or be refused by
    // `ConfigState::add_cluster` — failing the whole reconcile, since
    // translation is all-or-nothing. The cluster gets no check rather than
    // the default the Service set out to change, even with the default on.
    let cases: &[(&[(&str, &str)], &str)] = &[
        (
            &[("sozu.io/health-check", "http")],
            "expected \"tcp\" or \"off\"",
        ),
        (
            &[("sozu.io/health-check", "")],
            "expected \"tcp\" or \"off\"",
        ),
        (
            &[("sozu.io/health-check", "true")],
            "expected \"tcp\" or \"off\"",
        ),
        (
            &[("sozu.io/health-check-interval", "5s")],
            "sozu.io/health-check-interval is \"5s\", not a whole number",
        ),
        (
            &[("sozu.io/health-check-interval", "4294967296")],
            "not a whole number",
        ),
        (
            &[("sozu.io/health-check-interval", "0")],
            "health check interval must be > 0",
        ),
        (
            &[("sozu.io/health-check-timeout", "0")],
            "health check timeout must be > 0",
        ),
        (
            &[("sozu.io/health-check-healthy-threshold", "0")],
            "healthy_threshold must be > 0",
        ),
        (
            &[("sozu.io/health-check-unhealthy-threshold", "-1")],
            "sozu.io/health-check-unhealthy-threshold is \"-1\", not a whole number",
        ),
        (
            &[("sozu.io/health-check-port", "0")],
            "health check port must lie in 1..=65535",
        ),
        (
            &[("sozu.io/health-check-port", "70000")],
            "sozu.io/health-check-port is \"70000\", not a port number",
        ),
        (
            &[("sozu.io/health-check-port", "metrics")],
            "not a port number",
        ),
    ];
    for (annotations, reason) in cases {
        let out = build(&with_default_on(), &web_inputs(annotations));
        assert_eq!(check_of(&out), None, "{annotations:?}");
        let problems = health_problems(&out.results[0].problems);
        assert_eq!(problems.len(), 1, "{annotations:?}: {problems:?}");
        let Problem::InvalidHealthCheck {
            service,
            reason: why,
        } = problems[0]
        else {
            panic!("{annotations:?}: {problems:?}");
        };
        assert_eq!(service, "demo/web");
        assert!(why.contains(reason), "{annotations:?}: {why:?}");
        // The rest of the Service still routes.
        assert_eq!(out.ir.backends.len(), 1);
        assert_eq!(out.ir.frontends.len(), 1);
        assert_eq!(problems[0].reason(), "InvalidHealthCheck");
        assert!(
            problems[0].to_string().contains("not health-checked"),
            "{}",
            problems[0]
        );
    }
}

#[test]
fn a_huge_annotation_value_is_cut_in_the_report() {
    let value = "x".repeat(10_000);
    let out = build(
        &BuildConfig::default(),
        &web_inputs(&[("sozu.io/health-check", &value)]),
    );
    let message = out.results[0].problems[0].to_string();
    assert!(message.len() < 400, "{} bytes", message.len());
}

#[test]
fn every_object_routing_to_an_unusable_check_is_told() {
    let mut inputs = web_inputs(&[("sozu.io/health-check", "on")]);
    inputs.ingresses = arcs(vec![
        ingress("a", "a.example.com"),
        ingress("b", "b.example.com"),
    ]);
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.results.len(), 2);
    for result in &out.results {
        assert_eq!(
            health_problems(&result.problems).len(),
            1,
            "{}: {:?}",
            result.name,
            result.problems
        );
    }
}

/// The builder's admission and the translator's emission are separate pieces
/// of code over one wire type: whatever the builder admits must be what
/// `ConfigState` accepts from the translator, and whatever it refuses must be
/// refused there too — otherwise a check could pass the builder and fail every
/// reconcile, or be refused for no reason.
#[test]
fn the_builder_admits_exactly_the_checks_sozu_accepts_from_the_translator() {
    let samples = [
        DEFAULT,
        ir::HealthCheck {
            port: Some(1),
            ..DEFAULT
        },
        ir::HealthCheck {
            port: Some(65535),
            interval_secs: u32::MAX,
            timeout_secs: u32::MAX,
            healthy_threshold: u32::MAX,
            unhealthy_threshold: u32::MAX,
        },
        ir::HealthCheck {
            interval_secs: 1,
            timeout_secs: 30,
            ..DEFAULT
        },
        ir::HealthCheck {
            interval_secs: 0,
            ..DEFAULT
        },
        ir::HealthCheck {
            timeout_secs: 0,
            ..DEFAULT
        },
        ir::HealthCheck {
            healthy_threshold: 0,
            ..DEFAULT
        },
        ir::HealthCheck {
            unhealthy_threshold: 0,
            ..DEFAULT
        },
        ir::HealthCheck {
            port: Some(0),
            ..DEFAULT
        },
    ];
    for check in samples {
        let graph = ir::Ir {
            clusters: vec![ir::Cluster {
                id: "demo.web.80".into(),
                load_balancing: ir::LbAlgorithm::RoundRobin,
                sticky_session: false,
                https_redirect: false,
                max_connections_per_ip: None,
                retry_after: None,
                http2: false,
                health_check: Some(check.clone()),
            }],
            ..Default::default()
        };
        let requests = sozu_gw_translator::ir_to_requests(&graph);
        let Some(RequestType::AddCluster(cluster)) = &requests[0].request_type else {
            panic!("{check:?}: {requests:?}");
        };
        assert_eq!(
            cluster.health_check,
            Some(sozu_gw_translator::health_check_config(&check))
        );
        let accepted = ConfigState::new().dispatch(&requests[0]).is_ok();
        assert_eq!(
            health_check_refusal(&check).is_none(),
            accepted,
            "{check:?}: builder and ConfigState disagree"
        );
        // The translator's own fold agrees, so a refused check can only ever
        // fail translation, never reach the socket.
        assert_eq!(
            sozu_gw_translator::reconcile(&ir::Ir::default(), &graph).is_ok(),
            accepted,
            "{check:?}"
        );
    }
}

#[test]
fn an_unusable_check_leaves_an_http_route_accepted_and_resolved() {
    let class: GatewayClass = from_json(json!({
        "metadata": { "name": "sozu" },
        "spec": { "controllerName": "sozu.io/gateway-controller" }
    }));
    let gateway: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu",
            "listeners": [{ "name": "http", "protocol": "HTTP", "port": 80 }] }
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![class]),
        gateways: arcs(vec![gateway]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service(&[("sozu.io/health-check-interval", "0")])]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&with_default_on(), &inputs);
    assert_eq!(check_of(&out), None);
    let parent = &out.routes[0].parents[0];
    assert!(parent.accepted && parent.resolved_refs, "{parent:?}");
    assert_eq!(health_problems(&parent.problems).len(), 1);
    assert_eq!(out.ir.frontends.len(), 1, "the route still routes");
}

// ---------------------------------------------------------------------------
// Clusters nothing routes to
// ---------------------------------------------------------------------------

/// Service `name` in `demo` (port 80 → 8080) with one ready endpoint at `ip`.
fn http_backend(name: &str, ip: &str) -> (Service, EndpointSlice) {
    (
        from_json(json!({
            "metadata": { "name": name, "namespace": "demo" },
            "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
        })),
        from_json(json!({
            "metadata": { "name": format!("{name}-1"), "namespace": "demo",
                "labels": { "kubernetes.io/service-name": name } },
            "addressType": "IPv4",
            "ports": [{ "name": "http", "port": 8080 }],
            "endpoints": [{ "addresses": [ip], "conditions": { "ready": true } }]
        })),
    )
}

/// The checks every `AddCluster` the translator emits for `graph` carries,
/// by cluster id: what Sōzu would actually probe.
fn emitted_checks(graph: &ir::Ir) -> BTreeMap<String, bool> {
    sozu_gw_translator::ir_to_requests(graph)
        .into_iter()
        .filter_map(|request| match request.request_type {
            Some(RequestType::AddCluster(cluster)) => {
                Some((cluster.cluster_id, cluster.health_check.is_some()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_route_collision_loser_leaves_its_cluster_unchecked() {
    // Two Ingresses claim app.example.com/ for two Services: the older one
    // wins the route key, the newer one routes nothing, and its Service's
    // cluster — created when its backend resolved, before arbitration — must
    // not send TCP probes to backends no request can reach.
    let (web, web_slice) = http_backend("web", "10.244.0.5");
    let (api, api_slice) = http_backend("api", "10.244.0.6");
    let ingress_to = |name: &str, service: &str, created: &str| -> Ingress {
        from_json(json!({
            "metadata": { "name": name, "namespace": "demo", "creationTimestamp": created },
            "spec": {
                "ingressClassName": "sozu",
                "rules": [{ "host": "app.example.com", "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": service, "port": { "number": 80 } } } }
                ]}}]
            }
        }))
    };
    let inputs = Inputs {
        ingresses: arcs(vec![
            ingress_to("older", "web", "2026-01-01T00:00:00Z"),
            ingress_to("newer", "api", "2026-02-01T00:00:00Z"),
        ]),
        services: arcs(vec![web, api]),
        endpointslices: arcs(vec![web_slice, api_slice]),
        ..Default::default()
    };
    let out = build(&with_default_on(), &inputs);
    assert_eq!(out.ir.frontends.len(), 1);
    assert_eq!(
        out.ir.frontends[0].cluster_id.as_deref(),
        Some("demo.web.80")
    );
    let newer = out.results.iter().find(|r| r.name == "newer").unwrap();
    assert!(
        newer
            .problems
            .iter()
            .any(|p| matches!(p, Problem::RouteCollision { .. })),
        "{:?}",
        newer.problems
    );
    assert_eq!(check_on(&out, "demo.web.80"), Some(&DEFAULT));
    assert_eq!(check_on(&out, "demo.api.80"), None);
    assert!(health_problems(&newer.problems).is_empty());
    assert_eq!(
        emitted_checks(&out.ir),
        BTreeMap::from([
            ("demo.api.80".to_string(), false),
            ("demo.web.80".to_string(), true),
        ])
    );
}

#[test]
fn a_rule_whose_every_match_is_skipped_leaves_its_cluster_unchecked() {
    // A query match is reported and skipped after the backend resolved: the
    // rule programs no frontend, so its cluster carries nothing to check.
    let class: GatewayClass = from_json(json!({
        "metadata": { "name": "sozu" },
        "spec": { "controllerName": "sozu.io/gateway-controller" }
    }));
    let gateway: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu",
            "listeners": [{ "name": "http", "protocol": "HTTP", "port": 80 }] }
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "rules": [{
                "matches": [{ "queryParams": [{ "name": "canary", "value": "1" }] }],
                "backendRefs": [{ "name": "web", "port": 80 }]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![class]),
        gateways: arcs(vec![gateway]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service(&[])]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&with_default_on(), &inputs);
    assert!(out.ir.frontends.is_empty(), "{:?}", out.ir.frontends);
    assert_eq!(check_of(&out), None);
}

#[test]
fn a_header_match_skipped_for_precedence_leaves_its_cluster_unchecked() {
    // A plain Exact `/foo/bar` outranks a header match on the prefix `/foo`,
    // and Sōzu cannot order the plain route's copy above it in every order,
    // so the header match is skipped after collision resolution
    // (`HeaderMatchOutranksRoute`). Nothing then routes to its Service: no
    // more than a collision loser may its cluster keep a check — nor through
    // an identical header match from another route, folded into the skipped
    // one, which Sōzu would have served through that same frontend.
    let (web, web_slice) = http_backend("web", "10.244.0.5");
    let (api, api_slice) = http_backend("api", "10.244.0.6");
    let class: GatewayClass = from_json(json!({
        "metadata": { "name": "sozu" },
        "spec": { "controllerName": "sozu.io/gateway-controller" }
    }));
    let gateway: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu",
            "listeners": [{ "name": "http", "protocol": "HTTP", "port": 80 }] }
    }));
    let route = |name: &str, created: &str, matched: serde_json::Value, service: &str| {
        from_json::<HttpRoute>(json!({
            "metadata": { "name": name, "namespace": "demo", "creationTimestamp": created },
            "spec": {
                "parentRefs": [{ "name": "gw" }],
                "hostnames": ["app.example.com"],
                "rules": [{
                    "matches": [matched],
                    "backendRefs": [{ "name": service, "port": 80 }]
                }]
            }
        }))
    };
    let canary = json!({
        "path": { "type": "PathPrefix", "value": "/foo" },
        "headers": [{ "name": "x-canary", "value": "1" }]
    });
    let inputs = Inputs {
        gateway_classes: arcs(vec![class]),
        gateways: arcs(vec![gateway]),
        http_routes: arcs(vec![
            route(
                "plain",
                "2026-01-01T00:00:00Z",
                json!({ "path": { "type": "Exact", "value": "/foo/bar" } }),
                "api",
            ),
            route("canary", "2026-02-01T00:00:00Z", canary.clone(), "web"),
            route("canary-twin", "2026-03-01T00:00:00Z", canary, "web"),
        ]),
        services: arcs(vec![web, api]),
        endpointslices: arcs(vec![web_slice, api_slice]),
        ..Default::default()
    };
    let out = build(&with_default_on(), &inputs);
    assert_eq!(out.ir.frontends.len(), 1, "{:?}", out.ir.frontends);
    assert_eq!(
        out.ir.frontends[0].cluster_id.as_deref(),
        Some("demo.api.80")
    );
    let canary = out.routes.iter().find(|r| r.name == "canary").unwrap();
    assert!(
        canary.parents.iter().any(|p| p
            .problems
            .iter()
            .any(|p| matches!(p, Problem::HeaderMatchOutranksRoute { .. }))),
        "{:?}",
        canary.parents
    );
    assert_eq!(check_on(&out, "demo.api.80"), Some(&DEFAULT));
    assert_eq!(check_on(&out, "demo.web.80"), None);
    assert!(route_problems(&out, "canary").is_empty());
    assert!(route_problems(&out, "canary-twin").is_empty());
    assert_eq!(
        emitted_checks(&out.ir),
        BTreeMap::from([
            ("demo.api.80".to_string(), true),
            ("demo.web.80".to_string(), false),
        ])
    );
}

// ---------------------------------------------------------------------------
// UDP
// ---------------------------------------------------------------------------

fn l4_config(default_on: bool) -> BuildConfig {
    let mut cfg = BuildConfig::default();
    cfg.health_check.enabled = default_on;
    for (name, port, protocol, transport) in [
        ("dns-tcp", 5353, ExposedProtocol::Tcp, "TCP"),
        ("dns-udp", 5354, ExposedProtocol::Udp, "UDP"),
        ("dns-tcp2", 5355, ExposedProtocol::Tcp, "TCP"),
    ] {
        cfg.exposure.push(ExposedPort {
            name: name.to_string(),
            port,
            bind: port,
            protocol,
            transport: Some(transport.to_string()),
            owner: None,
        });
    }
    cfg
}

/// A Service serving 53 over TCP and over UDP: one Service port number, so one
/// cluster id (`demo.<name>.53`) whatever the route kind.
fn dns_service(name: &str, annotations: &[(&str, &str)]) -> Service {
    let annotations: BTreeMap<&str, &str> = annotations.iter().copied().collect();
    from_json(json!({
        "metadata": { "name": name, "namespace": "demo", "annotations": annotations },
        "spec": { "ports": [
            { "name": "dns-tcp", "port": 53, "protocol": "TCP", "targetPort": 53 },
            { "name": "dns-udp", "port": 53, "protocol": "UDP", "targetPort": 53 }
        ] }
    }))
}

fn dns_slice(name: &str, ip: &str) -> EndpointSlice {
    from_json(json!({
        "metadata": { "name": format!("{name}-1"), "namespace": "demo",
            "labels": { "kubernetes.io/service-name": name } },
        "addressType": "IPv4",
        "ports": [
            { "name": "dns-tcp", "port": 53, "protocol": "TCP" },
            { "name": "dns-udp", "port": 53, "protocol": "UDP" }
        ],
        "endpoints": [{ "addresses": [ip], "conditions": { "ready": true } }]
    }))
}

/// A TCPRoute or UDPRoute `name` to `service`:53, attached to the listener
/// named `listener`, or to every listener of its kind with `None`.
fn l4_route<T: serde::de::DeserializeOwned>(
    name: &str,
    listener: Option<&str>,
    service: &str,
    created: &str,
) -> T {
    let mut parent = json!({ "name": "gw" });
    if let Some(listener) = listener {
        parent["sectionName"] = json!(listener);
    }
    from_json(json!({
        "metadata": { "name": name, "namespace": "demo", "creationTimestamp": created },
        "spec": {
            "parentRefs": [parent],
            "rules": [{ "backendRefs": [{ "name": service, "port": 53 }] }]
        }
    }))
}

fn tcp_route(name: &str, listener: Option<&str>, service: &str, created: &str) -> TcpRoute {
    l4_route(name, listener, service, created)
}

fn udp_route(name: &str, listener: Option<&str>, service: &str, created: &str) -> UdpRoute {
    l4_route(name, listener, service, created)
}

/// The Gateway's listeners: an HTTP one, two TCP ones and a UDP one.
fn dns_world(annotations: &[(&str, &str)]) -> Inputs {
    let class: GatewayClass = from_json(json!({
        "metadata": { "name": "sozu" },
        "spec": { "controllerName": "sozu.io/gateway-controller" }
    }));
    let gateway: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80 },
            { "name": "dns-tcp", "protocol": "TCP", "port": 5353 },
            { "name": "dns-udp", "protocol": "UDP", "port": 5354 },
            { "name": "dns-tcp2", "protocol": "TCP", "port": 5355 }
        ]}
    }));
    Inputs {
        gateway_classes: arcs(vec![class]),
        gateways: arcs(vec![gateway]),
        services: arcs(vec![
            dns_service("dns", annotations),
            dns_service("other", &[]),
        ]),
        endpointslices: arcs(vec![
            dns_slice("dns", "10.244.0.12"),
            dns_slice("other", "10.244.0.13"),
        ]),
        ..Default::default()
    }
}

const OLD: &str = "2026-01-01T00:00:00Z";
const NEW: &str = "2026-02-01T00:00:00Z";

/// `dns`, with the TCPRoute `dns-tcp` and/or the UDPRoute `dns-udp` routing
/// to it, and an Ingress too when `ingress_too`.
fn dns_inputs(annotations: &[(&str, &str)], tcp: bool, udp: bool, ingress_too: bool) -> Inputs {
    let mut inputs = dns_world(annotations);
    if tcp {
        inputs.tcp_routes = arcs(vec![tcp_route("dns-tcp", Some("dns-tcp"), "dns", OLD)]);
    }
    if udp {
        inputs.udp_routes = arcs(vec![udp_route("dns-udp", Some("dns-udp"), "dns", OLD)]);
    }
    if ingress_too {
        inputs.ingresses = arcs(vec![from_json(json!({
            "metadata": { "name": "dns-over-http", "namespace": "demo" },
            "spec": {
                "ingressClassName": "sozu",
                "rules": [{ "host": "dns.example.com", "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "dns", "port": { "number": 53 } } } }
                ]}}]
            }
        }))]);
    }
    inputs
}

/// The health-check problems on each parent of the route named `name`.
fn route_problems<'a>(out: &'a BuildOutput, name: &str) -> Vec<&'a Problem> {
    let route = out
        .routes
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no route {name}"));
    route
        .parents
        .iter()
        .flat_map(|p| health_problems(&p.problems))
        .collect()
}

/// The check on cluster `id`.
fn check_on<'a>(out: &'a BuildOutput, id: &str) -> Option<&'a ir::HealthCheck> {
    out.ir
        .clusters
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("no cluster {id}: {:?}", out.ir.clusters))
        .health_check
        .as_ref()
}

#[test]
fn a_tcp_route_keeps_its_check() {
    let out = build(&l4_config(true), &dns_inputs(&[], true, false, false));
    assert_eq!(check_on(&out, "demo.dns.53"), Some(&DEFAULT));
    assert!(route_problems(&out, "dns-tcp").is_empty());
}

#[test]
fn a_tcp_route_sharing_its_service_port_with_a_udp_route_loses_its_check_and_is_told() {
    for (default_on, annotations) in [
        (true, &[][..]),
        (false, &[("sozu.io/health-check", "tcp")][..]),
    ] {
        let out = build(
            &l4_config(default_on),
            &dns_inputs(annotations, true, true, false),
        );
        assert_eq!(out.ir.l4_frontends.len(), 2);
        assert!(out
            .ir
            .l4_frontends
            .iter()
            .all(|f| f.cluster_id == "demo.dns.53"));
        assert_eq!(
            check_on(&out, "demo.dns.53"),
            None,
            "one UDP cluster runs no TCP check"
        );

        let tcp = route_problems(&out, "dns-tcp");
        assert_eq!(tcp.len(), 1, "{tcp:?}");
        let Problem::HealthCheckNotApplied { service, reason } = tcp[0] else {
            panic!("{tcp:?}");
        };
        assert_eq!(service, "demo/dns");
        assert!(reason.contains("UDPRoute demo/dns-udp"), "{reason}");
        assert_eq!(tcp[0].reason(), "HealthCheckNotApplied");

        let udp = route_problems(&out, "dns-udp");
        assert_eq!(udp.len(), 1, "{udp:?}");
        assert!(
            udp[0].to_string().contains("TCP or HTTP traffic"),
            "{}",
            udp[0]
        );
        // Reported, not refused: both routes still route.
        for route in &out.routes {
            assert!(route.parents[0].accepted && route.parents[0].resolved_refs);
        }
    }
}

#[test]
fn removing_the_udp_route_gives_the_check_back() {
    let shared = build(&l4_config(true), &dns_inputs(&[], true, true, false));
    assert_eq!(check_on(&shared, "demo.dns.53"), None);
    let alone = build(&l4_config(true), &dns_inputs(&[], true, false, false));
    assert_eq!(check_on(&alone, "demo.dns.53"), Some(&DEFAULT));
}

#[test]
fn a_udp_route_that_loses_its_socket_takes_no_check_away() {
    // `other` holds the UDP socket (older); `dns-udp` loses it, so no UDP
    // frontend names `dns`'s cluster and its TCP route keeps the check.
    let mut inputs = dns_world(&[]);
    inputs.tcp_routes = arcs(vec![tcp_route("dns-tcp", Some("dns-tcp"), "dns", OLD)]);
    inputs.udp_routes = arcs(vec![
        udp_route("other-udp", Some("dns-udp"), "other", OLD),
        udp_route("dns-udp", Some("dns-udp"), "dns", NEW),
    ]);
    let out = build(&l4_config(true), &inputs);
    assert_eq!(check_on(&out, "demo.dns.53"), Some(&DEFAULT));
    assert!(route_problems(&out, "dns-tcp").is_empty());
    assert!(route_problems(&out, "dns-udp").is_empty());
    assert_eq!(check_on(&out, "demo.other.53"), None);
}

#[test]
fn a_udp_route_that_loses_its_socket_leaves_its_cluster_unchecked() {
    // `dns-udp` is the only route to `dns` and loses the UDP socket to the
    // older `other-udp`: no frontend names `demo.dns.53`, so no UDP frontend
    // makes it a UDP cluster either. With a check left on it, the translator
    // would send a TCP-mode `AddCluster` and every worker would TCP-probe
    // backends that only ever serve datagrams.
    for (default_on, annotations) in [
        (true, &[][..]),
        (false, &[("sozu.io/health-check", "tcp")][..]),
    ] {
        let mut inputs = dns_world(annotations);
        inputs.udp_routes = arcs(vec![
            udp_route("other-udp", Some("dns-udp"), "other", OLD),
            udp_route("dns-udp", Some("dns-udp"), "dns", NEW),
        ]);
        let out = build(&l4_config(default_on), &inputs);
        assert_eq!(out.ir.l4_frontends.len(), 1);
        assert_eq!(out.ir.l4_frontends[0].cluster_id, "demo.other.53");
        assert_eq!(check_on(&out, "demo.dns.53"), None);
        assert_eq!(check_on(&out, "demo.other.53"), None);
        assert!(
            emitted_checks(&out.ir).values().all(|checked| !checked),
            "{:?}",
            emitted_checks(&out.ir)
        );
        // It routes nothing, so it is told about its socket, not a check.
        assert!(route_problems(&out, "dns-udp").is_empty());
    }
}

#[test]
fn every_tcp_route_on_the_shared_cluster_is_told_once_per_parent() {
    // `dns-tcp-dup` claims `dns-tcp`'s socket for the same Service — a benign
    // duplicate that never wins a socket of its own — and `dns-tcp-both`
    // attaches to both TCP listeners through one parentRef without
    // sectionName: two claims, one parent.
    let mut inputs = dns_world(&[]);
    inputs.tcp_routes = arcs(vec![
        tcp_route("dns-tcp", Some("dns-tcp"), "dns", OLD),
        tcp_route("dns-tcp-dup", Some("dns-tcp"), "dns", NEW),
        tcp_route("dns-tcp-both", None, "dns", NEW),
    ]);
    inputs.udp_routes = arcs(vec![udp_route("dns-udp", Some("dns-udp"), "dns", OLD)]);
    let out = build(&l4_config(true), &inputs);
    assert_eq!(check_on(&out, "demo.dns.53"), None);
    assert_eq!(route_problems(&out, "dns-tcp").len(), 1);
    assert_eq!(route_problems(&out, "dns-tcp-dup").len(), 1);
    let both = route_problems(&out, "dns-tcp-both");
    assert_eq!(both.len(), 1, "one parentRef, one problem: {both:?}");
}

/// The one health-check problem on the Ingress or HTTPRoute that lost its
/// check to `UDPRoute demo/dns-udp`.
fn assert_told_once_about_dns_udp(problems: Vec<&Problem>) {
    assert_eq!(problems.len(), 1, "{problems:?}");
    let Problem::HealthCheckNotApplied { service, reason } = problems[0] else {
        panic!("{problems:?}");
    };
    assert_eq!(service, "demo/dns");
    assert!(reason.contains("UDPRoute demo/dns-udp"), "{reason}");
}

#[test]
fn an_ingress_sharing_its_service_port_with_a_udp_route_is_told_and_so_is_the_udp_route() {
    for (default_on, annotations) in [
        (true, &[][..]),
        (false, &[("sozu.io/health-check", "tcp")][..]),
    ] {
        let mut inputs = dns_inputs(annotations, false, true, true);
        // Two hosts, and a second path on one: three frontends of one owner,
        // one problem.
        let mut ingress = (*inputs.ingresses[0]).clone();
        let spec = ingress.spec.as_mut().unwrap();
        let mut rules = spec.rules.clone().unwrap();
        let mut second = rules[0].clone();
        second.host = Some("dns2.example.com".to_string());
        let http = rules[0].http.as_mut().unwrap();
        let mut path = http.paths[0].clone();
        path.path = Some("/resolve".to_string());
        http.paths.push(path);
        rules.push(second);
        spec.rules = Some(rules);
        inputs.ingresses = arcs(vec![ingress]);

        let out = build(&l4_config(default_on), &inputs);
        assert_eq!(out.ir.frontends.len(), 3, "{:?}", out.ir.frontends);
        assert_eq!(check_on(&out, "demo.dns.53"), None);
        assert_eq!(route_problems(&out, "dns-udp").len(), 1);
        assert_eq!(out.results.len(), 1);
        assert_told_once_about_dns_udp(health_problems(&out.results[0].problems));
    }

    // Off: nothing lost, nothing said.
    let out = build(
        &l4_config(true),
        &dns_inputs(&[("sozu.io/health-check", "off")], false, true, true),
    );
    assert!(health_problems(&out.results[0].problems).is_empty());
    assert!(route_problems(&out, "dns-udp").is_empty());
}

#[test]
fn an_http_route_sharing_its_service_port_with_a_udp_route_is_told_and_so_is_the_udp_route() {
    let mut inputs = dns_inputs(&[], false, true, false);
    // Two rules to the same Service: two frontends of one parent, one problem.
    inputs.http_routes = arcs(vec![from_json::<HttpRoute>(json!({
        "metadata": { "name": "dns-over-http", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw", "sectionName": "http" }],
            "rules": [
                { "matches": [{ "path": { "type": "PathPrefix", "value": "/a" } }],
                  "backendRefs": [{ "name": "dns", "port": 53 }] },
                { "matches": [{ "path": { "type": "PathPrefix", "value": "/b" } }],
                  "backendRefs": [{ "name": "dns", "port": 53 }] }
            ]
        }
    }))]);
    let out = build(&l4_config(true), &inputs);
    assert_eq!(out.ir.frontends.len(), 2, "{:?}", out.routes);
    assert_eq!(check_on(&out, "demo.dns.53"), None);
    assert_eq!(route_problems(&out, "dns-udp").len(), 1);
    assert_told_once_about_dns_udp(route_problems(&out, "dns-over-http"));
    // Reported, not refused: the route still reads healthy.
    let parent = &out
        .routes
        .iter()
        .find(|r| r.name == "dns-over-http")
        .unwrap()
        .parents[0];
    assert!(parent.accepted && parent.resolved_refs, "{parent:?}");
}

#[test]
fn identical_ingresses_sharing_a_udp_routed_cluster_are_each_told() {
    // Two Ingresses asking for the identical host and path on the same Service
    // port: one route key, one effect, so arbitration keeps the older one's
    // frontend and folds the other in without a collision — both still route,
    // both through the cluster the UDPRoute strips of its check.
    let mut inputs = dns_inputs(&[], false, true, true);
    let older = (*inputs.ingresses[0]).clone();
    let mut newer = older.clone();
    newer.metadata.name = Some("dns-over-http-dup".to_string());
    newer.metadata.creation_timestamp = Some(from_json(json!(NEW)));
    let mut older = older;
    older.metadata.creation_timestamp = Some(from_json(json!(OLD)));
    inputs.ingresses = arcs(vec![newer, older]);

    let out = build(&l4_config(true), &inputs);
    assert_eq!(out.ir.frontends.len(), 1, "{:?}", out.ir.frontends);
    assert_eq!(check_on(&out, "demo.dns.53"), None);
    assert_eq!(out.results.len(), 2);
    for result in &out.results {
        assert!(
            !result
                .problems
                .iter()
                .any(|p| matches!(p, Problem::RouteCollision { .. })),
            "{}: a benign duplicate, not a collision: {:?}",
            result.name,
            result.problems
        );
        assert_told_once_about_dns_udp(health_problems(&result.problems));
    }
    assert_eq!(route_problems(&out, "dns-udp").len(), 1);
}

#[test]
fn identical_http_route_parents_sharing_a_udp_routed_cluster_are_each_told() {
    // `dns-over-http` attaches to the HTTP listeners of two Gateways, which
    // share the static HTTP listener: two parents emitting the identical
    // frontend. `dns-over-http-dup`, a second route asking for the same thing,
    // folds in the same way. One frontend is kept; every parent routes
    // through it, so every parent is told — once, whatever its rule count.
    let mut inputs = dns_inputs(&[], false, true, false);
    let mut gateways: Vec<Gateway> = inputs.gateways.iter().map(|g| (**g).clone()).collect();
    gateways.push(from_json(json!({
        "metadata": { "name": "gw2", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu",
            "listeners": [{ "name": "http", "protocol": "HTTP", "port": 80 }] }
    })));
    inputs.gateways = arcs(gateways);
    let http_route = |name: &str, created: &str| -> HttpRoute {
        from_json(json!({
            "metadata": { "name": name, "namespace": "demo", "creationTimestamp": created },
            "spec": {
                "parentRefs": [
                    { "name": "gw", "sectionName": "http" },
                    { "name": "gw2", "sectionName": "http" }
                ],
                "hostnames": ["dns.example.com"],
                "rules": [{ "backendRefs": [{ "name": "dns", "port": 53 }] }]
            }
        }))
    };
    inputs.http_routes = arcs(vec![
        http_route("dns-over-http-dup", NEW),
        http_route("dns-over-http", OLD),
    ]);

    let out = build(&l4_config(true), &inputs);
    assert_eq!(out.ir.frontends.len(), 1, "{:?}", out.ir.frontends);
    assert_eq!(check_on(&out, "demo.dns.53"), None);
    let told: Vec<(&str, Vec<usize>)> = ["dns-over-http", "dns-over-http-dup"]
        .into_iter()
        .map(|name| {
            let route = out.routes.iter().find(|r| r.name == name).unwrap();
            let counts = route
                .parents
                .iter()
                .map(|p| health_problems(&p.problems).len())
                .collect();
            (name, counts)
        })
        .collect();
    assert_eq!(
        told,
        [
            ("dns-over-http", vec![1, 1]),
            ("dns-over-http-dup", vec![1, 1])
        ],
        "health-check problems per parent"
    );
    for route in out.routes.iter().filter(|r| r.name != "dns-udp") {
        for parent in &route.parents {
            assert!(
                parent.accepted && parent.resolved_refs,
                "{}: a benign duplicate stays accepted: {parent:?}",
                route.name
            );
            assert_told_once_about_dns_udp(health_problems(&parent.problems));
        }
    }
    assert_eq!(route_problems(&out, "dns-udp").len(), 1);
}

#[test]
fn a_udp_only_service_is_reported_only_when_it_asks_for_a_check() {
    // The default never applied to datagrams: nothing to report.
    let out = build(&l4_config(true), &dns_inputs(&[], false, true, false));
    assert_eq!(check_on(&out, "demo.dns.53"), None);
    assert!(route_problems(&out, "dns-udp").is_empty());

    // The Service asked for one itself: say it is not running.
    let out = build(
        &l4_config(false),
        &dns_inputs(&[("sozu.io/health-check", "tcp")], false, true, false),
    );
    assert_eq!(check_on(&out, "demo.dns.53"), None);
    let udp = route_problems(&out, "dns-udp");
    assert_eq!(udp.len(), 1, "{udp:?}");
    assert!(udp[0].to_string().contains("UDP traffic"), "{}", udp[0]);

    // Off, nothing lost, nothing said.
    let out = build(
        &l4_config(true),
        &dns_inputs(&[("sozu.io/health-check", "off")], true, true, false),
    );
    assert_eq!(check_on(&out, "demo.dns.53"), None);
    assert!(route_problems(&out, "dns-tcp").is_empty());
    assert!(route_problems(&out, "dns-udp").is_empty());
}
