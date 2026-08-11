//! Behavioural tests for the Gateway API -> IR mapping (Phase 2).

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use serde_json::json;

use sozu_gw_builder::{build, BuildConfig, ExposedPort, ExposedProtocol, Inputs, Problem};
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute, TcpRoute, UdpRoute};
use sozu_gw_ir as ir;

const CERT_A: &str = include_str!("fixtures/cert_a.pem");
const KEY_A: &str = include_str!("fixtures/key_a.pem");

fn from_json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("valid k8s object json")
}

/// Wrap plain objects in the `Arc`s `Inputs` borrows (the controller passes
/// its reflector-cache `Arc`s straight through).
fn arcs<T>(items: Vec<T>) -> Vec<Arc<T>> {
    items.into_iter().map(Arc::new).collect()
}

fn web_service() -> Service {
    from_json(json!({
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }))
}

fn web_slice() -> EndpointSlice {
    from_json(json!({
        "metadata": { "name": "web-1", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "IPv4",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [
            { "addresses": ["10.244.0.5"], "conditions": { "ready": true } },
            { "addresses": ["10.244.0.6"], "conditions": { "ready": true } }
        ]
    }))
}

fn tls_secret() -> Secret {
    let mut data = BTreeMap::new();
    data.insert(
        "tls.crt".to_string(),
        ByteString(CERT_A.as_bytes().to_vec()),
    );
    data.insert("tls.key".to_string(), ByteString(KEY_A.as_bytes().to_vec()));
    Secret {
        metadata: ObjectMeta {
            name: Some("app-tls".to_string()),
            namespace: Some("demo".to_string()),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("kubernetes.io/tls".to_string()),
        ..Default::default()
    }
}

fn gateway_class(controller: &str) -> GatewayClass {
    from_json(json!({
        "metadata": { "name": "sozu" },
        "spec": { "controllerName": controller }
    }))
}

fn http_gateway() -> Gateway {
    from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo",
            "uid": "22222222-2222-2222-2222-222222222222" },
        "spec": { "gatewayClassName": "sozu",
            "listeners": [{ "name": "http", "protocol": "HTTP", "port": 80 }] }
    }))
}

fn https_gateway() -> Gateway {
    from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [{
            "name": "https", "protocol": "HTTPS", "port": 443,
            "hostname": "app.example.com",
            "tls": { "mode": "Terminate", "certificateRefs": [{ "name": "app-tls" }] }
        }]}
    }))
}

/// HTTPRoute to `web:80` with one prefix match. `extra_backend` adds a second
/// backendRef (to exercise the unsupported weighted-split path).
fn route_to_web(extra_backend: bool) -> HttpRoute {
    let mut backend_refs = vec![json!({ "name": "web", "port": 80 })];
    if extra_backend {
        backend_refs.push(json!({ "name": "web2", "port": 80, "weight": 50 }));
    }
    from_json(json!({
        "metadata": { "name": "route", "namespace": "demo",
            "uid": "33333333-3333-3333-3333-333333333333" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "backendRefs": backend_refs
            }]
        }
    }))
}

#[test]
fn http_route_maps_to_ir() {
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.clusters.len(), 1);
    assert_eq!(out.ir.backends.len(), 2, "two pod IPs");
    assert_eq!(out.ir.frontends.len(), 1, "one HTTP frontend");
    assert!(!out.ir.frontends[0].tls);
    assert!(out.gateway_classes[0].accepted);
    assert!(out.gateways[0].programmed);
    assert_eq!(out.routes.len(), 1);
    assert!(out.routes[0].parents[0].resolved_refs);
    assert!(out.routes[0].parents[0].problems.is_empty());

    // Both owners carry their source object's uid, so a problem Event on
    // either is visible under `kubectl describe`.
    assert_eq!(
        out.gateways[0].uid.as_deref(),
        Some("22222222-2222-2222-2222-222222222222")
    );
    assert_eq!(
        out.routes[0].uid.as_deref(),
        Some("33333333-3333-3333-3333-333333333333")
    );

    insta::assert_json_snapshot!(out.ir);
}

#[test]
fn https_listener_loads_cert() {
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![https_gateway()]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.certificates.len(), 1, "listener cert loaded");
    assert_eq!(out.ir.frontends.len(), 1);
    assert!(out.ir.frontends[0].tls, "HTTPS frontend");
    assert!(out.routes[0].parents[0].resolved_refs);
}

#[test]
fn other_controller_is_ignored() {
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("other.io/controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(!out.gateway_classes[0].accepted, "not our controllerName");
    assert!(
        out.gateways.is_empty(),
        "gateway of a foreign class is skipped"
    );
    assert!(out.ir.clusters.is_empty());
    assert!(out.ir.frontends.is_empty());
    assert!(out.routes.is_empty());
}

#[test]
fn weighted_backends_are_unsupported() {
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route_to_web(true)]), // two backendRefs
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(
        out.ir.frontends.is_empty(),
        "rule rejected, no route created"
    );
    let parent = &out.routes[0].parents[0];
    assert!(!parent.resolved_refs);
    assert!(parent
        .problems
        .contains(&Problem::WeightedBackendsUnsupported));
}

#[test]
fn zero_weight_single_backend_is_drained_not_served() {
    // weight: 0 on the (single) backendRef is the standard drain pattern: the
    // backend must receive NO traffic (the spec even calls for a 500 when all
    // weights are zero). Sōzu cannot weight or synthesize the 500, so the
    // rule is reported and skipped — never served at 100%.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80, "weight": 0 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));

    assert!(
        out.ir.frontends.is_empty(),
        "a drained backend gets nothing"
    );
    let p = &out.routes[0].parents[0];
    assert!(p.problems.contains(&Problem::ZeroWeightBackendUnsupported {
        service: "web".to_string(),
    }));
    // A skipped rule must show in the status, like every other skip path:
    // ResolvedRefs downgrades the same way the weighted-split rejection does.
    assert!(!p.resolved_refs, "the skipped rule must not read healthy");
    assert_eq!(p.resolved_refs_reason, "BackendNotFound");
}

#[test]
fn positive_weight_single_backend_still_routes() {
    // A single backendRef with any positive weight IS 100% — no problem.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80, "weight": 50 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));

    assert_eq!(out.ir.frontends.len(), 1);
    assert!(out.routes[0].parents[0].problems.is_empty());
}

#[test]
fn route_timeouts_are_reported_unsupported() {
    // Sōzu has no per-route timeout knob: the rule still routes (RequestMirror
    // precedent — drop the unsupported piece, never half-apply), but the user
    // must see that the timeout took no effect.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "timeouts": { "request": "10s" },
                "backendRefs": [{ "name": "web", "port": 80 }]
            }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));

    assert_eq!(out.ir.frontends.len(), 1, "the rule still routes");
    assert!(out.routes[0].parents[0]
        .problems
        .contains(&Problem::TimeoutsUnsupported));
}

#[test]
fn backend_ref_filters_are_reported_unsupported() {
    // Filters scoped to a backendRef have no Sōzu equivalent (filters wire
    // onto the frontend). They must be reported, not silently dropped — and
    // never half-applied onto the frontend.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "backendRefs": [{ "name": "web", "port": 80, "filters": [
                    { "type": "RequestHeaderModifier", "requestHeaderModifier": {
                        "set": [{ "name": "X-Env", "value": "prod" }] } }
                ]}]
            }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));

    assert_eq!(out.ir.frontends.len(), 1, "the backend still routes");
    assert!(
        out.ir.frontends[0].filters.header_mods.is_empty(),
        "the backendRef filter must not leak onto the frontend"
    );
    assert!(out.routes[0].parents[0].problems.iter().any(
        |p| matches!(p, Problem::FilterUnsupported { kind } if kind.contains("backendRef web"))
    ));
}

