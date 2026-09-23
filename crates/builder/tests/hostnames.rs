//! Hostnames Sōzu's router cannot parse are refused per object.
//!
//! Kubernetes validates an Ingress host and a Gateway API `Hostname` against
//! the RFC 1123 label grammar only. Sōzu 2.2.1 also runs every frontend
//! hostname through `idna::domain_to_ascii` and rejects the whole request when
//! it fails, which (translation being all-or-nothing) fails every reconcile of
//! the shared instance. These tests pin that such a name is reported on the
//! object that carries it and that nothing else stops converging.

use std::sync::Arc;

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use serde_json::json;
use sozu_command_lib::proto::command::request::RequestType;
use sozu_gw_builder::{build, BuildConfig, BuildOutput, Inputs, Problem};
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute};
use sozu_gw_ir as ir;

/// Names Sōzu 2.2.1 refuses ("parsing hostname failed"), each for a different
/// IDNA reason, although the apiserver admits every one of them.
const REFUSED: &[&str] = &[
    // Punycode that decodes to nothing valid.
    "xn--a.example.com",
    "xn--zz.x.com",
    // The same behind a wildcard: the wildcard does not shield the label.
    "*.xn--a.example.com",
    // A label starting with a digit in a domain with a right-to-left label
    // breaks the bidi rule, which then applies to every label.
    "0.xn--4gbrim.example.com",
    // So does the `*` label itself.
    "*.xn--4gbrim.example.com",
];

/// Names Sōzu 2.2.1 parses, including a valid A-label and a plain label in a
/// right-to-left domain.
const ACCEPTED: &[&str] = &[
    "good.example.com",
    "*.example.com",
    "xn--bcher-kva.example.com",
    "a.xn--4gbrim.example.com",
];

fn from_json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("valid k8s object json")
}

fn arcs<T>(items: Vec<T>) -> Vec<Arc<T>> {
    items.into_iter().map(Arc::new).collect()
}

fn svc(ns: &str) -> Service {
    from_json(json!({ "metadata": { "name": "web", "namespace": ns },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] } }))
}

