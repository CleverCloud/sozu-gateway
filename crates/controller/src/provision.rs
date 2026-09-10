//! Provision dedicated data-plane workloads for Gateways owned by this controller.
//!
//! Resource templates are operator supplied; Gateway objects never supply Pod
//! specifications or privilege settings. The installation ConfigMap owns every
//! generated resource because a Gateway in another namespace cannot do so.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, Service};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client, ResourceExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sozu_gw_gateway_api::{Gateway, GatewayClass};

const INSTALLATION: &str = "sozu.io/installation-uid";
const GATEWAY_UID: &str = "sozu.io/gateway-uid";
const GATEWAY_NAMESPACE: &str = "sozu.io/gateway-namespace";
const GATEWAY_NAME: &str = "sozu.io/gateway-name";
const LAST_TEMPLATE: &str = "sozu.io/provision-template";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const RETRIES: usize = 3;

/// Rendered by Helm and mounted read-only into the provisioner. These are
/// templates, never objects copied from the API server.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionConfig {
    pub namespace: String,
    pub template_config_map: String,
    pub deployment: Deployment,
    pub service: Service,
    pub config_map: ConfigMap,
    #[serde(default)]
    pub pod_disruption_budget: Option<PodDisruptionBudget>,
    #[serde(default)]
    pub metrics_service: Option<Service>,
    #[serde(default)]
    pub service_monitor: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    pub name: String,
    pub service_name: String,
    pub gateway_namespace: String,
    pub gateway_name: String,
    pub gateway_uid: String,
}

#[derive(Debug, Default)]
pub struct ProvisionOutcome {
    /// Successfully reconciled instances, keyed by the Gateway's immutable UID.
    pub instances: BTreeMap<String, Instance>,
    /// One failed Gateway must not stop provisioning its neighbours.
    pub failures: BTreeMap<String, String>,
}

pub struct Provisioner {
    client: Client,
    config: ProvisionConfig,
    installation_uid: String,
    confirmed: Mutex<BTreeMap<(Kind, String), ConfirmedWrite>>,
}

/// A successful write establishes the API server's accepted representation of
/// this exact template. Quantities and other fields may have been canonicalised
/// in the response; their original spelling must not cause endless no-op writes.
struct ConfirmedWrite {
    uid: String,
    resource_version: String,
    gateway_uid: String,
    template: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    ConfigMap,
    Service,
    Deployment,
    PodDisruptionBudget,
    ServiceMonitor,
}

impl Kind {
    const ALL: [Self; 5] = [
        Self::Deployment,
        Self::Service,
        Self::PodDisruptionBudget,
        Self::ServiceMonitor,
        Self::ConfigMap,
    ];

    fn resource(self) -> ApiResource {
        let (group, version, kind, plural) = match self {
            Self::ConfigMap => ("", "v1", "ConfigMap", "configmaps"),
            Self::Service => ("", "v1", "Service", "services"),
            Self::Deployment => ("apps", "v1", "Deployment", "deployments"),
            Self::PodDisruptionBudget => (
                "policy",
                "v1",
                "PodDisruptionBudget",
                "poddisruptionbudgets",
            ),
            Self::ServiceMonitor => (
                "monitoring.coreos.com",
                "v1",
                "ServiceMonitor",
                "servicemonitors",
            ),
        };
        ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, kind), plural)
    }
}

impl Provisioner {
    pub async fn new(client: Client, config: ProvisionConfig) -> Result<Self> {
        config.validate()?;
        let anchor = request(
            Api::<ConfigMap>::namespaced(client.clone(), &config.namespace)
                .get(&config.template_config_map),
        )
        .await?;
        if anchor.metadata.deletion_timestamp.is_some() {
            bail!("provisioning template ConfigMap is being deleted");
        }
        let installation_uid = anchor
            .uid()
            .context("provisioning template ConfigMap has no UID")?;
        validate_label(&installation_uid, "installation UID")?;
        let provisioner = Self {
            client,
            config,
            installation_uid,
            confirmed: Mutex::default(),
        };
        provisioner.verify_anchor(&anchor)?;
        Ok(provisioner)
    }

