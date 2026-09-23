//! Golden + behavioural tests for the Builder (K8s objects -> IR).

use std::collections::BTreeMap;
use std::sync::Arc;

use k8s_openapi::api::core::v1::{Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::ByteString;
use serde_json::json;

use sozu_gw_builder::{build, BuildConfig, ExposedPort, ExposedProtocol, Inputs, Problem};
use sozu_gw_ir as ir;

const CERT_A: &str = include_str!("fixtures/cert_a.pem");
const KEY_A: &str = include_str!("fixtures/key_a.pem");
// An RSA pair for `other.example.com` (key_a is ECDSA P-256), so the key
// loader is exercised on both algorithms and the pairing check has a key that
// parses but belongs to another certificate.
const CERT_B: &str = include_str!("fixtures/cert_b.pem");
const KEY_B: &str = include_str!("fixtures/key_b.pem");
/// An X509**v1** self-signed certificate (no extensions) and, separately, a key
/// that does not match it. webpki refuses to parse a v1 leaf, so a pairing
/// check routed through it would tolerate this mismatch; x509-parser (what Sōzu
/// uses) parses it, so the pairing check must still catch it.
const CERT_V1: &str = include_str!("fixtures/cert_v1.pem");
const KEY_V1_MATCH: &str = include_str!("fixtures/key_v1_match.pem");
const KEY_V1_OTHER: &str = include_str!("fixtures/key_v1_other.pem");
/// `cert_a`'s key again, but the certificate carries the public point in
/// **compressed** form (`03` prefix; `openssl ec -conv_form compressed`).
/// ring's ECDSA verifier takes only the uncompressed `04` form, so this leaf
/// cannot be paired the way a handshake pairs it — and Sōzu loads it anyway.
const CERT_A_COMPRESSED: &str = include_str!("fixtures/cert_a_compressed.pem");

fn from_json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("valid k8s object json")
}

/// Wrap plain objects in the `Arc`s `Inputs` borrows (the controller passes
/// its reflector-cache `Arc`s straight through).
fn arcs<T>(items: Vec<T>) -> Vec<Arc<T>> {
    items.into_iter().map(Arc::new).collect()
}

fn tls_secret(ns: &str, name: &str, crt: &str, key: &str) -> Secret {
    let mut data = BTreeMap::new();
    data.insert("tls.crt".to_string(), ByteString(crt.as_bytes().to_vec()));
    data.insert("tls.key".to_string(), ByteString(key.as_bytes().to_vec()));
    Secret {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        data: Some(data),
        type_: Some("kubernetes.io/tls".to_string()),
        ..Default::default()
    }
}

/// Service `web` in `demo`: port 80 (name "http") -> targetPort 8080.
fn web_service() -> Service {
    from_json(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }))
}

/// EndpointSlice for `web` with two ready endpoints + one not-ready (excluded).
fn web_slice() -> EndpointSlice {
    from_json(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": {
            "name": "web-abc", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "web" }
        },
        "addressType": "IPv4",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [
            { "addresses": ["10.244.0.5"], "conditions": { "ready": true } },
            { "addresses": ["10.244.0.6"], "conditions": { "ready": true } },
            { "addresses": ["10.244.0.7"], "conditions": { "ready": false } }
        ]
    }))
}

