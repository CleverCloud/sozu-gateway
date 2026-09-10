//! Replay commands against Sōzu's append-only routing lists. ConfigState itself
//! is unordered, so replaying only into ConfigState cannot detect precedence.
//! Selection follows sozu-proxy/sozu 2.2.1 lib/src/router/mod.rs:138–220.
use sozu_command_lib::proto::command::request::RequestType;
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

#[path = "common/routing.rs"]
mod routing;
use routing::RoutingTable;

fn front(
    host: &str,
    path: ir::PathMatch,
    backend: &str,
    method: Option<&str>,
    tls: bool,
) -> ir::Frontend {
    ir::Frontend {
        hostname: host.into(),
        multi_label_wildcard: false,
        path,
        cluster_id: Some(backend.into()),
        method: method.map(str::to_string),
        tls,
        listener: "0.0.0.0:8080".parse().unwrap(),
        filters: Default::default(),
    }
}

fn model(frontends: Vec<ir::Frontend>) -> ir::Ir {
    ir::Ir {
        frontends,
        ..Default::default()
    }
}

#[test]
fn exact_and_longest_prefix_survive_incremental_changes() {
    for tls in [false, true] {
        for host in ["*", "example.com"] {
            let root = front(host, ir::PathMatch::Prefix("/".into()), "a-root", None, tls);
            let mut previous = model(vec![root]);
            let mut router = RoutingTable::default();
            router.apply(tr::reconcile(&ir::Ir::default(), &previous).unwrap());
            let mut desired = previous.clone();
            desired.frontends.push(front(
                host,
                ir::PathMatch::Prefix("/v2".into()),
                "z-v2",
                None,
                tls,
            ));
            router.apply(tr::reconcile(&previous, &desired).unwrap());
            assert_eq!(router.backend("/v2", "GET"), "z-v2");
            assert_eq!(router.backend("/v2/?q=1", "GET"), "z-v2");
            assert_eq!(router.backend("/v2example", "GET"), "a-root");
            previous = desired.clone();
            desired.frontends.push(front(
                host,
                ir::PathMatch::Prefix("/v2/deep".into()),
                "b-deep",
                None,
                tls,
            ));
            desired.frontends.push(front(
                host,
                ir::PathMatch::Exact("/v2/deep".into()),
                "a-exact",
                None,
                tls,
            ));
            router.apply(tr::reconcile(&previous, &desired).unwrap());
            assert_eq!(router.backend("/v2/deep", "POST"), "a-exact");
            assert_eq!(router.backend("/v2/deep?q=1", "POST"), "a-exact");
            assert_eq!(router.backend("/v2/deep/child", "POST"), "b-deep");
            previous = desired.clone();
            desired.frontends[2].cluster_id = Some("retargeted".into());
            router.apply(tr::reconcile(&previous, &desired).unwrap());
            assert_eq!(router.backend("/v2/deep/child", "GET"), "retargeted");
            assert_eq!(router.backend("/v2/deep", "GET"), "a-exact");
            previous = desired.clone();
            desired.frontends[3].cluster_id = Some("exact-retargeted".into());
            router.apply(tr::reconcile(&previous, &desired).unwrap());
            assert_eq!(router.backend("/v2/deep?q=1", "GET"), "exact-retargeted");
            previous = desired.clone();
            desired.frontends.pop();
            router.apply(tr::reconcile(&previous, &desired).unwrap());
            assert_eq!(router.backend("/v2/deep", "GET"), "retargeted");
            previous = desired.clone();
            desired.frontends.pop();
            router.apply(tr::reconcile(&previous, &desired).unwrap());
            assert_eq!(router.backend("/v2/deep", "GET"), "z-v2");
            assert!(tr::reconcile(&desired, &desired).unwrap().is_empty());
            let cold = tr::ir_to_requests(&desired);
            desired.frontends.reverse();
            assert_eq!(tr::ir_to_requests(&desired), cold);
        }
    }
}