#[test]
fn http_route_filters_map_to_ir() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "filters": [
                    { "type": "RequestHeaderModifier", "requestHeaderModifier": {
                        "set": [{ "name": "X-Env", "value": "prod" }],
                        "remove": ["X-Debug"] } },
                    { "type": "ResponseHeaderModifier", "responseHeaderModifier": {
                        "add": [{ "name": "X-Served-By", "value": "sozu" }] } },
                    { "type": "URLRewrite", "urlRewrite": {
                        "hostname": "backend.svc",
                        "path": { "type": "ReplaceFullPath", "replaceFullPath": "/v2" } } }
                ],
                "backendRefs": [{ "name": "web", "port": 80 }]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1);
    let f = &out.ir.frontends[0].filters;
    assert_eq!(f.header_mods.len(), 3);
    assert!(f
        .header_mods
        .iter()
        .any(|m| matches!(m.on, ir::HeaderTarget::Request)
            && m.key == "X-Env"
            && m.value.as_deref() == Some("prod")));
    assert!(f
        .header_mods
        .iter()
        .any(|m| matches!(m.on, ir::HeaderTarget::Request)
            && m.key == "X-Debug"
            && m.value.is_none())); // remove
    assert!(f
        .header_mods
        .iter()
        .any(|m| matches!(m.on, ir::HeaderTarget::Response) && m.key == "X-Served-By"));
    // URLRewrite is reported unsupported (Sōzu's rewrite_host targets the backend
    // authority, incompatible with Gateway semantics) rather than mapped.
    assert!(f.rewrite.is_none());
    assert!(out.routes[0].parents[0]
        .problems
        .iter()
        .any(|p| matches!(p, Problem::FilterUnsupported { kind } if kind == "URLRewrite")));
    assert!(out.routes[0].parents[0].resolved_refs);
}

#[test]
fn redirect_filter_supported_and_unsupported_reported() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "filters": [
                    { "type": "RequestRedirect", "requestRedirect": { "scheme": "https", "statusCode": 301 } },
                    { "type": "RequestMirror", "requestMirror": { "backendRef": { "name": "mirror", "port": 80 } } }
                ]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    // Redirect-only route: a frontend with no cluster (backendRef-less).
    assert_eq!(out.ir.frontends.len(), 1);
    assert!(out.ir.frontends[0].cluster_id.is_none());
    let redirect = out.ir.frontends[0]
        .filters
        .redirect
        .as_ref()
        .expect("redirect");
    assert!(matches!(redirect.scheme, Some(ir::Scheme::Https)));
    assert!(matches!(
        redirect.status,
        ir::RedirectStatus::MovedPermanently
    ));
    // RequestMirror is not supported by Sōzu -> reported.
    assert!(out.routes[0].parents[0]
        .problems
        .iter()
        .any(|p| matches!(p, Problem::FilterUnsupported { .. })));
}

/// Build one HTTPRoute carrying a single `RequestRedirect` filter with the
/// given `requestRedirect` body, and return the resulting build.
fn build_with_redirect(request_redirect: serde_json::Value) -> sozu_gw_builder::BuildOutput {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["old.example.com"],
            "rules": [{
                "filters": [
                    { "type": "RequestRedirect", "requestRedirect": request_redirect }
                ]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    build(&BuildConfig::default(), &inputs)
}

#[test]
fn hostname_path_and_port_redirect_targets_are_programmed() {
    // These three were refused wholesale, on the belief that Sōzu could express
    // only a redirect's *scheme*. Measured on 2.2.0 (PROTOCOL.md §13), it builds
    // the whole `Location` from `rewrite_host`/`rewrite_path`/`rewrite_port`
    // under every policy — so refusing them was leaving a working feature on
    // the floor, not protecting anyone.
    for target in [
        json!({ "hostname": "new.example.com", "statusCode": 301 }),
        json!({ "path": { "type": "ReplaceFullPath", "replaceFullPath": "/v2" } }),
        json!({ "port": 8443, "scheme": "https" }),
    ] {
        let out = build_with_redirect(target.clone());
        assert_eq!(out.ir.frontends.len(), 1, "must program {target}");
        assert!(out.ir.frontends[0].filters.redirect.is_some(), "{target}");
        let parent = &out.routes[0].parents[0];
        assert!(parent.accepted, "{target}");
        assert!(
            parent.problems.is_empty(),
            "{target}: {:?}",
            parent.problems
        );
    }
}

#[test]
fn redirect_status_308_programs_natively() {
    // Sōzu has a PERMANENT_REDIRECT policy and it was going unused: 308 was
    // being served as 302 by the catch-all arm. The difference matters — 308
    // forbids the client rewriting the method to GET, which is why an author
    // picks it over 301.
    let out = build_with_redirect(json!({ "scheme": "https", "statusCode": 308 }));

    assert_eq!(out.ir.frontends.len(), 1);
    let redirect = out.ir.frontends[0]
        .filters
        .redirect
        .as_ref()
        .expect("redirect programmed");
    assert!(matches!(
        redirect.status,
        ir::RedirectStatus::PermanentRedirect
    ));
    assert!(out.routes[0].parents[0].accepted);
}

#[test]
fn a_redirect_status_sozu_cannot_emit_is_skipped_not_downgraded() {
    // v1.6.1 widened the allowed set from {301,302} to {301,302,303,307,308}.
    // 303 and 307 have no RedirectPolicy variant, and the catch-all used to
    // serve them as 302 — a different redirect than the one asked for, with no
    // signal to the author.
    for code in [303, 307] {
        let out = build_with_redirect(json!({ "scheme": "https", "statusCode": code }));
        assert!(
            out.ir.frontends.is_empty(),
            "statusCode {code} must not be served as something else"
        );
        let parent = &out.routes[0].parents[0];
        assert!(!parent.accepted, "{code}");
        assert_eq!(parent.accepted_reason, "UnsupportedValue");
        assert!(
            parent.problems.iter().any(|p| matches!(
                p,
                Problem::FilterUnsupported { kind } if kind.contains(&code.to_string())
            )),
            "the rejected code must be named: {:?}",
            parent.problems
        );
    }
}

#[test]
fn the_default_and_302_still_map_to_found() {
    for body in [
        json!({ "scheme": "https" }),
        json!({ "scheme": "https", "statusCode": 302 }),
    ] {
        let out = build_with_redirect(body.clone());
        let redirect = out.ir.frontends[0]
            .filters
            .redirect
            .as_ref()
            .expect("redirect programmed");
        assert!(
            matches!(redirect.status, ir::RedirectStatus::Found),
            "{body}"
        );
    }
}

#[test]
fn redirect_port_matching_the_scheme_still_programs() {
    // Gateway API derives the redirect port from the scheme when unset, so
    // `scheme: https` and `scheme: https, port: 443` are the same redirect —
    // and Sōzu expresses both with `redirect_scheme` alone. Rejecting the
    // explicit spelling would break a route that works.
    for target in [
        json!({ "scheme": "https", "port": 443, "statusCode": 301 }),
        json!({ "scheme": "http", "port": 80, "statusCode": 302 }),
    ] {
        let out = build_with_redirect(target.clone());
        assert_eq!(out.ir.frontends.len(), 1, "must still program {target}");
        assert!(out.ir.frontends[0].filters.redirect.is_some());
        assert!(out.routes[0].parents[0].accepted, "{target}");
    }

    // The implied port is dropped rather than emitted, so the Location stays
    // free of a redundant `:443`.
    let out = build_with_redirect(json!({ "scheme": "https", "port": 443, "statusCode": 301 }));
    assert_eq!(
        out.ir.frontends[0].filters.redirect.as_ref().unwrap().port,
        None
    );

    // A port the scheme does not imply is carried through — measured to land in
    // the Location as an explicit `:8443`.
    let out = build_with_redirect(json!({ "scheme": "https", "port": 8443, "statusCode": 301 }));
    assert_eq!(
        out.ir.frontends[0].filters.redirect.as_ref().unwrap().port,
        Some(8443)
    );
    assert!(out.routes[0].parents[0].accepted);
}

#[test]
fn redirect_without_scheme_is_skipped() {
    // Nothing left to change: USE_SAME + same host + same path is the same
    // self-redirect loop, just reached without any unsupported sub-field.
    let out = build_with_redirect(json!({ "statusCode": 301 }));

    assert!(out.ir.frontends.is_empty());
    let parent = &out.routes[0].parents[0];
    assert!(!parent.accepted);
    assert_eq!(parent.accepted_reason, "UnsupportedValue");
}

#[test]
fn scheme_only_redirect_still_programs() {
    // The supported shape must keep working untouched — this is what
    // examples/api-gateway/redirect.yaml and the e2e suite exercise.
    let out = build_with_redirect(json!({ "scheme": "https", "statusCode": 301 }));

    assert_eq!(out.ir.frontends.len(), 1);
    assert!(out.ir.frontends[0].cluster_id.is_none());
    let redirect = out.ir.frontends[0]
        .filters
        .redirect
        .as_ref()
        .expect("redirect programmed");
    assert!(matches!(redirect.scheme, Some(ir::Scheme::Https)));
    assert!(out.routes[0].parents[0].accepted);
}

#[test]
fn ingress_colliding_with_redirect_only_route_reports_the_ingress() {
    // A redirect-only HTTPRoute (cluster-less frontend) and an Ingress claim
    // the same host+path. The translator's dedup orders a cluster-less
    // frontend (None) before any cluster id, so the redirect wins; the losing
    // Ingress owner must see the collision.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "redirect", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "filters": [
                    { "type": "RequestRedirect",
                      "requestRedirect": { "scheme": "https", "statusCode": 301 } }
                ]
            }]
        }
    }));
    let ingress: Ingress = from_json(json!({
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": { "ingressClassName": "sozu", "rules": [{
            "host": "app.example.com",
            "http": { "paths": [{ "path": "/", "pathType": "Prefix",
                "backend": { "service": { "name": "web", "port": { "number": 80 } } } }] }
        }]}
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress]),
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1, "one frontend per route key");
    assert!(
        out.ir.frontends[0].cluster_id.is_none(),
        "the redirect-only frontend wins"
    );
    assert_eq!(
        out.results[0].problems,
        vec![Problem::RouteCollision {
            hostname: "app.example.com".to_string(),
            path: "/".to_string(),
            winner: "<redirect>".to_string(),
        }],
        "the losing Ingress carries the collision"
    );
    assert!(
        out.routes[0].parents[0].problems.is_empty(),
        "the winning route stays clean"
    );
}

