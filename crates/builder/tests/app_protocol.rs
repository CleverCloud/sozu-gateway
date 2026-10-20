//! Service port `appProtocol` on HTTP routes (Gateway API GEP-1911).
//!
//! `kubernetes.io/h2c` makes the cluster speak HTTP/2 to its backends;
//! `kubernetes.io/wss` cannot be spoken (Sōzu has no TLS towards a backend),
//! so a route to it is reported — `ResolvedRefs: False` on an HTTPRoute, a
//! Warning on an Ingress — and still programmed as before; every other value
//! keeps the HTTP/1.1 every route had before. Layer-4 routes ignore it.
//!
//! An HTTP route names a port by number only, and a Service may expose one
//! number over UDP and TCP: the route takes the TCP entry, wherever it is
//! listed, and each entry is its own cluster, fed only by the EndpointSlice
//! ports that can be that entry's.

use std::sync::Arc;

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use serde_json::{json, Value};

use sozu_gw_builder::{build, BuildConfig, ExposedPort, ExposedProtocol, Inputs, Problem};
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute, TcpRoute, UdpRoute};
use sozu_gw_ir as ir;

fn from_json<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).expect("valid k8s object json")
}

fn arcs<T>(items: Vec<T>) -> Vec<Arc<T>> {
    items.into_iter().map(Arc::new).collect()
}

/// Service `app` in `demo` with one port per entry: `(name, port,
/// appProtocol)`, each targeting the same number on the pod.
fn service(ports: &[(&str, i32, Option<&str>)]) -> Service {
    let ports: Vec<Value> = ports
        .iter()
        .map(|(name, port, app_protocol)| {
            let mut p = json!({ "name": name, "port": port, "targetPort": port });
            if let Some(app_protocol) = app_protocol {
                p["appProtocol"] = json!(app_protocol);
            }
            p
        })
        .collect();
    from_json(json!({
        "metadata": { "name": "app", "namespace": "demo" },
        "spec": { "ports": ports }
    }))
}

/// One ready pod behind every port of [`service`].
fn slice(ports: &[(&str, i32, Option<&str>)]) -> EndpointSlice {
    let ports: Vec<Value> = ports
        .iter()
        .map(|(name, port, _)| json!({ "name": name, "port": port }))
        .collect();
    from_json(json!({
        "metadata": { "name": "app-1", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "app" } },
        "addressType": "IPv4",
        "ports": ports,
        "endpoints": [{ "addresses": ["10.244.0.5"], "conditions": { "ready": true } }]
    }))
}

/// An Ingress on `host` with one `Prefix` path per `(path, port)`, the port
/// a number or a name.
fn ingress(host: &str, paths: &[(&str, Value)]) -> Ingress {
    let paths: Vec<Value> = paths
        .iter()
        .map(|(path, port)| {
            json!({ "path": path, "pathType": "Prefix",
                "backend": { "service": { "name": "app", "port": port } } })
        })
        .collect();
    from_json(json!({
        "metadata": { "name": "ing", "namespace": "demo" },
        "spec": { "ingressClassName": "sozu",
            "rules": [{ "host": host, "http": { "paths": paths } }] }
    }))
}

/// An HTTPRoute on `host` with one rule per `(path prefix, Service port)`.
fn http_route(host: &str, rules: &[(&str, i32)]) -> HttpRoute {
    let rules: Vec<Value> = rules
        .iter()
        .map(|(path, port)| {
            json!({ "matches": [{ "path": { "type": "PathPrefix", "value": path } }],
                "backendRefs": [{ "name": "app", "port": port }] })
        })
        .collect();
    from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": { "parentRefs": [{ "name": "gw" }], "hostnames": [host], "rules": rules }
    }))
}

fn tcp_route(port: i32) -> TcpRoute {
    from_json(json!({
        "metadata": { "name": "tcp", "namespace": "demo" },
        "spec": { "parentRefs": [{ "name": "gw" }],
            "rules": [{ "backendRefs": [{ "name": "app", "port": port }] }] }
    }))
}