#[test]
fn path_precedes_method_and_method_breaks_equal_path_ties() {
    for host in ["*", "example.com"] {
        let mut previous = model(vec![
            front(host, ir::PathMatch::Prefix("/".into()), "root", None, false),
            front(
                host,
                ir::PathMatch::Prefix("/api".into()),
                "api-get",
                Some("GET"),
                false,
            ),
        ]);
        let mut router = RoutingTable::default();
        router.apply(tr::reconcile(&ir::Ir::default(), &previous).unwrap());
        let mut desired = previous.clone();
        desired.frontends.push(front(
            host,
            ir::PathMatch::Prefix("/api/deep".into()),
            "deep",
            None,
            false,
        ));
        desired.frontends.push(front(
            host,
            ir::PathMatch::Exact("/api".into()),
            "exact",
            None,
            false,
        ));
        router.apply(tr::reconcile(&previous, &desired).unwrap());
        assert_eq!(router.backend("/api", "GET"), "exact");
        assert_eq!(router.backend("/api/child", "GET"), "api-get");
        assert_eq!(router.backend("/api/deep/child", "GET"), "deep");
        assert_eq!(router.backend("/api/deep/child", "CUSTOM"), "deep");
        previous = desired.clone();
        desired.frontends.push(front(
            host,
            ir::PathMatch::Prefix("/api/deep".into()),
            "deep-post",
            Some("POST"),
            false,
        ));
        router.apply(tr::reconcile(&previous, &desired).unwrap());
        assert_eq!(router.backend("/api/deep/child", "POST"), "deep-post");
        assert_eq!(router.backend("/api/deep/child", "GET"), "deep");
        previous = desired.clone();
        desired.frontends.pop();
        router.apply(tr::reconcile(&previous, &desired).unwrap());
        assert_eq!(router.backend("/api/deep/child", "POST"), "deep");
    }
}

#[test]
fn reordering_is_limited_to_the_changed_host_and_listener() {
    let previous = model(vec![
        front("*", ir::PathMatch::Prefix("/".into()), "root", None, false),
        front(
            "other.example.com",
            ir::PathMatch::Prefix("/".into()),
            "other",
            None,
            false,
        ),
        front("*", ir::PathMatch::Prefix("/".into()), "tls", None, true),
    ]);
    let mut desired = previous.clone();
    desired.frontends.push(front(
        "*",
        ir::PathMatch::Prefix("/v2".into()),
        "v2",
        None,
        false,
    ));
    let requests = tr::reconcile(&previous, &desired).unwrap();
    assert_eq!(requests.len(), 3);
    assert!(matches!(
        requests[0].request_type,
        Some(RequestType::RemoveHttpFrontend(_))
    ));
    assert!(requests.iter().all(|r| match &r.request_type {
        Some(RequestType::AddHttpFrontend(f) | RequestType::RemoveHttpFrontend(f)) =>
            f.hostname == "*",
        _ => false,
    }));
    insta::assert_json_snapshot!(requests);
}

#[test]
fn deleting_a_route_preserves_the_remaining_order_without_readds() {
    for host in ["*", "example.com"] {
        let previous = model(vec![
            front(host, ir::PathMatch::Prefix("/".into()), "root", None, false),
            front(host, ir::PathMatch::Prefix("/v2".into()), "v2", None, false),
            front(
                host,
                ir::PathMatch::Exact("/v2".into()),
                "exact",
                None,
                false,
            ),
        ]);
        let mut desired = previous.clone();
        desired.frontends.remove(1);
        let requests = tr::reconcile(&previous, &desired).unwrap();
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            requests[0].request_type,
            Some(RequestType::RemoveHttpFrontend(_))
        ));
        let mut router = RoutingTable::default();
        router.apply(tr::ir_to_requests(&previous));
        router.apply(requests);
        assert_eq!(router.backend("/v2", "GET"), "exact");
        assert_eq!(router.backend("/v2/child", "GET"), "root");
    }
}