#[test]
fn losing_route_parent_is_not_accepted_with_route_collision_reason() {
    // Two HTTPRoutes claim app.example.com "/": the redirect-only route wins
    // (a cluster-less frontend orders before any cluster id) and the backend
    // route loses. The loser's parent must not read fully healthy: its
    // Accepted condition downgrades with the implementation-specific
    // RouteCollision reason, so kubectl shows the collision, not just a log.
    let redirect: HttpRoute = from_json(json!({
        "metadata": { "name": "redirect", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "filters": [
                    { "type": "RequestRedirect",
                      "requestRedirect": { "scheme": "https", "statusCode": 301 } }
                ]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route_to_web(false), redirect]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1, "one frontend per route key");
    assert!(out.ir.frontends[0].cluster_id.is_none(), "redirect wins");
    let loser = out
        .routes
        .iter()
        .find(|r| r.name == "route")
        .expect("losing route present");
    let p = &loser.parents[0];
    assert!(!p.accepted, "the losing parent must not read accepted");
    assert_eq!(p.accepted_reason, "RouteCollision");
    assert!(p.problems.contains(&Problem::RouteCollision {
        hostname: "app.example.com".to_string(),
        path: "/".to_string(),
        winner: "<redirect>".to_string(),
    }));
    let winner = out
        .routes
        .iter()
        .find(|r| r.name == "redirect")
        .expect("winning route present");
    assert!(winner.parents[0].accepted, "the winner stays clean");
    assert!(winner.parents[0].problems.is_empty());
}

#[test]
fn collision_lands_on_the_parent_ref_that_produced_the_frontend() {
    // One route, TWO parentRefs to the SAME Gateway distinguished only by
    // sectionName. Only the frontend produced via listener "b" collides; the
    // attribution must key on the full parentRef identity (sectionName), not
    // stop at the first (gateway_namespace, gateway_name) match.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "a", "protocol": "HTTP", "port": 80, "hostname": "a.example.com" },
            { "name": "b", "protocol": "HTTP", "port": 80, "hostname": "b.example.com" }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [
                { "name": "gw", "sectionName": "a" },
                { "name": "gw", "sectionName": "b" }
            ],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    // Redirect-only route pinned to b.example.com: wins that key only.
    let redirect: HttpRoute = from_json(json!({
        "metadata": { "name": "redirect", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw", "sectionName": "b" }],
            "hostnames": ["b.example.com"],
            "rules": [{
                "filters": [
                    { "type": "RequestRedirect",
                      "requestRedirect": { "scheme": "https", "statusCode": 301 } }
                ]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route, redirect]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    let r = out.routes.iter().find(|r| r.name == "route").unwrap();
    assert_eq!(r.parents.len(), 2);
    let parent_a = r
        .parents
        .iter()
        .find(|p| p.section_name.as_deref() == Some("a"))
        .unwrap();
    assert!(parent_a.accepted, "listener a did not collide");
    assert!(parent_a.problems.is_empty());
    let parent_b = r
        .parents
        .iter()
        .find(|p| p.section_name.as_deref() == Some("b"))
        .unwrap();
    assert!(
        !parent_b.accepted,
        "the collision is on listener b's parent"
    );
    assert_eq!(parent_b.accepted_reason, "RouteCollision");
    assert!(parent_b.problems.contains(&Problem::RouteCollision {
        hostname: "b.example.com".to_string(),
        path: "/".to_string(),
        winner: "<redirect>".to_string(),
    }));
}

#[test]
fn gateway_and_ingress_share_one_cluster() {
    let ingress: Ingress = from_json(json!({
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": { "ingressClassName": "sozu", "rules": [{
            "host": "ing.example.com",
            "http": { "paths": [{ "path": "/", "pathType": "Prefix",
                "backend": { "service": { "name": "web", "port": { "number": 80 } } } }] }
        }]}
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress]),
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    // Both APIs target demo/web:80 -> a single shared cluster + deduped backends.
    assert_eq!(out.ir.clusters.len(), 1, "shared cluster");
    assert_eq!(out.ir.backends.len(), 2, "deduped backends");
    // Two HTTP frontends: one per host (ingress + gateway route).
    assert_eq!(out.ir.frontends.len(), 2);
}

#[test]
fn gateway_hostless_route_maps_to_catch_all() {
    // A route with no hostnames on a listener with no hostname is a catch-all:
    // it must produce a single `*` frontend (Sōzu DomainRule::Any), not be skipped.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "rules": [{
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "backendRefs": [{ "name": "web", "port": 80 }]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1);
    assert_eq!(out.ir.frontends[0].hostname, "*");
    assert!(!out.ir.frontends[0].tls);
    assert!(out.routes[0].parents[0].resolved_refs);
    assert!(out.routes[0].parents[0].problems.is_empty());
}

#[test]
fn route_hostname_not_matching_listener_is_silently_skipped() {
    // Listener constrained to a.example.com; route serves only b.example.com.
    // The route attaches elsewhere, so emit no frontend here AND no problem
    // (this is not a hostless rule).
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80, "hostname": "a.example.com" }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["b.example.com"],
            "rules": [{
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "backendRefs": [{ "name": "web", "port": 80 }]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(out.ir.frontends.is_empty());
    assert!(out.routes[0].parents[0].problems.is_empty());
}

#[test]
fn wildcard_route_on_specific_listener_narrows_to_the_listener_hostname() {
    // Listener pinned to test.example.com; route hostname *.example.com. The
    // Gateway API intersects the two: only test.example.com may be served, so
    // the frontend must carry the listener's (more specific) hostname — a
    // *.example.com frontend would also route other.example.com, which this
    // listener never admits.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80, "hostname": "test.example.com" }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["*.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1);
    assert_eq!(out.ir.frontends[0].hostname, "test.example.com");
    assert!(out.routes[0].parents[0].accepted);
}

#[test]
fn specific_route_on_wildcard_listener_uses_the_route_hostname() {
    // Listener *.example.com; route pinned to test.example.com: the route's
    // (more specific) hostname is the intersection and must be programmed.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80, "hostname": "*.example.com" }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["test.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1);
    assert_eq!(out.ir.frontends[0].hostname, "test.example.com");
}

#[test]
fn equal_route_and_listener_hostname_is_programmed_unchanged() {
    // https_gateway() pins app.example.com; the route serves the same name.
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![https_gateway()]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.frontends.len(), 1);
    assert_eq!(out.ir.frontends[0].hostname, "app.example.com");
}

fn inputs_with(route: HttpRoute) -> Inputs {
    Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    }
}

#[test]
fn referenced_services_cover_httproute_backends_resolved_or_not() {
    // Two rules: one resolves to `web`, one targets a Service that does not
    // exist. Both must land in `referenced_services` — the EndpointSlice ping
    // filter feeds on it, and a slice appearing later for the still-missing
    // backend has to wake the reconcile loop.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [
                { "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                  "backendRefs": [{ "name": "web", "port": 80 }] },
                { "matches": [{ "path": { "type": "PathPrefix", "value": "/missing" } }],
                  "backendRefs": [{ "name": "missing", "port": 80 }] }
            ]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));

    let referenced: Vec<&str> = out.referenced_services.iter().map(|s| s.as_str()).collect();
    assert_eq!(referenced, vec!["demo/missing", "demo/web"]);
    // Sanity: the second backend really did fail to resolve.
    assert!(out.routes[0].parents[0]
        .problems
        .contains(&Problem::ServiceNotFound {
            service: "missing".to_string()
        }));
}

#[test]
fn parentref_section_name_not_matching_listener_is_not_accepted() {
    // sectionName matches no listener -> Accepted=False / NoMatchingParent.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw", "sectionName": "does-not-exist" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));
    let p = &out.routes[0].parents[0];
    assert!(!p.accepted);
    assert_eq!(p.accepted_reason, "NoMatchingParent");
    assert!(out.ir.frontends.is_empty());
}

#[test]
fn non_service_backend_ref_is_invalid_kind() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "group": "x.io", "kind": "Foo", "name": "web", "port": 80 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));
    let p = &out.routes[0].parents[0];
    assert!(p.accepted);
    assert!(!p.resolved_refs);
    assert_eq!(p.resolved_refs_reason, "InvalidKind");
}

