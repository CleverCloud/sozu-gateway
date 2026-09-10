//! Shared TLS certificate coverage through the compiler and command diff.

use std::sync::Arc;

use k8s_openapi::ByteString;
use serde_json::json;
use sozu_command_lib::proto::command::request::RequestType;
use sozu_gw_builder::{build, BuildConfig, ExposedPort, ExposedProtocol, Inputs};
use sozu_gw_ir as ir;
use sozu_gw_translator::reconcile;

// These certificates use the existing key_a.pem test key. The Gateway fixtures
// have the conformance suite's SANs (*, *.org, *.wildcard.org), with a different
// CN to catch accidental CN expansion when DNS SANs are present. The rotated
// certificate has a different serial but the same names.
const CERT: &str = include_str!("fixtures/cert_gateway.pem");
const ROTATED_CERT: &str = include_str!("fixtures/cert_gateway_rotated.pem");
const CN_CERT: &str = include_str!("fixtures/cert_cn_only.pem");
const KEY: &str = include_str!("fixtures/key_a.pem");
const NAMESPACE: &str = "gateway-conformance-infra";
const GATEWAY: &str = "same-namespace-with-https-listener";
const SECRET: &str = "tls-validity-checks-certificate";

fn object<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).expect("valid fixture")
}

fn listener(name: &str, hostname: Option<&str>, port: u16) -> serde_json::Value {
    json!({
        "name": name, "hostname": hostname, "port": port, "protocol": "HTTPS",
        "allowedRoutes": { "namespaces": { "from": "Same" } },
        "tls": { "certificateRefs": [{ "name": SECRET }] }
    })
}

fn inputs() -> Inputs {
    // Gateway API v1.6.2 HTTPRouteHTTPSListener: all four base listeners share
    // one certificate. One route names example.org; the other inherits the
    // second listener's hostname. unknown-example.org must complete TLS but
    // has no frontend, so the data plane can return HTTP 404.
    let listeners = vec![
        listener("https", None, 443),
        listener("https-with-hostname", Some("second-example.org"), 443),
        listener("https-with-wildcard-hostname", Some("*.wildcard.org"), 443),
        listener(
            "https-with-hostname-matching-wildcard",
            Some("fourth-example.wildcard.org"),
            443,
        ),
    ];
    let mut inputs = Inputs {
        gateway_classes: vec![Arc::new(object(json!({
            "metadata": { "name": "sozu" },
            "spec": { "controllerName": "sozu.io/gateway-controller" }
        })))],
        gateways: vec![Arc::new(object(json!({
            "metadata": { "name": GATEWAY, "namespace": NAMESPACE },
            "spec": { "gatewayClassName": "sozu", "listeners": listeners }
        })))],
        http_routes: vec![
            Arc::new(object(json!({
                "metadata": { "name": "httproute-https-test", "namespace": NAMESPACE },
                "spec": {
                    "parentRefs": [{ "name": GATEWAY }], "hostnames": ["example.org"],
                    "rules": [{ "backendRefs": [{ "name": "infra-backend-v1", "port": 8080 }] }]
                }
            }))),
            Arc::new(object(json!({
                "metadata": { "name": "httproute-https-test-no-hostname", "namespace": NAMESPACE },
                "spec": {
                    "parentRefs": [{ "name": GATEWAY, "sectionName": "https-with-hostname" }],
                    "rules": [{ "backendRefs": [{ "name": "infra-backend-v2", "port": 8080 }] }]
                }
            }))),
        ],
        secrets: vec![Arc::new(object(json!({
            "metadata": { "name": SECRET, "namespace": NAMESPACE },
            "type": "kubernetes.io/tls",
            "data": {
                "tls.crt": ByteString(CERT.as_bytes().to_vec()),
                "tls.key": ByteString(KEY.as_bytes().to_vec())
            }
        })))],
        ..Default::default()
    };
    for (name, address) in [
        ("infra-backend-v1", "10.0.0.1"),
        ("infra-backend-v2", "10.0.0.2"),
    ] {
        inputs.services.push(Arc::new(object(json!({
            "metadata": { "name": name, "namespace": NAMESPACE },
            "spec": { "ports": [{ "name": "http", "port": 8080 }] }
        }))));
        inputs.endpointslices.push(Arc::new(object(json!({
            "metadata": { "name": name, "namespace": NAMESPACE,
                "labels": { "kubernetes.io/service-name": name } },
            "addressType": "IPv4", "ports": [{ "name": "http", "port": 8080 }],
            "endpoints": [{ "addresses": [address], "conditions": { "ready": true } }]
        }))));
    }
    inputs
}