fn udp_route(port: i32) -> UdpRoute {
    from_json(json!({
        "metadata": { "name": "udp", "namespace": "demo" },
        "spec": { "parentRefs": [{ "name": "gw" }],
            "rules": [{ "backendRefs": [{ "name": "app", "port": port }] }] }
    }))
}

fn gateway_class() -> GatewayClass {
    from_json(json!({
        "metadata": { "name": "sozu" },
        "spec": { "controllerName": "sozu.io/gateway-controller" }
    }))
}

/// HTTP on 80, TCP on 9443 and UDP on 9053, the ports [`config`] exposes for
/// layer 4.
fn gateway() -> Gateway {
    from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80 },
            { "name": "tcp", "protocol": "TCP", "port": 9443 },
            { "name": "udp", "protocol": "UDP", "port": 9053 }
        ]}
    }))
}

fn config() -> BuildConfig {
    let mut cfg = BuildConfig::default();
    cfg.exposure.push(ExposedPort {
        name: "tcp".to_string(),
        port: 9443,
        bind: 9443,
        protocol: ExposedProtocol::Tcp,
        transport: Some("TCP".to_string()),
        owner: None,
    });
    cfg.exposure.push(ExposedPort {
        name: "udp".to_string(),
        port: 9053,
        bind: 9053,
        protocol: ExposedProtocol::Udp,
        transport: Some("UDP".to_string()),
        owner: None,
    });
    cfg
}

fn inputs(
    ports: &[(&str, i32, Option<&str>)],
    ingresses: Vec<Ingress>,
    http_routes: Vec<HttpRoute>,
    tcp_routes: Vec<TcpRoute>,
) -> Inputs {
    Inputs {
        ingresses: arcs(ingresses),
        services: arcs(vec![service(ports)]),
        endpointslices: arcs(vec![slice(ports)]),
        gateway_classes: arcs(vec![gateway_class()]),
        gateways: arcs(vec![gateway()]),
        http_routes: arcs(http_routes),
        tcp_routes: arcs(tcp_routes),
        ..Default::default()
    }
}

/// One port of a TCP/UDP twin Service: `(name, protocol, port, targetPort,
/// appProtocol)`. Unlike [`service`], the protocol is explicit and the pod
/// port differs from the Service port, so the entry a route resolved to shows
/// in its backend address.
type PortSpec<'a> = (&'a str, &'a str, i32, i32, Option<&'a str>);

/// `53/UDP` and `53/TCP` on one Service, each with its own pod port, the TCP
/// one declaring h2c (DNS and DNS-over-HTTP/2 behind one name).
const DNS_UDP: PortSpec = ("dns", "UDP", 53, 5353, None);
const DOH_H2C: PortSpec = ("doh", "TCP", 53, 8053, H2C);

/// [`inputs`] with Service `app` and its slice spelled by [`PortSpec`]s.
fn twin_inputs(
    ports: &[PortSpec],
    ingresses: Vec<Ingress>,
    http_routes: Vec<HttpRoute>,
    tcp_routes: Vec<TcpRoute>,
    udp_routes: Vec<UdpRoute>,
) -> Inputs {
    let service_ports: Vec<Value> = ports
        .iter()
        .map(|(name, protocol, port, target, app_protocol)| {
            let mut p = json!({ "name": name, "protocol": protocol,
                "port": port, "targetPort": target });
            if let Some(app_protocol) = app_protocol {
                p["appProtocol"] = json!(app_protocol);
            }
            p
        })
        .collect();
    let slice_ports: Vec<Value> = ports
        .iter()
        .map(|(name, protocol, _, target, _)| {
            json!({ "name": name, "protocol": protocol, "port": target })
        })
        .collect();
    let mut inputs = inputs(&[], ingresses, http_routes, tcp_routes);
    inputs.services = arcs(vec![from_json(json!({
        "metadata": { "name": "app", "namespace": "demo" },
        "spec": { "ports": service_ports }
    }))]);
    inputs.endpointslices = arcs(vec![from_json(json!({
        "metadata": { "name": "app-1", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "app" } },
        "addressType": "IPv4",
        "ports": slice_ports,
        "endpoints": [{ "addresses": ["10.244.0.5"], "conditions": { "ready": true } }]
    }))]);
    inputs.udp_routes = arcs(udp_routes);
    inputs
}

