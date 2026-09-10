use sozu_command_lib::proto::command::{request::RequestType, RulePosition};
use sozu_gw_ir as ir;
use sozu_gw_translator as tr;

#[path = "common/routing.rs"]
mod routing;
use routing::RoutingTable;

fn front(hostname: &str, path: ir::PathMatch, backend: &str, tls: bool) -> ir::Frontend {
    ir::Frontend {
        hostname: hostname.into(),
        multi_label_wildcard: hostname.starts_with("*."),
        path,
        cluster_id: Some(backend.into()),
        method: None,
        tls,
        listener: if tls { "0.0.0.0:8443" } else { "0.0.0.0:8080" }
            .parse()
            .unwrap(),
        filters: Default::default(),
    }
}

fn root(hostname: &str, backend: &str, tls: bool) -> ir::Frontend {
    front(hostname, ir::PathMatch::Prefix("/".into()), backend, tls)
}

#[test]
fn gateway_wildcards_match_whole_hosts_across_multiple_labels() {
    for tls in [false, true] {
        let desired = ir::Ir {
            frontends: vec![
                root("*", "fallback", tls),
                root("*.wildcard.io", "wildcard", tls),
            ],
            ..Default::default()
        };
        let requests = tr::ir_to_requests(&desired);
        let wildcard = requests
            .iter()
            .find_map(|req| match &req.request_type {
                Some(RequestType::AddHttpFrontend(f) | RequestType::AddHttpsFrontend(f))
                    if f.cluster_id.as_deref() == Some("wildcard") =>
                {
                    Some(f)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(wildcard.hostname, r"/[^.]+(?:\.[^.]+)*\.wildcard\.io/");
        assert_eq!(wildcard.position, RulePosition::Post as i32);
        let mut router = RoutingTable::default();
        router.apply(requests);
        for hostname in [
            "foo.wildcard.io",
            "foo.bar.wildcard.io",
            "multiple.prefixes.wildcard.io",
        ] {
            assert_eq!(router.backend_for(Some(hostname), "/", "GET"), "wildcard");
        }
        for hostname in [
            "wildcard.io",
            ".wildcard.io",
            "foo..wildcard.io",
            "foo.notwildcard.io",
            "foo.wildcard.io.evil.org",
        ] {
            assert_eq!(router.backend_for(Some(hostname), "/", "GET"), "fallback");
        }
    }
}

#[test]
fn wildcard_hostname_precedence_survives_incremental_changes() {
    for tls in [false, true] {
        let mut previous = ir::Ir {
            frontends: vec![root("*", "fallback", tls)],
            ..Default::default()
        };
        let mut router = RoutingTable::default();
        router.apply(tr::ir_to_requests(&previous));
        assert_eq!(router.backend("/", "GET"), "fallback");
        let mut desired = previous.clone();
        desired.frontends.push(root("*.example.com", "broad", tls));
        router.apply(tr::reconcile(&previous, &desired).unwrap());
        assert_eq!(
            router.backend_for(Some("a.b.example.com"), "/", "GET"),
            "broad"
        );
        previous = desired.clone();
        desired
            .frontends
            .push(root("*.b.example.com", "narrow", tls));
        desired.frontends.push(front(
            "*.example.com",
            ir::PathMatch::Exact("/api".into()),
            "broad-exact",
            tls,
        ));
        desired
            .frontends
            .push(root("a.b.example.com", "exact-host", tls));
        router.apply(tr::reconcile(&previous, &desired).unwrap());
        assert_eq!(
            router.backend_for(Some("a.b.example.com"), "/api", "GET"),
            "exact-host"
        );
        assert_eq!(
            router.backend_for(Some("other.b.example.com"), "/api", "GET"),
            "narrow"
        );
        assert_eq!(
            router.backend_for(Some("b.example.com"), "/api", "GET"),
            "broad-exact"
        );
        assert_eq!(
            router.backend_for(Some("b.example.com"), "/child", "GET"),
            "broad"
        );
        previous = desired.clone();
        desired.frontends[2].cluster_id = Some("narrow-updated".into());
        router.apply(tr::reconcile(&previous, &desired).unwrap());
        assert_eq!(
            router.backend_for(Some("other.b.example.com"), "/api", "POST"),
            "narrow-updated"
        );
        previous = desired.clone();
        desired.frontends.remove(2);
        router.apply(tr::reconcile(&previous, &desired).unwrap());
        assert_eq!(
            router.backend_for(Some("other.b.example.com"), "/api?q=1", "GET"),
            "broad-exact"
        );
        assert!(tr::reconcile(&desired, &desired).unwrap().is_empty());
        let cold = tr::ir_to_requests(&desired);
        desired.frontends.reverse();
        assert_eq!(tr::ir_to_requests(&desired), cold);
    }
}

#[test]
fn ingress_wildcards_keep_single_label_routing_and_a_distinct_key() {
    let mut ingress = root("*.example.com", "ingress", true);
    ingress.multi_label_wildcard = false;
    let desired = ir::Ir {
        frontends: vec![ingress, root("*.example.com", "gateway", true)],
        ..Default::default()
    };
    let requests = tr::reconcile(&ir::Ir::default(), &desired).unwrap();
    assert_eq!(
        requests.len(),
        2,
        "wildcard depths have different routing keys"
    );
    let mut router = RoutingTable::default();
    router.apply(requests);
    assert_eq!(
        router.backend_for(Some("foo.example.com"), "/", "GET"),
        "ingress"
    );
    assert_eq!(
        router.backend_for(Some("foo.bar.example.com"), "/", "GET"),
        "gateway"
    );
    let legacy = serde_json::to_value(&desired.frontends[0]).unwrap();
    assert!(legacy.get("multi_label_wildcard").is_none());
    let decoded: ir::Frontend = serde_json::from_value(legacy).unwrap();
    assert!(!decoded.multi_label_wildcard);
    let round_trip: ir::Ir =
        serde_json::from_str(&serde_json::to_string(&desired).unwrap()).unwrap();
    assert_eq!(round_trip, desired);
}

#[test]
fn adding_a_wildcard_reorders_its_post_list_without_touching_tree_or_tls() {
    let previous = ir::Ir {
        frontends: vec![
            root("*", "fallback", false),
            root("exact.example.com", "exact", false),
            root("*", "tls", true),
        ],
        ..Default::default()
    };
    let mut desired = previous.clone();
    desired
        .frontends
        .push(root("*.example.com", "wildcard", false));
    let requests = tr::reconcile(&previous, &desired).unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|req| matches!(&req.request_type,
        Some(RequestType::AddHttpFrontend(f) | RequestType::RemoveHttpFrontend(f))
        if f.position == RulePosition::Post as i32
    )));
    insta::assert_json_snapshot!(requests);
}
