//! Kubernetes API enums are open: a newer CRD may carry members generated
//! before they existed. A closed Rust enum makes the *whole list page* fail to
//! deserialize, which hides every object of that kind from the controller — the
//! reflector store never syncs and the process exits. These tests pin the
//! catch-all that `scripts/add-unknown-variants.py` adds after generation.

use sozu_gw_gateway_api::httproute::HttpRouteRulesFiltersType;
use sozu_gw_gateway_api::HttpRoute;

fn route_with_filter(kind: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
        "metadata": { "name": "r", "namespace": "demo" },
        "spec": { "rules": [{ "filters": [{ "type": kind }] }] }
    })
}

#[test]
fn a_filter_type_from_a_newer_crd_deserialises_as_unknown() {
    // `CORS` is a real HTTPRoute filter type in Gateway API v1.6.1, and these
    // types are generated from v1.2.1.
    let route: HttpRoute = serde_json::from_value(route_with_filter("CORS")).expect("must parse");
    let rules = route.spec.rules.expect("one rule");
    let filters = rules[0].filters.as_ref().expect("one filter");
    assert!(matches!(
        filters[0].r#type,
        HttpRouteRulesFiltersType::Unknown
    ));
}

#[test]
fn one_unknown_member_does_not_hide_the_whole_page() {
    // This is the failure that matters: the reflector decodes a list, not one
    // object, so a single unparseable item used to take every route with it.
    let page = serde_json::json!({
        "apiVersion": "v1", "kind": "List",
        "items": [
            { "apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute",
              "metadata": { "name": "healthy", "namespace": "demo" },
              "spec": { "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }] } },
            route_with_filter("CORS"),
        ]
    });
    let list: kube::core::ObjectList<HttpRoute> =
        serde_json::from_value(page).expect("the page must still decode");
    assert_eq!(
        list.items.len(),
        2,
        "the healthy route survives its neighbour"
    );
}

#[test]
fn known_members_are_unaffected() {
    let route: HttpRoute =
        serde_json::from_value(route_with_filter("RequestRedirect")).expect("must parse");
    let rules = route.spec.rules.expect("one rule");
    let filters = rules[0].filters.as_ref().expect("one filter");
    assert!(matches!(
        filters[0].r#type,
        HttpRouteRulesFiltersType::RequestRedirect
    ));
}