/// The addresses of a cluster's backends.
fn backend_addrs(out: &ir::Ir, id: &str) -> Vec<String> {
    out.backends
        .iter()
        .filter(|b| b.cluster_id == id)
        .map(|b| b.address.to_string())
        .collect()
}

fn cluster<'a>(out: &'a ir::Ir, id: &str) -> &'a ir::Cluster {
    out.clusters
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("no cluster {id} in {:?}", out.clusters))
}

const H2C: Option<&str> = Some("kubernetes.io/h2c");
const WSS: Option<&str> = Some("kubernetes.io/wss");

#[test]
fn an_h2c_port_makes_the_cluster_speak_http2() {
    let ports = [("grpc", 50051, H2C), ("metrics", 8080, None)];
    let out = build(
        &config(),
        &inputs(
            &ports,
            vec![ingress(
                "ing.example.com",
                &[
                    ("/by-number", json!({ "number": 50051 })),
                    ("/by-name", json!({ "name": "grpc" })),
                    ("/metrics", json!({ "number": 8080 })),
                ],
            )],
            vec![http_route("route.example.com", &[("/", 50051)])],
            vec![],
        ),
    );

    assert!(cluster(&out.ir, "demo.app.50051").http2);
    // The same Service's other port keeps HTTP/1.1: the flag is per port.
    assert!(!cluster(&out.ir, "demo.app.8080").http2);
    assert!(out.results[0].problems.is_empty(), "{:?}", out.results);
    let parent = &out.routes[0].parents[0];
    assert!(parent.accepted && parent.resolved_refs, "{parent:?}");
    assert!(parent.problems.is_empty(), "{parent:?}");
    // Every path still routes.
    assert_eq!(out.ir.frontends.len(), 4, "{:?}", out.ir.frontends);
}

/// Only `kubernetes.io/h2c` and `kubernetes.io/wss` are acted on. Everything
/// else — the other Kubernetes constant, IANA names, the free-form spellings
/// Services carry in front of backends that also speak HTTP/1.1, and
/// near-misses of the constant itself — routes exactly as before.
#[test]
fn every_other_app_protocol_keeps_http1_and_routes() {
    for app_protocol in [
        None,
        Some("http"),
        Some("kubernetes.io/ws"),
        Some("grpc"),
        Some("http2"),
        Some("h2c"),
        Some("tcp"),
        Some("https"),
        Some("kuma.io/http"),
    ] {
        let ports = [("web", 80, app_protocol)];
        let out = build(
            &config(),
            &inputs(
                &ports,
                vec![ingress(
                    "ing.example.com",
                    &[("/", json!({ "number": 80 }))],
                )],
                vec![http_route("route.example.com", &[("/", 80)])],
                vec![],
            ),
        );
        assert!(!cluster(&out.ir, "demo.app.80").http2, "{app_protocol:?}");
        assert_eq!(out.ir.frontends.len(), 2, "{app_protocol:?}");
        assert!(out.results[0].problems.is_empty(), "{app_protocol:?}");
        let parent = &out.routes[0].parents[0];
        assert!(parent.resolved_refs, "{app_protocol:?}: {parent:?}");
        assert!(parent.problems.is_empty(), "{app_protocol:?}: {parent:?}");
    }
}