    pub async fn reconcile(
        &self,
        gateways: &[Arc<Gateway>],
        classes: &[Arc<GatewayClass>],
        controller_name: &str,
    ) -> Result<ProvisionOutcome> {
        // A delayed old process must not revive resources after Helm removes or
        // recreates its installation anchor under the same name.
        self.check_installation().await?;

        let ours: BTreeSet<_> = classes
            .iter()
            .filter(|c| {
                c.spec.controller_name == controller_name && c.metadata.deletion_timestamp.is_none()
            })
            .map(|c| c.name_any())
            .collect();
        let mut outcome = ProvisionOutcome::default();
        let mut desired = BTreeSet::new();
        for gateway in gateways {
            if gateway.metadata.deletion_timestamp.is_some()
                || !ours.contains(&gateway.spec.gateway_class_name)
            {
                continue;
            }
            let Some(uid) = gateway.uid() else {
                outcome
                    .failures
                    .insert(gateway.name_any(), "Gateway has no UID".into());
                continue;
            };
            // Preserve an existing instance even when rebuilding its template
            // fails. Cleanup only follows independently verified ownership loss.
            desired.insert(uid.clone());
            match self.ensure_gateway(gateway, controller_name).await {
                Ok(Some(instance)) => {
                    outcome.instances.insert(uid, instance);
                }
                Ok(None) => {
                    desired.remove(&uid);
                }
                Err(error) => {
                    outcome.failures.insert(uid, format!("{error:#}"));
                }
            }
        }

        for kind in Kind::ALL {
            let objects = match request(self.api(kind).list(
                &ListParams::default().labels(&format!("{INSTALLATION}={}", self.installation_uid)),
            ))
            .await
            {
                Ok(objects) => objects,
                // ServiceMonitor is an optional CRD. A forbidden request is
                // never interpreted as absence.
                Err(error)
                    if kind == Kind::ServiceMonitor
                        && self.config.service_monitor.is_none()
                        && api_code(&error) == Some(404) =>
                {
                    continue
                }
                Err(error) => {
                    outcome
                        .failures
                        .insert(format!("cleanup/{kind:?}"), format!("{error:#}"));
                    continue;
                }
            };
            for object in objects {
                let uid = object
                    .annotations()
                    .get(GATEWAY_UID)
                    .cloned()
                    .unwrap_or_default();
                if !self.owned(&object, &uid) {
                    continue;
                }
                let obsolete_optional = match kind {
                    Kind::PodDisruptionBudget => self.config.pod_disruption_budget.is_none(),
                    Kind::ServiceMonitor => self.config.service_monitor.is_none(),
                    Kind::Service => {
                        self.config.metrics_service.is_none()
                            && object.name_any().ends_with("-metrics")
                    }
                    _ => false,
                };
                if desired.contains(&uid) && !obsolete_optional {
                    continue;
                }
                if let Err(error) = self
                    .prune(kind, &object, controller_name, obsolete_optional)
                    .await
                {
                    outcome.failures.insert(
                        format!("cleanup/{:?}/{}", kind, object.name_any()),
                        format!("{error:#}"),
                    );
                }
            }
        }
        self.confirmed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, entry| desired.contains(&entry.gateway_uid));
        Ok(outcome)
    }

    fn api(&self, kind: Kind) -> Api<DynamicObject> {
        Api::namespaced_with(
            self.client.clone(),
            &self.config.namespace,
            &kind.resource(),
        )
    }

    fn verify_anchor(&self, anchor: &ConfigMap) -> Result<()> {
        if anchor.uid().as_deref() != Some(&self.installation_uid)
            || anchor.metadata.deletion_timestamp.is_some()
        {
            bail!("provisioning installation identity changed or is being deleted");
        }
        let encoded = anchor
            .data
            .as_ref()
            .and_then(|data| data.get("template.json"))
            .context("provisioning anchor has no template.json")?;
        let current: ProvisionConfig = serde_json::from_str(encoded)
            .context("parse current provisioning anchor template.json")?;
        if serde_json::to_value(current)? != serde_json::to_value(&self.config)? {
            bail!("provisioning template changed; this process must restart with the current template");
        }
        Ok(())
    }

    async fn check_installation(&self) -> Result<()> {
        let anchor = request(
            Api::<ConfigMap>::namespaced(self.client.clone(), &self.config.namespace)
                .get(&self.config.template_config_map),
        )
        .await?;
        self.verify_anchor(&anchor)
    }

    async fn live_gateway(&self, namespace: &str, name: &str) -> Result<Option<Gateway>> {
        match request(Api::<Gateway>::namespaced(self.client.clone(), namespace).get(name)).await {
            Ok(gateway) => Ok(Some(gateway)),
            Err(error) if api_code(&error) == Some(404) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn live_owned(&self, gateway: &Gateway, controller_name: &str) -> Result<bool> {
        if gateway.metadata.deletion_timestamp.is_some() {
            return Ok(false);
        }
        match request(
            Api::<GatewayClass>::all(self.client.clone()).get(&gateway.spec.gateway_class_name),
        )
        .await
        {
            Ok(class) => Ok(class.metadata.deletion_timestamp.is_none()
                && class.spec.controller_name == controller_name),
            Err(error) if api_code(&error) == Some(404) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn ensure_gateway(
        &self,
        cached: &Gateway,
        controller_name: &str,
    ) -> Result<Option<Instance>> {
        let namespace = cached.namespace().context("Gateway has no namespace")?;
        let name = cached.name_any();
        let Some(gateway) = self.live_gateway(&namespace, &name).await? else {
            return Ok(None);
        };
        if gateway.uid() != cached.uid() || !self.live_owned(&gateway, controller_name).await? {
            return Ok(None);
        }
        let instance = self.instance(&gateway)?;
        for (kind, resource) in self.resources(&instance)? {
            self.ensure(kind, resource, &instance.gateway_uid)
                .await
                .with_context(|| format!("provision {kind:?} for Gateway {namespace}/{name}"))?;
        }
        Ok(Some(instance))
    }

    fn instance(&self, gateway: &Gateway) -> Result<Instance> {
        let uid = gateway.uid().context("Gateway has no UID")?;
        validate_label(&uid, "Gateway UID")?;
        // The UUID spelling normally fits directly. Hashing unusually long UIDs
        // preserves bounded names; the full UID is still checked before writes.
        let suffix = if uid.len() <= 36
            && uid
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            uid.clone()
        } else {
            format!("{:016x}", stable_hash(uid.as_bytes()))
        };
        let installation = format!("{:016x}", stable_hash(self.installation_uid.as_bytes()));
        let name = format!("sozu-gw-{}-{suffix}", &installation[..8]);
        Ok(Instance {
            service_name: name.clone(),
            name,
            gateway_namespace: gateway.namespace().context("Gateway has no namespace")?,
            gateway_name: gateway.name_any(),
            gateway_uid: uid,
        })
    }

    fn resources(&self, instance: &Instance) -> Result<Vec<(Kind, Value)>> {
        let selector =
            json!({ INSTALLATION: self.installation_uid, GATEWAY_UID: instance.gateway_uid });
        let mut config_map = serde_json::to_value(&self.config.config_map)?;
        self.identity(
            &mut config_map,
            &format!("{}-sozu", instance.name),
            instance,
        );
        let mut deployment = serde_json::to_value(&self.config.deployment)?;
        self.identity(&mut deployment, &instance.name, instance);
        let old_selector = deployment["spec"]["selector"].clone();
        deployment["spec"]["selector"] = json!({"matchLabels": selector});
        let pod = deployment["spec"]["template"]
            .as_object_mut()
            .context("Deployment has no Pod template")?;
        let metadata = pod.entry("metadata").or_insert_with(|| json!({}));
        self.pod_labels(metadata, instance);
        metadata["annotations"]["checksum/config"] = Value::String(format!(
            "{:016x}",
            stable_hash(serde_json::to_string(&config_map)?.as_bytes())
        ));
        let spec = pod.get_mut("spec").context("Deployment has no Pod spec")?;
        let containers = spec["containers"]
            .as_array_mut()
            .context("Deployment has no containers")?;
        let worker = containers
            .iter_mut()
            .find(|c| c["name"] == "controller")
            .context("Deployment requires a controller container")?;
        let env = worker
            .as_object_mut()
            .context("worker must be an object")?
            .entry("env")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("worker env must be an array")?;
        for (key, value) in [
            (
                "SOZU_GW_GATEWAY_SCOPE",
                format!("{}/{}", instance.gateway_namespace, instance.gateway_name),
            ),
            ("SOZU_GW_GATEWAY_UID", instance.gateway_uid.clone()),
            (
                "SOZU_GW_PUBLISH_SERVICE",
                format!("{}/{}", self.config.namespace, instance.service_name),
            ),
            ("SOZU_GW_INGRESS_STATUS_WRITES", "false".into()),
        ] {
            env.retain(|e| e["name"] != key);
            env.push(json!({"name": key, "value": value}));
        }
        env.retain(|e| {
            !matches!(
                e["name"].as_str(),
                Some(
                    "SOZU_GW_PROVISION_TEMPLATE"
                        | "SOZU_GW_INGRESS_ONLY"
                        | "SOZU_GW_EXCLUDE_GATEWAY"
                )
            )
        });
        let volumes = spec["volumes"]
            .as_array_mut()
            .context("Deployment requires volumes")?;
        let volume = volumes
            .iter_mut()
            .find(|v| v["name"] == "sozu-config")
            .context("Deployment requires sozu-config volume")?;
        volume["configMap"]["name"] = Value::String(format!("{}-sozu", instance.name));
        // Topology and anti-affinity selectors inherited from the default
        // Deployment must follow this instance, otherwise every Gateway competes
        // against the default Pods or ignores its own replicas.
        rewrite_pod_selectors(spec, &old_selector, &selector);
        let mut service = serde_json::to_value(&self.config.service)?;
        self.identity(&mut service, &instance.service_name, instance);
        service["spec"]["selector"] = selector.clone();
        let mut resources = vec![(Kind::ConfigMap, config_map), (Kind::Service, service)];
        if let Some(template) = &self.config.metrics_service {
            let mut metrics = serde_json::to_value(template)?;
            self.identity(
                &mut metrics,
                &format!("{}-metrics", instance.name),
                instance,
            );
            metrics["spec"]["selector"] = selector.clone();
            resources.push((Kind::Service, metrics));
        }
        if let Some(template) = &self.config.pod_disruption_budget {
            let mut budget = serde_json::to_value(template)?;
            self.identity(&mut budget, &instance.name, instance);
            budget["spec"]["selector"] = json!({"matchLabels": selector});
            resources.push((Kind::PodDisruptionBudget, budget));
        }
        resources.push((Kind::Deployment, deployment));
        if let Some(template) = &self.config.service_monitor {
            let mut monitor = template.clone();
            self.identity(&mut monitor, &instance.name, instance);
            monitor["spec"]["selector"] = json!({"matchLabels": { INSTALLATION: self.installation_uid, GATEWAY_UID: instance.gateway_uid, "sozu.io/metrics-service": "true" }});
            monitor["spec"]["namespaceSelector"] = json!({"matchNames": [self.config.namespace]});
            resources.push((Kind::ServiceMonitor, monitor));
        }
        Ok(resources)
    }

    fn pod_labels(&self, metadata: &mut Value, instance: &Instance) {
        if !metadata["labels"].is_object() {
            metadata["labels"] = json!({});
        }
        metadata["labels"][INSTALLATION] = Value::String(self.installation_uid.clone());
        metadata["labels"][GATEWAY_UID] = Value::String(instance.gateway_uid.clone());
        // Existing default Services select their release label. Never carry
        // that same value into automatically provisioned Pods.
        metadata["labels"]["app.kubernetes.io/instance"] =
            Value::String(format!("{}_gw", instance.name));
        metadata["labels"]["app.kubernetes.io/managed-by"] = json!("sozu-gateway");
        metadata["labels"]["sozu.io/gateway-instance"] = Value::String(instance.name.clone());
        if !metadata["annotations"].is_object() {
            metadata["annotations"] = json!({});
        }
    }

    fn identity(&self, resource: &mut Value, name: &str, instance: &Instance) {
        let metadata = &mut resource["metadata"];
        metadata["name"] = Value::String(name.into());
        metadata["namespace"] = Value::String(self.config.namespace.clone());
        self.pod_labels(metadata, instance);
        metadata["annotations"][GATEWAY_UID] = Value::String(instance.gateway_uid.clone());
        metadata["annotations"][GATEWAY_NAMESPACE] =
            Value::String(instance.gateway_namespace.clone());
        metadata["annotations"][GATEWAY_NAME] = Value::String(instance.gateway_name.clone());
        metadata["ownerReferences"] = json!([{
            "apiVersion": "v1", "kind": "ConfigMap", "name": self.config.template_config_map,
            "uid": self.installation_uid, "controller": true, "blockOwnerDeletion": false
        }]);
        if name.ends_with("-metrics") {
            metadata["labels"]["sozu.io/metrics-service"] = json!("true");
        }
    }

    fn owned(&self, resource: &DynamicObject, gateway_uid: &str) -> bool {
        !gateway_uid.is_empty()
            && resource.namespace().as_deref() == Some(&self.config.namespace)
            && resource.labels().get(INSTALLATION) == Some(&self.installation_uid)
            && resource.labels().get(GATEWAY_UID).map(String::as_str) == Some(gateway_uid)
            && resource.annotations().get(GATEWAY_UID).map(String::as_str) == Some(gateway_uid)
            && resource
                .metadata
                .owner_references
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|o| {
                    o.api_version == "v1"
                        && o.kind == "ConfigMap"
                        && o.name == self.config.template_config_map
                        && o.uid == self.installation_uid
                        && o.controller == Some(true)
                })
    }

    async fn ensure(&self, kind: Kind, template: Value, gateway_uid: &str) -> Result<()> {
        let api = self.api(kind);
        let name = template["metadata"]["name"]
            .as_str()
            .context("template name missing")?;
        let cache_key = (kind, name.to_owned());
        for attempt in 0..RETRIES {
            let live = match request(api.get(name)).await {
                Ok(live) => Some(live),
                Err(error) if api_code(&error) == Some(404) => {
                    self.confirmed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&cache_key);
                    None
                }
                Err(error) => return Err(error),
            };
            let mut desired = template.clone();
            let encoded = serde_json::to_string(&desired)?;
            if let Some(live) = &live {
                if !self.owned(live, gateway_uid) {
                    bail!("refusing to adopt foreign {kind:?} {name}");
                }
                if live.metadata.deletion_timestamp.is_some() {
                    bail!("{kind:?} {name} is still being deleted");
                }
                let confirmed = self
                    .confirmed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&cache_key)
                    .is_some_and(|entry| {
                        live.metadata.uid.as_deref() == Some(&entry.uid)
                            && live.metadata.resource_version.as_deref()
                                == Some(&entry.resource_version)
                            && entry.gateway_uid == gateway_uid
                            && entry.template == encoded
                    });
                if confirmed {
                    return Ok(());
                }
                if kind == Kind::Service {
                    preserve_service_allocations(&mut desired, &serde_json::to_value(live)?);
                }
            }
            if encoded.len() > 128 * 1024 {
                bail!("provisioning template exceeds the 128 KiB annotation budget");
            }
            desired["metadata"]["annotations"][LAST_TEMPLATE] = Value::String(encoded.clone());
            let result = if let Some(live) = &live {
                let live_value = serde_json::to_value(live)?;
                if subset(&desired, &live_value) {
                    return Ok(());
                }
                let previous = live
                    .annotations()
                    .get(LAST_TEMPLATE)
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_else(|| json!({}));
                let mut patch = merge_patch(&previous, &desired);
                patch["metadata"]["uid"] = json!(live.uid().context("live object has no UID")?);
                patch["metadata"]["resourceVersion"] = json!(live
                    .resource_version()
                    .context("live object has no resourceVersion")?);
                self.check_installation().await?;
                request(api.patch(name, &PatchParams::default(), &Patch::Merge(patch))).await
            } else {
                let object: DynamicObject = serde_json::from_value(desired)?;
                self.check_installation().await?;
                request(api.create(&PostParams::default(), &object)).await
            };
            match result {
                Ok(written) => {
                    // Only the server's write response can confirm a version.
                    // Never mark a rejected write or an arbitrary GET as applied.
                    let entry = ConfirmedWrite {
                        uid: written.uid().context("written object has no UID")?,
                        resource_version: written
                            .resource_version()
                            .context("written object has no resourceVersion")?,
                        gateway_uid: gateway_uid.to_owned(),
                        template: encoded,
                    };
                    self.confirmed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(cache_key.clone(), entry);
                    return Ok(());
                }
                Err(error) if api_code(&error) == Some(409) && attempt + 1 < RETRIES => {}
                Err(error) => return Err(error),
            }
        }
        bail!("{kind:?} {name} changed during every provisioning attempt")
    }

    async fn prune(
        &self,
        kind: Kind,
        object: &DynamicObject,
        controller_name: &str,
        obsolete_optional: bool,
    ) -> Result<()> {
        let annotations = object.annotations();
        let namespace = annotations
            .get(GATEWAY_NAMESPACE)
            .context("owned object has no Gateway namespace")?;
        let name = annotations
            .get(GATEWAY_NAME)
            .context("owned object has no Gateway name")?;
        let gateway_uid = annotations
            .get(GATEWAY_UID)
            .context("owned object has no Gateway UID")?;
        // Never infer deletion from a stale or not-yet-relisted cache. A live
        // read also protects a newly recreated Gateway with the same name.
        if let Some(gateway) = self.live_gateway(namespace, name).await? {
            if gateway.uid().as_deref() == Some(gateway_uid)
                && self.live_owned(&gateway, controller_name).await?
                && !obsolete_optional
            {
                return Ok(());
            }
        }
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(object.uid().context("owned object has no UID")?),
                resource_version: object.resource_version(),
            }),
            ..Default::default()
        };
        self.check_installation().await?;
        match request(self.api(kind).delete(&object.name_any(), &params)).await {
            Ok(_) => {
                self.confirmed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&(kind, object.name_any()));
                Ok(())
            }
            Err(error) if api_code(&error) == Some(404) => {
                self.confirmed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&(kind, object.name_any()));
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

