use super::*;
use std::sync::Mutex;

use http_body_util::BodyExt;
use kube::client::Body;

const ANCHOR_UID: &str = "11111111-2222-3333-4444-555555555555";
const GATEWAY_A: &str = "aaaaaaaa-bbbb-cccc-dddd-111111111111";
const GATEWAY_B: &str = "aaaaaaaa-bbbb-cccc-dddd-222222222222";

fn config() -> ProvisionConfig {
    serde_json::from_value(json!({
        "namespace":"sozu-system", "template_config_map":"release-gateway-template",
        "deployment":{
            "apiVersion":"apps/v1", "kind":"Deployment", "metadata":{"name":"template"},
            "spec":{"replicas":2,"selector":{"matchLabels":{"app":"sozu","app.kubernetes.io/instance":"release"}},
                "template":{"metadata":{"labels":{"app":"sozu","app.kubernetes.io/instance":"release"}},"spec":{
                    "serviceAccountName":"worker", "automountServiceAccountToken":false,
                    "securityContext":{"runAsUser":1000}, "terminationGracePeriodSeconds":120,
                    "containers":[
                        {"name":"sozu","image":"clevercloud/sozu:2.2.1","resources":{"requests":{"cpu":"100m"}},"lifecycle":{"preStop":{"exec":{"command":["sozu","shutdown"]}}}},
                        {"name":"controller","image":"controller:test","env":[{"name":"SOZU_GW_EXPOSURE","value":"[]"},{"name":"SOZU_GW_PROVISION_TEMPLATE","value":"/template.json"},{"name":"SOZU_GW_INGRESS_ONLY","value":"true"}]}
                    ],
                    "volumes":[{"name":"sozu-config","configMap":{"name":"template-sozu"}}],
                    "topologySpreadConstraints":[{"maxSkew":1,"topologyKey":"kubernetes.io/hostname","whenUnsatisfiable":"DoNotSchedule","labelSelector":{"matchLabels":{"app":"sozu","app.kubernetes.io/instance":"release"}}}],
                    "affinity":{"podAffinity":{"requiredDuringSchedulingIgnoredDuringExecution":[{"topologyKey":"zone","labelSelector":{"matchLabels":{"other":"application"}}}]}}
                }}}
        },
        "service":{"apiVersion":"v1","kind":"Service","metadata":{"name":"template"},"spec":{"type":"LoadBalancer","selector":{"app":"sozu"},"ports":[{"name":"http","port":80,"targetPort":8080}]}},
        "config_map":{"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"template-sozu"},"data":{"config.toml":"worker_count = 2\n"}}
    })).unwrap()
}

fn gateway(uid: &str, name: &str) -> Arc<Gateway> {
    Arc::new(serde_json::from_value(json!({
        "apiVersion":"gateway.networking.k8s.io/v1", "kind":"Gateway",
        "metadata":{"namespace":"tenant", "name":name, "uid":uid, "resourceVersion":"1", "generation":1},
        "spec":{"gatewayClassName":"sozu", "listeners":[{"name":"http","port":80,"protocol":"HTTP"}]}
    })).unwrap())
}

fn class() -> Arc<GatewayClass> {
    Arc::new(serde_json::from_value(json!({
        "apiVersion":"gateway.networking.k8s.io/v1", "kind":"GatewayClass", "metadata":{"name":"sozu","uid":"class-uid"},
        "spec":{"controllerName":"sozu.io/gateway-controller"}
    })).unwrap())
}

fn gateway_path(name: &str) -> String {
    format!("/apis/gateway.networking.k8s.io/v1/namespaces/tenant/gateways/{name}")
}

#[derive(Default)]
struct Mock {
    objects: BTreeMap<String, Value>,
    requests: Vec<(String, String, Value)>,
    forbidden: BTreeSet<String>,
    replace_on_patch: Option<(String, Value)>,
    conflict_all_patches: bool,
    next_uid: usize,
}

impl Mock {
    fn mutations(&self) -> Vec<&(String, String, Value)> {
        self.requests
            .iter()
            .filter(|(method, _, _)| method != "GET")
            .collect()
    }

    fn insert_gateway(&mut self, gateway: &Gateway) {
        self.objects.insert(
            gateway_path(&gateway.name_any()),
            serde_json::to_value(gateway).unwrap(),
        );
    }
}

fn status(code: u16) -> Value {
    json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":match code {404=>"NotFound",409=>"Conflict",403=>"Forbidden",_=>"Unknown"},"message":"mock API failure","code":code})
}

fn merge(target: &mut Value, patch: &Value) {
    if let Value::Object(fields) = patch {
        if !target.is_object() {
            *target = json!({});
        }
        for (key, value) in fields {
            if value.is_null() {
                target.as_object_mut().unwrap().remove(key);
            } else {
                merge(&mut target[key], value);
            }
        }
    } else {
        *target = patch.clone();
    }
}

fn omit_null_fields(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|_, value| !value.is_null());
            for value in fields.values_mut() {
                omit_null_fields(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                omit_null_fields(value);
            }
        }
        _ => {}
    }
}