fn ingress_tls() -> Ingress {
    from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        // The uid is carried into the Event reference, so pin a real one here
        // rather than let the snapshot record `null` and assert nothing.
        "metadata": { "name": "web", "namespace": "demo",
            "uid": "11111111-1111-1111-1111-111111111111" },
        "spec": {
            "ingressClassName": "sozu",
            "tls": [{ "hosts": ["app.example.com"], "secretName": "app-tls" }],
            "rules": [{
                "host": "app.example.com",
                "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }))
}

#[test]
fn happy_path_http_and_tls() {
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    // The source object's uid reaches the result, which is what lets an Event
    // reference its owner the way `kubectl describe` looks it up.
    assert_eq!(
        out.results[0].uid.as_deref(),
        Some("11111111-1111-1111-1111-111111111111")
    );

    // 1 cluster, 2 ready backends, http + https frontends, 1 cert, accepted clean.
    assert_eq!(out.ir.clusters.len(), 1);
    assert_eq!(out.ir.backends.len(), 2);
    assert_eq!(out.ir.frontends.len(), 2);
    assert_eq!(out.ir.certificates.len(), 1);
    assert_eq!(out.results.len(), 1);
    assert!(out.results[0].problems.is_empty(), "{:?}", out.results[0]);

    insta::assert_json_snapshot!(out);
}

#[test]
fn ignores_other_ingress_class() {
    let mut ing: Ingress = ingress_tls();
    ing.spec.as_mut().unwrap().ingress_class_name = Some("nginx".to_string());
    let inputs = Inputs {
        ingresses: arcs(vec![ing]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(out.ir.clusters.is_empty());
    assert!(
        out.results.is_empty(),
        "non-ours ingress must not appear in results"
    );
}

#[test]
fn missing_secret_reports_problem_and_skips_tls() {
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![]), // secret absent
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.certificates.len(), 0, "no cert without the secret");
    assert_eq!(out.ir.frontends.len(), 1, "only the HTTP frontend remains");
    assert_eq!(
        out.results[0].problems,
        vec![Problem::SecretNotFound {
            secret: "app-tls".to_string()
        }]
    );
}

#[test]
fn service_not_found_reports_problem() {
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![]), // service absent
        endpointslices: arcs(vec![]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(out.ir.clusters.is_empty());
    assert!(out.ir.frontends.is_empty());
    assert_eq!(
        out.results[0].problems,
        vec![Problem::ServiceNotFound {
            service: "web".to_string()
        }]
    );
}

#[test]
fn no_ready_endpoints_keeps_cluster_reports_problem() {
    let slice: EndpointSlice = from_json(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": { "name": "web-x", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "IPv4",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [{ "addresses": ["10.244.0.9"], "conditions": { "ready": false } }]
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![slice]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.clusters.len(), 1, "cluster is still declared");
    assert_eq!(out.ir.backends.len(), 0);
    assert!(out.results[0]
        .problems
        .contains(&Problem::NoReadyEndpoints {
            service: "web".to_string()
        }));
}

#[test]
fn fqdn_endpointslice_is_reported_and_ip_slices_keep_resolving() {
    // An FQDN-type slice carries hostnames, not IPs: it must be reported as
    // its own problem (not silently vanish into a misleading NoReadyEndpoints)
    // while IPv4/IPv6 slices for the same Service keep resolving.
    let fqdn_slice: EndpointSlice = from_json(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": { "name": "web-fqdn", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "FQDN",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [{ "addresses": ["pod.example.internal"], "conditions": { "ready": true } }]
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![fqdn_slice, web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.backends.len(), 2, "the IPv4 slice still resolves");
    assert_eq!(
        out.results[0].problems,
        vec![Problem::FqdnEndpointsUnsupported {
            service: "web".to_string()
        }]
    );
}

#[test]
fn fqdn_only_service_reports_fqdn_problem_not_just_no_endpoints() {
    let fqdn_slice: EndpointSlice = from_json(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": { "name": "web-fqdn", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "FQDN",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [{ "addresses": ["pod.example.internal"], "conditions": { "ready": true } }]
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![fqdn_slice]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(out.ir.backends.is_empty());
    assert!(
        out.results[0]
            .problems
            .contains(&Problem::FqdnEndpointsUnsupported {
                service: "web".to_string()
            }),
        "the FQDN slice must be named as the cause, got {:?}",
        out.results[0].problems
    );
}

#[test]
fn out_of_range_endpoint_port_is_skipped_not_truncated() {
    // The EndpointPort wire type is i32; a value outside u16 cannot be a real
    // port, but an `as u16` cast would silently rewrite 65616 into 80 and
    // route traffic to a port nobody declared. The entry must be skipped.
    let slice: EndpointSlice = from_json(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": { "name": "web-bad", "namespace": "demo",
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "IPv4",
        "ports": [{ "name": "http", "port": 65616 }],
        "endpoints": [{ "addresses": ["10.244.0.5"], "conditions": { "ready": true } }]
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![slice]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(
        out.ir.backends.is_empty(),
        "an out-of-range port must not truncate into a bogus backend: {:?}",
        out.ir.backends
    );
}

#[test]
fn tls_entry_without_hosts_reports_problem_and_skips_cert() {
    // secretName without hosts: SNI names come from tls.hosts, so there is
    // nothing to bind the cert to and no rule host turns HTTPS. Loading the
    // cert anyway would be half-applied material — it must be skipped and the
    // gap reported.
    let ing: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "tls": [{ "secretName": "app-tls" }],
            "rules": [{
                "host": "app.example.com",
                "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ing]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(
        out.ir.certificates.is_empty(),
        "a hostless TLS entry must not load a cert with empty SNI names"
    );
    assert_eq!(
        out.ir.frontends.len(),
        1,
        "only the plain-HTTP frontend remains"
    );
    assert!(!out.ir.frontends[0].tls);
    assert_eq!(
        out.results[0].problems,
        vec![Problem::TlsEntryWithoutHosts {
            secret: "app-tls".to_string()
        }]
    );
}

#[test]
fn path_types_map_correctly() {
    let ing: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "paths", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "host": "app.example.com",
                "http": { "paths": [
                    { "path": "/exact", "pathType": "Exact",
                      "backend": { "service": { "name": "web", "port": { "name": "http" } } } },
                    { "path": "/regex.*", "pathType": "ImplementationSpecific",
                      "backend": { "service": { "name": "web", "port": { "name": "http" } } } }
                ]}
            }]
        }
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ing]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    // Two HTTP frontends (Exact + Regex), resolved via the named service port.
    assert_eq!(out.ir.frontends.len(), 2);
    insta::assert_json_snapshot!(out.ir.frontends);
}

/// Ingress with two `ImplementationSpecific` (regex) paths on one host.
fn regex_paths_ingress(first: &str, second: &str) -> Ingress {
    from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "paths", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "host": "app.example.com",
                "http": { "paths": [
                    { "path": first, "pathType": "ImplementationSpecific",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } },
                    { "path": second, "pathType": "ImplementationSpecific",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }))
}

#[test]
fn a_regex_path_sozu_cannot_compile_is_skipped_and_reported() {
    // Measured live (2026-09-21): the pattern reaches Sōzu verbatim, which
    // answers "Could not parse rule from frontend path" — and because
    // translation is all-or-nothing, every reconcile of the shared instance
    // failed until the Ingress was removed (a new route stayed 404 meanwhile).
    // The bad path programs nothing; its valid sibling still does.
    let inputs = Inputs {
        ingresses: arcs(vec![regex_paths_ingress("/foo([", "^/api/v[0-9]+")]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1, "{:?}", out.ir.frontends);
    assert_eq!(
        out.ir.frontends[0].path,
        ir::PathMatch::Regex("^/api/v[0-9]+".to_string())
    );
    match &out.results[0].problems[..] {
        [Problem::InvalidPathRegex { path, reason }] => {
            assert_eq!(path, "/foo([");
            assert!(
                reason.contains("unclosed character class"),
                "the reason carries the regex diagnostic: {reason}"
            );
            assert!(!reason.contains('\n'), "one line: {reason:?}");
        }
        other => panic!("expected InvalidPathRegex, got {other:?}"),
    }
}

#[test]
fn a_literal_path_too_long_for_the_compiled_rule_is_refused_not_forwarded() {
    // A path Kubernetes accepts without a length bound compiles, as an Exact
    // or a non-root Prefix, to an anchored regex — and the `regex` crate Sōzu
    // uses refuses a compiled program past its 10 MiB limit, which a literal
    // of a few hundred kilobytes reaches (measured: 100 000 bytes compile,
    // 500 001 do not). Forwarded, that one path would fail every reconcile of
    // the shared instance. It must be refused here, per path, with its
    // siblings untouched — and the report must not carry the whole path, or
    // the Event itself would exceed what the apiserver stores.
    let huge = format!("/{}", "a".repeat(500_000));
    for path_type in ["Exact", "Prefix"] {
        let ing: Ingress = from_json(json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
            "metadata": { "name": "paths", "namespace": "demo" },
            "spec": {
                "ingressClassName": "sozu",
                "rules": [{
                    "host": "app.example.com",
                    "http": { "paths": [
                        { "path": huge, "pathType": path_type,
                          "backend": { "service": { "name": "web", "port": { "number": 80 } } } },
                        { "path": "/ok", "pathType": path_type,
                          "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                    ]}
                }]
            }
        }));
        let inputs = Inputs {
            ingresses: arcs(vec![ing]),
            services: arcs(vec![web_service()]),
            endpointslices: arcs(vec![web_slice()]),
            ..Default::default()
        };
        let out = build(&BuildConfig::default(), &inputs);
        let paths: Vec<&str> = out
            .ir
            .frontends
            .iter()
            .map(|f| match &f.path {
                ir::PathMatch::Prefix(v) | ir::PathMatch::Exact(v) | ir::PathMatch::Regex(v) => {
                    v.as_str()
                }
            })
            .collect();
        assert_eq!(paths, vec!["/ok"], "{path_type}: only the sibling programs");
        match &out.results[0].problems[..] {
            [Problem::InvalidPathRegex { path, reason }] => {
                assert!(
                    path.len() < 200 && path.ends_with("(500001 bytes)"),
                    "{path_type}: the report names the length, not the whole path: {path:?}"
                );
                assert!(reason.contains("size limit"), "{path_type}: {reason}");
            }
            other => panic!("{path_type}: expected InvalidPathRegex, got {other:?}"),
        }
    }
}

#[test]
fn a_valid_regex_path_reaches_the_ir_unchanged() {
    // Regression guard for the compile check: validation must not rewrite,
    // anchor or escape a pattern Sōzu accepts.
    let inputs = Inputs {
        ingresses: arcs(vec![regex_paths_ingress("^/api/v[0-9]+", "/(a|b)/c$")]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(out.results[0].problems.is_empty(), "{:?}", out.results[0]);
    let paths: Vec<&ir::PathMatch> = out.ir.frontends.iter().map(|f| &f.path).collect();
    assert_eq!(
        paths,
        vec![
            &ir::PathMatch::Regex("^/api/v[0-9]+".to_string()),
            &ir::PathMatch::Regex("/(a|b)/c$".to_string()),
        ]
    );
}

/// `web` Service carrying the load-balancing + sticky-session annotations.
fn annotated_service(lb: &str, sticky: &str) -> Service {
    from_json(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "web", "namespace": "demo",
            "annotations": {
                "sozu.io/load-balancing": lb,
                "sozu.io/sticky-sessions": sticky,
            } },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }))
}

#[test]
fn service_annotations_set_cluster_lb_and_sticky() {
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![annotated_service("least-loaded", "true")]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.clusters.len(), 1);
    assert!(matches!(
        out.ir.clusters[0].load_balancing,
        ir::LbAlgorithm::LeastLoaded
    ));
    assert!(out.ir.clusters[0].sticky_session);
}

#[test]
fn lb_annotation_is_normalised_and_unknown_defaults_to_round_robin() {
    // Spacing/underscores/case are normalised; an unknown value is not an error,
    // it just keeps the round-robin default.
    let cases = [
        ("Power_Of Two", true), // -> PowerOfTwo
        ("bogus", false),       // -> RoundRobin (unknown)
    ];
    for (value, is_p2c) in cases {
        let inputs = Inputs {
            ingresses: arcs(vec![ingress_tls()]),
            services: arcs(vec![annotated_service(value, "false")]),
            endpointslices: arcs(vec![web_slice()]),
            secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
            ..Default::default()
        };
        let out = build(&BuildConfig::default(), &inputs);
        let lb = &out.ir.clusters[0].load_balancing;
        if is_p2c {
            assert!(matches!(lb, ir::LbAlgorithm::PowerOfTwo), "value={value:?}");
        } else {
            assert!(matches!(lb, ir::LbAlgorithm::RoundRobin), "value={value:?}");
        }
        assert!(!out.ir.clusters[0].sticky_session);
    }
}

#[test]
fn no_annotations_keep_round_robin_no_sticky() {
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(matches!(
        out.ir.clusters[0].load_balancing,
        ir::LbAlgorithm::RoundRobin
    ));
    assert!(!out.ir.clusters[0].sticky_session);
}

#[test]
fn service_annotations_set_connection_limit_per_ip() {
    let svc: Service = from_json(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "web", "namespace": "demo",
            "annotations": {
                "sozu.io/max-connections-per-ip": "100",
                "sozu.io/retry-after": "30",
            } },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![svc]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.clusters[0].max_connections_per_ip, Some(100));
    assert_eq!(out.ir.clusters[0].retry_after, Some(30));
}

#[test]
fn non_numeric_connection_limit_is_ignored() {
    // A typo'd value falls back to the global default rather than failing.
    let svc: Service = from_json(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "web", "namespace": "demo",
            "annotations": { "sozu.io/max-connections-per-ip": "lots" } },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![svc]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.clusters[0].max_connections_per_ip, None);
    assert_eq!(out.ir.clusters[0].retry_after, None);
}