impl ProvisionConfig {
    fn validate(&self) -> Result<()> {
        validate_label(&self.namespace, "provisioning namespace")?;
        if self.template_config_map.is_empty() {
            bail!("template_config_map must not be empty");
        }
        for resource in [
            Some(serde_json::to_value(&self.deployment)?),
            Some(serde_json::to_value(&self.service)?),
            Some(serde_json::to_value(&self.config_map)?),
            self.pod_disruption_budget
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?,
            self.metrics_service
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?,
            self.service_monitor.clone(),
        ]
        .into_iter()
        .flatten()
        {
            for key in [
                "uid",
                "resourceVersion",
                "creationTimestamp",
                "deletionTimestamp",
                "managedFields",
                "generation",
                "ownerReferences",
                "finalizers",
                "generateName",
            ] {
                if resource["metadata"].get(key).is_some_and(|v| !v.is_null()) {
                    bail!("resource template contains server or ownership metadata {key}");
                }
            }
            if resource.get("status").is_some_and(|s| !s.is_null()) {
                bail!("resource template must not contain status");
            }
            if resource["metadata"]["annotations"]
                .get(LAST_TEMPLATE)
                .is_some()
            {
                bail!("resource template contains reserved provisioning annotation");
            }
        }
        if let Some(monitor) = &self.service_monitor {
            if monitor["apiVersion"] != "monitoring.coreos.com/v1"
                || monitor["kind"] != "ServiceMonitor"
                || !monitor["spec"].is_object()
            {
                bail!("service_monitor must be a monitoring.coreos.com/v1 ServiceMonitor");
            }
            if self.metrics_service.is_none() {
                bail!("service_monitor requires metrics_service");
            }
        }
        let spec = self
            .deployment
            .spec
            .as_ref()
            .context("deployment template requires spec")?;
        let pod = spec
            .template
            .spec
            .as_ref()
            .context("deployment template requires Pod spec")?;
        let worker = pod
            .containers
            .iter()
            .find(|c| c.name == "controller")
            .context("deployment template requires controller container")?;
        if worker.args.as_deref().unwrap_or_default().iter().any(|a| {
            a.starts_with("--provision-template")
                || a.starts_with("--gateway-scope")
                || a.starts_with("--gateway-uid")
                || a.starts_with("--ingress-only")
                || a.starts_with("--exclude-gateway")
                || a.starts_with("--publish-service")
        }) {
            bail!("worker ownership and provisioning arguments must be supplied through generated environment variables");
        }
        for service in [
            &self.service,
            self.metrics_service.as_ref().unwrap_or(&self.service),
        ] {
            if let Some(spec) = &service.spec {
                if spec.cluster_ip.is_some()
                    || spec.cluster_ips.is_some()
                    || spec.health_check_node_port.is_some()
                    || spec
                        .ports
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .any(|p| p.node_port.is_some())
                {
                    bail!("Service templates must not contain server-allocated IPs or node ports");
                }
            }
        }
        Ok(())
    }
}