#[test]
fn nonexistent_backend_ref_is_backend_not_found() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "ghost", "port": 80 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));
    let p = &out.routes[0].parents[0];
    assert!(!p.resolved_refs);
    assert_eq!(p.resolved_refs_reason, "BackendNotFound");
}

#[test]
fn cross_namespace_backend_without_grant_is_ref_not_permitted() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "namespace": "other", "name": "web", "port": 80 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));
    let p = &out.routes[0].parents[0];
    assert!(!p.resolved_refs);
    assert_eq!(p.resolved_refs_reason, "RefNotPermitted");
}

#[test]
fn cross_namespace_route_to_same_listener_is_not_allowed() {
    // http_gateway() (ns "demo") has no allowedRoutes -> default `Same`. A route in
    // another namespace must NOT bind: Accepted=False / NotAllowedByListeners.
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "other" },
        "spec": {
            "parentRefs": [{ "name": "gw", "namespace": "demo" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let out = build(&BuildConfig::default(), &inputs_with(route));
    let p = &out.routes[0].parents[0];
    assert!(!p.accepted);
    assert_eq!(p.accepted_reason, "NotAllowedByListeners");
    assert!(out.ir.frontends.is_empty());
}

#[test]
fn cross_namespace_route_to_all_listener_is_accepted() {
    // A listener with `allowedRoutes.namespaces.from: All` admits routes from any
    // namespace (the backend ref is unrelated to this assertion).
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80,
              "allowedRoutes": { "namespaces": { "from": "All" } } }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "other" },
        "spec": {
            "parentRefs": [{ "name": "gw", "namespace": "demo" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    let p = &out.routes[0].parents[0];
    assert!(p.accepted);
    assert_eq!(p.accepted_reason, "Accepted");
}

/// A Namespace carrying `labels`, so a selector has something to match on.
fn namespace(name: &str, labels: &[(&str, &str)]) -> k8s_openapi::api::core::v1::Namespace {
    let labels: serde_json::Map<String, serde_json::Value> = labels
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v)))
        .collect();
    from_json(json!({ "metadata": { "name": name, "labels": labels } }))
}