#[test]
fn tls_ingress_redirects_http_to_https_by_default() {
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    // Sorted (tls, host, cluster): [0] = HTTP frontend, [1] = HTTPS frontend.
    let http = &out.ir.frontends[0];
    let https = &out.ir.frontends[1];
    assert!(!http.tls);
    assert!(https.tls);
    let r = http
        .filters
        .redirect
        .as_ref()
        .expect("HTTP frontend redirects");
    assert!(matches!(r.scheme, Some(ir::Scheme::Https)));
    assert!(matches!(r.status, ir::RedirectStatus::MovedPermanently));
    assert!(
        https.filters.redirect.is_none(),
        "the HTTPS frontend must serve, not redirect"
    );
}

#[test]
fn ssl_redirect_can_be_opted_out() {
    let mut ing = ingress_tls();
    ing.metadata.annotations = Some(
        [("sozu.io/ssl-redirect".to_string(), "false".to_string())]
            .into_iter()
            .collect(),
    );
    let inputs = Inputs {
        ingresses: arcs(vec![ing]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(
        out.ir
            .frontends
            .iter()
            .all(|f| f.filters.redirect.is_none()),
        "opt-out disables the auto HTTP→HTTPS redirect"
    );
}

#[test]
fn http_only_ingress_is_not_redirected() {
    // No TLS on this Ingress -> nothing to redirect to, so it keeps serving HTTP.
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]), // reuse, but omit the secret below
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![]), // cert never loads -> host is not TLS-ready
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.frontends.len(), 1, "only the HTTP frontend");
    assert!(out.ir.frontends[0].filters.redirect.is_none());
}