fn mock_client(gateways: &[Arc<Gateway>]) -> (Client, Arc<Mutex<Mock>>) {
    let mut initial = Mock::default();
    initial.objects.insert("/api/v1/namespaces/sozu-system/configmaps/release-gateway-template".into(), json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"release-gateway-template","namespace":"sozu-system","uid":ANCHOR_UID,"resourceVersion":"1"},"data":{"template.json":serde_json::to_string(&config()).unwrap()}}));
    initial.objects.insert(
        "/apis/gateway.networking.k8s.io/v1/gatewayclasses/sozu".into(),
        serde_json::to_value(class().as_ref()).unwrap(),
    );
    for gateway in gateways {
        initial.insert_gateway(gateway);
    }
    let state = Arc::new(Mutex::new(initial));
    let shared = state.clone();
    let service = tower::service_fn(move |request: http::Request<Body>| {
        let shared = shared.clone();
        async move {
            let method = request.method().as_str().to_string();
            let path = request.uri().path().to_string();
            let query = request.uri().query().unwrap_or_default().to_string();
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            let body: Value = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            let (code, response) = {
                let mut state = shared.lock().unwrap();
                state
                    .requests
                    .push((method.clone(), path.clone(), body.clone()));
                if state.forbidden.contains(&path) {
                    (403, status(403))
                } else {
                    match method.as_str() {
                        "GET" if query.contains("labelSelector") => {
                            if path.ends_with("/servicemonitors")
                                && !state
                                    .objects
                                    .keys()
                                    .any(|p| p.starts_with(&(path.clone() + "/")))
                            {
                                (404, status(404))
                            } else {
                                let items: Vec<_> = state
                                    .objects
                                    .iter()
                                    .filter(|(p, _)| {
                                        p.strip_prefix(&(path.clone() + "/"))
                                            .is_some_and(|s| !s.contains('/'))
                                    })
                                    .map(|(_, v)| v.clone())
                                    .collect();
                                (
                                    200,
                                    json!({"apiVersion":"v1","kind":"List","metadata":{"resourceVersion":"1"},"items":items}),
                                )
                            }
                        }
                        "GET" => state
                            .objects
                            .get(&path)
                            .cloned()
                            .map(|v| (200, v))
                            .unwrap_or((404, status(404))),
                        "POST" => {
                            let object_path =
                                format!("{}/{}", path, body["metadata"]["name"].as_str().unwrap());
                            if state.objects.contains_key(&object_path) {
                                (409, status(409))
                            } else {
                                state.next_uid += 1;
                                let mut object = body;
                                object["metadata"]["uid"] =
                                    json!(format!("resource-{}", state.next_uid));
                                object["metadata"]["resourceVersion"] = json!("1");
                                if object["kind"] == "Service" {
                                    object["spec"]["clusterIP"] =
                                        json!(format!("10.0.0.{}", state.next_uid));
                                    object["spec"]["ipFamilies"] = json!(["IPv4"]);
                                    if object["spec"]["type"] == "LoadBalancer" {
                                        object["spec"]["ports"][0]["nodePort"] =
                                            json!(30000 + state.next_uid);
                                    }
                                }
                                omit_null_fields(&mut object);
                                state.objects.insert(object_path, object.clone());
                                (201, object)
                            }
                        }
                        "PATCH" => {
                            if state
                                .replace_on_patch
                                .as_ref()
                                .is_some_and(|(p, _)| *p == path)
                            {
                                let (_, replacement) = state.replace_on_patch.take().unwrap();
                                state.objects.insert(path.clone(), replacement);
                            }
                            let reject = state.conflict_all_patches;
                            match state.objects.get_mut(&path) {
                                Some(live)
                                    if !reject
                                        && body["metadata"]["uid"] == live["metadata"]["uid"]
                                        && body["metadata"]["resourceVersion"]
                                            == live["metadata"]["resourceVersion"] =>
                                {
                                    let next = live["metadata"]["resourceVersion"]
                                        .as_str()
                                        .unwrap()
                                        .parse::<u64>()
                                        .unwrap()
                                        + 1;
                                    merge(live, &body);
                                    live["metadata"]["resourceVersion"] = json!(next.to_string());
                                    (200, live.clone())
                                }
                                Some(_) => (409, status(409)),
                                None => (404, status(404)),
                            }
                        }
                        "DELETE" => {
                            if let Some(live) = state.objects.get(&path) {
                                if body["preconditions"]["uid"] != live["metadata"]["uid"]
                                    || body["preconditions"]["resourceVersion"]
                                        != live["metadata"]["resourceVersion"]
                                {
                                    (409, status(409))
                                } else {
                                    state.objects.remove(&path);
                                    (
                                        200,
                                        json!({"apiVersion":"v1","kind":"Status","status":"Success","code":200}),
                                    )
                                }
                            } else {
                                (404, status(404))
                            }
                        }
                        _ => panic!("unexpected request {method} {path}"),
                    }
                }
            };
            Ok::<_, std::convert::Infallible>(
                http::Response::builder()
                    .status(code)
                    .body(Body::from(serde_json::to_vec(&response).unwrap()))
                    .unwrap(),
            )
        }
    });
    (Client::new(service, "sozu-system"), state)
}