#[test]
fn a_selector_listener_admits_exactly_the_namespaces_it_selects() {
    // `from: Selector` is evaluated against the Namespace cache's labels. It
    // *replaces* `Same` rather than extending it, so the Gateway's own
    // namespace is admitted only if its labels match — like any other.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80,
              "allowedRoutes": { "namespaces": { "from": "Selector",
                  "selector": { "matchLabels": { "team": "web" } } } } }
        ]}
    }));
    let admitted: HttpRoute = from_json(json!({
        "metadata": { "name": "admitted", "namespace": "other" },
        "spec": {
            "parentRefs": [{ "name": "gw", "namespace": "demo" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let refused: HttpRoute = from_json(json!({
        "metadata": { "name": "refused", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![admitted, refused]),
        // `other` carries the label; `demo` — the Gateway's own namespace —
        // does not.
        namespaces: arcs(vec![
            namespace("other", &[("team", "web")]),
            namespace("demo", &[("team", "infra")]),
        ]),
        // The admitted route lives in `other`, so its backendRef resolves
        // there: give that namespace its own Service, or the route would be
        // admitted and then route nowhere.
        services: arcs(vec![
            web_service(),
            from_json(json!({
                "metadata": { "name": "web", "namespace": "other" },
                "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
            })),
        ]),
        endpointslices: arcs(vec![
            web_slice(),
            from_json(json!({
                "metadata": { "name": "web-1", "namespace": "other",
                    "labels": { "kubernetes.io/service-name": "web" } },
                "addressType": "IPv4",
                "ports": [{ "name": "http", "port": 8080 }],
                "endpoints": [{ "addresses": ["10.244.1.5"], "conditions": { "ready": true } }]
            })),
        ]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    let admitted = out.routes.iter().find(|r| r.name == "admitted").unwrap();
    assert!(admitted.parents[0].accepted, "the labelled namespace is in");
    assert_eq!(admitted.parents[0].accepted_reason, "Accepted");
    let refused = out.routes.iter().find(|r| r.name == "refused").unwrap();
    assert!(
        !refused.parents[0].accepted,
        "Selector replaces Same: the Gateway's own namespace is not special"
    );
    assert_eq!(refused.parents[0].accepted_reason, "NotAllowedByListeners");

    // The listener is ordinary now: programmed, no problem reported, and it
    // counts only the route it actually admits.
    let l = &out.gateways[0].listeners[0];
    assert!(
        l.programmed,
        "a selector that parses is just an admission policy"
    );
    assert_eq!(l.attached_routes, 1);
    assert!(out.gateways[0].problems.is_empty());
    assert_eq!(out.ir.frontends.len(), 1);
}

#[test]
fn a_selector_listener_loads_its_certificates() {
    // The old fail-closed stance skipped cert loading, because material for a
    // listener that serves nothing has no business in Sōzu. A selector that
    // parses serves routes, so its certificates load like any other listener's.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [{
            "name": "https", "protocol": "HTTPS", "port": 443,
            "hostname": "app.example.com",
            "tls": { "mode": "Terminate", "certificateRefs": [{ "name": "app-tls" }] },
            "allowedRoutes": { "namespaces": { "from": "Selector",
                "selector": { "matchLabels": { "team": "web" } } } }
        }]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route_to_web(false)]),
        namespaces: arcs(vec![namespace("demo", &[("team", "web")])]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.certificates.len(), 1);
    assert!(!out.ir.frontends.is_empty());
    let l = &out.gateways[0].listeners[0];
    assert!(l.programmed);
    assert!(out.gateways[0].problems.is_empty());
}

#[test]
fn a_selector_that_cannot_be_evaluated_still_fails_closed() {
    // `operator` is a bare string in the CRD, so a newer Gateway API can add
    // one this build cannot name. That is the case the fail-closed stance is
    // for: admit nothing, do not read cleanly Programmed, load no certificate,
    // and say why — never widen a restriction into `All` by guessing.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [{
            "name": "https", "protocol": "HTTPS", "port": 443,
            "hostname": "app.example.com",
            "tls": { "mode": "Terminate", "certificateRefs": [{ "name": "app-tls" }] },
            "allowedRoutes": { "namespaces": { "from": "Selector", "selector": {
                "matchExpressions": [{ "key": "team", "operator": "Sorta", "values": ["web"] }]
            } } }
        }]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route_to_web(false)]),
        namespaces: arcs(vec![namespace("demo", &[("team", "web")])]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(out.ir.frontends.is_empty(), "no route admitted");
    assert!(out.ir.certificates.is_empty(), "and no certificate loaded");
    let l = &out.gateways[0].listeners[0];
    assert!(!l.programmed);
    assert_eq!(l.programmed_reason, "Invalid");
    assert!(out.gateways[0].problems.iter().any(|p| matches!(
        p,
        Problem::NamespaceSelectorInvalid { listener, reason }
            if listener == "https" && reason.contains("Sorta")
    )));
    assert_eq!(
        out.routes[0].parents[0].accepted_reason,
        "NotAllowedByListeners"
    );
}

#[test]
fn a_namespace_missing_from_the_cache_admits_nothing() {
    // Only reachable in the window before the Namespace cache syncs. A route
    // whose namespace we cannot look up cannot be matched against the
    // selector, so it is refused rather than waved through; the next event
    // re-evaluates it.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80,
              "allowedRoutes": { "namespaces": { "from": "Selector",
                  "selector": {} } } }
        ]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route_to_web(false)]),
        namespaces: arcs(vec![]), // cache still empty
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    // The selector itself is valid and matches everything, so nothing is
    // *reported* — there is no gap to report, only a namespace we cannot see.
    assert!(out.ir.frontends.is_empty());
    assert!(out.gateways[0].problems.is_empty());
    assert_eq!(
        out.routes[0].parents[0].accepted_reason,
        "NotAllowedByListeners"
    );
}

#[test]
fn listener_port_mismatch_is_reported_and_not_programmed() {
    // The advertised gateway ports default to 80/443: a listener declaring
    // port 8080 is not served on any client-visible port. Its routes must
    // NOT silently land on :80 — fail closed and report the mismatch — and
    // a route whose ONLY matching listener is port-mismatched must not read
    // healthy or count toward attachedRoutes.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http-alt", "protocol": "HTTP", "port": 8080 }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(out.ir.frontends.is_empty(), "no traffic on the wrong port");
    assert!(out.gateways[0].problems.contains(&Problem::PortNotExposed {
        listener: "http-alt".to_string(),
        declared: 8080,
        protocol: "HTTP".to_string(),
        // The message names what the author could have written instead.
        exposed: vec![80],
    }));
    let l = &out.gateways[0].listeners[0];
    assert!(!l.accepted);
    assert_eq!(l.accepted_reason, "PortUnavailable");
    assert!(!l.programmed);
    assert_eq!(l.programmed_reason, "Invalid");
    assert_eq!(
        l.attached_routes, 0,
        "a mismatched listener carries no routes"
    );
    let p = &out.routes[0].parents[0];
    assert!(!p.accepted, "the route must not read healthy");
    assert_eq!(p.accepted_reason, "NotAllowedByListeners");
}

#[test]
fn listener_on_the_advertised_port_is_programmed() {
    // The check compares against the *configured* advertised port, not a
    // hardcoded 80: with gateway_http_port overridden to 8080, a listener
    // declaring 8080 programs fine.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 8080 }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    // Advertise HTTP on 8080 instead of 80: a listener declaring 80 is then
    // no longer on the menu.
    let cfg = BuildConfig {
        exposure: vec![ExposedPort {
            name: "http".into(),
            port: 8080,
            bind: 8080,
            protocol: ExposedProtocol::Http,
            transport: None,
            owner: None,
        }],
        ..Default::default()
    };
    let out = build(&cfg, &inputs);

    assert_eq!(out.ir.frontends.len(), 1);
    let l = &out.gateways[0].listeners[0];
    assert!(l.accepted && l.programmed);
    assert!(out.gateways[0].problems.is_empty());
}

#[test]
fn standard_gateway_ports_are_accepted_on_unprivileged_binds() {
    // The shipped chart binds the pod on 8080/8443 while the LoadBalancer
    // Service exposes 80/443. `listener.port` is the client-visible port, so
    // a standard Gateway declaring 80/443 MUST be accepted under that config
    // — comparing against the bind ports would reject every default install.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80 },
            { "name": "https", "protocol": "HTTPS", "port": 443,
              "hostname": "app.example.com",
              "tls": { "mode": "Terminate", "certificateRefs": [{ "name": "app-tls" }] } }
        ]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret()]),
        ..Default::default()
    };
    // The chart's real shape: 80/443 advertised, bound unprivileged. The two
    // ports of an entry differing is the normal case, not an edge one.
    let cfg = BuildConfig {
        exposure: vec![
            ExposedPort {
                name: "http".into(),
                port: 80,
                bind: 8080,
                protocol: ExposedProtocol::Http,
                transport: None,
                owner: None,
            },
            ExposedPort {
                name: "https".into(),
                port: 443,
                bind: 8443,
                protocol: ExposedProtocol::Https,
                transport: None,
                owner: None,
            },
        ],
        ..Default::default()
    };
    let out = build(&cfg, &inputs);

    assert!(out.gateways[0].problems.is_empty(), "no port mismatch");
    for l in &out.gateways[0].listeners {
        assert!(l.accepted && l.programmed, "listener {} healthy", l.name);
    }
    assert_eq!(out.ir.frontends.len(), 2, "HTTP + HTTPS frontends emitted");
    assert_eq!(out.ir.certificates.len(), 1, "listener cert loaded");
    assert!(out.routes[0].parents[0].accepted);
}

#[test]
fn gateway_listener_status_counts_attached_routes() {
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80 },
            { "name": "http-unattached", "protocol": "HTTP", "port": 80 }
        ]}
    }));
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "route", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw", "sectionName": "http" }],
            "hostnames": ["app.example.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    let g = &out.gateways[0];
    assert_eq!(g.listeners.len(), 2, "status for every declared listener");
    let http = g.listeners.iter().find(|l| l.name == "http").unwrap();
    assert_eq!(http.attached_routes, 1);
    assert_eq!(http.supported_kinds, vec!["HTTPRoute".to_string()]);
    assert!(http.accepted && http.programmed && http.resolved_refs);
    let unattached = g
        .listeners
        .iter()
        .find(|l| l.name == "http-unattached")
        .unwrap();
    assert_eq!(unattached.attached_routes, 0);
}