fn validate_label(value: &str, what: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 63
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !valid {
        bail!("{what} is not a Kubernetes label value");
    }
    Ok(())
}

fn stable_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn rewrite_pod_selectors(spec: &mut Value, previous: &Value, selector: &Value) {
    if let Some(constraints) = spec
        .get_mut("topologySpreadConstraints")
        .and_then(Value::as_array_mut)
    {
        for constraint in constraints {
            if constraint["labelSelector"] == *previous {
                constraint["labelSelector"] = json!({"matchLabels": selector});
            }
        }
    }
    for affinity in ["podAffinity", "podAntiAffinity"] {
        for mode in [
            "requiredDuringSchedulingIgnoredDuringExecution",
            "preferredDuringSchedulingIgnoredDuringExecution",
        ] {
            if let Some(terms) = spec
                .get_mut("affinity")
                .and_then(|v| v.get_mut(affinity))
                .and_then(|v| v.get_mut(mode))
                .and_then(Value::as_array_mut)
            {
                for term in terms {
                    let term = if mode.starts_with("preferred") {
                        term.get_mut("podAffinityTerm")
                    } else {
                        Some(term)
                    };
                    let Some(term) = term else {
                        continue;
                    };
                    if term["labelSelector"] == *previous {
                        term["labelSelector"] = json!({"matchLabels": selector});
                    }
                }
            }
        }
    }
}