/// GEP-1911 asks for `ResolvedRefs: False` / `UnsupportedProtocol`, and the
/// route gets it, but the rule stays programmed exactly as before: dropping
/// its match would hand `/ws` to the `/` rule's backend.
#[test]
fn an_httproute_to_a_wss_port_is_reported_and_still_routed() {
    let ports = [("wss", 443, WSS), ("web", 80, None)];
    let out = build(
        &config(),
        &inputs(
            &ports,
            vec![],
            vec![http_route("route.example.com", &[("/ws", 443), ("/", 80)])],
            vec![],
        ),
    );

    let parent = &out.routes[0].parents[0];
    // Still attached: the backend is what is invalid, not the route.
    assert!(parent.accepted, "{parent:?}");
    assert!(!parent.resolved_refs);
    assert_eq!(parent.resolved_refs_reason, "UnsupportedProtocol");
    assert_eq!(
        parent.problems,
        [Problem::UnsupportedAppProtocol {
            service: "app".into(),
            port: 443,
            app_protocol: "kubernetes.io/wss".into(),
        }]
    );
    // `/ws` still reaches the wss port, over HTTP/1.1 as before; `/` its own.
    let routed = |prefix: &str| {
        out.ir
            .frontends
            .iter()
            .find(|f| f.path == ir::PathMatch::Prefix(prefix.to_string()))
            .and_then(|f| f.cluster_id.clone())
    };
    assert_eq!(out.ir.frontends.len(), 2, "{:?}", out.ir.frontends);
    assert_eq!(routed("/ws").as_deref(), Some("demo.app.443"));
    assert_eq!(routed("/").as_deref(), Some("demo.app.80"));
    assert!(!cluster(&out.ir, "demo.app.443").http2);
    assert_eq!(backend_addrs(&out.ir, "demo.app.443"), ["10.244.0.5:443"]);
}

/// An Ingress has no `ResolvedRefs` to set: it is warned once (a Warning
/// Event on the Ingress), never un-programmed.
#[test]
fn an_ingress_to_a_wss_port_is_warned_and_still_programmed() {
    let ports = [("wss", 443, WSS)];
    let out = build(
        &config(),
        &inputs(
            &ports,
            vec![ingress(
                "ing.example.com",
                &[
                    ("/a", json!({ "number": 443 })),
                    ("/b", json!({ "name": "wss" })),
                ],
            )],
            vec![],
            vec![],
        ),
    );

    assert_eq!(
        out.results[0].problems,
        [Problem::UnsupportedAppProtocol {
            service: "app".into(),
            port: 443,
            app_protocol: "kubernetes.io/wss".into(),
        }],
        "reported once, not once per path"
    );
    assert_eq!(out.ir.frontends.len(), 2);
    assert!(!cluster(&out.ir, "demo.app.443").http2);
    assert_eq!(out.ir.backends.len(), 1);
}

/// A layer-4 route forwards bytes: a TLS port behind it is passthrough, and
/// an h2c port needs nothing from Sōzu either.
#[test]
fn layer4_routes_ignore_app_protocol() {
    for (app_protocol, port) in [(WSS, 443), (H2C, 50051)] {
        let ports = [("p", port, app_protocol)];
        let out = build(
            &config(),
            &inputs(&ports, vec![], vec![], vec![tcp_route(port)]),
        );
        let parent = &out.routes[0].parents[0];
        assert!(parent.accepted && parent.resolved_refs, "{parent:?}");
        assert!(parent.problems.is_empty(), "{parent:?}");
        assert_eq!(out.ir.l4_frontends.len(), 1, "{app_protocol:?}");
        let id = format!("demo.app.{port}");
        assert_eq!(out.ir.l4_frontends[0].cluster_id, id);
        assert!(!cluster(&out.ir, &id).http2, "{app_protocol:?}");
    }
}

/// One Service port reached by an HTTP route and a TCPRoute is one cluster.
/// The HTTP route sets `http2`; Sōzu's TCP proxy never reads it, so the
/// layer-4 frontend on the same cluster is unaffected.
#[test]
fn a_cluster_shared_with_a_tcproute_still_speaks_http2() {
    let ports = [("grpc", 50051, H2C)];
    let out = build(
        &config(),
        &inputs(
            &ports,
            vec![],
            vec![http_route("route.example.com", &[("/", 50051)])],
            vec![tcp_route(50051)],
        ),
    );
    assert_eq!(out.ir.clusters.len(), 1);
    assert!(cluster(&out.ir, "demo.app.50051").http2);
    assert_eq!(
        out.ir.frontends[0].cluster_id.as_deref(),
        Some("demo.app.50051")
    );
    assert_eq!(out.ir.l4_frontends[0].cluster_id, "demo.app.50051");
}

