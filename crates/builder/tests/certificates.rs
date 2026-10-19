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
const INVALID_SAN_CERT: &str = include_str!("fixtures/cert_invalid_san.pem");
const NO_NAMES_CERT: &str = include_str!("fixtures/cert_no_names.pem");
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
fn removing_the_hostname_less_listener_restricts_the_desired_names() {
    let mut inputs = inputs();
    let before = build(&BuildConfig::default(), &inputs).ir;
    assert_eq!(before.certificates[0].names, expected_names());
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
    // This verifies the compiler's desired state. Applying new names to an
    // already loaded fingerprint still requires the Sōzu change tracked in #81.
    assert_eq!(
        after.certificates[0].certificate,
        before.certificates[0].certificate
    );
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

// A shared certificate may have usable explicit overrides even when its SANs
// cannot enter Sōzu's trie. Adding an invalid inference must leave that existing
// certificate installed, regardless of listener order.
fn assert_rejected_inference_preserves_named_listener(pem: &str, reason: &str) {
    for inferred_first in [false, true] {
        let mut inputs = inputs();
        replace_secret_cert(&mut inputs, pem);
        let listeners = &mut Arc::make_mut(&mut inputs.gateways[0]).spec.listeners;
        let inferred = listeners.remove(0);
        listeners.truncate(1);
        let before = build(&BuildConfig::default(), &inputs);
        assert!(before.gateways[0].listeners[0].programmed);
        assert!(before.gateways[0].problems.is_empty());
        assert_eq!(before.ir.certificates.len(), 1);
        assert_eq!(before.ir.certificates[0].names, vec!["second-example.org"]);
        assert_eq!(before.ir.frontends.len(), 1);

        let listeners = &mut Arc::make_mut(&mut inputs.gateways[0]).spec.listeners;
        listeners.insert(if inferred_first { 0 } else { 1 }, inferred);
        let after = build(&BuildConfig::default(), &inputs);
        let inferred_status = after.gateways[0]
            .listeners
            .iter()
            .find(|listener| listener.name == "https")
            .unwrap();
        assert!(!inferred_status.resolved_refs);
        assert_eq!(
            inferred_status.resolved_refs_reason,
            "InvalidCertificateRef"
        );
        assert!(!inferred_status.programmed);
        let named_status = after.gateways[0]
            .listeners
            .iter()
            .find(|listener| listener.name == "https-with-hostname")
            .unwrap();
        assert!(named_status.resolved_refs && named_status.programmed);
        assert!(after.gateways[0].problems.iter().any(|problem| matches!(
            problem,
            sozu_gw_builder::Problem::InvalidCertificate { reason: detail, .. }
                if detail.contains(reason)
        )));
        assert_eq!(after.ir.certificates, before.ir.certificates);
        assert_eq!(after.ir.frontends, before.ir.frontends);
        assert!(
            reconcile(&before.ir, &after.ir)
                .unwrap()
                .iter()
                .all(|request| !matches!(
                    request.request_type,
                    Some(
                        RequestType::RemoveCertificate(_)
                            | RequestType::AddCertificate(_)
                            | RequestType::ReplaceCertificate(_)
                    )
                )),
            "a rejected listener must not remove or reload the working certificate"
        );
    }
}

#[test]
fn invalid_inferred_san_preserves_the_named_listeners_certificate() {
    // The valid SAN matches the named listener; the extra malformed SAN must
    // not enter the merged names when another listener requests inference.
    assert_rejected_inference_preserves_named_listener(
        INVALID_SAN_CERT,
        "unsupported inferred SNI",
    );
}

#[test]
fn missing_inferred_names_fail_only_the_hostname_less_listener() {
    assert_rejected_inference_preserves_named_listener(NO_NAMES_CERT, "no DNS SAN or common name");
}

#[test]
fn a_hostname_less_listener_needs_at_least_one_inferred_name() {
    let mut inputs = inputs();
    replace_secret_cert(&mut inputs, NO_NAMES_CERT);
    Arc::make_mut(&mut inputs.gateways[0])
        .spec
        .listeners
        .truncate(1);
    let out = build(&BuildConfig::default(), &inputs);
    let listener = &out.gateways[0].listeners[0];
    assert!(!listener.resolved_refs);
    assert_eq!(listener.resolved_refs_reason, "InvalidCertificateRef");
    assert!(!listener.programmed);
    assert!(out.ir.certificates.is_empty());
    assert!(out.ir.frontends.is_empty());
}

/// One SNI name per listener, held by its oldest claimant. Sōzu serves a name
/// from whichever of its certificates expires last, so these pin who the
/// builder lets keep a contested name, whatever the certificates' expiry.
mod sni_name_ownership {
    use super::*;
    use k8s_openapi::api::core::v1::Secret;
    use k8s_openapi::api::networking::v1::Ingress;
    use sozu_gw_builder::{BuildOutput, ClaimedName, Problem};
    use sozu_gw_gateway_api::Gateway;

    /// `app.example.com`, ECDSA, expires 2036-06-19.
    const EARLY_CERT: &str = include_str!("fixtures/cert_a.pem");
    const EARLY_KEY: &str = include_str!("fixtures/key_a.pem");
    /// `other.example.com`, RSA, expires 2036-09-18: the one Sōzu would pick.
    const LATE_CERT: &str = include_str!("fixtures/cert_b.pem");
    const LATE_KEY: &str = include_str!("fixtures/key_b.pem");
    const OLD: &str = "2020-01-01T00:00:00Z";
    const NEW: &str = "2024-01-01T00:00:00Z";

    fn secret(ns: &str, name: &str, crt: &str, key: &str) -> Arc<Secret> {
        Arc::new(object(json!({
            "metadata": { "name": name, "namespace": ns },
            "type": "kubernetes.io/tls",
            "data": {
                "tls.crt": ByteString(crt.as_bytes().to_vec()),
                "tls.key": ByteString(key.as_bytes().to_vec())
            }
        })))
    }

    /// An Ingress with TLS entries only, `(hosts, secret)`: no rule is needed
    /// to load a certificate, so none is needed to claim a name.
    fn tls_ingress(
        ns: &str,
        name: &str,
        created: &str,
        entries: &[(&[&str], &str)],
    ) -> Arc<Ingress> {
        let tls: Vec<_> = entries
            .iter()
            .map(|(hosts, secret)| json!({ "hosts": hosts, "secretName": secret }))
            .collect();
        Arc::new(object(json!({
            "metadata": { "name": name, "namespace": ns, "creationTimestamp": created },
            "spec": { "ingressClassName": "sozu", "tls": tls }
        })))
    }

    /// A Gateway with one HTTPS listener `https` on 443 and no hostname, so
    /// its names are inferred from the certificate.
    fn hostless_gateway(ns: &str, name: &str, created: &str, secret: &str) -> Arc<Gateway> {
        Arc::new(object(json!({
            "metadata": { "name": name, "namespace": ns, "creationTimestamp": created },
            "spec": { "gatewayClassName": "sozu", "listeners": [{
                "name": "https", "port": 443, "protocol": "HTTPS",
                "tls": { "certificateRefs": [{ "name": secret }] }
            }] }
        })))
    }

    /// The names a certificate is loaded with, found by its leaf PEM (the
    /// chain split keeps the block, not the file's trailing newline).
    fn names_of<'a>(out: &'a BuildOutput, pem: &str) -> Option<&'a [String]> {
        out.ir
            .certificates
            .iter()
            .find(|c| c.certificate.trim() == pem.trim())
            .map(|c| c.names.as_slice())
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn ingress_problems<'a>(out: &'a BuildOutput, ns: &str) -> &'a [Problem] {
        &out.results
            .iter()
            .find(|r| r.namespace == ns)
            .expect("an Ingress result")
            .problems
    }

    /// One name lost on an Ingress, to `owner`.
    fn claimed(name: &str, secret: &str, owner_kind: &'static str, owner: &str) -> Problem {
        Problem::CertificateNameClaimed {
            secret: secret.into(),
            listener: None,
            names: vec![ClaimedName {
                name: name.into(),
                owner_kind,
                owner: owner.into(),
            }],
        }
    }

    /// A victim and an attacker in their own namespaces claim one host with
    /// different certificates; the attacker also names a host of its own, so
    /// its certificate stays loaded.
    fn contested(victim_created: &str, attacker_created: &str) -> Inputs {
        Inputs {
            ingresses: vec![
                tls_ingress(
                    "victim",
                    "web",
                    victim_created,
                    &[(&["api.example.com"], "victim-tls")],
                ),
                tls_ingress(
                    "attacker",
                    "web",
                    attacker_created,
                    &[(&["api.example.com", "attacker.example.com"], "attacker-tls")],
                ),
            ],
            secrets: vec![
                secret("victim", "victim-tls", EARLY_CERT, EARLY_KEY),
                secret("attacker", "attacker-tls", LATE_CERT, LATE_KEY),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn the_oldest_claimant_keeps_a_contested_name_whatever_the_expiry() {
        // The victim's certificate expires first, so Sōzu alone would serve
        // the attacker's for api.example.com.
        let out = build(&BuildConfig::default(), &contested(OLD, NEW));
        assert_eq!(
            names_of(&out, EARLY_CERT),
            Some(&names(&["api.example.com"])[..])
        );
        assert_eq!(
            names_of(&out, LATE_CERT),
            Some(&names(&["attacker.example.com"])[..])
        );
        assert_eq!(
            ingress_problems(&out, "attacker"),
            [claimed(
                "api.example.com",
                "attacker-tls",
                "Ingress",
                "victim/web"
            )]
        );
        assert!(ingress_problems(&out, "victim").is_empty());

        // Age decides, not expiry: the other way round, the later-expiring
        // certificate keeps the name and the earlier one is not loaded.
        let out = build(&BuildConfig::default(), &contested(NEW, OLD));
        assert_eq!(names_of(&out, EARLY_CERT), None);
        assert_eq!(
            names_of(&out, LATE_CERT),
            Some(&names(&["api.example.com", "attacker.example.com"])[..])
        );
        assert_eq!(
            ingress_problems(&out, "victim"),
            [claimed(
                "api.example.com",
                "victim-tls",
                "Ingress",
                "attacker/web"
            )]
        );
        assert!(ingress_problems(&out, "attacker").is_empty());
    }

    #[test]
    fn one_report_per_certificate_however_many_names_it_loses() {
        // The attacker's Secret backs two TLS entries; a third entry uses
        // another Secret. Every name the victim holds is lost: one report per
        // certificate the attacker asked for, never one per name.
        let mut inputs = contested(OLD, NEW);
        inputs.ingresses[0] = tls_ingress(
            "victim",
            "web",
            OLD,
            &[(
                &[
                    "a.example.com",
                    "b.example.com",
                    "c.example.com",
                    "d.example.com",
                    "e.example.com",
                ],
                "victim-tls",
            )],
        );
        inputs.ingresses[1] = tls_ingress(
            "attacker",
            "web",
            NEW,
            &[
                (
                    &["d.example.com", "c.example.com", "b.example.com"],
                    "attacker-tls",
                ),
                (&["a.example.com", "b.example.com"], "attacker-tls"),
                (&["e.example.com"], "other-tls"),
            ],
        );
        inputs
            .secrets
            .push(secret("attacker", "other-tls", CERT, KEY));
        let out = build(&BuildConfig::default(), &inputs);
        assert_eq!(out.ir.certificates.len(), 1, "{:?}", out.ir.certificates);
        let lost = |names: &[&str]| {
            names
                .iter()
                .map(|name| ClaimedName {
                    name: name.to_string(),
                    owner_kind: "Ingress",
                    owner: "victim/web".into(),
                })
                .collect::<Vec<_>>()
        };
        let problems = ingress_problems(&out, "attacker");
        assert_eq!(
            problems,
            [
                Problem::CertificateNameClaimed {
                    secret: "attacker-tls".into(),
                    listener: None,
                    names: lost(&[
                        "a.example.com",
                        "b.example.com",
                        "c.example.com",
                        "d.example.com"
                    ]),
                },
                claimed("e.example.com", "other-tls", "Ingress", "victim/web"),
            ]
        );
        // The message is an Event note: it names a few, counts the rest.
        assert_eq!(
            problems[0].to_string(),
            "4 SNI names are claimed at this listener by objects created earlier (or first by \
             namespace/name); the certificate from Secret \"attacker-tls\" does not serve them: \
             \"a.example.com\" (Ingress victim/web), \"b.example.com\" (Ingress victim/web), \
             \"c.example.com\" (Ingress victim/web) and 1 more"
        );
    }

    /// A tenant lists as many hosts as its objects hold, so arbitration and
    /// its reports must stay linear in them: this runs inline in the global
    /// reconcile, ahead of every other tenant's endpoint changes.
    #[test]
    fn a_hundred_thousand_lost_names_are_settled_quickly_and_reported_once_per_object() {
        const HOSTS: usize = 50_000;
        let hosts: Vec<String> = (0..HOSTS).map(|i| format!("h{i}.example.com")).collect();
        let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
        let mut inputs = Inputs {
            ingresses: vec![tls_ingress("victim", "web", OLD, &[(&hosts, "tls")])],
            secrets: vec![secret("victim", "tls", EARLY_CERT, EARLY_KEY)],
            ..Default::default()
        };
        for ns in ["one", "two"] {
            inputs
                .ingresses
                .push(tls_ingress(ns, "web", NEW, &[(&hosts, "tls")]));
            inputs.secrets.push(secret(ns, "tls", LATE_CERT, LATE_KEY));
        }

        let started = std::time::Instant::now();
        let out = build(&BuildConfig::default(), &inputs);
        let elapsed = started.elapsed();
        // 0.6 s in a debug build. A scan of the lost names per lost name, as
        // a first version did, took 31 s on the same input.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "100,000 lost names took {elapsed:?}"
        );

        assert_eq!(out.ir.certificates.len(), 1);
        assert_eq!(names_of(&out, EARLY_CERT).map(<[String]>::len), Some(HOSTS));
        for ns in ["one", "two"] {
            let problems = ingress_problems(&out, ns);
            assert_eq!(problems.len(), 1, "one report, not one per name");
            let Problem::CertificateNameClaimed { names, .. } = &problems[0] else {
                panic!("{problems:?}");
            };
            assert_eq!(names.len(), HOSTS);
            assert!(
                problems[0].to_string().len() < 1024,
                "an Event note is 1 kB at most"
            );
        }
    }

    #[test]
    fn a_loser_left_with_no_name_is_not_loaded() {
        let mut inputs = contested(OLD, NEW);
        inputs.ingresses[1] = tls_ingress(
            "attacker",
            "web",
            NEW,
            &[(&["api.example.com"], "attacker-tls")],
        );
        let out = build(&BuildConfig::default(), &inputs);
        // Never `names: []`, which Sōzu reads as "infer them from the SANs".
        assert_eq!(out.ir.certificates.len(), 1, "{:?}", out.ir.certificates);
        assert_eq!(
            names_of(&out, EARLY_CERT),
            Some(&names(&["api.example.com"])[..])
        );
        assert_eq!(
            ingress_problems(&out, "attacker"),
            [claimed(
                "api.example.com",
                "attacker-tls",
                "Ingress",
                "victim/web"
            )]
        );
    }

    /// The certificate loaded with `pem` as its leaf.
    fn cert_of<'a>(out: &'a BuildOutput, pem: &str) -> &'a ir::Certificate {
        out.ir
            .certificates
            .iter()
            .find(|c| c.certificate.trim() == pem.trim())
            .unwrap_or_else(|| panic!("no certificate for the leaf: {:?}", out.ir.certificates))
    }

    /// `a` loads the early leaf alone for its own host. `c` loads the same
    /// leaf with an intermediate for a host `b` holds with another
    /// certificate, so `c` keeps no name at all.
    fn chain_lender(with_b: bool) -> Inputs {
        let chained = format!("{EARLY_CERT}{LATE_CERT}");
        let mut inputs = Inputs {
            ingresses: vec![
                tls_ingress("a", "web", OLD, &[(&["a.example.com"], "tls")]),
                tls_ingress("c", "web", NEW, &[(&["b.example.com"], "tls")]),
            ],
            secrets: vec![
                secret("a", "tls", EARLY_CERT, EARLY_KEY),
                secret("c", "tls", &chained, EARLY_KEY),
            ],
            ..Default::default()
        };
        if with_b {
            inputs.ingresses.push(tls_ingress(
                "b",
                "web",
                "2022-01-01T00:00:00Z",
                &[(&["b.example.com"], "tls")],
            ));
            inputs.secrets.push(secret("b", "tls", LATE_CERT, LATE_KEY));
        }
        inputs
    }

    #[test]
    fn a_loser_left_with_no_name_still_lends_its_fuller_chain_to_a_shared_leaf() {
        // Who keeps a name never decides the chain a leaf is served with:
        // `a.example.com` has no contest, and must not lose the intermediate
        // because the only Secret carrying it lost its own name elsewhere.
        let out = build(&BuildConfig::default(), &chain_lender(true));
        assert_eq!(out.ir.certificates.len(), 2, "{:?}", out.ir.certificates);
        let early = cert_of(&out, EARLY_CERT);
        assert_eq!(early.names, names(&["a.example.com"]));
        assert_eq!(early.chain.len(), 1, "the intermediate is still served");
        assert_eq!(cert_of(&out, LATE_CERT).names, names(&["b.example.com"]));
        assert_eq!(
            ingress_problems(&out, "c"),
            [claimed("b.example.com", "tls", "Ingress", "b/web")]
        );
        assert!(ingress_problems(&out, "a").is_empty());

        // Whatever the read order.
        let mut reversed = chain_lender(true);
        reversed.ingresses.reverse();
        reversed.secrets.reverse();
        assert_eq!(
            build(&BuildConfig::default(), &reversed).ir.certificates,
            out.ir.certificates
        );

        // `b` arriving takes `b.example.com` off the shared leaf in place;
        // the chain served for `a.example.com` does not change with it.
        let before = build(&BuildConfig::default(), &chain_lender(false));
        assert_eq!(cert_of(&before, EARLY_CERT).chain, early.chain);
        let requests = reconcile(&before.ir, &out.ir).unwrap();
        let early_replaces: Vec<_> = requests
            .iter()
            .filter_map(|r| match &r.request_type {
                Some(RequestType::ReplaceCertificate(r))
                    if r.new_certificate.certificate.trim() == EARLY_CERT.trim() =>
                {
                    Some(r)
                }
                _ => None,
            })
            .collect();
        assert_eq!(early_replaces.len(), 1, "{requests:?}");
        assert_eq!(
            early_replaces[0].new_certificate.certificate_chain.len(),
            1,
            "no chain downgrade is pushed"
        );
    }

    /// The longest pieces a lost-name report carries: a name inferred from a
    /// certificate (Sōzu's 4096-byte cap), an owner `namespace/name` (63 +
    /// 253 bytes) and a Secret name (253 bytes). Two-byte characters put every
    /// cut inside a UTF-8 sequence.
    fn longest_claim(names: usize) -> Problem {
        let owner = format!("x{}/{}", "é".repeat(31), "ü".repeat(126));
        Problem::CertificateNameClaimed {
            secret: "ß".repeat(126),
            listener: Some("https".into()),
            names: (0..names)
                .map(|i| ClaimedName {
                    name: format!("{i:04}{}", "é".repeat(2046)),
                    owner_kind: "Gateway",
                    owner: owner.clone(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_lost_name_report_fits_an_event_note_however_long_its_pieces() {
        for count in [1, 2, 3, 4, 9_999] {
            let message = longest_claim(count).to_string();
            // An Event note is refused past 1 kB, and a refused one is never
            // retried; the builder keeps to 900 bytes.
            assert!(
                message.len() <= 900,
                "{count} names: {} bytes: {message}",
                message.len()
            );
            // Every cut lands on a character boundary and says so.
            for cut in ["é…\"", "ß…\"", "ü…"] {
                assert!(message.contains(cut), "{cut} in {message}");
            }
            if count == 1 {
                assert!(message.starts_with("SNI name \"0000é"), "{message}");
                continue;
            }
            assert!(
                message.starts_with(&format!("{count} SNI names are claimed")),
                "{message}"
            );
            // At least one name is listed, and the rest are counted.
            let shown = message.matches("(Gateway ").count();
            assert!(shown >= 1, "{message}");
            assert!(
                message.ends_with(&format!(" and {} more", count - shown)),
                "{message}"
            );
        }
    }

    #[test]
    fn long_owner_and_secret_names_keep_every_report_within_the_budget() {
        // The longest object and Secret names Kubernetes admits, through a
        // whole build: one object loses several names, another a single one.
        let ns = "n".repeat(63);
        let long = |c: &str| c.repeat(253);
        let host = |i: usize| format!("{i}{}.{}.example.com", "h".repeat(60), "h".repeat(62));
        let hosts: Vec<String> = (0..5).map(host).collect();
        let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
        let inputs = Inputs {
            ingresses: vec![
                tls_ingress(&ns, &long("v"), OLD, &[(&hosts, &long("t"))]),
                tls_ingress(&ns, &long("a"), NEW, &[(&hosts, &long("u"))]),
                tls_ingress(&ns, &long("s"), NEW, &[(&hosts[..1], &long("w"))]),
            ],
            secrets: vec![
                secret(&ns, &long("t"), EARLY_CERT, EARLY_KEY),
                secret(&ns, &long("u"), LATE_CERT, LATE_KEY),
                secret(&ns, &long("w"), CERT, KEY),
            ],
            ..Default::default()
        };
        let out = build(&BuildConfig::default(), &inputs);
        let reports: Vec<String> = out
            .results
            .iter()
            .flat_map(|r| &r.problems)
            .map(Problem::to_string)
            .collect();
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert!(reports.iter().any(|r| r.starts_with("5 SNI names")));
        assert!(reports.iter().any(|r| r.starts_with("SNI name ")));
        for report in &reports {
            assert!(report.len() <= 900, "{} bytes: {report}", report.len());
            // The 317-byte owner and the 253-byte Secret are cut, visibly.
            assert!(report.contains(&format!("Ingress {ns}/vvv")), "{report}");
            assert!(report.contains("v…"), "{report}");
            assert!(report.contains("…\" does not serve"), "{report}");
        }
    }

    #[test]
    fn deleting_the_older_claimant_hands_the_name_over_in_one_rotation() {
        let mut inputs = contested(OLD, NEW);
        inputs.ingresses[1] = tls_ingress(
            "attacker",
            "web",
            NEW,
            &[(&["api.example.com"], "attacker-tls")],
        );
        let before = build(&BuildConfig::default(), &inputs);
        inputs.ingresses.remove(0);
        let after = build(&BuildConfig::default(), &inputs);
        assert_eq!(after.ir.certificates.len(), 1);
        assert_eq!(
            names_of(&after, LATE_CERT),
            Some(&names(&["api.example.com"])[..])
        );
        assert!(ingress_problems(&after, "attacker").is_empty());
        // Same listener, same name set: the name changes hands in one
        // ReplaceCertificate, with no gap.
        let requests = reconcile(&before.ir, &after.ir).unwrap();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            requests[0].request_type,
            Some(RequestType::ReplaceCertificate(_))
        ));
    }

    #[test]
    fn equal_timestamps_are_settled_by_the_joined_namespace_and_name() {
        // `-` sorts before `/`: "a-b/z" < "a/a" as one string, while the
        // (namespace, name) tuples sort the other way round. Route-key
        // arbitration compares the joined string; this must agree with it.
        let inputs = Inputs {
            ingresses: vec![
                tls_ingress("a", "a", OLD, &[(&["api.example.com"], "tls")]),
                tls_ingress("a-b", "z", OLD, &[(&["api.example.com"], "tls")]),
            ],
            secrets: vec![
                secret("a", "tls", EARLY_CERT, EARLY_KEY),
                secret("a-b", "tls", LATE_CERT, LATE_KEY),
            ],
            ..Default::default()
        };
        let out = build(&BuildConfig::default(), &inputs);
        assert_eq!(names_of(&out, EARLY_CERT), None);
        assert_eq!(
            names_of(&out, LATE_CERT),
            Some(&names(&["api.example.com"])[..])
        );
        assert_eq!(
            ingress_problems(&out, "a"),
            [claimed("api.example.com", "tls", "Ingress", "a-b/z")]
        );
    }

    #[test]
    fn a_shared_leaf_lends_no_seniority_for_a_name_its_older_holder_never_claimed() {
        // `old` loads the early certificate for its own name only. `new`
        // loads the same leaf for api.example.com, which `middle` (older than
        // `new`, younger than `old`) claims with another certificate.
        // Seniority belongs to the claim, not the leaf.
        let inputs = Inputs {
            ingresses: vec![
                tls_ingress("old", "web", OLD, &[(&["old.example.com"], "tls")]),
                tls_ingress(
                    "middle",
                    "web",
                    "2022-01-01T00:00:00Z",
                    &[(&["api.example.com"], "tls")],
                ),
                tls_ingress("new", "web", NEW, &[(&["api.example.com"], "tls")]),
            ],
            secrets: vec![
                secret("old", "tls", EARLY_CERT, EARLY_KEY),
                secret("middle", "tls", LATE_CERT, LATE_KEY),
                secret("new", "tls", EARLY_CERT, EARLY_KEY),
            ],
            ..Default::default()
        };
        let out = build(&BuildConfig::default(), &inputs);
        assert_eq!(
            names_of(&out, EARLY_CERT),
            Some(&names(&["old.example.com"])[..])
        );
        assert_eq!(
            names_of(&out, LATE_CERT),
            Some(&names(&["api.example.com"])[..])
        );
        assert_eq!(
            ingress_problems(&out, "new"),
            [claimed("api.example.com", "tls", "Ingress", "middle/web")]
        );
        assert!(ingress_problems(&out, "old").is_empty());
        assert!(ingress_problems(&out, "middle").is_empty());
    }

    #[test]
    fn one_leaf_naming_a_host_from_several_objects_is_no_conflict() {
        // Copies of one certificate in two namespaces, for the same host:
        // a shared certificate, not a contest.
        let inputs = Inputs {
            ingresses: vec![
                tls_ingress("one", "web", OLD, &[(&["api.example.com"], "tls")]),
                tls_ingress("two", "web", NEW, &[(&["api.example.com"], "tls")]),
            ],
            secrets: vec![
                secret("one", "tls", EARLY_CERT, EARLY_KEY),
                secret("two", "tls", EARLY_CERT, EARLY_KEY),
            ],
            ..Default::default()
        };
        let out = build(&BuildConfig::default(), &inputs);
        assert_eq!(out.ir.certificates.len(), 1);
        assert_eq!(
            names_of(&out, EARLY_CERT),
            Some(&names(&["api.example.com"])[..])
        );
        assert!(out.results.iter().all(|r| r.problems.is_empty()));
    }

    #[test]
    fn the_oldest_claimant_keeps_every_certificate_it_loaded_for_the_name() {
        // An ECDSA and an RSA certificate for one host from one object is a
        // choice its owner made (Sōzu picks among them as before); only the
        // newer object's certificate is a rival.
        let inputs = Inputs {
            ingresses: vec![
                tls_ingress(
                    "owner",
                    "web",
                    OLD,
                    &[
                        (&["api.example.com"], "ecdsa"),
                        (&["api.example.com"], "rsa"),
                    ],
                ),
                tls_ingress("rival", "web", NEW, &[(&["api.example.com"], "tls")]),
            ],
            secrets: vec![
                secret("owner", "ecdsa", EARLY_CERT, EARLY_KEY),
                secret("owner", "rsa", LATE_CERT, LATE_KEY),
                secret("rival", "tls", CERT, KEY),
            ],
            ..Default::default()
        };
        let out = build(&BuildConfig::default(), &inputs);
        for pem in [EARLY_CERT, LATE_CERT] {
            assert_eq!(names_of(&out, pem), Some(&names(&["api.example.com"])[..]));
        }
        assert_eq!(names_of(&out, CERT), None);
        assert!(ingress_problems(&out, "owner").is_empty());
        assert_eq!(
            ingress_problems(&out, "rival"),
            [claimed("api.example.com", "tls", "Ingress", "owner/web")]
        );
    }

    /// A Gateway whose hostname-less listener infers `*`, `*.org` and
    /// `*.wildcard.org` from its certificate, and an Ingress claiming `*.org`.
    fn gateway_and_ingress(gateway_created: &str, ingress_created: &str) -> Inputs {
        Inputs {
            gateway_classes: vec![Arc::new(object(json!({
                "metadata": { "name": "sozu" },
                "spec": { "controllerName": "sozu.io/gateway-controller" }
            })))],
            gateways: vec![hostless_gateway("infra", "gw", gateway_created, "gw-tls")],
            ingresses: vec![tls_ingress(
                "tenant",
                "web",
                ingress_created,
                &[(&["*.org"], "tenant-tls")],
            )],
            secrets: vec![
                secret("infra", "gw-tls", CERT, KEY),
                secret("tenant", "tenant-tls", LATE_CERT, LATE_KEY),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn a_gateway_loses_only_the_inferred_names_an_older_object_claims() {
        let out = build(&BuildConfig::default(), &gateway_and_ingress(NEW, OLD));
        assert_eq!(
            names_of(&out, CERT),
            Some(&names(&["*", "*.wildcard.org"])[..])
        );
        assert_eq!(names_of(&out, LATE_CERT), Some(&names(&["*.org"])[..]));
        let gateway = &out.gateways[0];
        assert_eq!(
            gateway.problems,
            [Problem::CertificateNameClaimed {
                secret: "gw-tls".into(),
                listener: Some("https".into()),
                names: vec![ClaimedName {
                    name: "*.org".into(),
                    owner_kind: "Ingress",
                    owner: "tenant/web".into(),
                }],
            }]
        );
        // The listener still serves, under the Ingress's certificate for the
        // name it lost; the problem names it, so its status can cite it.
        let listener = &gateway.listeners[0];
        assert!(listener.programmed && listener.resolved_refs);
        assert_eq!(gateway.problems[0].listener(), Some("https"));

        // An older Gateway keeps all of its inferred names, and the Ingress's
        // certificate, left with none, is not loaded.
        let out = build(&BuildConfig::default(), &gateway_and_ingress(OLD, NEW));
        assert_eq!(
            names_of(&out, CERT),
            Some(&names(&["*", "*.org", "*.wildcard.org"])[..])
        );
        assert_eq!(names_of(&out, LATE_CERT), None);
        assert!(out.gateways[0].problems.is_empty());
        assert_eq!(
            ingress_problems(&out, "tenant"),
            [claimed("*.org", "tenant-tls", "Gateway", "infra/gw")]
        );
    }
}