async fn reconcile(provisioner: &Provisioner, gateways: &[Arc<Gateway>]) -> ProvisionOutcome {
    provisioner
        .reconcile(gateways, &[class()], "sozu.io/gateway-controller")
        .await
        .unwrap()
}

fn anchor_config(state: &Arc<Mutex<Mock>>, config: &ProvisionConfig) {
    state
        .lock()
        .unwrap()
        .objects
        .get_mut("/api/v1/namespaces/sozu-system/configmaps/release-gateway-template")
        .unwrap()["data"]["template.json"] = json!(serde_json::to_string(config).unwrap());
}

#[tokio::test]
async fn gateways_created_after_startup_receive_isolated_resources_and_settle_without_writes() {
    let a = gateway(GATEWAY_A, "a");
    let b = gateway(GATEWAY_B, "b");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let provisioner = Provisioner::new(client, config()).await.unwrap();
    let first = reconcile(&provisioner, std::slice::from_ref(&a)).await;
    assert!(first.failures.is_empty(), "{:?}", first.failures);
    state.lock().unwrap().insert_gateway(&b);
    let second = reconcile(&provisioner, &[a.clone(), b.clone()]).await;
    assert!(second.failures.is_empty(), "{:?}", second.failures);
    assert_eq!(second.instances.len(), 2);
    assert_ne!(
        second.instances[GATEWAY_A].name,
        second.instances[GATEWAY_B].name
    );
    {
        let state = state.lock().unwrap();
        assert_eq!(state.mutations().len(), 6);
        for instance in second.instances.values() {
            assert!(format!("{}-metrics", instance.name).len() <= 63);
            let deployment = &state.objects[&format!(
                "/apis/apps/v1/namespaces/sozu-system/deployments/{}",
                instance.name
            )];
            let pod = &deployment["spec"]["template"];
            assert_eq!(
                deployment["metadata"]["ownerReferences"][0]["uid"],
                ANCHOR_UID
            );
            assert_eq!(
                deployment["metadata"]["ownerReferences"][0]["blockOwnerDeletion"],
                false
            );
            assert_eq!(pod["metadata"]["labels"][GATEWAY_UID], instance.gateway_uid);
            assert!(pod["metadata"]["labels"]["app.kubernetes.io/instance"]
                .as_str()
                .unwrap()
                .contains('_'));
            assert_eq!(pod["spec"]["securityContext"]["runAsUser"], 1000);
            assert_eq!(pod["spec"]["terminationGracePeriodSeconds"], 120);
            assert_eq!(
                pod["spec"]["containers"][0]["lifecycle"]["preStop"]["exec"]["command"],
                json!(["sozu", "shutdown"])
            );
            assert_eq!(
                pod["spec"]["topologySpreadConstraints"][0]["labelSelector"],
                deployment["spec"]["selector"]
            );
            assert_eq!(
                pod["spec"]["affinity"]["podAffinity"]
                    ["requiredDuringSchedulingIgnoredDuringExecution"][0]["labelSelector"],
                json!({"matchLabels":{"other":"application"}})
            );
            let env = pod["spec"]["containers"][1]["env"].as_array().unwrap();
            assert!(env
                .iter()
                .any(|e| e["name"] == "SOZU_GW_GATEWAY_UID" && e["value"] == instance.gateway_uid));
            assert!(
                !env.iter().any(|e| e["name"] == "SOZU_GW_PROVISION_TEMPLATE"
                    || e["name"] == "SOZU_GW_INGRESS_ONLY")
            );
            let service = &state.objects
                [&format!("/api/v1/namespaces/sozu-system/services/{}", instance.name)];
            assert_eq!(
                service["spec"]["selector"],
                deployment["spec"]["selector"]["matchLabels"]
            );
        }
    }
    let mutations = state.lock().unwrap().mutations().len();
    let third = reconcile(&provisioner, &[a, b]).await;
    assert!(third.failures.is_empty());
    assert_eq!(
        state.lock().unwrap().mutations().len(),
        mutations,
        "a periodic resync must issue no writes"
    );
}