/// A build with the given Secret material must report `InvalidCertificate`,
/// load no cert, and keep the host's frontend plain HTTP (no HTTPS, no
/// redirect) — the "TLS-ready only with a successfully loaded cert" rule.
/// Returns the problem's reason, for tests that pin the diagnosis.
fn assert_invalid_certificate(crt: &str, key: &str) -> String {
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", crt, key)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.certificates.len(), 0, "invalid cert must not load");
    assert_eq!(out.ir.frontends.len(), 1, "only the HTTP frontend remains");
    assert!(!out.ir.frontends[0].tls);
    assert!(
        out.ir.frontends[0].filters.redirect.is_none(),
        "no HTTPS to redirect to"
    );
    assert_eq!(out.results.len(), 1, "build still succeeds");
    match &out.results[0].problems[..] {
        [Problem::InvalidCertificate { secret, reason }] if secret == "app-tls" => reason.clone(),
        other => panic!("expected InvalidCertificate, got {other:?}"),
    }
}

#[test]
fn cert_with_valid_markers_but_garbage_base64_is_rejected() {
    // split_certificate_chain is purely textual: this passes the marker scan
    // but the body is not base64. It must be reported per-Secret, not crash
    // the translator's diff downstream.
    let garbage = "-----BEGIN CERTIFICATE-----\nnot!!base64@@data\n-----END CERTIFICATE-----\n";
    assert_invalid_certificate(garbage, KEY_A);
}

#[test]
fn cert_with_valid_base64_but_non_der_body_is_rejected() {
    // The body IS valid base64 (so a decode-and-hash check alone would pass)
    // but the decoded bytes are not DER. Sōzu parses the X509 when the
    // AddCertificate is applied and rejects it, aborting the whole apply
    // batch every cycle — so the builder must reject it up front.
    let garbage = "-----BEGIN CERTIFICATE-----\n\
                   bm90IGEgY2VydGlmaWNhdGUsIGp1c3QgYmFzZTY0IGdhcmJhZ2U=\n\
                   -----END CERTIFICATE-----\n";
    assert_invalid_certificate(garbage, KEY_A);
}

#[test]
fn chain_cert_with_non_der_body_is_rejected() {
    // A valid leaf followed by a valid-base64/non-DER intermediate: the chain
    // rides in the same AddCertificate, so it must be validated too.
    let garbage_chain = format!(
        "{CERT_A}-----BEGIN CERTIFICATE-----\n\
         bm90IGEgY2VydGlmaWNhdGUsIGp1c3QgYmFzZTY0IGdhcmJhZ2U=\n\
         -----END CERTIFICATE-----\n"
    );
    assert_invalid_certificate(&garbage_chain, KEY_A);
}

#[test]
fn cert_with_garbage_key_is_rejected() {
    // A parseable cert with a corrupt key would be rejected by Sōzu at
    // AddCertificate time, blocking every frontend add (certs tier first).
    let garbage = "-----BEGIN PRIVATE KEY-----\nnot!!base64@@data\n-----END PRIVATE KEY-----\n";
    assert_invalid_certificate(CERT_A, garbage);
}

#[test]
fn key_with_non_key_pem_label_is_rejected() {
    // A well-formed PEM block that is not a private key (here: a certificate)
    // is not plausible tls.key material.
    assert_invalid_certificate(CERT_A, CERT_A);
}

/// PKCS#8 framing around a body that is valid base64 but not a key.
const GARBAGE_BODY_KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
                                bm90IGEgcHJpdmF0ZSBrZXksIGp1c3QgYmFzZTY0IGdhcmJhZ2U=\n\
                                -----END PRIVATE KEY-----\n";

#[test]
fn key_with_valid_base64_but_non_key_body_is_rejected() {
    // Passes a PEM framing + label check, which is all that used to be done.
    // Measured live (2026-09-21): Sōzu rejects the AddCertificate with
    // "invalid private key: failed to parse private key as RSA, ECDSA, or
    // EdDSA", and since translation is all-or-nothing every reconcile of the
    // shared instance failed until the Secret was removed.
    let reason = assert_invalid_certificate(CERT_A, GARBAGE_BODY_KEY);
    assert!(
        reason.starts_with("invalid private key in tls.key: "),
        "{reason}"
    );
}