fn expected_names() -> Vec<String> {
    [
        "*",
        "*.org",
        "*.wildcard.org",
        "fourth-example.wildcard.org",
        "second-example.org",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn replace_secret_cert(inputs: &mut Inputs, pem: &str) {
    Arc::make_mut(&mut inputs.secrets[0])
        .data
        .as_mut()
        .unwrap()
        .insert("tls.crt".into(), ByteString(pem.as_bytes().to_vec()));
}

#[test]
fn https_conformance_listeners_preserve_inferred_and_explicit_names() {
    let out = build(&BuildConfig::default(), &inputs());
    assert!(out.gateways[0].programmed);
    assert!(out.routes.iter().all(|route| route
        .parents
        .iter()
        .all(|parent| parent.accepted && parent.resolved_refs)));
    assert_eq!(out.ir.certificates.len(), 1);
    assert_eq!(out.ir.certificates[0].names, expected_names());
    assert_eq!(out.ir.frontends.len(), 2);
    for (hostname, backend) in [
        ("example.org", "infra-backend-v1"),
        ("second-example.org", "infra-backend-v2"),
    ] {
        let frontend = out
            .ir
            .frontends
            .iter()
            .find(|f| f.hostname == hostname)
            .unwrap();
        assert!(frontend.tls);
        assert_eq!(
            frontend.cluster_id,
            Some(format!("{NAMESPACE}.{backend}.8080"))
        );
    }
    assert!(out
        .ir
        .frontends
        .iter()
        .all(|f| f.hostname != "unknown-example.org"));

    let requests = reconcile(&ir::Ir::default(), &out.ir).unwrap();
    let certs: Vec<_> = requests
        .iter()
        .filter_map(|r| match &r.request_type {
            Some(RequestType::AddCertificate(c)) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(certs.len(), 1);
    assert_eq!(
        certs[0].certificate.get_overriding_names().unwrap(),
        expected_names()
    );
    assert!(reconcile(&out.ir, &out.ir).unwrap().is_empty());
}

#[test]
fn removing_the_hostname_less_listener_restores_explicit_name_restrictions() {
    let mut inputs = inputs();
    let before = build(&BuildConfig::default(), &inputs).ir;
    Arc::make_mut(&mut inputs.gateways[0])
        .spec
        .listeners
        .remove(0);
    let after = build(&BuildConfig::default(), &inputs).ir;
    assert_eq!(
        after.certificates[0].names,
        vec![
            "*.wildcard.org",
            "fourth-example.wildcard.org",
            "second-example.org"
        ]
    );
    let requests = reconcile(&before, &after).unwrap();
    let certificate_requests: Vec<_> = requests
        .iter()
        .filter_map(|r| match &r.request_type {
            Some(
                request @ (RequestType::RemoveCertificate(_)
                | RequestType::AddCertificate(_)
                | RequestType::ReplaceCertificate(_)),
            ) => Some(request),
            _ => None,
        })
        .collect();
    let [RequestType::RemoveCertificate(remove), RequestType::AddCertificate(add)] =
        certificate_requests.as_slice()
    else {
        panic!("restricting SNI names requires removal before re-adding the certificate");
    };
    assert_eq!(remove.address, add.address);
    assert_eq!(add.certificate.names, after.certificates[0].names);
}

#[test]
fn shared_cn_only_certificate_keeps_the_cn_fallback() {
    let mut inputs = inputs();
    replace_secret_cert(&mut inputs, CN_CERT);
    Arc::make_mut(&mut inputs.gateways[0])
        .spec
        .listeners
        .truncate(2);
    let out = build(&BuildConfig::default(), &inputs);
    assert_eq!(
        out.ir.certificates[0].names,
        vec!["cn-only.example.org", "second-example.org"]
    );
}

#[test]
fn inferred_names_are_scoped_to_their_listener_bind() {
    let mut inputs = inputs();
    let listeners = &mut Arc::make_mut(&mut inputs.gateways[0]).spec.listeners;
    listeners.truncate(2);
    listeners[1].port = 9443;
    let mut cfg = BuildConfig::default();
    cfg.exposure.push(ExposedPort {
        name: "https-alt".into(),
        port: 9443,
        bind: 9444,
        protocol: ExposedProtocol::Https,
        transport: Some("TCP".into()),
        owner: None,
    });
    let out = build(&cfg, &inputs);
    assert_eq!(out.ir.certificates.len(), 2);
    for cert in &out.ir.certificates {
        match cert.listener.port() {
            443 => assert_eq!(cert.names, vec!["*", "*.org", "*.wildcard.org"]),
            9444 => assert_eq!(cert.names, vec!["second-example.org"]),
            port => panic!("unexpected certificate bind {port}"),
        }
    }
}

#[test]
fn equivalent_der_and_listener_order_keep_the_same_name_coverage() {
    let mut inputs = inputs();
    let before = build(&BuildConfig::default(), &inputs).ir;
    let body: String = CERT
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let wrapped = body
        .as_bytes()
        .chunks(48)
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let pem = format!("-----BEGIN CERTIFICATE-----\n{wrapped}\n-----END CERTIFICATE-----\n");
    assert_ne!(pem, CERT);
    let mut alternate = (*inputs.secrets[0]).clone();
    alternate.metadata.name = Some("rewrapped".into());
    alternate
        .data
        .as_mut()
        .unwrap()
        .insert("tls.crt".into(), ByteString(pem.into_bytes()));
    inputs.secrets.push(Arc::new(alternate));
    let listeners = &mut Arc::make_mut(&mut inputs.gateways[0]).spec.listeners;
    listeners[1]
        .tls
        .as_mut()
        .unwrap()
        .certificate_refs
        .as_mut()
        .unwrap()[0]
        .name = "rewrapped".into();
    listeners.reverse();
    let after = build(&BuildConfig::default(), &inputs).ir;
    assert_eq!(after.certificates.len(), 1);
    assert_eq!(after.certificates[0].names, expected_names());
    assert!(reconcile(&before, &after).unwrap().is_empty());
}

#[test]
fn shared_certificate_rotation_keeps_names_in_a_single_replace() {
    let mut inputs = inputs();
    let before = build(&BuildConfig::default(), &inputs).ir;
    replace_secret_cert(&mut inputs, ROTATED_CERT);
    let after = build(&BuildConfig::default(), &inputs).ir;
    let requests = reconcile(&before, &after).unwrap();
    assert_eq!(requests.len(), 1);
    let Some(RequestType::ReplaceCertificate(replacement)) = &requests[0].request_type else {
        panic!("certificate rotation must be one replacement");
    };
    assert_eq!(replacement.new_certificate.names, expected_names());
    assert_ne!(
        before.certificates[0].certificate,
        after.certificates[0].certificate
    );
    assert!(reconcile(&after, &after).unwrap().is_empty());
}

#[test]
fn an_existing_shadow_is_repaired_without_a_schema_change() {
    let desired = build(&BuildConfig::default(), &inputs()).ir;
    // The previous compiler lost the inferred names when it merged the four
    // listeners. Its persisted bare Ir must still load and request a reload
    // of that same fingerprint. Remove then Add updates worker SNI names;
    // Replace with the same fingerprint would be a worker no-op.
    let mut old_shadow = serde_json::to_value(&desired).unwrap();
    old_shadow["certificates"][0]["names"] = json!([
        "*.wildcard.org",
        "fourth-example.wildcard.org",
        "second-example.org"
    ]);
    let previous: ir::Ir = serde_json::from_value(old_shadow).unwrap();
    let requests = reconcile(&previous, &desired).unwrap();
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        requests[0].request_type,
        Some(RequestType::RemoveCertificate(_))
    ));
    let Some(RequestType::AddCertificate(add)) = &requests[1].request_type else {
        panic!("the old shadow must converge through a certificate reload");
    };
    assert_eq!(add.certificate.names, expected_names());
    assert!(reconcile(&desired, &desired).unwrap().is_empty());
}