#[tokio::test]
async fn stale_empty_cache_cannot_delete_a_live_gateway() {
    let a = gateway(GATEWAY_A, "a");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let provisioner = Provisioner::new(client, config()).await.unwrap();
    reconcile(&provisioner, &[a]).await;
    let mutations = state.lock().unwrap().mutations().len();
    assert!(reconcile(&provisioner, &[]).await.failures.is_empty());
    assert_eq!(state.lock().unwrap().mutations().len(), mutations);
}

#[tokio::test]
async fn deletion_and_same_name_recreation_never_reuse_the_old_identity() {
    let old = gateway(GATEWAY_A, "same");
    let new = gateway(GATEWAY_B, "same");
    let (client, state) = mock_client(std::slice::from_ref(&old));
    let provisioner = Provisioner::new(client, config()).await.unwrap();
    let old_name = reconcile(&provisioner, std::slice::from_ref(&old))
        .await
        .instances[GATEWAY_A]
        .name
        .clone();
    state.lock().unwrap().insert_gateway(&new);
    let stale = reconcile(&provisioner, &[old]).await;
    assert!(stale.instances.is_empty());
    assert!(stale.failures.is_empty(), "{:?}", stale.failures);
    assert!(!state
        .lock()
        .unwrap()
        .objects
        .keys()
        .any(|path| path.contains(&old_name)));
    let replacement = reconcile(&provisioner, &[new]).await;
    assert!(replacement.failures.is_empty());
    assert_ne!(replacement.instances[GATEWAY_B].name, old_name);
    let state = state.lock().unwrap();
    let deletes: Vec<_> = state
        .requests
        .iter()
        .filter(|(method, _, _)| method == "DELETE")
        .collect();
    assert_eq!(deletes.len(), 3);
    assert!(deletes
        .iter()
        .all(|(_, _, body)| body["preconditions"]["uid"].is_string()
            && body["preconditions"]["resourceVersion"].is_string()));
}