/// An HTTP route names only a number, and Sōzu reaches its backend over TCP,
/// so of a TCP/UDP twin it takes the TCP entry — that entry's `appProtocol`
/// and pod port — wherever the Service lists it. The listing order decides
/// only the cluster id: the first entry carrying the number keeps the bare one.
#[test]
fn an_http_route_by_number_takes_the_tcp_entry_of_a_udp_twin() {
    for (ports, id) in [
        ([DNS_UDP, DOH_H2C], "demo.app.53.tcp"),
        ([DOH_H2C, DNS_UDP], "demo.app.53"),
    ] {
        let out = build(
            &config(),
            &twin_inputs(
                &ports,
                vec![ingress(
                    "ing.example.com",
                    &[("/", json!({ "number": 53 }))],
                )],
                vec![http_route("route.example.com", &[("/", 53)])],
                vec![],
                vec![],
            ),
        );
        assert_eq!(out.ir.clusters.len(), 1, "{id}: {:?}", out.ir.clusters);
        assert!(cluster(&out.ir, id).http2, "{id}");
        assert_eq!(backend_addrs(&out.ir, id), ["10.244.0.5:8053"], "{id}");
        assert_eq!(out.ir.frontends.len(), 2, "{id}");
        assert!(out
            .ir
            .frontends
            .iter()
            .all(|f| f.cluster_id.as_deref() == Some(id)));
        assert!(
            out.results[0].problems.is_empty(),
            "{id}: {:?}",
            out.results
        );
        let parent = &out.routes[0].parents[0];
        assert!(parent.accepted && parent.resolved_refs, "{id}: {parent:?}");
        assert!(parent.problems.is_empty(), "{id}: {parent:?}");
    }
}

/// The same holds for a `kubernetes.io/wss` TCP entry listed after a UDP
/// twin: the HTTPRoute and the Ingress are reported, and both keep sending
/// cleartext to the TCP entry's pod port.
#[test]
fn a_wss_tcp_entry_behind_a_udp_twin_is_reported() {
    let ports = [
        ("quic", "UDP", 443, 4433, None),
        ("wss", "TCP", 443, 8443, WSS),
    ];
    let out = build(
        &config(),
        &twin_inputs(
            &ports,
            vec![ingress(
                "ing.example.com",
                &[("/", json!({ "number": 443 }))],
            )],
            vec![http_route("route.example.com", &[("/", 443)])],
            vec![],
            vec![],
        ),
    );
    let reported = [Problem::UnsupportedAppProtocol {
        service: "app".into(),
        port: 443,
        app_protocol: "kubernetes.io/wss".into(),
    }];
    let parent = &out.routes[0].parents[0];
    assert!(parent.accepted && !parent.resolved_refs, "{parent:?}");
    assert_eq!(parent.resolved_refs_reason, "UnsupportedProtocol");
    assert_eq!(parent.problems, reported);
    assert_eq!(out.results[0].problems, reported);
    assert_eq!(out.ir.frontends.len(), 2, "{:?}", out.ir.frontends);
    assert!(out
        .ir
        .frontends
        .iter()
        .all(|f| f.cluster_id.as_deref() == Some("demo.app.443.tcp")));
    assert_eq!(
        backend_addrs(&out.ir, "demo.app.443.tcp"),
        ["10.244.0.5:8443"]
    );
}

