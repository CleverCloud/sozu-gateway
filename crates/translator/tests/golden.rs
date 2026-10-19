//! Golden + property tests for the Translator (IR -> Sōzu commands, and diffs).
//!
//! Snapshots are JSON of the emitted `Vec<Request>`; determinism comes from the
//! canonical tier ordering. Cert tests use real PEM fixtures because Sōzu parses
//! certificates (fingerprint + SNI names).

use std::net::SocketAddr;

use sozu_command_lib::proto::command::request::RequestType;
use sozu_command_lib::proto::command::{LoadBalancingParams, PathRuleKind, Request};
use sozu_command_lib::state::ConfigState;
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

const CERT_A: &str = include_str!("fixtures/cert_a.pem");
const KEY_A: &str = include_str!("fixtures/key_a.pem");
const CERT_B: &str = include_str!("fixtures/cert_b.pem");
const KEY_B: &str = include_str!("fixtures/key_b.pem");

/// Requests as JSON, for assertion messages.
///
/// Since sozu-command-lib 2.2.1 the `Debug` of a request prints a bare variant
/// name for `AddBackend` and friends — deliberate, because that `Debug` also
/// reaches production logs that must not carry certificate or key material. It
/// leaves a failing golden test saying only which *kind* of request it saw, so
/// the value is re-rendered here instead. This is a test binary over fixtures:
/// nothing on the controller's logging path should copy it.
fn dump(reqs: &[Request]) -> String {
    serde_json::to_string_pretty(reqs).unwrap_or_else(|e| format!("<unserialisable: {e}>"))
}

fn addr(s: &str) -> SocketAddr {
    s.parse().expect("valid socket addr in test")
}

/// Re-wrap a PEM's base64 body at a different line width: byte-different text,
/// identical DER — so Sōzu computes the same fingerprint for both encodings.
fn rewrap_pem(pem: &str) -> String {
    let mut header = "";
    let mut footer = "";
    let mut body = String::new();
    for line in pem.lines() {
        if line.starts_with("-----BEGIN") {
            header = line;
        } else if line.starts_with("-----END") {
            footer = line;
        } else {
            body.push_str(line.trim());
        }
    }
    let wrapped: Vec<&str> = body
        .as_bytes()
        .chunks(48)
        .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
        .collect();
    format!("{header}\n{}\n{footer}\n", wrapped.join("\n"))
}

fn cluster(id: &str, lb: ir::LbAlgorithm, sticky: bool) -> ir::Cluster {
    ir::Cluster {
        id: id.to_string(),
        load_balancing: lb,
        sticky_session: sticky,
        https_redirect: false,
        max_connections_per_ip: None,
        retry_after: None,
        http2: false,
    }
}

fn backend(cluster_id: &str, addr_s: &str, weight: Option<i32>) -> ir::Backend {
    ir::Backend {
        cluster_id: cluster_id.to_string(),
        backend_id: format!("{cluster_id}-{}", addr_s.replace([':', '.'], "-")),
        address: addr(addr_s),
        weight,
        sticky_id: None,
    }
}

/// A backend of a sticky cluster, carrying the id the builder derives.
fn sticky_backend(cluster_id: &str, addr_s: &str, weight: Option<i32>, id: &str) -> ir::Backend {
    ir::Backend {
        sticky_id: Some(id.to_string()),
        ..backend(cluster_id, addr_s, weight)
    }
}

fn frontend(host: &str, path: ir::PathMatch, cluster_id: &str, tls: bool) -> ir::Frontend {
    ir::Frontend {
        hostname: host.to_string(),
        path,
        method: None,
        cluster_id: Some(cluster_id.to_string()),
        tls,
        listener: addr(if tls { "0.0.0.0:443" } else { "0.0.0.0:80" }),
        filters: ir::FrontendFilters::default(),
    }
}

fn cert(certificate: &str, key: &str) -> ir::Certificate {
    ir::Certificate {
        listener: addr("0.0.0.0:443"),
        certificate: certificate.to_string(),
        chain: vec![],
        key: key.to_string(),
        names: vec!["app.example.com".to_string()],
    }
}

/// A representative multi-host IR: two clusters, weighted/unweighted backends,
/// HTTP + HTTPS frontends, an exact-path route, and one TLS certificate.
fn sample_ir() -> ir::Ir {
    ir::Ir {
        clusters: vec![
            cluster("app", ir::LbAlgorithm::RoundRobin, false),
            cluster("api", ir::LbAlgorithm::LeastLoaded, true),
        ],
        backends: vec![
            backend("app", "10.0.0.1:8080", None),
            backend("app", "10.0.0.2:8080", None),
            sticky_backend("api", "10.0.1.1:9090", Some(5), "5f1c0a93d2e47b68"),
        ],
        frontends: vec![
            frontend(
                "app.example.com",
                ir::PathMatch::Prefix("/".into()),
                "app",
                false,
            ),
            frontend(
                "app.example.com",
                ir::PathMatch::Prefix("/".into()),
                "app",
                true,
            ),
            frontend(
                "api.example.com",
                ir::PathMatch::Exact("/v1".into()),
                "api",
                false,
            ),
        ],
        certificates: vec![cert(CERT_A, KEY_A)],
        l4_frontends: vec![],
    }
}

#[test]
fn ir_to_requests_full() {
    insta::assert_json_snapshot!(tr::ir_to_requests(&sample_ir()));
}