#[tokio::test]
async fn lost_class_ownership_is_checked_live_before_cleanup() {
    let a = gateway(GATEWAY_A, "a");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let provisioner = Provisioner::new(client, config()).await.unwrap();
    reconcile(&provisioner, std::slice::from_ref(&a)).await;
    state
        .lock()
        .unwrap()
        .objects
        .get_mut("/apis/gateway.networking.k8s.io/v1/gatewayclasses/sozu")
        .unwrap()["spec"]["controllerName"] = json!("another.io/controller");
    let result = reconcile(&provisioner, &[a]).await;
    assert!(result.instances.is_empty());
    assert!(result.failures.is_empty(), "{:?}", result.failures);
    assert_eq!(
        state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _, _)| method == "DELETE")
            .count(),
        3
    );
}

#[tokio::test]
async fn forbidden_live_gateway_lookup_preserves_its_resources() {
    let a = gateway(GATEWAY_A, "a");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let provisioner = Provisioner::new(client, config()).await.unwrap();
    reconcile(&provisioner, &[a]).await;
    state.lock().unwrap().forbidden.insert(gateway_path("a"));
    let result = reconcile(&provisioner, &[]).await;
    assert_eq!(result.failures.len(), 3);
    assert!(!state
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|(method, _, _)| method == "DELETE"));
}

#[tokio::test]
async fn foreign_name_collision_is_neither_adopted_nor_pruned() {
    let a = gateway(GATEWAY_A, "a");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let provisioner = Provisioner::new(client, config()).await.unwrap();
    let name = provisioner.instance(&a).unwrap().name;
    state.lock().unwrap().objects.insert(format!("/api/v1/namespaces/sozu-system/configmaps/{name}-sozu"),json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":format!("{name}-sozu"),"namespace":"sozu-system","uid":"foreign","resourceVersion":"1"},"data":{"keep":"me"}}));
    let result = reconcile(&provisioner, &[a]).await;
    assert!(result.failures[GATEWAY_A].contains("refusing to adopt"));
    assert!(state.lock().unwrap().mutations().is_empty());
}

#[tokio::test]
async fn updates_preserve_allocations_remove_old_template_keys_and_converge() {
    let a = gateway(GATEWAY_A, "a");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let mut original = config();
    original
        .config_map
        .data
        .as_mut()
        .unwrap()
        .insert("old".into(), "remove".into());
    anchor_config(&state, &original);
    let provisioner = Provisioner::new(client.clone(), original).await.unwrap();
    let name = reconcile(&provisioner, std::slice::from_ref(&a))
        .await
        .instances[GATEWAY_A]
        .name
        .clone();
    let service_path = format!("/api/v1/namespaces/sozu-system/services/{name}");
    let ip = state.lock().unwrap().objects[&service_path]["spec"]["clusterIP"].clone();
    let node_port =
        state.lock().unwrap().objects[&service_path]["spec"]["ports"][0]["nodePort"].clone();
    let mut update = config();
    update
        .service
        .spec
        .as_mut()
        .unwrap()
        .ports
        .as_mut()
        .unwrap()[0]
        .target_port = Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(8181));
    anchor_config(&state, &update);
    let provisioner = Provisioner::new(client, update).await.unwrap();
    let result = reconcile(&provisioner, std::slice::from_ref(&a)).await;
    assert!(result.failures.is_empty(), "{:?}", result.failures);
    {
        let state = state.lock().unwrap();
        assert_eq!(state.objects[&service_path]["spec"]["clusterIP"], ip);
        assert_eq!(
            state.objects[&service_path]["spec"]["ports"][0]["nodePort"],
            node_port
        );
        assert_eq!(
            state.objects[&service_path]["spec"]["ports"][0]["targetPort"],
            8181
        );
        assert!(
            state.objects[&format!("/api/v1/namespaces/sozu-system/configmaps/{name}-sozu")]
                ["data"]
                .get("old")
                .is_none()
        );
    }
    let count = state.lock().unwrap().mutations().len();
    assert!(reconcile(&provisioner, &[a]).await.failures.is_empty());
    assert_eq!(state.lock().unwrap().mutations().len(), count);
}