fn slice(ns: &str) -> EndpointSlice {
    from_json(json!({ "metadata": { "name": "web-1", "namespace": ns,
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "IPv4", "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [{ "addresses": ["10.244.0.5"], "conditions": { "ready": true } }] }))
}

/// An Ingress routing each of `hosts` (one rule each, `None` for a hostless
/// rule) to Service `web`, with two paths per rule.
fn ingress(ns: &str, name: &str, hosts: &[Option<&str>]) -> Ingress {
    let rules: Vec<_> = hosts
        .iter()
        .map(|host| {
            let paths = json!([
                { "path": "/", "pathType": "Prefix",
                  "backend": { "service": { "name": "web", "port": { "number": 80 } } } },
                { "path": "/api", "pathType": "Prefix",
                  "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
            ]);
            match host {
                Some(h) => json!({ "host": h, "http": { "paths": paths } }),
                None => json!({ "http": { "paths": paths } }),
            }
        })
        .collect();
    from_json(json!({ "metadata": { "name": name, "namespace": ns },
        "spec": { "ingressClassName": "sozu", "rules": rules } }))
}

/// The hostnames of the frontends the translator emits for `out`, which also
/// proves the IR still translates.
fn emitted_hosts(out: &BuildOutput) -> Vec<String> {
    let reqs = sozu_gw_translator::reconcile(&ir::Ir::default(), &out.ir).expect("translates");
    let mut hosts: Vec<String> = reqs
        .iter()
        .filter_map(|r| match &r.request_type {
            Some(RequestType::AddHttpFrontend(f)) | Some(RequestType::AddHttpsFrontend(f)) => {
                Some(f.hostname.clone())
            }
            _ => None,
        })
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

fn invalid_hostnames(problems: &[Problem]) -> Vec<(&str, Option<&str>)> {
    problems
        .iter()
        .filter_map(|p| match p {
            Problem::InvalidHostname {
                hostname, listener, ..
            } => Some((hostname.as_str(), listener.as_deref())),
            _ => None,
        })
        .collect()
}

#[test]
fn ingress_hosts_are_admitted_exactly_when_sozu_parses_them() {
    let names: Vec<String> = (0..REFUSED.len() + ACCEPTED.len())
        .map(|i| format!("i{i}"))
        .collect();
    let ingresses: Vec<Ingress> = REFUSED
        .iter()
        .chain(ACCEPTED)
        .zip(&names)
        .map(|(host, name)| ingress("demo", name, &[Some(host)]))
        .collect();
    let inputs = Inputs {
        ingresses: arcs(ingresses),
        services: arcs(vec![svc("demo")]),
        endpointslices: arcs(vec![slice("demo")]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    for (host, result) in REFUSED.iter().zip(&out.results) {
        assert_eq!(
            invalid_hostnames(&result.problems),
            vec![(*host, None)],
            "{host} is refused once, on its own Ingress: {:?}",
            result.problems
        );
    }
    for (host, result) in ACCEPTED.iter().zip(&out.results[REFUSED.len()..]) {
        assert!(result.problems.is_empty(), "{host}: {:?}", result.problems);
    }
    let mut expected: Vec<String> = ACCEPTED.iter().map(|h| h.to_string()).collect();
    expected.sort();
    assert_eq!(emitted_hosts(&out), expected);
}

#[test]
fn an_unparseable_ingress_host_drops_only_its_own_rule() {
    // One Ingress mixes a bad host with a good one and a hostless rule; another
    // tenant's Ingress sits next to it. Only the bad rule goes.
    let inputs = Inputs {
        ingresses: arcs(vec![
            ingress(
                "tenant",
                "mixed",
                &[Some("xn--a.example.com"), Some("ok.example.com"), None],
            ),
            ingress("victim", "bystander", &[Some("bystander.example.com")]),
        ]),
        services: arcs(vec![svc("tenant"), svc("victim")]),
        endpointslices: arcs(vec![slice("tenant"), slice("victim")]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    let mixed = &out.results[0];
    match &mixed.problems[..] {
        [problem @ Problem::InvalidHostname {
            hostname,
            listener: None,
            ..
        }] => {
            assert_eq!(hostname, "xn--a.example.com");
            assert_eq!(problem.reason(), "InvalidHostname");
            assert_eq!(problem.listener(), None);
            let message = problem.to_string();
            assert!(
                message.contains("\"xn--a.example.com\"") && message.contains("IDNA"),
                "{message}"
            );
        }
        other => panic!("one InvalidHostname for the rule, not per path: {other:?}"),
    }
    assert!(out.results[1].problems.is_empty());
    assert_eq!(
        emitted_hosts(&out),
        vec!["*", "bystander.example.com", "ok.example.com"]
    );
    // Both paths of each surviving rule are still there.
    let ok_paths = out
        .ir
        .frontends
        .iter()
        .filter(|f| f.hostname == "ok.example.com")
        .count();
    assert_eq!(ok_paths, 2);
}

// ---------------------------------------------------------------------------
// Gateway API
// ---------------------------------------------------------------------------

fn gateway_class() -> GatewayClass {
    from_json(json!({ "metadata": { "name": "sozu" },
        "spec": { "controllerName": "sozu.io/gateway-controller" } }))
}

/// A Gateway in `infra` with an open `http` listener plus one HTTP listener
/// per `(name, hostname)`.
fn gateway(extra: &[(&str, &str)]) -> Gateway {
    let mut listeners = vec![json!({ "name": "http", "protocol": "HTTP", "port": 80,
        "allowedRoutes": { "namespaces": { "from": "All" } } })];
    for (name, hostname) in extra {
        listeners.push(json!({ "name": name, "protocol": "HTTP", "port": 80,
            "hostname": hostname, "allowedRoutes": { "namespaces": { "from": "All" } } }));
    }
    from_json(json!({ "metadata": { "name": "gw", "namespace": "infra" },
        "spec": { "gatewayClassName": "sozu", "listeners": listeners } }))
}

fn route(ns: &str, name: &str, section: &str, hostnames: Option<&[&str]>) -> HttpRoute {
    let mut spec = json!({
        "parentRefs": [{ "name": "gw", "namespace": "infra", "sectionName": section }],
        "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
    });
    if let Some(hostnames) = hostnames {
        spec["hostnames"] = json!(hostnames);
    }
    from_json(json!({ "metadata": { "name": name, "namespace": ns }, "spec": spec }))
}

fn gateway_inputs(gw: Gateway, routes: Vec<HttpRoute>) -> Inputs {
    Inputs {
        gateway_classes: arcs(vec![gateway_class()]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(routes),
        services: arcs(vec![svc("tenant"), svc("victim")]),
        endpointslices: arcs(vec![slice("tenant"), slice("victim")]),
        ..Default::default()
    }
}

fn route_parent<'a>(
    out: &'a BuildOutput,
    ns: &str,
    name: &str,
) -> &'a sozu_gw_builder::RouteParentResult {
    let route = out
        .routes
        .iter()
        .find(|r| r.namespace == ns && r.name == name)
        .unwrap_or_else(|| panic!("route {ns}/{name} has a result"));
    &route.parents[0]
}

#[test]
fn an_unparseable_route_hostname_drops_only_its_own_frontends() {
    let inputs = gateway_inputs(
        gateway(&[]),
        vec![
            route(
                "tenant",
                "r",
                "http",
                Some(&["xn--a.example.com", "ok.example.com"]),
            ),
            route("victim", "r", "http", Some(&["victim.example.com"])),
        ],
    );
    let out = build(&BuildConfig::default(), &inputs);

    let tenant = route_parent(&out, "tenant", "r");
    assert_eq!(
        invalid_hostnames(&tenant.problems),
        vec![("xn--a.example.com", None)]
    );
    // Reported like an unservable path match: the route keeps its other
    // hostname and stays accepted.
    assert!(tenant.accepted, "{tenant:?}");
    assert!(route_parent(&out, "victim", "r").problems.is_empty());
    assert_eq!(
        emitted_hosts(&out),
        vec!["ok.example.com", "victim.example.com"]
    );
}

#[test]
fn an_unparseable_listener_hostname_refuses_that_listener_only() {
    let inputs = gateway_inputs(
        gateway(&[("bad", "xn--zz.x.com"), ("badwild", "*.xn--a.example.com")]),
        vec![
            // Inherits the listener's hostname.
            route("tenant", "inherit", "bad", None),
            // Its own name beneath the wildcard is just as unparseable.
            route(
                "tenant",
                "beneath",
                "badwild",
                Some(&["b.xn--a.example.com"]),
            ),
            route("victim", "r", "http", Some(&["victim.example.com"])),
        ],
    );
    let out = build(&BuildConfig::default(), &inputs);

    let gw = &out.gateways[0];
    assert_eq!(
        invalid_hostnames(&gw.problems),
        vec![
            ("xn--zz.x.com", Some("bad")),
            ("*.xn--a.example.com", Some("badwild"))
        ]
    );
    for name in ["bad", "badwild"] {
        let l = gw.listeners.iter().find(|l| l.name == name).unwrap();
        assert!(!l.accepted && !l.programmed, "{l:?}");
        assert_eq!(l.accepted_reason, "UnsupportedValue");
        assert_eq!(l.programmed_reason, "Invalid");
        assert_eq!(l.attached_routes, 0);
    }
    let http = gw.listeners.iter().find(|l| l.name == "http").unwrap();
    assert!(http.accepted && http.programmed, "{http:?}");
    assert!(gw.programmed);
    assert_eq!(gw.accepted_reason, "ListenersNotValid");

    // Routes on the refused listeners bind nowhere, the way they do on any
    // listener that is not accepted.
    for name in ["inherit", "beneath"] {
        let parent = route_parent(&out, "tenant", name);
        assert!(!parent.accepted);
        assert_eq!(parent.accepted_reason, "NotAllowedByListeners");
    }
    assert_eq!(emitted_hosts(&out), vec!["victim.example.com"]);
}

#[test]
fn a_right_to_left_wildcard_listener_keeps_the_names_beneath_it() {
    // In a domain with a right-to-left label the `*` label fails the bidi rule
    // while an ordinary label beneath it passes. The listener must not be
    // refused for that: only the frontend that would carry the wildcard is.
    let inputs = gateway_inputs(
        gateway(&[("rtl", "*.xn--4gbrim.example.com")]),
        vec![
            route(
                "tenant",
                "named",
                "rtl",
                Some(&["a.xn--4gbrim.example.com"]),
            ),
            route("tenant", "inherit", "rtl", None),
        ],
    );
    let out = build(&BuildConfig::default(), &inputs);

    let gw = &out.gateways[0];
    assert!(gw.problems.is_empty(), "{:?}", gw.problems);
    let rtl = gw.listeners.iter().find(|l| l.name == "rtl").unwrap();
    assert!(rtl.accepted && rtl.programmed, "{rtl:?}");

    assert!(route_parent(&out, "tenant", "named").problems.is_empty());
    assert_eq!(
        invalid_hostnames(&route_parent(&out, "tenant", "inherit").problems),
        vec![("*.xn--4gbrim.example.com", None)]
    );
    assert_eq!(emitted_hosts(&out), vec!["a.xn--4gbrim.example.com"]);
}