#[test]
fn catch_all_frontend_uses_post_position() {
    // A hostname-less ("*") frontend must use Sōzu's POST rule position (1) so it
    // is a fallback and never shadows specific-host (TREE) frontends.
    let model = ir::Ir {
        clusters: vec![cluster("app", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("app", "10.0.0.1:8080", None)],
        frontends: vec![frontend(
            "*",
            ir::PathMatch::Prefix("/".into()),
            "app",
            false,
        )],
        certificates: vec![],
        l4_frontends: vec![],
    };
    insta::assert_json_snapshot!(tr::ir_to_requests(&model));
}

/// The single path rule emitted for one frontend's IR path match.
fn emitted_path(model: &ir::Ir) -> (i32, String) {
    let reqs = tr::ir_to_requests(model);
    let mut paths: Vec<(i32, String)> = reqs
        .iter()
        .filter_map(|r| match &r.request_type {
            Some(RequestType::AddHttpFrontend(f)) => Some((f.path.kind, f.path.value.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        paths.len(),
        1,
        "one frontend, one rule: {dump}",
        dump = dump(&reqs)
    );
    paths.pop().expect("one rule")
}

fn one_prefix_route(path: &str) -> ir::Ir {
    ir::Ir {
        clusters: vec![cluster("app", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("app", "10.0.0.1:8080", None)],
        frontends: vec![frontend(
            "app.example.com",
            ir::PathMatch::Prefix(path.into()),
            "app",
            false,
        )],
        ..Default::default()
    }
}

#[test]
fn prefix_path_compiles_to_an_anchored_boundary_regex() {
    // Kubernetes `Prefix` matches on path element boundaries: `/foo` covers
    // `/foo`, `/foo?page=2` and `/foo/bar`, never `/foobar`. Sōzu's Prefix rule
    // is a raw string prefix and matches all four, so this needs a regex.
    //
    // Both halves of the pattern are load-bearing and were measured against a
    // live Sōzu: the `\?` branch because Sōzu matches path rules against the
    // request target with the query string still attached, and the `^` because
    // Full-span on purpose: Sōzu 2.2.1 does not anchor regexes itself (an
    // unanchored rule would also match `/xx/foo`) and 2.3.0 and later anchor
    // every one, so the rule carries both anchors and consumes the remainder.
    assert_eq!(
        emitted_path(&one_prefix_route("/foo")),
        (
            PathRuleKind::Regex as i32,
            r"^/foo(?:[/?](?-u:.*))?$".to_string()
        )
    );

    // A trailing slash is insignificant in Kubernetes, so `/foo/` compiles to
    // the same rule — otherwise the two spellings would diff forever.
    assert_eq!(
        emitted_path(&one_prefix_route("/foo/")),
        emitted_path(&one_prefix_route("/foo"))
    );

    // Regex metacharacters in a path are literals, not syntax. `/v1.0` must not
    // become a wildcard that also matches `/v1x0`.
    assert_eq!(
        emitted_path(&one_prefix_route("/v1.0")),
        (
            PathRuleKind::Regex as i32,
            r"^/v1\.0(?:[/?](?-u:.*))?$".to_string()
        )
    );

    // The root prefix stays a plain rule: every target starts with "/", so the
    // regex would be pure overhead on the most common route in any cluster.
    assert_eq!(
        emitted_path(&one_prefix_route("/")),
        (PathRuleKind::Prefix as i32, "/".to_string())
    );

    // ImplementationSpecific maps one-to-one.
    let mut regex = one_prefix_route("/foo");
    regex.frontends[0].path = ir::PathMatch::Regex("^/foo/[0-9]+$".into());
    assert_eq!(
        emitted_path(&regex),
        (PathRuleKind::Regex as i32, "^/foo/[0-9]+$".to_string())
    );

    // One rule per frontend, so the diff round-trips as it always did.
    let model = one_prefix_route("/foo");
    assert!(tr::reconcile(&model, &model).expect("idem").is_empty());
    assert_eq!(
        tr::reconcile(&model, &ir::Ir::default())
            .expect("teardown")
            .iter()
            .filter(|r| matches!(r.request_type, Some(RequestType::RemoveHttpFrontend(_))))
            .count(),
        1
    );
}

#[test]
fn exact_path_compiles_to_an_anchored_whole_path_regex() {
    // Never `Equals`: measured on Sōzu 2.2.x, `Equals("/get")` does not match
    // `/get?x=1` (the rule is compared against the query-bearing target), and
    // a removed `Equals` rule keeps matching after Sōzu acknowledged the
    // removal (its rule equality has no `Equals` arm), so the route stayed
    // exposed as a zombie. A full-span regex has neither defect, and reads
    // the same under Sōzu's next, anchoring router.
    let mut exact = one_prefix_route("/foo");
    exact.frontends[0].path = ir::PathMatch::Exact("/foo".into());
    assert_eq!(
        emitted_path(&exact),
        (
            PathRuleKind::Regex as i32,
            "^/foo(?:\\?(?-u:.*))?$".to_string()
        )
    );
    // No `/` alternative: `/foo/bar` is not `/foo`. And the trailing slash is
    // kept literal — Exact means exact, unlike Prefix's canonicalisation.
    exact.frontends[0].path = ir::PathMatch::Exact("/foo/".into());
    assert_eq!(
        emitted_path(&exact),
        (
            PathRuleKind::Regex as i32,
            "^/foo/(?:\\?(?-u:.*))?$".to_string()
        )
    );
    exact.frontends[0].path = ir::PathMatch::Exact("/".into());
    assert_eq!(
        emitted_path(&exact),
        (
            PathRuleKind::Regex as i32,
            "^/(?:\\?(?-u:.*))?$".to_string()
        )
    );
    // Regex metacharacters in the literal path are escaped.
    exact.frontends[0].path = ir::PathMatch::Exact("/a.b+c".into());
    assert_eq!(
        emitted_path(&exact),
        (
            PathRuleKind::Regex as i32,
            "^/a\\.b\\+c(?:\\?(?-u:.*))?$".to_string()
        )
    );
}

#[test]
fn an_exact_route_and_an_identical_user_regex_are_one_route() {
    // They compile to the same Sōzu rule, so Sōzu holds one route key: the
    // translator must emit one add — and the builder, keying its collision
    // report on the same compiled rule, is what names the loser.
    let model = ir::Ir {
        clusters: vec![
            cluster("exact", ir::LbAlgorithm::RoundRobin, false),
            cluster("regex", ir::LbAlgorithm::RoundRobin, false),
        ],
        backends: vec![],
        frontends: vec![
            frontend(
                "h.example.com",
                ir::PathMatch::Exact("/foo".into()),
                "exact",
                false,
            ),
            frontend(
                "h.example.com",
                ir::PathMatch::Regex("^/foo(?:\\?(?-u:.*))?$".into()),
                "regex",
                false,
            ),
        ],
        ..Default::default()
    };
    let reqs = tr::reconcile(&ir::Ir::default(), &model).expect("must not fail the translation");
    assert_eq!(
        reqs.iter()
            .filter(|r| matches!(r.request_type, Some(RequestType::AddHttpFrontend(_))))
            .count(),
        1,
        "one Sōzu route key, one add: {dump}",
        dump = dump(&reqs)
    );
}

#[test]
fn a_prefix_route_never_takes_an_exact_route_key() {
    // The boundary rule must not be expressible as an `Equals`, or a `Prefix`
    // and an `Exact` route on the same path would fight for one Sōzu route key
    // — and since translation is all-or-nothing, that clash would fail every
    // reconcile, not just this route. Distinct kinds keep both programmable.
    let model = ir::Ir {
        clusters: vec![
            cluster("pfx", ir::LbAlgorithm::RoundRobin, false),
            cluster("exact", ir::LbAlgorithm::RoundRobin, false),
        ],
        backends: vec![],
        frontends: vec![
            frontend(
                "h.example.com",
                ir::PathMatch::Prefix("/foo".into()),
                "pfx",
                false,
            ),
            frontend(
                "h.example.com",
                ir::PathMatch::Exact("/foo".into()),
                "exact",
                false,
            ),
        ],
        ..Default::default()
    };
    let reqs = tr::reconcile(&ir::Ir::default(), &model).expect("must not fail the translation");
    assert_eq!(
        reqs.iter()
            .filter(|r| matches!(r.request_type, Some(RequestType::AddHttpFrontend(_))))
            .count(),
        2,
        "both routes are programmed, on distinct keys: {dump}",
        dump = dump(&reqs)
    );
}

/// Every rule kind the IR can compile to (a plain root prefix, the regexes a
/// non-root prefix and an exact path become, a user regex — `;` included),
/// with and without a method, on the HTTP and HTTPS maps, IPv4 and IPv6.
/// The IR never emits Sōzu's `Equals` kind (see `ir::PathMatch::sozu_rule`).
fn route_key_matrix() -> Vec<ir::Frontend> {
    let paths = [
        ir::PathMatch::Prefix("/".into()),
        ir::PathMatch::Prefix("/api".into()),
        ir::PathMatch::Exact("/a;b".into()),
        ir::PathMatch::Regex("/x;GET".into()),
    ];
    let v6_scoped = SocketAddr::V6(std::net::SocketAddrV6::new(
        "fe80::1".parse().expect("valid v6"),
        8443,
        7,
        3,
    ));
    let mut out = Vec::new();
    for path in &paths {
        for method in [None, Some("GET")] {
            for tls in [false, true] {
                for listener in [None, Some(addr("[::]:8080")), Some(v6_scoped)] {
                    let mut f = frontend("h.example.com", path.clone(), "c", tls);
                    f.method = method.map(str::to_string);
                    if let Some(listener) = listener {
                        f.listener = listener;
                    }
                    out.push(f);
                }
            }
        }
    }
    let mut catch_all = frontend("*", ir::PathMatch::Prefix("/".into()), "c", false);
    catch_all.cluster_id = None;
    out.push(catch_all);
    out
}

#[test]
fn the_ir_route_key_is_the_key_sozu_stores_the_frontend_under() {
    // The builder arbitrates collisions on `sozu_route_key` without calling
    // Sōzu, so it must be byte-for-byte the key Sōzu's own ConfigState uses —
    // `RequestHttpFrontend`'s `Display` — or a clash could slip past it.
    for f in route_key_matrix() {
        let model = ir::Ir {
            clusters: vec![cluster("c", ir::LbAlgorithm::RoundRobin, false)],
            frontends: vec![f.clone()],
            ..Default::default()
        };
        let reqs = tr::reconcile(&ir::Ir::default(), &model).expect("single frontend translates");
        let expected = f.sozu_route_key();
        let (https, payload) = reqs
            .iter()
            .find_map(|r| match &r.request_type {
                Some(RequestType::AddHttpFrontend(p)) => Some((false, p)),
                Some(RequestType::AddHttpsFrontend(p)) => Some((true, p)),
                _ => None,
            })
            .expect("one frontend add");
        assert_eq!(https, expected.https, "frontend map for {f:?}");
        assert_eq!(payload.to_string(), expected.key, "Display key for {f:?}");

        let mut state = ConfigState::new();
        for r in &reqs {
            state.dispatch(r).expect("request folds into ConfigState");
        }
        let stored = if expected.https {
            &state.https_fronts
        } else {
            &state.http_fronts
        };
        assert_eq!(
            stored.keys().collect::<Vec<_>>(),
            vec![&expected.key],
            "ConfigState key for {f:?}"
        );
    }
}

#[test]
fn a_regex_whose_semicolon_spells_a_method_is_one_route_key() {
    // Sōzu joins rule and method with a bare `;`, so a regex `/x;GET` with no
    // method and a regex `/x` restricted to GET are one key to it: emitting
    // both would fail the whole translation with `StateError::Exists`, for
    // every tenant of the instance. The first is kept, and a `;` regex that
    // spells no clash stays programmable.
    let spelled = frontend(
        "h.example.com",
        ir::PathMatch::Regex("/x;GET".into()),
        "a",
        false,
    );
    let mut method = frontend(
        "h.example.com",
        ir::PathMatch::Regex("/x".into()),
        "b",
        false,
    );
    method.method = Some("GET".into());
    let mut other_method = method.clone();
    other_method.method = Some("POST".into());
    let model = ir::Ir {
        clusters: vec![
            cluster("a", ir::LbAlgorithm::RoundRobin, false),
            cluster("b", ir::LbAlgorithm::RoundRobin, false),
        ],
        frontends: vec![spelled, method, other_method],
        ..Default::default()
    };
    let reqs = tr::reconcile(&ir::Ir::default(), &model).expect("must not fail the translation");
    let adds: Vec<_> = reqs
        .iter()
        .filter_map(|r| match &r.request_type {
            Some(RequestType::AddHttpFrontend(p)) => Some((p.cluster_id.clone(), p.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(
        adds,
        vec![
            (
                Some("a".to_string()),
                "0.0.0.0:80;h.example.com;R/x;GET".to_string()
            ),
            (
                Some("b".to_string()),
                "0.0.0.0:80;h.example.com;R/x;POST".to_string()
            ),
        ],
        "first claimant of the shared key wins, the distinct key stays: {dump}",
        dump = dump(&reqs)
    );
}

#[test]
fn reconcile_from_empty_equals_full_adds() {
    let reqs = tr::reconcile(&ir::Ir::default(), &sample_ir()).expect("reconcile");
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_is_idempotent() {
    let sample = sample_ir();
    assert!(
        tr::reconcile(&sample, &sample)
            .expect("reconcile")
            .is_empty(),
        "re-applying an unchanged IR must emit no commands"
    );
}

#[test]
fn reconcile_scale_up_emits_single_add_backend() {
    let mut after = sample_ir();
    after.backends.push(backend("app", "10.0.0.3:8080", None));
    let reqs = tr::reconcile(&sample_ir(), &after).expect("reconcile");
    assert_eq!(reqs.len(), 1, "scale-up should be exactly one request");
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_scale_down_emits_single_remove_backend() {
    let mut after = sample_ir();
    after
        .backends
        .retain(|b| b.address != addr("10.0.0.2:8080"));
    let reqs = tr::reconcile(&sample_ir(), &after).expect("reconcile");
    assert_eq!(reqs.len(), 1, "scale-down should be exactly one request");
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_weight_change_keeps_backend_alive() {
    // ConfigState::diff emits a changed backend (same cluster/backend/address,
    // new weight) as Remove-then-Add, and canonicalize reorders adds before
    // backend removes. Sōzu's add_backend is an upsert and remove_backend
    // matches on (backend_id, address), so a surviving Remove would delete the
    // backend the Add just updated — it must be dropped, not just reordered.
    let before = sample_ir();
    let mut after = sample_ir();
    for b in &mut after.backends {
        if b.cluster_id == "api" {
            b.weight = Some(7); // was Some(5)
        }
    }
    let reqs = tr::reconcile(&before, &after).expect("reconcile");

    assert!(
        reqs.iter().any(|r| matches!(
            &r.request_type,
            Some(RequestType::AddBackend(b))
                if b.cluster_id == "api"
                    && b.load_balancing_parameters == Some(LoadBalancingParams { weight: 7 })
        )),
        "the weight change must arrive as an AddBackend upsert: {dump}",
        dump = dump(&reqs)
    );
    assert!(
        !reqs
            .iter()
            .any(|r| matches!(r.request_type, Some(RequestType::RemoveBackend(_)))),
        "no RemoveBackend may accompany the upsert (it would delete the backend): {dump}",
        dump = dump(&reqs)
    );
    assert_eq!(reqs.len(), 1, "a weight change is exactly one request");

    // Replay onto Sōzu's own state model, seeded with the before-state: the
    // backend must survive the batch and carry the new weight.
    let mut state = ConfigState::new();
    for req in tr::ir_to_requests(&before).iter().chain(reqs.iter()) {
        state.dispatch(req).expect("dispatch");
    }
    let api = state.backends.get("api").expect("api cluster backends");
    assert_eq!(api.len(), 1, "the api cluster must keep its backend");
    assert_eq!(
        api[0].load_balancing_parameters,
        Some(LoadBalancingParams { weight: 7 }),
        "the surviving backend must carry the new weight"
    );
}

#[test]
fn sticky_backends_carry_their_sticky_id_and_others_do_not() {
    let reqs = tr::ir_to_requests(&sample_ir());
    let sticky_ids: Vec<(&str, Option<&str>)> = reqs
        .iter()
        .filter_map(|r| match &r.request_type {
            Some(RequestType::AddBackend(b)) => {
                Some((b.cluster_id.as_str(), b.sticky_id.as_deref()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        sticky_ids,
        [
            ("api", Some("5f1c0a93d2e47b68")),
            ("app", None),
            ("app", None)
        ],
        "{dump}",
        dump = dump(&reqs)
    );
}

#[test]
fn reconcile_from_a_shadow_without_sticky_ids_upserts_the_sticky_backends() {
    // A shadow written before backends carried a sticky id reads back with
    // none, which is also what Sōzu holds: the diff must re-send exactly the
    // sticky cluster's backends so an existing install converges in place,
    // with no RemoveBackend (it would delete the backend the upsert updates).
    let mut old: serde_json::Value = serde_json::to_value(sample_ir()).expect("serialise");
    for backend in old["backends"].as_array_mut().expect("backends") {
        backend
            .as_object_mut()
            .expect("backend object")
            .remove("sticky_id");
    }
    let old: ir::Ir = serde_json::from_value(old).expect("a pre-sticky-id shadow still loads");
    assert!(old.backends.iter().all(|b| b.sticky_id.is_none()));

    let reqs = tr::reconcile(&old, &sample_ir()).expect("reconcile");
    assert_eq!(
        reqs.len(),
        1,
        "only the sticky backend changes: {}",
        dump(&reqs)
    );
    assert!(
        matches!(
            &reqs[0].request_type,
            Some(RequestType::AddBackend(b))
                if b.cluster_id == "api" && b.sticky_id.as_deref() == Some("5f1c0a93d2e47b68")
        ),
        "{dump}",
        dump = dump(&reqs)
    );

    // Replayed onto Sōzu's own state model: the backend survives, now sticky.
    let mut state = ConfigState::new();
    for req in tr::ir_to_requests(&old).iter().chain(reqs.iter()) {
        state.dispatch(req).expect("dispatch");
    }
    let api = state.backends.get("api").expect("api cluster backends");
    assert_eq!(api.len(), 1);
    assert_eq!(api[0].sticky_id.as_deref(), Some("5f1c0a93d2e47b68"));

    // Dropping the id again (annotation removed) is the same in-place upsert.
    let reqs = tr::reconcile(&sample_ir(), &old).expect("reconcile");
    assert_eq!(reqs.len(), 1, "{}", dump(&reqs));
    assert!(matches!(
        &reqs[0].request_type,
        Some(RequestType::AddBackend(b)) if b.sticky_id.is_none()
    ));
}

#[test]
fn reconcile_cert_rotation_is_single_replace() {
    let mut after = sample_ir();
    after.certificates = vec![cert(CERT_B, KEY_B)]; // same listener+names, new key/cert
    let reqs = tr::reconcile(&sample_ir(), &after).expect("reconcile");

    assert_eq!(
        reqs.len(),
        1,
        "rotation should be a single ReplaceCertificate"
    );
    assert!(
        matches!(
            reqs[0].request_type,
            Some(RequestType::ReplaceCertificate(_))
        ),
        "rotation with identical (listener, names) must use ReplaceCertificate (no TLS gap)"
    );
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_remove_all_includes_remove_certificate() {
    // This is the case that tripped sozu's ConfigState::diff debug-assert; our
    // own certificate diff must handle it without panicking.
    let reqs = tr::reconcile(&sample_ir(), &ir::Ir::default()).expect("reconcile");
    assert!(reqs
        .iter()
        .any(|r| matches!(r.request_type, Some(RequestType::RemoveCertificate(_)))));
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_same_cert_on_two_listeners_adds_both() {
    // Identity is (listener, fingerprint): the same PEM on two listeners must
    // produce two AddCertificate, not be deduped to one by fingerprint alone.
    let mut c2 = cert(CERT_A, KEY_A);
    c2.listener = addr("0.0.0.0:8443");
    let desired = ir::Ir {
        certificates: vec![cert(CERT_A, KEY_A), c2],
        ..Default::default()
    };
    let reqs = tr::reconcile(&ir::Ir::default(), &desired).expect("reconcile");
    let adds = reqs
        .iter()
        .filter(|r| matches!(r.request_type, Some(RequestType::AddCertificate(_))))
        .count();
    assert_eq!(adds, 2, "same cert on two listeners must be added on each");
}

#[test]
fn reconcile_cert_name_change_replaces_in_place() {
    // Same PEM (same fingerprint), different SNI names -> ReplaceCertificate
    // (a plain AddCertificate would be skipped by Sōzu as the fp already exists).
    let before = ir::Ir {
        certificates: vec![cert(CERT_A, KEY_A)],
        ..Default::default()
    };
    let mut renamed = cert(CERT_A, KEY_A);
    renamed.names = vec!["app.example.com".into(), "www.example.com".into()];
    let after = ir::Ir {
        certificates: vec![renamed],
        ..Default::default()
    };
    let reqs = tr::reconcile(&before, &after).expect("reconcile");
    assert_eq!(reqs.len(), 1);
    // Since Sōzu 2.3.2 (0bc160b7) the worker applies a same-fingerprint
    // Replace whose names changed; 2.2.1 ignored it (#81).
    assert_eq!(
        in_place_replace(&reqs[0]),
        Some(vec!["app.example.com".into(), "www.example.com".into()]),
        "a name-only change must Replace in place"
    );
}

/// The names a ReplaceCertificate installs, when it replaces the certificate
/// with itself (same fingerprint) — the in-place update.
fn in_place_replace(req: &Request) -> Option<Vec<String>> {
    match &req.request_type {
        Some(RequestType::ReplaceCertificate(r)) => {
            let own = sozu_command_lib::certificate::calculate_fingerprint(
                r.new_certificate.certificate.as_bytes(),
            )
            .expect("fingerprint");
            let own: String = own.iter().map(|b| format!("{b:02x}")).collect();
            (r.old_fingerprint == own).then(|| r.new_certificate.names.clone())
        }
        _ => None,
    }
}

#[test]
fn a_chain_change_on_the_same_leaf_replaces_it_in_place() {
    // A Secret that gains (or fixes) an intermediate keeps its leaf, so the
    // fingerprint is unchanged; without a Replace the old chain stays served.
    let before = ir::Ir {
        certificates: vec![cert(CERT_A, KEY_A)],
        ..Default::default()
    };
    let mut chained = cert(CERT_A, KEY_A);
    chained.chain = vec![CERT_B.to_string()];
    let after = ir::Ir {
        certificates: vec![chained],
        ..Default::default()
    };
    let reqs = tr::reconcile(&before, &after).expect("reconcile");
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert!(in_place_replace(&reqs[0]).is_some(), "{reqs:?}");
    match &reqs[0].request_type {
        Some(RequestType::ReplaceCertificate(r)) => {
            assert_eq!(
                r.new_certificate.certificate_chain,
                vec![CERT_B.to_string()]
            )
        }
        other => panic!("expected ReplaceCertificate, got {other:?}"),
    }
}

#[test]
fn a_chain_spelled_differently_is_no_change() {
    // What Sōzu serves is the decoded chain without copies of the leaf: a
    // re-wrapped PEM, a fullchain bundle carrying the leaf again, or the same
    // intermediates split across entries differently send nothing.
    let mut chained = cert(CERT_A, KEY_A);
    chained.chain = vec![CERT_B.to_string()];
    let before = ir::Ir {
        certificates: vec![chained.clone()],
        ..Default::default()
    };
    let mut respelled = chained;
    respelled.chain = vec![format!("{CERT_A}\n{}\n\n", CERT_B.trim_end())];
    let after = ir::Ir {
        certificates: vec![respelled],
        ..Default::default()
    };
    assert!(tr::reconcile(&before, &after)
        .expect("reconcile")
        .is_empty());
}

#[test]
fn secrets_sharing_a_leaf_serve_one_chain_whatever_their_order() {
    // Two Secrets carry the same leaf with different chains: one of them is
    // served, and which one must not depend on the order they were read in,
    // or every reconcile would replace the certificate back and forth.
    let mut chained = cert(CERT_A, KEY_A);
    chained.chain = vec![CERT_B.to_string()];
    let bare = cert(CERT_A, KEY_A);
    let one = ir::Ir {
        certificates: vec![chained.clone(), bare.clone()],
        ..Default::default()
    };
    let other = ir::Ir {
        certificates: vec![bare, chained],
        ..Default::default()
    };
    assert!(tr::reconcile(&one, &other).expect("reconcile").is_empty());
    assert!(tr::reconcile(&other, &one).expect("reconcile").is_empty());
}

#[test]
fn reconcile_duplicate_fingerprint_certs_is_idempotent() {
    // Two byte-different PEM encodings of the SAME certificate (fingerprints
    // are computed over the parsed DER) at one listener, each carrying a
    // different SNI name. They are one identity in Sōzu's cert store, so
    // re-applying the unchanged IR must be a no-op — not a ReplaceCertificate
    // every cycle that clamps SNI coverage to one entry's names.
    let a = cert(CERT_A, KEY_A); // names: app.example.com
    let mut b = cert(&rewrap_pem(CERT_A), KEY_A);
    b.names = vec!["www.example.com".to_string()];

    assert_ne!(a.certificate, b.certificate, "PEMs must be byte-different");
    let fp = |pem: &str| {
        sozu_command_lib::certificate::calculate_fingerprint(pem.as_bytes())
            .expect("valid certificate")
    };
    assert_eq!(
        fp(&a.certificate),
        fp(&b.certificate),
        "both encodings must share one fingerprint"
    );

    let model = ir::Ir {
        certificates: vec![a, b],
        ..Default::default()
    };
    assert!(
        tr::reconcile(&model, &model).expect("reconcile").is_empty(),
        "duplicate-fingerprint entries must reconcile to no requests"
    );
}

#[test]
fn reconcile_duplicate_fingerprint_certs_union_names() {
    // The desired side has duplicate entries for one cert whose SNI name UNION
    // differs from the loaded names: exactly one ReplaceCertificate, carrying
    // the union — neither hostname may lose coverage.
    let before = ir::Ir {
        certificates: vec![cert(CERT_A, KEY_A)], // names: app.example.com
        ..Default::default()
    };
    let mut dup = cert(&rewrap_pem(CERT_A), KEY_A);
    dup.names = vec!["www.example.com".to_string()];
    let after = ir::Ir {
        certificates: vec![cert(CERT_A, KEY_A), dup],
        ..Default::default()
    };
    let reqs = tr::reconcile(&before, &after).expect("reconcile");
    assert_eq!(
        reqs.len(),
        1,
        "one ReplaceCertificate expected: {dump}",
        dump = dump(&reqs)
    );
    match &reqs[0].request_type {
        Some(RequestType::ReplaceCertificate(r)) => {
            assert_eq!(
                r.new_certificate.names,
                vec!["app.example.com".to_string(), "www.example.com".to_string()],
                "the replacement must carry the union of the SNI names"
            );
        }
        other => panic!("expected ReplaceCertificate, got {other:?}"),
    }
}

/// A certificate at the HTTPS listener carrying `names`.
fn named(certificate: &str, key: &str, names: &[&str]) -> ir::Certificate {
    ir::Certificate {
        names: names.iter().map(|n| n.to_string()).collect(),
        ..cert(certificate, key)
    }
}

fn certificates(certificates: Vec<ir::Certificate>) -> ir::Ir {
    ir::Ir {
        certificates,
        ..Default::default()
    }
}

/// What each certificate request does, in batch order: the verb, the leaf it
/// installs and the names it installs it with.
fn certificate_steps(reqs: &[Request]) -> Vec<(&'static str, &str, Vec<String>)> {
    reqs.iter()
        .map(|r| match &r.request_type {
            Some(RequestType::AddCertificate(a)) => (
                "add",
                a.certificate.certificate.as_str(),
                a.certificate.names.clone(),
            ),
            Some(RequestType::ReplaceCertificate(r)) => (
                "replace",
                r.new_certificate.certificate.as_str(),
                r.new_certificate.names.clone(),
            ),
            other => panic!("unexpected request {other:?}"),
        })
        .collect()
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| n.to_string()).collect()
}

#[test]
fn a_name_taken_by_a_new_certificate_is_added_before_the_loser_drops_it() {
    // The builder hands api.example.com to an older claimant's certificate
    // and strips it from the one that held it. Sōzu 2.3.2 applies both at
    // once, so dropping first would fall back to the default certificate
    // until the add lands.
    let before = certificates(vec![named(
        CERT_B,
        KEY_B,
        &["api.example.com", "x.example.com"],
    )]);
    let after = certificates(vec![
        named(CERT_A, KEY_A, &["api.example.com"]),
        named(CERT_B, KEY_B, &["x.example.com"]),
    ]);
    let reqs = tr::reconcile(&before, &after).expect("reconcile");
    assert_eq!(
        certificate_steps(&reqs),
        vec![
            ("add", CERT_A, names(&["api.example.com"])),
            ("replace", CERT_B, names(&["x.example.com"])),
        ],
        "{}",
        dump(&reqs)
    );
    assert!(in_place_replace(&reqs[1]).is_some());
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn a_name_moving_between_loaded_certificates_is_given_before_it_is_taken() {
    // Both certificates stay loaded, so both requests are replaces, and the
    // JSON encoding would put CERT_B's first (its PEM sorts first).
    let before = certificates(vec![
        named(CERT_A, KEY_A, &["app.example.com"]),
        named(CERT_B, KEY_B, &["api.example.com", "x.example.com"]),
    ]);
    let after = certificates(vec![
        named(CERT_A, KEY_A, &["api.example.com", "app.example.com"]),
        named(CERT_B, KEY_B, &["x.example.com"]),
    ]);
    let reqs = tr::reconcile(&before, &after).expect("reconcile");
    assert_eq!(
        certificate_steps(&reqs),
        vec![
            (
                "replace",
                CERT_A,
                names(&["api.example.com", "app.example.com"])
            ),
            ("replace", CERT_B, names(&["x.example.com"])),
        ],
        "{}",
        dump(&reqs)
    );
}

#[test]
fn a_replace_that_gives_a_name_away_runs_before_one_that_only_drops() {
    // CERT_A drops n but takes m from CERT_B, which only drops it: CERT_A's
    // replace must land first, though both take a name away and CERT_B's
    // PEM sorts first.
    let before = certificates(vec![
        named(CERT_A, KEY_A, &["n.example.com", "x.example.com"]),
        named(CERT_B, KEY_B, &["m.example.com", "y.example.com"]),
    ]);
    let after = certificates(vec![
        named(CERT_A, KEY_A, &["m.example.com", "x.example.com"]),
        named(CERT_B, KEY_B, &["y.example.com"]),
    ]);
    let reqs = tr::reconcile(&before, &after).expect("reconcile");
    assert_eq!(
        certificate_steps(&reqs),
        vec![
            (
                "replace",
                CERT_A,
                names(&["m.example.com", "x.example.com"])
            ),
            ("replace", CERT_B, names(&["y.example.com"])),
        ],
        "{}",
        dump(&reqs)
    );
}

#[test]
fn a_chain_of_name_transfers_runs_every_recipient_before_its_donor() {
    // y.example.com moves from `donor` to `recipient`, while each also drops
    // or gains a name of its own: both replaces trade names, and the chain is
    // acyclic, so `recipient` must go first whichever PEM sorts first.
    for (recipient, recipient_key, donor, donor_key) in [
        (CERT_A, KEY_A, CERT_B, KEY_B),
        (CERT_B, KEY_B, CERT_A, KEY_A),
    ] {
        let before = vec![
            named(recipient, recipient_key, &["x.example.com"]),
            named(donor, donor_key, &["y.example.com"]),
        ];
        let after = vec![
            named(recipient, recipient_key, &["y.example.com"]),
            named(donor, donor_key, &["z.example.com"]),
        ];
        for reversed in [false, true] {
            let (mut before, mut after) = (before.clone(), after.clone());
            if reversed {
                before.reverse();
                after.reverse();
            }
            let reqs =
                tr::reconcile(&certificates(before), &certificates(after)).expect("reconcile");
            assert_eq!(
                certificate_steps(&reqs),
                vec![
                    ("replace", recipient, names(&["y.example.com"])),
                    ("replace", donor, names(&["z.example.com"])),
                ],
                "{}",
                dump(&reqs)
            );
        }
    }
}

#[test]
fn certificates_swapping_names_keep_the_json_order_whatever_the_input_order() {
    // Each gains the name the other drops: a cycle, so neither can go first
    // without one name falling back for a moment. The batch still settles,
    // in the JSON order, whatever order the certificates come in.
    let before = vec![
        named(CERT_A, KEY_A, &["x.example.com"]),
        named(CERT_B, KEY_B, &["y.example.com"]),
    ];
    let after = vec![
        named(CERT_A, KEY_A, &["y.example.com"]),
        named(CERT_B, KEY_B, &["x.example.com"]),
    ];
    let reqs = tr::reconcile(&certificates(before.clone()), &certificates(after.clone()))
        .expect("reconcile");
    let mut json_order = reqs.clone();
    json_order.sort_by_cached_key(|r| serde_json::to_string(r).expect("json"));
    assert_eq!(reqs, json_order, "{}", dump(&reqs));
    assert_eq!(
        certificate_steps(&reqs),
        vec![
            ("replace", CERT_B, names(&["x.example.com"])),
            ("replace", CERT_A, names(&["y.example.com"])),
        ],
        "{}",
        dump(&reqs)
    );
    let (mut before, mut after) = (before, after);
    before.reverse();
    after.reverse();
    assert_eq!(
        tr::reconcile(&certificates(before), &certificates(after)).expect("reconcile"),
        reqs
    );
}

#[test]
fn reconcile_retarget_route_removes_before_adds() {
    // Re-pointing the same host+path at a DIFFERENT cluster. Sōzu keys a route by
    // host+path (not cluster_id), so this is a Remove(old)+Add(new) on the same
    // route key. The old frontend MUST be removed before the new one is added —
    // otherwise the live add_http_frontend rejects the duplicate (StateError::Exists)
    // and the trailing remove deletes the route, so the reconcile never converges.
    let before = ir::Ir {
        clusters: vec![cluster("old", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("old", "10.0.0.1:8080", None)],
        frontends: vec![frontend(
            "app.example.com",
            ir::PathMatch::Prefix("/".into()),
            "old",
            false,
        )],
        certificates: vec![],
        l4_frontends: vec![],
    };
    let after = ir::Ir {
        clusters: vec![cluster("new", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("new", "10.0.0.2:8080", None)],
        frontends: vec![frontend(
            "app.example.com",
            ir::PathMatch::Prefix("/".into()),
            "new",
            false,
        )],
        certificates: vec![],
        l4_frontends: vec![],
    };
    let reqs = tr::reconcile(&before, &after).expect("reconcile");

    let remove_idx = reqs
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::RemoveHttpFrontend(_))))
        .expect("a RemoveHttpFrontend for the old route");
    let add_idx = reqs
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::AddHttpFrontend(_))))
        .expect("an AddHttpFrontend for the new route");
    assert!(
        remove_idx < add_idx,
        "old frontend must be removed before the new one is added (same Sōzu route key), \
         got remove at {remove_idx}, add at {add_idx}: {dump}",
        dump = dump(&reqs)
    );
    insta::assert_json_snapshot!(reqs);
}

#[test]
fn reconcile_retarget_https_route_removes_before_adds() {
    // Same retarget but on a TLS frontend: RemoveHttpsFrontend must precede
    // AddHttpsFrontend. Assertion-only (the HTTP case already pins a snapshot).
    let ir_for = |cluster_id: &str, addr_s: &str| ir::Ir {
        clusters: vec![cluster(cluster_id, ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend(cluster_id, addr_s, None)],
        frontends: vec![frontend(
            "app.example.com",
            ir::PathMatch::Prefix("/".into()),
            cluster_id,
            true,
        )],
        certificates: vec![cert(CERT_A, KEY_A)],
        l4_frontends: vec![],
    };
    let before = ir_for("old", "10.0.0.1:8080");
    let after = ir_for("new", "10.0.0.2:8080");
    let reqs = tr::reconcile(&before, &after).expect("reconcile");

    let remove_idx = reqs
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::RemoveHttpsFrontend(_))))
        .expect("a RemoveHttpsFrontend for the old route");
    let add_idx = reqs
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::AddHttpsFrontend(_))))
        .expect("an AddHttpsFrontend for the new route");
    assert!(
        remove_idx < add_idx,
        "old HTTPS frontend must be removed before the new one is added, \
         got remove at {remove_idx}, add at {add_idx}: {dump}",
        dump = dump(&reqs)
    );
}

#[test]
fn ir_to_requests_with_filters() {
    let mut f = frontend(
        "app.example.com",
        ir::PathMatch::Prefix("/".into()),
        "app",
        false,
    );
    f.filters = ir::FrontendFilters {
        header_mods: vec![
            ir::HeaderMod {
                on: ir::HeaderTarget::Request,
                key: "X-Env".into(),
                value: Some("prod".into()),
            },
            ir::HeaderMod {
                on: ir::HeaderTarget::Response,
                key: "Server".into(),
                value: None, // delete
            },
        ],
        redirect: Some(ir::Redirect {
            scheme: Some(ir::Scheme::Https),
            status: ir::RedirectStatus::MovedPermanently,
            hostname: None,
            path: None,
            port: None,
        }),
        rewrite: Some(ir::Rewrite {
            hostname: Some("backend.svc".into()),
            path: Some("/v2".into()),
        }),
    };
    let model = ir::Ir {
        clusters: vec![cluster("app", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("app", "10.0.0.1:8080", None)],
        frontends: vec![f],
        certificates: vec![],
        l4_frontends: vec![],
    };
    insta::assert_json_snapshot!(tr::ir_to_requests(&model));
}

#[test]
fn reconcile_add_new_route() {
    let small = ir::Ir {
        clusters: vec![cluster("app", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("app", "10.0.0.1:8080", None)],
        frontends: vec![frontend(
            "app.example.com",
            ir::PathMatch::Prefix("/".into()),
            "app",
            false,
        )],
        certificates: vec![],
        l4_frontends: vec![],
    };
    insta::assert_json_snapshot!(tr::reconcile(&small, &sample_ir()).expect("reconcile"));
}

#[test]
fn reconcile_redirect_only_frontend_folds() {
    let mut f = frontend(
        "app.example.com",
        ir::PathMatch::Prefix("/".into()),
        "x",
        false,
    );
    f.cluster_id = None;
    f.filters.redirect = Some(ir::Redirect {
        scheme: Some(ir::Scheme::Https),
        status: ir::RedirectStatus::Found,
        hostname: None,
        path: None,
        port: None,
    });
    let model = ir::Ir {
        frontends: vec![f],
        ..Default::default()
    };
    let reqs = tr::reconcile(&ir::Ir::default(), &model).expect("redirect-only frontend must fold");
    assert_eq!(reqs.len(), 1);
}

#[test]
fn cluster_connection_limit_maps_to_request() {
    let mut c = cluster("app", ir::LbAlgorithm::RoundRobin, false);
    c.max_connections_per_ip = Some(100);
    c.retry_after = Some(30);
    let model = ir::Ir {
        clusters: vec![c],
        ..Default::default()
    };
    insta::assert_json_snapshot!(tr::ir_to_requests(&model));
}

/// A cluster whose backends speak h2c asks Sōzu for HTTP/2 towards them; the
/// cluster next to it keeps the request it always had (`http2` absent).
#[test]
fn h2c_cluster_maps_to_http2() {
    let mut grpc = cluster("grpc", ir::LbAlgorithm::RoundRobin, false);
    grpc.http2 = true;
    let model = ir::Ir {
        clusters: vec![cluster("app", ir::LbAlgorithm::RoundRobin, false), grpc],
        ..Default::default()
    };
    insta::assert_json_snapshot!(tr::ir_to_requests(&model));
}

/// `http2` of each `AddCluster` a reconcile emits, by cluster id.
fn cluster_http2(reqs: &[Request]) -> Vec<(String, Option<bool>)> {
    reqs.iter()
        .filter_map(|r| match &r.request_type {
            Some(RequestType::AddCluster(c)) => Some((c.cluster_id.clone(), c.http2)),
            _ => None,
        })
        .collect()
}

/// A Service port gaining or losing `kubernetes.io/h2c` reaches Sōzu as one
/// `AddCluster` upsert, both ways, and nothing else moves. Off is the field
/// left out, never `Some(false)`: the worker reads it as `unwrap_or(false)`
/// and an upsert replaces the whole cluster, so `None` resets the backend to
/// HTTP/1.1 (measured on 2.3.2, see docs/probes/h2c-backend_*).
#[test]
fn reconcile_flips_http2_both_ways() {
    let off = sample_ir();
    let mut on = sample_ir();
    on.clusters[0].http2 = true;

    let up = tr::reconcile(&off, &on).expect("reconcile");
    assert_eq!(up.len(), 1, "{}", dump(&up));
    assert_eq!(cluster_http2(&up), [("app".to_string(), Some(true))]);
    insta::assert_json_snapshot!("reconcile_turns_http2_on", up);

    let down = tr::reconcile(&on, &off).expect("reconcile");
    assert_eq!(down.len(), 1, "{}", dump(&down));
    assert_eq!(cluster_http2(&down), [("app".to_string(), None)]);
    insta::assert_json_snapshot!("reconcile_turns_http2_off", down);

    assert!(tr::reconcile(&on, &on).expect("reconcile").is_empty());
}

#[test]
fn a_udp_cluster_carries_no_per_ip_cap() {
    // Sōzu 2.3 would turn the cap into a silent per-source UDP flow limit;
    // the same annotation keeps capping a TCP cluster.
    let mut dns = cluster("dns", ir::LbAlgorithm::RoundRobin, false);
    dns.max_connections_per_ip = Some(10);
    let mut pg = cluster("pg", ir::LbAlgorithm::RoundRobin, false);
    pg.max_connections_per_ip = Some(10);
    let model = ir::Ir {
        clusters: vec![dns, pg],
        l4_frontends: vec![
            ir::L4Frontend {
                protocol: ir::L4Protocol::Udp,
                listener: addr("0.0.0.0:5353"),
                cluster_id: "dns".into(),
            },
            ir::L4Frontend {
                protocol: ir::L4Protocol::Tcp,
                listener: addr("0.0.0.0:5432"),
                cluster_id: "pg".into(),
            },
        ],
        ..Default::default()
    };
    let caps: Vec<(String, Option<u64>, bool)> = tr::ir_to_requests(&model)
        .into_iter()
        .filter_map(|r| match r.request_type {
            Some(RequestType::AddCluster(c)) => {
                Some((c.cluster_id, c.max_connections_per_ip, c.udp.is_some()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        caps,
        [("dns".into(), None, true), ("pg".into(), Some(10), false)]
    );
}

#[test]
fn a_frontend_naming_an_undeclared_cluster_is_refused() {
    // Upstream's diff re-adds such a frontend after removing its cluster; our
    // tiers would add it first and Sōzu 2.3's cascade would then delete it.
    let previous = sample_ir();
    let mut desired = previous.clone();
    let named = desired.frontends[0]
        .cluster_id
        .clone()
        .expect("sample frontends name a cluster");
    desired.clusters.retain(|c| c.id != named);
    desired.backends.retain(|b| b.cluster_id != named);
    let err = tr::reconcile(&previous, &desired).unwrap_err();
    assert!(
        matches!(&err, tr::TranslatorError::UnknownCluster(_, id) if *id == named),
        "{err:?}"
    );
    assert!(
        tr::reconcile(&desired.clone(), &previous).is_ok(),
        "only the desired state is checked"
    );
}

#[test]
fn ir_to_requests_with_l4_tcp() {
    let model = ir::Ir {
        clusters: vec![cluster("pg", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("pg", "10.0.0.1:5432", None)],
        l4_frontends: vec![ir::L4Frontend {
            protocol: ir::L4Protocol::Tcp,
            listener: addr("0.0.0.0:5432"),
            cluster_id: "pg".into(),
        }],
        ..Default::default()
    };
    insta::assert_json_snapshot!(tr::ir_to_requests(&model));
}

#[test]
fn reconcile_tolerates_exact_duplicate_l4_frontends() {
    // Two sources mapping the same port to the same cluster are a benign
    // duplicate, exactly like overlapping HTTP frontends: the reconcile must
    // fold them into one AddTcpFrontend, not hard-fail with
    // `StateError::Exists` on every cycle.
    let l4 = ir::L4Frontend {
        protocol: ir::L4Protocol::Tcp,
        listener: addr("0.0.0.0:5432"),
        cluster_id: "pg".into(),
    };
    let model = ir::Ir {
        clusters: vec![cluster("pg", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("pg", "10.0.0.1:5432", None)],
        l4_frontends: vec![l4.clone(), l4],
        ..Default::default()
    };
    let reqs = tr::reconcile(&ir::Ir::default(), &model).expect("exact duplicates must fold");
    assert_eq!(
        reqs.iter()
            .filter(|r| matches!(r.request_type, Some(RequestType::AddTcpFrontend(_))))
            .count(),
        1,
        "exactly one AddTcpFrontend for the deduplicated pair: {dump}",
        dump = dump(&reqs)
    );
    assert!(
        tr::reconcile(&model, &model).expect("idem").is_empty(),
        "the duplicate-carrying IR must stay idempotent"
    );
}

#[test]
fn reconcile_rejects_conflicting_l4_frontends() {
    // Same port, DIFFERENT cluster: a real conflict. It must still fail the
    // reconcile — silently picking a winner would route the port's traffic to
    // an arbitrary Service.
    let front = |cluster_id: &str| ir::L4Frontend {
        protocol: ir::L4Protocol::Tcp,
        listener: addr("0.0.0.0:5432"),
        cluster_id: cluster_id.into(),
    };
    let model = ir::Ir {
        clusters: vec![
            cluster("pg", ir::LbAlgorithm::RoundRobin, false),
            cluster("redis", ir::LbAlgorithm::RoundRobin, false),
        ],
        backends: vec![
            backend("pg", "10.0.0.1:5432", None),
            backend("redis", "10.0.0.2:6379", None),
        ],
        l4_frontends: vec![front("pg"), front("redis")],
        ..Default::default()
    };
    assert!(
        tr::reconcile(&ir::Ir::default(), &model).is_err(),
        "two clusters claiming one L4 port must fail, not pick a winner"
    );
}

#[test]
fn reconcile_adds_then_removes_l4_route() {
    let model = ir::Ir {
        clusters: vec![cluster("pg", ir::LbAlgorithm::RoundRobin, false)],
        backends: vec![backend("pg", "10.0.0.1:5432", None)],
        l4_frontends: vec![ir::L4Frontend {
            protocol: ir::L4Protocol::Tcp,
            listener: addr("0.0.0.0:5432"),
            cluster_id: "pg".into(),
        }],
        ..Default::default()
    };
    // empty -> model: listener added + activated, plus the TCP frontend.
    let add = tr::reconcile(&ir::Ir::default(), &model).expect("reconcile add");
    assert!(add
        .iter()
        .any(|r| matches!(r.request_type, Some(RequestType::AddTcpListener(_)))));
    // ConfigState::diff emits the activation of a new active listener twice
    // (inline + trailing sweep); exactly one may survive the reconcile.
    assert_eq!(
        add.iter()
            .filter(|r| matches!(r.request_type, Some(RequestType::ActivateListener(_))))
            .count(),
        1,
        "a new listener must be activated exactly once: {add:#?}"
    );
    assert!(add
        .iter()
        .any(|r| matches!(r.request_type, Some(RequestType::AddTcpFrontend(_)))));
    insta::assert_json_snapshot!(add);

    // Re-applying the same state is a no-op.
    assert!(tr::reconcile(&model, &model).expect("idem").is_empty());

    // model -> empty: the frontend and listener are torn down, and the
    // listener is deactivated *before* it is removed (Sōzu's own teardown
    // order — pinned by explicit tiers, not by request-name sort order).
    let rm = tr::reconcile(&model, &ir::Ir::default()).expect("reconcile rm");
    assert!(rm
        .iter()
        .any(|r| matches!(r.request_type, Some(RequestType::RemoveTcpFrontend(_)))));
    let deactivate_idx = rm
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::DeactivateListener(_))))
        .expect("a DeactivateListener for the torn-down listener");
    let remove_idx = rm
        .iter()
        .position(|r| matches!(r.request_type, Some(RequestType::RemoveListener(_))))
        .expect("a RemoveListener for the torn-down listener");
    assert!(
        deactivate_idx < remove_idx,
        "the listener must be deactivated before it is removed, \
         got deactivate at {deactivate_idx}, remove at {remove_idx}: {rm:#?}"
    );
}

#[test]
fn two_spellings_of_one_prefix_never_fail_the_translation() {
    // `/foo` and `/foo/` are one path in Kubernetes and compile to one rule.
    // The builder canonicalises the spelling, but the translator must never be
    // able to fail on an IR: one clash would take down every route, not just
    // this one.
    let model = ir::Ir {
        clusters: vec![
            cluster("a", ir::LbAlgorithm::RoundRobin, false),
            cluster("b", ir::LbAlgorithm::RoundRobin, false),
        ],
        backends: vec![],
        frontends: vec![
            frontend(
                "h.example.com",
                ir::PathMatch::Prefix("/foo".into()),
                "a",
                false,
            ),
            frontend(
                "h.example.com",
                ir::PathMatch::Prefix("/foo/".into()),
                "b",
                false,
            ),
        ],
        ..Default::default()
    };
    let reqs = tr::reconcile(&ir::Ir::default(), &model).expect("must not fail the translation");
    assert_eq!(
        reqs.iter()
            .filter(|r| matches!(r.request_type, Some(RequestType::AddHttpFrontend(_))))
            .count(),
        1,
        "one route key, first frontend wins: {dump}",
        dump = dump(&reqs)
    );
}