#[tokio::test]
async fn replaced_object_during_patch_is_not_adopted_on_conflict_retry() {
    let a = gateway(GATEWAY_A, "a");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let provisioner = Provisioner::new(client.clone(), config()).await.unwrap();
    let name = reconcile(&provisioner, std::slice::from_ref(&a))
        .await
        .instances[GATEWAY_A]
        .name
        .clone();
    let path = format!("/api/v1/namespaces/sozu-system/configmaps/{name}-sozu");
    let replacement = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":format!("{name}-sozu"),"namespace":"sozu-system","uid":"replacement","resourceVersion":"2"},"data":{"keep":"replacement"}});
    state.lock().unwrap().replace_on_patch = Some((path.clone(), replacement.clone()));
    let mut changed = config();
    changed
        .config_map
        .data
        .as_mut()
        .unwrap()
        .insert("new".into(), "value".into());
    anchor_config(&state, &changed);
    let provisioner = Provisioner::new(client, changed).await.unwrap();
    assert!(reconcile(&provisioner, &[a]).await.failures[GATEWAY_A].contains("refusing to adopt"));
    assert_eq!(state.lock().unwrap().objects[&path], replacement);
}

#[tokio::test]
async fn retries_are_bounded_and_other_gateways_can_still_converge() {
    let a = gateway(GATEWAY_A, "a");
    let b = gateway(GATEWAY_B, "b");
    let (client, state) = mock_client(&[a.clone(), b.clone()]);
    let provisioner = Provisioner::new(client.clone(), config()).await.unwrap();
    reconcile(&provisioner, std::slice::from_ref(&a)).await;
    state.lock().unwrap().conflict_all_patches = true;
    let mut changed = config();
    changed
        .config_map
        .data
        .as_mut()
        .unwrap()
        .insert("new".into(), "value".into());
    anchor_config(&state, &changed);
    let provisioner = Provisioner::new(client, changed).await.unwrap();
    let result = reconcile(&provisioner, &[a, b]).await;
    assert!(result.failures.contains_key(GATEWAY_A));
    assert!(result.instances.contains_key(GATEWAY_B));
    assert_eq!(
        state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(method, _, _)| method == "PATCH")
            .count(),
        RETRIES
    );
}

#[tokio::test]
async fn replacing_installation_anchor_stops_old_provisioner() {
    let a = gateway(GATEWAY_A, "a");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let provisioner = Provisioner::new(client, config()).await.unwrap();
    state
        .lock()
        .unwrap()
        .objects
        .get_mut("/api/v1/namespaces/sozu-system/configmaps/release-gateway-template")
        .unwrap()["metadata"]["uid"] = json!("new-installation");
    assert!(provisioner
        .reconcile(&[a], &[class()], "sozu.io/gateway-controller")
        .await
        .is_err());
    assert!(state.lock().unwrap().mutations().is_empty());
}

#[tokio::test]
async fn old_process_cannot_overwrite_new_installation_template() {
    let a = gateway(GATEWAY_A, "a");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let provisioner = Provisioner::new(client, config()).await.unwrap();
    reconcile(&provisioner, std::slice::from_ref(&a)).await;
    let before = state.lock().unwrap().mutations().len();
    let mut current = config();
    current.deployment.spec.as_mut().unwrap().replicas = Some(3);
    anchor_config(&state, &current);
    let error = provisioner
        .reconcile(&[a], &[class()], "sozu.io/gateway-controller")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("template changed"));
    assert_eq!(state.lock().unwrap().mutations().len(), before);
}

#[test]
fn live_objects_and_allocated_service_fields_are_rejected_as_templates() {
    let mut template = config();
    template.deployment.metadata.uid = Some("live-uid".into());
    assert!(template
        .validate()
        .unwrap_err()
        .to_string()
        .contains("metadata uid"));
    let mut template = config();
    template.service.spec.as_mut().unwrap().cluster_ip = Some("10.0.0.1".into());
    assert!(template
        .validate()
        .unwrap_err()
        .to_string()
        .contains("server-allocated"));
}