fn preserve_service_allocations(desired: &mut Value, live: &Value) {
    if matches!(
        desired["spec"]["type"].as_str(),
        Some("NodePort" | "LoadBalancer")
    ) {
        if let Some(ports) = desired["spec"]["ports"].as_array_mut() {
            for port in ports {
                if let Some(existing) =
                    live["spec"]["ports"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|p| {
                            p["name"] == port["name"]
                                && p["protocol"].as_str().unwrap_or("TCP")
                                    == port["protocol"].as_str().unwrap_or("TCP")
                        })
                {
                    if let Some(node_port) = existing.get("nodePort") {
                        port["nodePort"] = node_port.clone();
                    }
                }
            }
        }
    }
}

fn subset(desired: &Value, live: &Value) -> bool {
    match desired {
        Value::Object(fields) => fields
            .iter()
            .all(|(key, value)| live.get(key).is_some_and(|current| subset(value, current))),
        Value::Array(items) => live.as_array().is_some_and(|current| {
            items.len() == current.len() && items.iter().zip(current).all(|(a, b)| subset(a, b))
        }),
        _ => desired == live,
    }
}

fn merge_patch(previous: &Value, desired: &Value) -> Value {
    let (Some(old), Some(new)) = (previous.as_object(), desired.as_object()) else {
        return desired.clone();
    };
    let mut patch = new.clone();
    for (key, value) in old {
        patch.insert(
            key.clone(),
            match new.get(key) {
                Some(next) => merge_patch(value, next),
                None => Value::Null,
            },
        );
    }
    Value::Object(patch)
}

async fn request<T>(
    future: impl std::future::Future<Output = Result<T, kube::Error>>,
) -> Result<T> {
    tokio::time::timeout(REQUEST_TIMEOUT, future)
        .await
        .map_err(|_| anyhow!("Kubernetes provisioning request timed out"))?
        .map_err(anyhow::Error::from)
}

fn api_code(error: &anyhow::Error) -> Option<u16> {
    error
        .downcast_ref::<kube::Error>()
        .and_then(|error| match error {
            kube::Error::Api(response) => Some(response.code),
            _ => None,
        })
}

#[cfg(test)]
mod tests;