#[test]
fn gateway_listener_invalid_route_kind() {
    // allowedRoutes.kinds requests a kind we don't serve -> supportedKinds empty,
    // ResolvedRefs=False / InvalidRouteKinds.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80,
              "allowedRoutes": { "kinds": [{ "kind": "TCPRoute" }] } }
        ]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    let l = &out.gateways[0].listeners[0];
    assert!(l.supported_kinds.is_empty());
    assert!(!l.resolved_refs);
    assert_eq!(l.resolved_refs_reason, "InvalidRouteKinds");
    assert_eq!(l.attached_routes, 0);
}

#[test]
fn cross_namespace_cert_without_grant_is_ref_not_permitted() {
    // HTTPS listener whose certificateRef is in another namespace, with no
    // ReferenceGrant -> listener ResolvedRefs=False / RefNotPermitted, unprogrammed.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [{
            "name": "https", "protocol": "HTTPS", "port": 443,
            "hostname": "app.example.com",
            "tls": { "mode": "Terminate",
                     "certificateRefs": [{ "name": "app-tls", "namespace": "certs" }] }
        }]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    let l = &out.gateways[0].listeners[0];
    assert!(!l.programmed);
    assert!(!l.resolved_refs);
    assert_eq!(l.resolved_refs_reason, "RefNotPermitted");
}

#[test]
fn cert_grant_with_wrong_from_group_is_not_permitted() {
    // A ReferenceGrant in the right namespace but with a non-matching `from.group`
    // must NOT permit the ref (group is part of the match).
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [{
            "name": "https", "protocol": "HTTPS", "port": 443, "hostname": "app.example.com",
            "tls": { "mode": "Terminate",
                     "certificateRefs": [{ "name": "app-tls", "namespace": "certs" }] }
        }]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        reference_grants: arcs(vec![from_json(json!({
            "metadata": { "name": "g", "namespace": "certs" },
            "spec": {
                "from": [{ "group": "wrong.group", "kind": "Gateway", "namespace": "demo" }],
                "to": [{ "group": "", "kind": "Secret" }]
            }
        }))]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    let l = &out.gateways[0].listeners[0];
    assert!(!l.resolved_refs);
    assert_eq!(l.resolved_refs_reason, "RefNotPermitted");
}

// ---------------------------------------------------------------------------
// Layer-4 routes (TCPRoute / UDPRoute)
// ---------------------------------------------------------------------------

/// The chart defaults plus one exposed TCP port and one exposed UDP port.
fn l4_config() -> BuildConfig {
    let mut cfg = BuildConfig::default();
    cfg.exposure.push(ExposedPort {
        name: "postgres".to_string(),
        port: 5432,
        bind: 5432,
        protocol: ExposedProtocol::Tcp,
        transport: Some("TCP".to_string()),
        owner: None,
    });
    cfg.exposure.push(ExposedPort {
        name: "dns".to_string(),
        port: 5353,
        bind: 5353,
        protocol: ExposedProtocol::Udp,
        transport: Some("UDP".to_string()),
        owner: None,
    });
    cfg
}

fn l4_gateway() -> Gateway {
    from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "postgres", "protocol": "TCP", "port": 5432 },
            { "name": "dns", "protocol": "UDP", "port": 5353 }
        ]}
    }))
}

/// A Service with one ready pod IP, so an L4 route resolves to a real backend.
fn l4_service(name: &str, port: i32) -> Service {
    from_json(json!({
        "metadata": { "name": name, "namespace": "demo" },
        "spec": { "ports": [{ "name": "svc", "port": port, "targetPort": port }] }
    }))
}

fn l4_slice(name: &str, port: i32, ip: &str) -> EndpointSlice {
    from_json(json!({
        "metadata": { "name": format!("{name}-1"), "namespace": "demo",
            "labels": { "kubernetes.io/service-name": name } },
        "addressType": "IPv4",
        "ports": [{ "name": "svc", "port": port }],
        "endpoints": [{ "addresses": [ip], "conditions": { "ready": true } }]
    }))
}

fn tcp_route(name: &str, created: Option<&str>, backend: serde_json::Value) -> TcpRoute {
    let mut meta = json!({ "name": name, "namespace": "demo",
        "uid": format!("44444444-4444-4444-4444-{name:0>12}") });
    if let Some(created) = created {
        meta["creationTimestamp"] = json!(created);
    }
    from_json(json!({
        "metadata": meta,
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "rules": [{ "backendRefs": backend }]
        }
    }))
}

fn udp_route(name: &str) -> UdpRoute {
    from_json(json!({
        "metadata": { "name": name, "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "rules": [{ "backendRefs": [{ "name": "coredns", "port": 53 }] }]
        }
    }))
}

fn l4_inputs(tcp: Vec<TcpRoute>, udp: Vec<UdpRoute>) -> Inputs {
    Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![l4_gateway()]),
        tcp_routes: arcs(tcp),
        udp_routes: arcs(udp),
        services: arcs(vec![
            l4_service("postgres", 5432),
            l4_service("postgres-b", 5432),
            l4_service("coredns", 53),
        ]),
        endpointslices: arcs(vec![
            l4_slice("postgres", 5432, "10.244.0.10"),
            l4_slice("postgres-b", 5432, "10.244.0.11"),
            l4_slice("coredns", 53, "10.244.0.12"),
        ]),
        ..Default::default()
    }
}

#[test]
fn tcp_and_udp_routes_map_to_l4_frontends() {
    let inputs = l4_inputs(
        vec![tcp_route(
            "db",
            Some("2026-01-01T00:00:00Z"),
            json!([{ "name": "postgres", "port": 5432 }]),
        )],
        vec![udp_route("dns")],
    );
    let out = build(&l4_config(), &inputs);

    assert_eq!(out.ir.l4_frontends.len(), 2, "one TCP + one UDP frontend");
    let tcp = out
        .ir
        .l4_frontends
        .iter()
        .find(|f| f.protocol == ir::L4Protocol::Tcp)
        .expect("a TCP frontend");
    assert_eq!(tcp.listener.port(), 5432, "the exposure entry's bind");
    assert_eq!(tcp.cluster_id, "demo.postgres.5432");
    let udp = out
        .ir
        .l4_frontends
        .iter()
        .find(|f| f.protocol == ir::L4Protocol::Udp)
        .expect("a UDP frontend");
    assert_eq!(udp.cluster_id, "demo.coredns.53");

    // Both routes report themselves accepted on their listener, and each
    // listener counts its attached route.
    assert_eq!(out.routes.len(), 2);
    for r in &out.routes {
        assert!(r.parents[0].accepted, "{} not accepted", r.name);
        assert!(r.parents[0].resolved_refs, "{} refs unresolved", r.name);
        assert!(r.parents[0].problems.is_empty());
    }
    for l in &out.gateways[0].listeners {
        assert!(l.programmed, "listener {} not programmed", l.name);
        assert_eq!(l.attached_routes, 1, "listener {}", l.name);
    }
    assert_eq!(out.gateways[0].listeners[0].supported_kinds, ["TCPRoute"]);
    assert_eq!(out.gateways[0].listeners[1].supported_kinds, ["UDPRoute"]);
}

/// Two routes claiming one socket for different Services: the older keeps it,
/// the younger is dropped with a Problem on its own status — and the build as
/// a whole still succeeds. That last part is the point: settling this in the
/// translator returns an error that fails the *entire* reconcile, so one
/// tenant's second route would stop routing for every other tenant.
#[test]
fn the_older_route_keeps_a_contested_socket() {
    let inputs = l4_inputs(
        vec![
            tcp_route(
                "younger",
                Some("2026-02-01T00:00:00Z"),
                json!([{ "name": "postgres-b", "port": 5432 }]),
            ),
            tcp_route(
                "older",
                Some("2026-01-01T00:00:00Z"),
                json!([{ "name": "postgres", "port": 5432 }]),
            ),
        ],
        vec![],
    );
    let out = build(&l4_config(), &inputs);

    assert_eq!(out.ir.l4_frontends.len(), 1, "a socket carries one route");
    assert_eq!(out.ir.l4_frontends[0].cluster_id, "demo.postgres.5432");

    let younger = out.routes.iter().find(|r| r.name == "younger").unwrap();
    assert!(!younger.parents[0].accepted);
    assert_eq!(younger.parents[0].accepted_reason, "RouteConflict");
    assert!(matches!(
        younger.parents[0].problems.first(),
        Some(Problem::L4RouteConflict { port: 5432, protocol: "TCP", winner }) if winner == "demo/older"
    ));
    let older = out.routes.iter().find(|r| r.name == "older").unwrap();
    assert!(older.parents[0].accepted);
    assert!(older.parents[0].problems.is_empty());
}