#[tokio::test]
async fn optional_metrics_and_disruption_resources_follow_only_their_instance() {
    let a = gateway(GATEWAY_A, "a");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let mut enabled = config();
    let mut metrics = enabled.service.clone();
    metrics.spec.as_mut().unwrap().type_ = Some("ClusterIP".into());
    enabled.metrics_service = Some(metrics);
    enabled.pod_disruption_budget = Some(
        serde_json::from_value(json!({
            "apiVersion":"policy/v1", "kind":"PodDisruptionBudget", "metadata":{"name":"template"},
            "spec":{"minAvailable":1,"selector":{"matchLabels":{"app":"sozu"}}}
        }))
        .unwrap(),
    );
    enabled.service_monitor = Some(json!({
        "apiVersion":"monitoring.coreos.com/v1", "kind":"ServiceMonitor", "metadata":{"name":"template","labels":{"monitoring":"enabled"}},
        "spec":{"selector":{"matchLabels":{"app":"sozu"}},"endpoints":[{"port":"metrics"}]}
    }));
    anchor_config(&state, &enabled);
    let provisioner = Provisioner::new(client.clone(), enabled).await.unwrap();
    let result = reconcile(&provisioner, std::slice::from_ref(&a)).await;
    assert!(result.failures.is_empty(), "{:?}", result.failures);
    let name = result.instances[GATEWAY_A].name.clone();
    {
        let state = state.lock().unwrap();
        assert_eq!(state.mutations().len(), 6);
        let monitor = &state.objects[&format!(
            "/apis/monitoring.coreos.com/v1/namespaces/sozu-system/servicemonitors/{name}"
        )];
        let metrics =
            &state.objects[&format!("/api/v1/namespaces/sozu-system/services/{name}-metrics")];
        assert_eq!(
            monitor["spec"]["selector"]["matchLabels"][GATEWAY_UID],
            GATEWAY_A
        );
        assert_eq!(
            monitor["spec"]["selector"]["matchLabels"]["sozu.io/metrics-service"],
            "true"
        );
        assert_eq!(
            metrics["metadata"]["labels"]["sozu.io/metrics-service"],
            "true"
        );
        assert_eq!(monitor["metadata"]["labels"]["monitoring"], "enabled");
    }
    assert!(reconcile(&provisioner, std::slice::from_ref(&a))
        .await
        .failures
        .is_empty());
    assert_eq!(state.lock().unwrap().mutations().len(), 6);
    let disabled = config();
    anchor_config(&state, &disabled);
    let provisioner = Provisioner::new(client, disabled).await.unwrap();
    assert!(reconcile(&provisioner, &[a]).await.failures.is_empty());
    let state = state.lock().unwrap();
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|(method, _, _)| method == "DELETE")
            .count(),
        3
    );
    assert!(state.objects.contains_key(&format!(
        "/apis/apps/v1/namespaces/sozu-system/deployments/{name}"
    )));
    assert!(state
        .objects
        .contains_key(&format!("/api/v1/namespaces/sozu-system/services/{name}")));
}

#[tokio::test(start_paused = true)]
async fn blackholed_api_request_has_a_finite_deadline() {
    let started = tokio::time::Instant::now();
    let error = request::<()>(std::future::pending()).await.unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert_eq!(started.elapsed(), REQUEST_TIMEOUT);
}

#[tokio::test]
async fn absent_scheduling_fields_remain_absent_and_do_not_trigger_rollouts() {
    let a = gateway(GATEWAY_A, "a");
    let (client, state) = mock_client(std::slice::from_ref(&a));
    let mut template = config();
    let pod = template
        .deployment
        .spec
        .as_mut()
        .unwrap()
        .template
        .spec
        .as_mut()
        .unwrap();
    pod.affinity = None;
    pod.topology_spread_constraints = None;
    anchor_config(&state, &template);
    let provisioner = Provisioner::new(client, template).await.unwrap();
    let result = reconcile(&provisioner, std::slice::from_ref(&a)).await;
    assert!(result.failures.is_empty(), "{:?}", result.failures);
    let name = &result.instances[GATEWAY_A].name;
    {
        let state = state.lock().unwrap();
        let spec = &state.objects
            [&format!("/apis/apps/v1/namespaces/sozu-system/deployments/{name}")]["spec"]
            ["template"]["spec"];
        assert!(spec.get("affinity").is_none());
        assert!(spec.get("topologySpreadConstraints").is_none());
    }
    assert!(reconcile(&provisioner, &[a]).await.failures.is_empty());
    assert_eq!(state.lock().unwrap().mutations().len(), 3);
}