/// Layer-4 resolution is unchanged: a TCPRoute or UDPRoute still takes the
/// first entry carrying the number, whatever its protocol, under the bare
/// cluster id, and never sets `http2`. Built next to an HTTP route to the
/// TCP twin, neither cluster picks up the other's pod port.
///
/// A TCPRoute therefore still lands on the UDP entry's pod port here, as it
/// did before; which entry a layer-4 route should take is not GEP-1911's
/// question and is left alone.
#[test]
fn layer4_routes_to_a_udp_twin_are_unaffected() {
    for (tcp_routes, udp_routes) in [(vec![tcp_route(53)], vec![]), (vec![], vec![udp_route(53)])] {
        let out = build(
            &config(),
            &twin_inputs(
                &[DNS_UDP, DOH_H2C],
                vec![],
                vec![http_route("route.example.com", &[("/", 53)])],
                tcp_routes,
                udp_routes,
            ),
        );
        assert_eq!(out.ir.l4_frontends.len(), 1);
        let kind = out.ir.l4_frontends[0].protocol;
        assert_eq!(out.ir.l4_frontends[0].cluster_id, "demo.app.53", "{kind:?}");
        assert!(!cluster(&out.ir, "demo.app.53").http2, "{kind:?}");
        assert_eq!(
            backend_addrs(&out.ir, "demo.app.53"),
            ["10.244.0.5:5353"],
            "{kind:?}"
        );
        assert!(cluster(&out.ir, "demo.app.53.tcp").http2, "{kind:?}");
        assert_eq!(
            backend_addrs(&out.ir, "demo.app.53.tcp"),
            ["10.244.0.5:8053"],
            "{kind:?}"
        );
        assert_eq!(out.routes.len(), 2);
        for route in &out.routes {
            let parent = &route.parents[0];
            assert!(parent.accepted && parent.resolved_refs, "{parent:?}");
            assert!(parent.problems.is_empty(), "{parent:?}");
        }
    }
}

/// Each entry of a TCP/UDP twin is its own cluster, with its own
/// `appProtocol` and pod port, so an Ingress naming both by name gets two,
/// whichever path is built first. Which one keeps the bare id follows the
/// Service's listing order.
#[test]
fn twin_entries_by_name_are_one_cluster_each() {
    for paths in [
        [
            ("/a", json!({ "name": "dns" })),
            ("/b", json!({ "name": "doh" })),
        ],
        [
            ("/a", json!({ "name": "doh" })),
            ("/b", json!({ "name": "dns" })),
        ],
    ] {
        for (ports, udp_id, tcp_id) in [
            ([DNS_UDP, DOH_H2C], "demo.app.53", "demo.app.53.tcp"),
            ([DOH_H2C, DNS_UDP], "demo.app.53.udp", "demo.app.53"),
        ] {
            let out = build(
                &config(),
                &twin_inputs(
                    &ports,
                    vec![ingress("ing.example.com", &paths)],
                    vec![],
                    vec![],
                    vec![],
                ),
            );
            assert_eq!(out.ir.clusters.len(), 2, "{paths:?}");
            assert!(!cluster(&out.ir, udp_id).http2, "{paths:?}");
            assert!(cluster(&out.ir, tcp_id).http2, "{paths:?}");
            assert_eq!(backend_addrs(&out.ir, udp_id), ["10.244.0.5:5353"]);
            assert_eq!(backend_addrs(&out.ir, tcp_id), ["10.244.0.5:8053"]);
        }
    }
}