#[test]
fn key_belonging_to_another_certificate_is_rejected() {
    // Both halves are valid on their own. Sōzu loads the pair without checking
    // that they belong together, so this would program and then fail every
    // handshake for the host; the builder refuses it with a reason that names
    // the mismatch, in both algorithm directions.
    assert_eq!(
        assert_invalid_certificate(CERT_A, KEY_B),
        "tls.key does not match tls.crt"
    );
    assert_eq!(
        assert_invalid_certificate(CERT_B, KEY_A),
        "tls.key does not match tls.crt"
    );
}

#[test]
fn a_mismatched_key_is_caught_even_on_an_x509v1_leaf_webpki_would_reject() {
    // The pairing check must not depend on webpki: it rejects a v1 leaf
    // before looking at the key, and Sōzu (x509-parser) loads and serves such
    // a certificate, so a mismatch here has to be caught against the public
    // key Sōzu's own parser yields — not swallowed as "could not compare".
    assert_eq!(
        assert_invalid_certificate(CERT_V1, KEY_V1_OTHER),
        "tls.key does not match tls.crt"
    );
}

#[test]
fn a_matching_x509v1_pair_still_loads() {
    // ...and the same permissive parsing must accept a v1 certificate whose
    // key does match, exactly as Sōzu would.
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls()]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_V1, KEY_V1_MATCH)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(out.ir.certificates.len(), 1, "a valid v1 pair must load");
    assert!(out.results[0].problems.is_empty(), "{:?}", out.results[0]);
}

#[test]
fn a_compressed_ec_point_certificate_loads_as_it_does_in_sozu() {
    // The pairing check signs with the key and verifies against the leaf's
    // public key through ring, which only takes an uncompressed (`04`) EC
    // point. A leaf carrying a compressed point is valid, Sōzu loads it, and
    // TLS clients decompress it — so refusing it here would be stricter than
    // the data plane. It is admitted unpaired: the matching key loads, and so
    // does a foreign one, which is exactly what Sōzu does with that pair.
    for key in [KEY_A, KEY_B] {
        let inputs = Inputs {
            ingresses: arcs(vec![ingress_tls()]),
            services: arcs(vec![web_service()]),
            endpointslices: arcs(vec![web_slice()]),
            secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A_COMPRESSED, key)]),
            ..Default::default()
        };
        let out = build(&BuildConfig::default(), &inputs);
        assert_eq!(out.ir.certificates.len(), 1, "compressed point must load");
        assert!(out.results[0].problems.is_empty(), "{:?}", out.results[0]);
    }
}

#[test]
fn rsa_and_ecdsa_pairs_both_load() {
    // The loader must accept every key type Sōzu accepts: key_a is ECDSA
    // P-256, key_b is RSA-2048. A pairing check that only knew one of them
    // would refuse working material.
    for (crt, key) in [(CERT_A, KEY_A), (CERT_B, KEY_B)] {
        let inputs = Inputs {
            ingresses: arcs(vec![ingress_tls()]),
            services: arcs(vec![web_service()]),
            endpointslices: arcs(vec![web_slice()]),
            secrets: arcs(vec![tls_secret("demo", "app-tls", crt, key)]),
            ..Default::default()
        };
        let out = build(&BuildConfig::default(), &inputs);
        assert_eq!(out.ir.certificates.len(), 1);
        assert!(out.results[0].problems.is_empty(), "{:?}", out.results[0]);
    }
}