/// `creationTimestamp` has one-second granularity, so two routes applied
/// together tie routinely. Without the name as a second key the winner would
/// follow cache iteration order and flip between reconciles.
#[test]
fn a_creation_timestamp_tie_is_broken_by_name() {
    let same = Some("2026-01-01T00:00:00Z");
    let build_with = |order: [&str; 2]| {
        let routes = order
            .iter()
            .map(|n| {
                let svc = if *n == "aaa" {
                    "postgres"
                } else {
                    "postgres-b"
                };
                tcp_route(n, same, json!([{ "name": svc, "port": 5432 }]))
            })
            .collect();
        build(&l4_config(), &l4_inputs(routes, vec![]))
    };
    for order in [["aaa", "zzz"], ["zzz", "aaa"]] {
        let out = build_with(order);
        assert_eq!(out.ir.l4_frontends.len(), 1);
        assert_eq!(
            out.ir.l4_frontends[0].cluster_id, "demo.postgres.5432",
            "input order {order:?} must not decide the winner"
        );
    }
}

/// A route with no `creationTimestamp` sorts last: an established route must
/// not lose its socket to one whose age cannot be established.
#[test]
fn a_route_without_a_creation_timestamp_never_evicts_one_with_it() {
    let inputs = l4_inputs(
        vec![
            tcp_route(
                "aaa-undated",
                None,
                json!([{ "name": "postgres-b", "port": 5432 }]),
            ),
            tcp_route(
                "zzz-dated",
                Some("2026-06-01T00:00:00Z"),
                json!([{ "name": "postgres", "port": 5432 }]),
            ),
        ],
        vec![],
    );
    let out = build(&l4_config(), &inputs);
    assert_eq!(out.ir.l4_frontends.len(), 1);
    assert_eq!(out.ir.l4_frontends[0].cluster_id, "demo.postgres.5432");
}

/// The rejections HTTPRoute already applies hold unchanged at layer 4. The
/// weight-0 drain is the one that is easy to forget: there is no traffic
/// shaping down here to remind you it exists.
#[test]
fn weighted_and_drained_backends_are_refused() {
    let zero = build(
        &l4_config(),
        &l4_inputs(
            vec![tcp_route(
                "db",
                None,
                json!([{ "name": "postgres", "port": 5432, "weight": 0 }]),
            )],
            vec![],
        ),
    );
    assert!(
        zero.ir.l4_frontends.is_empty(),
        "a drained backend routes nothing"
    );
    assert!(!zero.routes[0].parents[0].resolved_refs);
    assert!(zero.routes[0].parents[0]
        .problems
        .iter()
        .any(|p| matches!(p, Problem::ZeroWeightBackendUnsupported { .. })));

    let split = build(
        &l4_config(),
        &l4_inputs(
            vec![tcp_route(
                "db",
                None,
                json!([
                    { "name": "postgres", "port": 5432, "weight": 50 },
                    { "name": "postgres-b", "port": 5432, "weight": 50 }
                ]),
            )],
            vec![],
        ),
    );
    assert!(
        split.ir.l4_frontends.is_empty(),
        "Sōzu cannot weight a split"
    );
    assert!(split.routes[0].parents[0]
        .problems
        .contains(&Problem::WeightedBackendsUnsupported));
}

/// A listener serves exactly one route kind. A TCPRoute pointed at an
/// HTTP-only Gateway has no parent to bind to — it must say so, not attach
/// silently to a listener that cannot carry it.
#[test]
fn a_tcp_route_does_not_bind_to_an_http_listener() {
    let mut inputs = l4_inputs(
        vec![tcp_route(
            "db",
            None,
            json!([{ "name": "postgres", "port": 5432 }]),
        )],
        vec![],
    );
    inputs.gateways = arcs(vec![http_gateway()]);
    let out = build(&l4_config(), &inputs);

    assert!(out.ir.l4_frontends.is_empty());
    // The parentRef did address a listener; that listener just does not serve
    // TCPRoute — which the Gateway API spells `NotAllowedByListeners`.
    assert_eq!(
        out.routes[0].parents[0].accepted_reason,
        "NotAllowedByListeners"
    );
    assert_eq!(out.gateways[0].listeners[0].attached_routes, 0);
}

/// A layer-4 port the exposure table does not carry has no Service port
/// routing to it, so a listener on it could never receive traffic.
#[test]
fn an_unexposed_l4_listener_port_is_refused() {
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "postgres", "protocol": "TCP", "port": 9999 }
        ]}
    }));
    let mut inputs = l4_inputs(
        vec![tcp_route(
            "db",
            None,
            json!([{ "name": "postgres", "port": 5432 }]),
        )],
        vec![],
    );
    inputs.gateways = arcs(vec![gw]);
    let out = build(&l4_config(), &inputs);

    assert!(out.ir.l4_frontends.is_empty());
    let l = &out.gateways[0].listeners[0];
    assert!(!l.accepted);
    assert_eq!(l.accepted_reason, "PortUnavailable");
    assert!(out.gateways[0]
        .problems
        .iter()
        .any(|p| matches!(p, Problem::PortNotExposed { declared: 9999, .. })));
    // The listener still declares which kind it would have served.
    assert_eq!(l.supported_kinds, ["TCPRoute"]);
}

/// A layer-4 socket carries exactly one route and has no hostname to
/// arbitrate with, so the exposure table may name the namespace allowed to
/// claim it — and a Gateway from anywhere else is refused, not served.
#[test]
fn an_l4_listener_honours_the_ports_owner() {
    let mut cfg = l4_config();
    cfg.exposure
        .iter_mut()
        .find(|e| e.name == "postgres")
        .unwrap()
        .owner = Some("infra".to_string());
    let inputs = l4_inputs(
        vec![tcp_route(
            "db",
            None,
            json!([{ "name": "postgres", "port": 5432 }]),
        )],
        vec![],
    );
    let out = build(&cfg, &inputs);

    assert!(out.ir.l4_frontends.is_empty());
    let l = &out.gateways[0].listeners[0];
    assert!(!l.accepted);
    assert_eq!(l.accepted_reason, "PortUnavailable");
    assert!(out.gateways[0].problems.iter().any(|p| matches!(
        p,
        Problem::ListenerPortNotOwned { port: 5432, owner, claimed_by, .. }
            if owner == "infra" && claimed_by == "demo"
    )));
}