/// A single-port EndpointSlice of Service `app` with one ready pod; `None`
/// leaves the field out. The EndpointSlice controller groups a Service's pods
/// by the ports each one resolves, so pods exposing only one port's named
/// `targetPort` are published in a slice carrying that port alone.
fn single_port_slice(
    name: Option<&str>,
    protocol: Option<&str>,
    port: i32,
    ip: &str,
) -> EndpointSlice {
    let mut p = json!({ "port": port });
    if let Some(name) = name {
        p["name"] = json!(name);
    }
    if let Some(protocol) = protocol {
        p["protocol"] = json!(protocol);
    }
    from_json(json!({
        "metadata": { "name": format!("app-{ip}"), "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "app" } },
        "addressType": "IPv4",
        "ports": [p],
        "endpoints": [{ "addresses": [ip], "conditions": { "ready": true } }]
    }))
}

/// Every route parent attached, resolved and problem-free.
fn assert_routes_resolved(out: &sozu_gw_builder::BuildOutput, case: &str) {
    for route in &out.routes {
        for parent in &route.parents {
            assert!(
                parent.accepted && parent.resolved_refs,
                "{case}: {parent:?}"
            );
            assert!(parent.problems.is_empty(), "{case}: {parent:?}");
        }
    }
}

/// A TCP/UDP twin whose entries are published in two single-port slices:
/// each cluster gets its own entry's pods only. A slice's sole port is used
/// for a Service port it does not name only when it has no name and that
/// port's protocol, so the h2c cluster never dials the DNS pod port, nor the
/// UDP cluster the DoH one.
#[test]
fn twin_ports_in_separate_slices_keep_their_own_addresses() {
    // Named, as the EndpointSlice controller publishes them; then unnamed, as
    // only a hand-written slice is, where the protocol alone tells them apart.
    for (dns, doh) in [
        (Some("dns"), Some("doh")),
        (Some(""), Some("")),
        (None, None),
    ] {
        let case = format!("{dns:?}/{doh:?}");
        let mut inputs = twin_inputs(
            &[DNS_UDP, DOH_H2C],
            vec![],
            vec![http_route("route.example.com", &[("/", 53)])],
            vec![],
            vec![udp_route(53)],
        );
        inputs.endpointslices = arcs(vec![
            single_port_slice(dns, Some("UDP"), 5353, "10.244.0.5"),
            single_port_slice(doh, Some("TCP"), 8053, "10.244.0.6"),
        ]);
        let out = build(&config(), &inputs);

        // The HTTPRoute's cluster: the TCP entry, listed second.
        assert!(cluster(&out.ir, "demo.app.53.tcp").http2, "{case}");
        assert_eq!(
            backend_addrs(&out.ir, "demo.app.53.tcp"),
            ["10.244.0.6:8053"],
            "{case}"
        );
        assert_eq!(
            out.ir.frontends[0].cluster_id.as_deref(),
            Some("demo.app.53.tcp"),
            "{case}"
        );
        // The UDPRoute's cluster: the UDP entry, listed first.
        assert!(!cluster(&out.ir, "demo.app.53").http2, "{case}");
        assert_eq!(
            backend_addrs(&out.ir, "demo.app.53"),
            ["10.244.0.5:5353"],
            "{case}"
        );
        assert_eq!(out.ir.l4_frontends[0].cluster_id, "demo.app.53", "{case}");
        assert_routes_resolved(&out, &case);
    }
}

/// Two TCP ports in two single-port slices differ by name only: a slice's
/// sole port named for another Service port belongs to that port.
#[test]
fn same_protocol_ports_in_separate_slices_keep_their_own_addresses() {
    const WEB: PortSpec = ("web", "TCP", 80, 8080, None);
    const ADMIN: PortSpec = ("admin", "TCP", 81, 9090, None);
    let mut inputs = twin_inputs(
        &[WEB, ADMIN],
        vec![ingress(
            "ing.example.com",
            &[
                ("/admin", json!({ "name": "admin" })),
                ("/", json!({ "number": 80 })),
            ],
        )],
        vec![http_route(
            "route.example.com",
            &[("/admin", 81), ("/", 80)],
        )],
        vec![],
        vec![],
    );
    inputs.endpointslices = arcs(vec![
        single_port_slice(Some("web"), Some("TCP"), 8080, "10.244.0.5"),
        single_port_slice(Some("admin"), Some("TCP"), 9090, "10.244.0.6"),
    ]);
    let out = build(&config(), &inputs);

    assert_eq!(backend_addrs(&out.ir, "demo.app.80"), ["10.244.0.5:8080"]);
    assert_eq!(backend_addrs(&out.ir, "demo.app.81"), ["10.244.0.6:9090"]);
    assert!(out.results[0].problems.is_empty(), "{:?}", out.results);
    assert_routes_resolved(&out, "web/admin");
}

/// What the sole-port fallback exists for keeps resolving: one Service port,
/// and a slice whose only port has the Service port's name, or none, in that
/// port's protocol, unset meaning TCP on either side. A missing name and `""`,
/// which the EndpointSlice controller writes for an unnamed Service port, are
/// one name. A sole port named for another port, or of another protocol when
/// it has no name to match by, is not this one.
#[test]
fn a_sole_slice_port_resolves_only_when_it_can_be_the_service_port() {
    type Spec<'a> = (Option<&'a str>, Option<&'a str>);
    // (Service port name, protocol), (slice port name, protocol)
    let resolves: [(Spec, Spec); 10] = [
        // An unnamed Service port: matched by name, `""` being none.
        ((None, None), (Some(""), Some("TCP"))),
        ((None, Some("TCP")), (Some(""), None)),
        ((None, None), (None, None)),
        ((None, Some("UDP")), (Some(""), Some("UDP"))),
        // A named one, and an unnamed slice port of its protocol.
        ((Some("http"), None), (Some(""), Some("TCP"))),
        ((Some("http"), Some("TCP")), (None, None)),
        ((Some("dns"), Some("UDP")), (Some(""), Some("UDP"))),
        ((Some("sig"), Some("SCTP")), (None, Some("SCTP"))),
        // Matched by name, which never looked at the protocol and still does
        // not.
        ((Some("http"), None), (Some("http"), Some("TCP"))),
        ((Some("http"), Some("UDP")), (Some("http"), None)),
    ];
    let refused: [(Spec, Spec); 5] = [
        ((None, None), (Some("http"), Some("TCP"))),
        ((Some("http"), None), (Some("web"), Some("TCP"))),
        ((Some("http"), None), (Some(""), Some("UDP"))),
        ((Some("dns"), Some("UDP")), (None, None)),
        ((Some("dns"), Some("UDP")), (Some(""), Some("TCP"))),
    ];
    for (expected, cases) in [(true, &resolves[..]), (false, &refused[..])] {
        for &((svc_name, svc_protocol), (slice_name, slice_protocol)) in cases {
            let case =
                format!("{svc_name:?}/{svc_protocol:?} <- {slice_name:?}/{slice_protocol:?}");
            let mut port = json!({ "port": 80, "targetPort": 8080 });
            if let Some(name) = svc_name {
                port["name"] = json!(name);
            }
            if let Some(protocol) = svc_protocol {
                port["protocol"] = json!(protocol);
            }
            let mut inputs = inputs(
                &[],
                vec![ingress(
                    "ing.example.com",
                    &[("/", json!({ "number": 80 }))],
                )],
                vec![],
                vec![],
            );
            inputs.services = arcs(vec![from_json(json!({
                "metadata": { "name": "app", "namespace": "demo" },
                "spec": { "ports": [port] }
            }))]);
            inputs.endpointslices = arcs(vec![single_port_slice(
                slice_name,
                slice_protocol,
                8080,
                "10.244.0.5",
            )]);
            let out = build(&config(), &inputs);
            let addrs = backend_addrs(&out.ir, "demo.app.80");
            if expected {
                assert_eq!(addrs, ["10.244.0.5:8080"], "{case}");
            } else {
                assert!(addrs.is_empty(), "{case}: {addrs:?}");
            }
        }
    }

    // A slice of several ports is matched by name only, `""` being no name
    // there too, and never by position.
    for (svc_name, expected) in [(None, Some("10.244.0.5:8080")), (Some("http"), None)] {
        let mut port = json!({ "port": 80, "targetPort": 8080 });
        if let Some(name) = svc_name {
            port["name"] = json!(name);
        }
        let mut inputs = inputs(
            &[],
            vec![ingress(
                "ing.example.com",
                &[("/", json!({ "number": 80 }))],
            )],
            vec![],
            vec![],
        );
        inputs.services = arcs(vec![from_json(json!({
            "metadata": { "name": "app", "namespace": "demo" },
            "spec": { "ports": [port] }
        }))]);
        inputs.endpointslices = arcs(vec![from_json(json!({
            "metadata": { "name": "app-1", "namespace": "demo",
                "labels": { "kubernetes.io/service-name": "app" } },
            "addressType": "IPv4",
            "ports": [
                { "name": "", "protocol": "TCP", "port": 8080 },
                { "name": "metrics", "protocol": "TCP", "port": 9090 }
            ],
            "endpoints": [{ "addresses": ["10.244.0.5"], "conditions": { "ready": true } }]
        }))]);
        let out = build(&config(), &inputs);
        let addrs = backend_addrs(&out.ir, "demo.app.80");
        assert_eq!(addrs, Vec::from_iter(expected), "{svc_name:?}");
    }
}