#[test]
fn a_bad_key_in_one_ingress_leaves_the_others_untouched() {
    // The point of catching it in the builder: the failure is scoped to the
    // owning object. The Ingress with the bad Secret keeps its HTTP frontend,
    // and an unrelated Ingress in the same build programs as if it were alone.
    let (svc, slice) = web_service_in("other");
    let inputs = Inputs {
        ingresses: arcs(vec![
            ingress_tls(),
            plain_ingress("other", "web", "other.example.com"),
        ]),
        services: arcs(vec![web_service(), svc]),
        endpointslices: arcs(vec![web_slice(), slice]),
        secrets: arcs(vec![tls_secret(
            "demo",
            "app-tls",
            CERT_A,
            GARBAGE_BODY_KEY,
        )]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(out.ir.certificates.is_empty());
    let hosts: Vec<(&str, bool)> = out
        .ir
        .frontends
        .iter()
        .map(|f| (f.hostname.as_str(), f.tls))
        .collect();
    assert_eq!(
        hosts,
        vec![("app.example.com", false), ("other.example.com", false)]
    );
    let other = out
        .results
        .iter()
        .find(|r| r.namespace == "other")
        .expect("other result");
    assert!(other.problems.is_empty(), "{other:?}");
    let demo = out
        .results
        .iter()
        .find(|r| r.namespace == "demo")
        .expect("demo result");
    assert!(
        matches!(&demo.problems[..], [Problem::InvalidCertificate { .. }]),
        "{demo:?}"
    );
}

#[test]
fn certs_sharing_a_secret_are_merged_with_unioned_names() {
    // One TLS Secret backing two hosts (here two TLS entries on one Ingress, the
    // same shape as an Ingress + a Gateway listener sharing a Secret) must yield
    // ONE certificate with both names — Sōzu keys a cert by (listener, fp), so a
    // second entry would make the translator ReplaceCertificate forever.
    let ing: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "tls": [
                { "hosts": ["b.example.com"], "secretName": "app-tls" },
                { "hosts": ["a.example.com"], "secretName": "app-tls" }
            ],
            "rules": [
                { "host": "a.example.com", "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } } ] } },
                { "host": "b.example.com", "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } } ] } }
            ]
        }
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ing]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(
        out.ir.certificates.len(),
        1,
        "one cert per (listener, fingerprint)"
    );
    assert_eq!(
        out.ir.certificates[0].names,
        vec!["a.example.com".to_string(), "b.example.com".to_string()],
        "names unioned and sorted"
    );
}

#[test]
fn same_der_cert_with_different_pem_wrapping_is_merged() {
    // Re-encode CERT_A's base64 body at a different line width: same DER (so
    // the same fingerprint — Sōzu's identity), byte-different PEM text. This
    // is the cert-manager vs hand-made Secret shape. The two entries must
    // merge into ONE certificate with the unioned names, or the translator
    // would churn ReplaceCertificate forever and one hostname would lose TLS.
    let body: String = CERT_A.lines().filter(|l| !l.starts_with("-----")).collect();
    let mut rewrapped = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in body.as_bytes().chunks(48) {
        rewrapped.push_str(std::str::from_utf8(chunk).expect("ascii base64"));
        rewrapped.push('\n');
    }
    rewrapped.push_str("-----END CERTIFICATE-----\n");
    assert_ne!(rewrapped, CERT_A, "the PEM texts must differ");

    let ingress = |name: &str, host: &str, secret: &str| -> Ingress {
        from_json(json!({
            "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
            "metadata": { "name": name, "namespace": "demo" },
            "spec": {
                "ingressClassName": "sozu",
                "tls": [{ "hosts": [host], "secretName": secret }],
                "rules": [{
                    "host": host,
                    "http": { "paths": [
                        { "path": "/", "pathType": "Prefix",
                          "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                    ]}
                }]
            }
        }))
    };
    let inputs = Inputs {
        ingresses: arcs(vec![
            ingress("a", "a.example.com", "tls-a"),
            ingress("b", "b.example.com", "tls-b"),
        ]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![
            tls_secret("demo", "tls-a", CERT_A, KEY_A),
            tls_secret("demo", "tls-b", &rewrapped, KEY_A),
        ]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(
        out.ir.certificates.len(),
        1,
        "one cert per (listener, fingerprint), regardless of PEM wrapping"
    );
    assert_eq!(
        out.ir.certificates[0].names,
        vec!["a.example.com".to_string(), "b.example.com".to_string()],
        "names unioned across both Secrets"
    );
}

/// Service `web` + ready EndpointSlice in an arbitrary namespace.
fn web_service_in(ns: &str) -> (Service, EndpointSlice) {
    let svc = from_json(json!({
        "apiVersion": "v1", "kind": "Service",
        "metadata": { "name": "web", "namespace": ns },
        "spec": { "ports": [{ "name": "http", "port": 80, "targetPort": 8080 }] }
    }));
    let slice = from_json(json!({
        "apiVersion": "discovery.k8s.io/v1", "kind": "EndpointSlice",
        "metadata": { "name": "web-abc", "namespace": ns,
            "labels": { "kubernetes.io/service-name": "web" } },
        "addressType": "IPv4",
        "ports": [{ "name": "http", "port": 8080 }],
        "endpoints": [{ "addresses": ["10.244.0.5"], "conditions": { "ready": true } }]
    }));
    (svc, slice)
}

/// Plain-HTTP Ingress `<ns>/<name>` routing `host` `/` to `<ns>/web:80`.
fn plain_ingress(ns: &str, name: &str, host: &str) -> Ingress {
    from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": name, "namespace": ns },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "host": host,
                "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }))
}

fn timed_ingress(ns: &str, name: &str, host: &str, created: &str) -> Ingress {
    from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": name, "namespace": ns, "creationTimestamp": created },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "host": host,
                "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }))
}

#[test]
fn the_older_ingress_wins_a_host_collision_regardless_of_cluster_id_order() {
    // The fix for cross-namespace host takeover: the winner is the OLDEST
    // claimant, not the smallest backend cluster id. Here the older Ingress is
    // in namespace "zzz" (cluster id "zzz.web.80"), which the previous
    // lexicographic policy would have made lose to the newer "aaa.web.80".
    let (svc_z, slice_z) = web_service_in("zzz");
    let (svc_a, slice_a) = web_service_in("aaa");
    let inputs = Inputs {
        ingresses: arcs(vec![
            timed_ingress("aaa", "web", "clash.example.com", "2024-01-01T00:00:00Z"),
            timed_ingress("zzz", "web", "clash.example.com", "2020-01-01T00:00:00Z"),
        ]),
        services: arcs(vec![svc_a, svc_z]),
        endpointslices: arcs(vec![slice_a, slice_z]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1, "one frontend per route key");
    assert_eq!(
        out.ir.frontends[0].cluster_id.as_deref(),
        Some("zzz.web.80"),
        "the older Ingress wins even though its cluster id sorts last"
    );
    let loser = out
        .results
        .iter()
        .find(|r| r.namespace == "aaa")
        .expect("aaa result");
    assert!(
        matches!(
            loser.problems.as_slice(),
            [Problem::RouteCollision { winner, .. }] if winner == "zzz.web.80"
        ),
        "the newer Ingress is told it lost to the older one: {loser:?}"
    );
    let winner = out
        .results
        .iter()
        .find(|r| r.namespace == "zzz")
        .expect("zzz result");
    assert!(winner.problems.is_empty(), "{winner:?}");
}

#[test]
fn cross_namespace_host_path_collision_is_reported_on_the_loser() {
    // Two Ingresses in different namespaces claim the same host+path with
    // different Services. Sōzu keys the route by host+path (not by cluster),
    // so only one can win — the winner must be the one the translator's dedup
    // already kept (smallest cluster id), and the loser must SEE the theft
    // instead of both owners reading accepted-with-no-problems.
    let (svc_a, slice_a) = web_service_in("aaa");
    let (svc_b, slice_b) = web_service_in("bbb");
    let inputs = Inputs {
        ingresses: arcs(vec![
            plain_ingress("bbb", "web", "clash.example.com"),
            plain_ingress("aaa", "web", "clash.example.com"),
        ]),
        services: arcs(vec![svc_a, svc_b]),
        endpointslices: arcs(vec![slice_a, slice_b]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1, "one frontend per route key");
    assert_eq!(
        out.ir.frontends[0].cluster_id.as_deref(),
        Some("aaa.web.80"),
        "with no timestamps, the tie-break is namespace/name: aaa before bbb"
    );
    let winner = out
        .results
        .iter()
        .find(|r| r.namespace == "aaa")
        .expect("aaa result");
    assert!(winner.problems.is_empty(), "{winner:?}");
    let loser = out
        .results
        .iter()
        .find(|r| r.namespace == "bbb")
        .expect("bbb result");
    assert_eq!(
        loser.problems,
        vec![Problem::RouteCollision {
            hostname: "clash.example.com".to_string(),
            path: "/".to_string(),
            winner: "aaa.web.80".to_string(),
            key_only: false,
        }]
    );
    assert_eq!(
        loser.problems[0].to_string(),
        "host+path clash.example.com/ is already served by aaa.web.80; this route was dropped"
    );
}

#[test]
fn the_documented_ingress_regex_spelling_is_admitted() {
    // An Ingress path must start with `/` (apiserver validation), so the
    // documented way to start-anchor an Ingress regex is a slash matched zero
    // times before the anchor. It has to compile on Sōzu's regex version, or
    // the documentation recommends a path the builder then refuses.
    let ingress: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "re", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "host": "re.example.com",
                "http": { "paths": [
                    { "path": "/{0}^/api(?:[/?](?-u:.*))?$", "pathType": "ImplementationSpecific",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);
    assert!(out.results[0].problems.is_empty(), "{:?}", out.results[0]);
    assert_eq!(
        out.ir.frontends[0].path,
        ir::PathMatch::Regex("/{0}^/api(?:[/?](?-u:.*))?$".into())
    );
}

#[test]
fn an_exact_path_collides_with_a_user_regex_spelling_the_same_rule() {
    // `Exact("/foo")` compiles to the same Sōzu rule as the user regex
    // `^/foo(?:\?(?-u:.*))?$`; Sōzu holds that route once. The collision must be
    // arbitrated and reported here, on the compiled rule, not silently
    // dropped by the translator's dedup. (The apiserver requires an Ingress
    // path to start with `/`, so in a cluster only an HTTPRoute
    // `RegularExpression` can spell this; the builder does not know that, and
    // the arbitration is the same for both kinds.)
    let (svc_a, slice_a) = web_service_in("aaa");
    let (svc_b, slice_b) = web_service_in("bbb");
    let exact: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "exact", "namespace": "aaa" },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "host": "clash.example.com",
                "http": { "paths": [
                    { "path": "/foo", "pathType": "Exact",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }));
    let regex: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "regex", "namespace": "bbb" },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "host": "clash.example.com",
                "http": { "paths": [
                    { "path": "^/foo(?:\\?(?-u:.*))?$", "pathType": "ImplementationSpecific",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![regex, exact]),
        services: arcs(vec![svc_a, svc_b]),
        endpointslices: arcs(vec![slice_a, slice_b]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(
        out.ir.frontends.len(),
        1,
        "one frontend per compiled route key"
    );
    let loser = out
        .results
        .iter()
        .find(|r| r.namespace == "bbb")
        .expect("bbb result");
    assert!(
        matches!(
            loser.problems.as_slice(),
            [Problem::RouteCollision { hostname, winner, .. }]
                if hostname == "clash.example.com" && winner == "aaa.web.80"
        ),
        "{loser:?}"
    );
}

#[test]
fn identical_duplicate_routes_are_benign_and_unreported() {
    // Two Ingresses claiming the same host+path with the SAME Service and the
    // same filters are a harmless overlap: one frontend, zero problems.
    let inputs = Inputs {
        ingresses: arcs(vec![
            plain_ingress("demo", "ing-a", "app.example.com"),
            plain_ingress("demo", "ing-b", "app.example.com"),
        ]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 1, "deduped to one frontend");
    for r in &out.results {
        assert!(r.problems.is_empty(), "{r:?}");
    }
}

#[test]
fn referenced_services_cover_resolved_and_unresolved_backends() {
    // The EndpointSlice ping filter feeds on this set: a Service must be in it
    // whether it resolved or not — a slice appearing later for a still-broken
    // backend (ServiceNotFound, NoReadyEndpoints) has to wake the loop, or a
    // deploy that fixes the backend would never be routed to.
    let broken: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "ghost", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "host": "ghost.example.com",
                "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "ghost", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ingress_tls(), broken]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    let referenced: Vec<&str> = out.referenced_services.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        referenced,
        vec!["demo/ghost", "demo/web"],
        "resolved and unresolved backends both count"
    );
}

#[test]
fn default_backend_only_ingress_reports_unsupported() {
    // spec.defaultBackend has no verified Sōzu mapping: an Ingress made of
    // only a defaultBackend builds to nothing, so the owner must see WHY
    // instead of accepted-with-no-problems while requests 404.
    let ing: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "defaultBackend": { "service": { "name": "web", "port": { "number": 80 } } }
        }
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ing]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert!(out.ir.frontends.is_empty(), "defaultBackend is not routed");
    assert_eq!(
        out.results[0].problems,
        vec![Problem::DefaultBackendUnsupported]
    );
}

#[test]
fn default_backend_next_to_rules_still_builds_the_rules() {
    let mut ing = ingress_tls();
    ing.spec.as_mut().expect("spec").default_backend = Some(from_json(json!({
        "service": { "name": "web", "port": { "number": 80 } }
    })));
    let inputs = Inputs {
        ingresses: arcs(vec![ing]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        secrets: arcs(vec![tls_secret("demo", "app-tls", CERT_A, KEY_A)]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(out.ir.frontends.len(), 2, "the rules translate as usual");
    assert!(out.results[0]
        .problems
        .contains(&Problem::DefaultBackendUnsupported));
}

#[test]
fn ingress_hostless_rule_maps_to_catch_all() {
    // An Ingress rule with no `host` is a catch-all: emit one plain-HTTP `*`
    // frontend (Sōzu DomainRule::Any), no HTTPS frontend, no cert, no problem.
    let ing: Ingress = from_json(json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "Ingress",
        "metadata": { "name": "web", "namespace": "demo" },
        "spec": {
            "ingressClassName": "sozu",
            "rules": [{
                "http": { "paths": [
                    { "path": "/", "pathType": "Prefix",
                      "backend": { "service": { "name": "web", "port": { "number": 80 } } } }
                ]}
            }]
        }
    }));
    let inputs = Inputs {
        ingresses: arcs(vec![ing]),
        services: arcs(vec![web_service()]),
        endpointslices: arcs(vec![web_slice()]),
        ..Default::default()
    };
    let out = build(&BuildConfig::default(), &inputs);

    assert_eq!(
        out.ir.frontends.len(),
        1,
        "only the HTTP catch-all frontend"
    );
    assert_eq!(out.ir.frontends[0].hostname, "*");
    assert!(!out.ir.frontends[0].tls);
    assert_eq!(out.ir.certificates.len(), 0);
    assert!(out.results[0].problems.is_empty(), "{:?}", out.results[0]);
}

#[test]
fn problem_display_carries_the_detail_and_reason_stays_machine_readable() {
    let p = Problem::ServicePortNotFound {
        service: "demo/web".into(),
        port: "http".into(),
    };
    assert!(p.to_string().contains("demo/web") && p.to_string().contains("http"));
    assert_eq!(p.reason(), "ServicePortNotFound");

    let p = Problem::PortNotExposed {
        listener: "https".into(),
        declared: 8443,
        protocol: "HTTPS".into(),
        exposed: vec![443],
    };
    // The declared port and the menu both appear: knowing 8443 is wrong is
    // useless without knowing 443 was available.
    assert!(p.to_string().contains("8443") && p.to_string().contains("443"));
    assert_eq!(
        p.listener(),
        Some("https"),
        "listener-scoped variants say so"
    );
    assert_eq!(Problem::TimeoutsUnsupported.listener(), None);

    let p = Problem::InvalidPathRegex {
        path: "/foo([".into(),
        reason: "unclosed character class".into(),
    };
    assert!(p.to_string().contains("/foo([") && p.to_string().contains("unclosed"));
    assert_eq!(p.reason(), "InvalidPathRegex");
}

#[test]
fn colliding_binds_are_detected_on_the_bind_not_the_advertised_key() {
    // The trap the table exists to catch. These two entries collide on nothing
    // a user reads — different name, different advertised port, different
    // protocol — and still resolve to one socket. Sōzu would reject the second
    // listener add, and since the translation is all-or-nothing the whole
    // reconcile fails with it.
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
                name: "postgres".into(),
                port: 5432,
                bind: 8080,
                protocol: ExposedProtocol::Tcp,
                transport: None,
                owner: None,
            },
        ],
        ..Default::default()
    };

    let clashes = cfg.colliding_binds();
    assert_eq!(clashes.len(), 1, "one pair collides: {clashes:?}");
    assert_eq!(clashes[0].0.name, "http");
    assert_eq!(clashes[0].1.name, "postgres");

    // The advertised keys really are distinct, which is the whole point.
    assert_ne!(
        (clashes[0].0.protocol, clashes[0].0.port),
        (clashes[0].1.protocol, clashes[0].1.port)
    );
}

#[test]
fn the_chart_default_shape_is_collision_free() {
    let cfg = BuildConfig {
        exposure: vec![
            ExposedPort {
                name: "http".into(),
                port: 80,
                bind: 8080,
                protocol: ExposedProtocol::Http,
                transport: Some("TCP".into()),
                owner: None,
            },
            ExposedPort {
                name: "https".into(),
                port: 443,
                bind: 8443,
                protocol: ExposedProtocol::Https,
                transport: Some("TCP".into()),
                owner: None,
            },
        ],
        ..Default::default()
    };
    assert!(cfg.colliding_binds().is_empty());
    assert_eq!(cfg.advertised_for(ExposedProtocol::Http), Some(80));
    assert_eq!(
        cfg.bind_for(ExposedProtocol::Https).map(|a| a.port()),
        Some(8443)
    );
    // A layer-4 route may not land on a bind an exposed entry already holds.
    assert_eq!(cfg.reserved_binds(), [8080, 8443].into_iter().collect());
}

#[test]
fn an_l4_port_names_its_owner_and_http_stays_shared() {
    // HTTP and HTTPS multiplex on hostname, so any number of Gateways share
    // them. A layer-4 port carries exactly one route and has no hostname to
    // arbitrate with, so it names the namespace allowed to claim it.
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
                name: "postgres".into(),
                port: 5432,
                bind: 5432,
                protocol: ExposedProtocol::Tcp,
                transport: None,
                owner: Some("data".into()),
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        cfg.exposed(ExposedProtocol::Http, 80)
            .and_then(|e| e.owner.as_deref()),
        None
    );
    assert_eq!(
        cfg.exposed(ExposedProtocol::Tcp, 5432)
            .and_then(|e| e.owner.as_deref()),
        Some("data")
    );
    // A port nobody exposes is simply not on the menu.
    assert!(cfg.exposed(ExposedProtocol::Tcp, 9999).is_none());
    assert_eq!(cfg.advertised_ports(ExposedProtocol::Tcp), vec![5432]);
}