/// A listener attaches to the exposure entry that serves *its own* port, not
/// to the first entry of its protocol. With several HTTPS ports exposed,
/// picking the first would land a Gateway's routes — and its certificate — on
/// a port it never declared.
///
/// Two HTTPS entries is more than the chart will render today (Sōzu's static
/// listeners are one per protocol, so `validateExposure` caps it), but the
/// builder is what defines the contract: an entry is looked up, not guessed
/// from the protocol.
#[test]
fn a_listener_binds_to_the_entry_serving_its_own_port() {
    let cfg = BuildConfig {
        exposure: vec![
            ExposedPort {
                name: "https".to_string(),
                port: 443,
                bind: 8443,
                protocol: ExposedProtocol::Https,
                transport: Some("TCP".to_string()),
                owner: None,
            },
            ExposedPort {
                name: "https-alt".to_string(),
                port: 9443,
                bind: 9444,
                protocol: ExposedProtocol::Https,
                transport: Some("TCP".to_string()),
                owner: None,
            },
        ],
        ..Default::default()
    };
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [{
            "name": "https", "protocol": "HTTPS", "port": 9443,
            "hostname": "app.example.com",
            "tls": { "mode": "Terminate", "certificateRefs": [{ "name": "app-tls" }] }
        }]}
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![route_to_web(false)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret()]),
        ..Default::default()
    };
    let out = build(&cfg, &inputs);

    assert_eq!(out.ir.certificates.len(), 1);
    assert_eq!(out.ir.certificates[0].listener.port(), 9444);
    assert!(!out.ir.frontends.is_empty());
    for f in &out.ir.frontends {
        assert_eq!(f.listener.port(), 9444, "frontends follow their listener");
    }
}

#[test]
fn a_route_sharing_no_hostname_with_its_listener_is_not_accepted() {
    // The shape the conformance suite pins: a listener with a hostname, two
    // routes that inherit it, and one whose own hostname intersects nothing.
    //
    // The odd one out is attached to nothing, so it must say so — reading
    // Accepted while serving no traffic is the silent-acceptance this project
    // exists to avoid — and it must not inflate the listener's attachedRoutes,
    // which is the count of routes actually bound to it.
    let gw: Gateway = from_json(json!({
        "metadata": { "name": "gw", "namespace": "demo" },
        "spec": { "gatewayClassName": "sozu", "listeners": [
            { "name": "http", "protocol": "HTTP", "port": 80, "hostname": "foo.example.com" }
        ]}
    }));
    let inherits: HttpRoute = from_json(json!({
        "metadata": { "name": "inherits", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let elsewhere: HttpRoute = from_json(json!({
        "metadata": { "name": "elsewhere", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["not-accepted.test.com"],
            "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![gw]),
        http_routes: arcs(vec![inherits, elsewhere]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    let inherits = out.routes.iter().find(|r| r.name == "inherits").unwrap();
    assert!(inherits.parents[0].accepted);
    let elsewhere = out.routes.iter().find(|r| r.name == "elsewhere").unwrap();
    assert!(!elsewhere.parents[0].accepted, "it is attached to nothing");
    assert_eq!(
        elsewhere.parents[0].accepted_reason,
        "NoMatchingListenerHostname"
    );

    assert_eq!(
        out.gateways[0].listeners[0].attached_routes, 1,
        "only the route the listener actually carries counts"
    );
    // And nothing was programmed for the route that matches no hostname.
    assert_eq!(out.ir.frontends.len(), 1);
    assert_eq!(out.ir.frontends[0].hostname, "foo.example.com");
}

// ---------------------------------------------------------------------------
// RequestRedirect targets (hostname / path / port)
// ---------------------------------------------------------------------------

/// An HTTPRoute with one redirect-only rule carrying `requestRedirect`.
fn redirect_route(request_redirect: serde_json::Value) -> HttpRoute {
    from_json(json!({
        "metadata": { "name": "redir", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "filters": [{ "type": "RequestRedirect", "requestRedirect": request_redirect }]
            }]
        }
    }))
}

fn build_redirect(request_redirect: serde_json::Value) -> sozu_gw_builder::BuildOutput {
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![redirect_route(request_redirect)]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    build(&BuildConfig::default(), &inputs)
}

/// The exact shape `HTTPRouteRedirectHostAndStatus` pins: a hostname target,
/// no scheme and no statusCode. It used to be refused wholesale — the
/// scheme-less guard existed because without a target the `Location` echoes the
/// request and the client loops. A hostname *is* a target, so there is no loop.
#[test]
fn a_hostname_redirect_needs_neither_scheme_nor_status() {
    let out = build_redirect(json!({ "hostname": "example.org" }));

    assert!(out.routes[0].parents[0].accepted);
    assert!(out.routes[0].parents[0].problems.is_empty());
    let redirect = out.ir.frontends[0]
        .filters
        .redirect
        .as_ref()
        .expect("programmed");
    assert_eq!(redirect.hostname.as_deref(), Some("example.org"));
    // Unset scheme and path keep the request's own — Sōzu's USE_SAME.
    assert_eq!(redirect.scheme, None);
    assert_eq!(redirect.path, None);
    // Gateway API's default statusCode is 302.
    assert_eq!(redirect.status, ir::RedirectStatus::Found);
}

#[test]
fn a_redirect_carries_status_path_and_port_targets() {
    let out = build_redirect(json!({
        "statusCode": 301,
        "hostname": "example.org",
        "port": 8443,
        "path": { "type": "ReplaceFullPath", "replaceFullPath": "/moved" }
    }));
    let redirect = out.ir.frontends[0].filters.redirect.as_ref().unwrap();
    assert_eq!(redirect.status, ir::RedirectStatus::MovedPermanently);
    assert_eq!(redirect.hostname.as_deref(), Some("example.org"));
    assert_eq!(redirect.path.as_deref(), Some("/moved"));
    assert_eq!(redirect.port, Some(8443));
}

/// `ReplacePrefixMatch` needs the matched prefix's remainder, and `$PATH[n]`
/// indexes the path rule's regex captures — the regex a Kubernetes prefix
/// compiles to has exactly one group, the element boundary. Measured, so this
/// is a refusal with a reason rather than a gap.
#[test]
fn replace_prefix_match_is_refused() {
    let out = build_redirect(json!({
        "hostname": "example.org",
        "path": { "type": "ReplacePrefixMatch", "replacePrefixMatch": "/prefix" }
    }));
    assert!(out.ir.frontends.is_empty(), "the rule is skipped");
    assert!(!out.routes[0].parents[0].accepted);
    assert_eq!(out.routes[0].parents[0].accepted_reason, "UnsupportedValue");
}

/// The one that would take the cluster down. `$` opens Sōzu's rewrite-template
/// grammar and an unparseable template makes it **reject the frontend**;
/// translation is all-or-nothing, so a single such route would fail every
/// reconcile, for every tenant. Refused in the builder instead.
#[test]
fn a_dollar_in_a_redirect_target_is_refused_not_forwarded() {
    for target in [
        json!({ "hostname": "ex$ample.org" }),
        json!({ "hostname": "example.org",
                "path": { "type": "ReplaceFullPath", "replaceFullPath": "/price$100" } }),
    ] {
        let out = build_redirect(target.clone());
        assert!(
            out.ir.frontends.is_empty(),
            "{target} must not be programmed"
        );
        assert!(out.routes[0].parents[0]
            .problems
            .iter()
            .any(|p| matches!(p, Problem::FilterUnsupported { kind } if kind.contains('$'))));
    }
}

/// A redirect target and a URLRewrite compile to the *same* three Sōzu fields,
/// so one would silently overwrite the other.
#[test]
fn a_redirect_combined_with_url_rewrite_is_refused() {
    let route: HttpRoute = from_json(json!({
        "metadata": { "name": "redir", "namespace": "demo" },
        "spec": {
            "parentRefs": [{ "name": "gw" }],
            "hostnames": ["app.example.com"],
            "rules": [{
                "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                "filters": [
                    { "type": "RequestRedirect", "requestRedirect": { "hostname": "example.org" } },
                    { "type": "URLRewrite", "urlRewrite": { "hostname": "other.example.com" } }
                ]
            }]
        }
    }));
    let inputs = Inputs {
        gateway_classes: arcs(vec![gateway_class("sozu.io/gateway-controller")]),
        gateways: arcs(vec![http_gateway()]),
        http_routes: arcs(vec![route]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(out.ir.frontends.is_empty());
    assert!(out.routes[0].parents[0]
        .problems
        .iter()
        .any(|p| matches!(p, Problem::FilterUnsupported { kind }
            if kind.contains("combined with RequestRedirect"))));
}
