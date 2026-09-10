//! Ownership of an explicitly provisioned Gateway data plane.
use std::{collections::BTreeSet, fmt, str::FromStr};

use sozu_gw_builder::Inputs;

const GW_GROUP: &str = "gateway.networking.k8s.io";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GatewayId {
    pub namespace: String,
    pub name: String,
}

fn dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && s.as_bytes()[0] != b'-'
        && !s.ends_with('-')
}

impl FromStr for GatewayId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (namespace, name) = value
            .split_once('/')
            .ok_or_else(|| format!("Gateway scope {value:?} must be namespace/name"))?;
        if !dns_label(namespace) || name.len() > 253 || !name.split('.').all(dns_label) {
            return Err(format!(
                "Gateway scope {value:?} needs a DNS namespace and Gateway name"
            ));
        }
        Ok(Self {
            namespace: namespace.into(),
            name: name.into(),
        })
    }
}

impl fmt::Display for GatewayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

#[derive(Debug, Clone, Default, clap::Args)]
pub struct GatewayScope {
    /// Own only this Gateway (namespace/name), excluding Ingress and GatewayClass status.
    #[arg(
        long,
        env = "SOZU_GW_GATEWAY_SCOPE",
        conflicts_with = "exclude_gateway"
    )]
    pub gateway_scope: Option<GatewayId>,
    /// Gateways owned by separate instances (repeatable, or comma-separated).
    #[arg(long, env = "SOZU_GW_EXCLUDE_GATEWAY", value_delimiter = ',')]
    pub exclude_gateway: Vec<GatewayId>,
}

impl GatewayScope {
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for gateway in &self.exclude_gateway {
            if !seen.insert(gateway) {
                return Err(format!("Gateway {gateway} is excluded more than once"));
            }
        }
        Ok(())
    }

    pub fn is_default(&self) -> bool {
        self.gateway_scope.is_none()
    }

    pub fn owns_gateway(&self, namespace: &str, name: &str) -> bool {
        let matches = |gateway: &GatewayId| gateway.namespace == namespace && gateway.name == name;
        match &self.gateway_scope {
            Some(gateway) => matches(gateway),
            None => !self.exclude_gateway.iter().any(matches),
        }
    }

    pub fn owns_parent(
        &self,
        route_namespace: &str,
        group: Option<&str>,
        kind: Option<&str>,
        namespace: Option<&str>,
        name: &str,
    ) -> bool {
        group.unwrap_or(GW_GROUP) == GW_GROUP
            && kind.unwrap_or("Gateway") == "Gateway"
            && self.owns_gateway(namespace.unwrap_or(route_namespace), name)
    }

    pub fn filter_inputs(&self, inputs: &mut Inputs) {
        if !self.is_default() {
            inputs.ingresses.clear();
        }
        inputs.gateways.retain(|gateway| {
            self.owns_gateway(
                gateway.metadata.namespace.as_deref().unwrap_or("default"),
                gateway.metadata.name.as_deref().unwrap_or_default(),
            )
        });
        // Keep route objects and reference caches intact. The builder attaches
        // only to Gateways present above; the status writer still needs routes
        // that lost their last owned parent to remove its stale status entry.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sozu_gw_builder::{build, BuildConfig};
    use std::sync::Arc;

    #[test]
    fn gateway_ids_reject_ambiguous_or_invalid_names() {
        for name in ["namespace/name", "demo/my.gateway", "ns0/1"] {
            assert!(name.parse::<GatewayId>().is_ok());
        }
        for name in [
            "name", "a/b/c", "/name", "ns/", "Name/gw", "ns/gw_1", "-ns/gw", "ns/a..b",
        ] {
            assert!(name.parse::<GatewayId>().is_err(), "{name}");
        }
        assert!(format!("{}/gw", "a".repeat(64))
            .parse::<GatewayId>()
            .is_err());
        let scope = GatewayScope {
            gateway_scope: None,
            exclude_gateway: vec!["ns/gw".parse().unwrap(); 2],
        };
        assert!(scope.validate().is_err());
    }

    #[test]
    fn separate_instances_build_disjoint_routes_and_ignore_ingress() {
        let mut inputs = Inputs::default();
        inputs.gateway_classes.push(Arc::new(
            serde_json::from_value(json!({
                "metadata": {"name":"sozu"}, "spec":{"controllerName":"sozu.io/gateway-controller"}
            }))
            .unwrap(),
        ));
        for name in ["one", "two"] {
            inputs.gateways.push(Arc::new(serde_json::from_value(json!({
                "metadata":{"namespace":"demo","name":name},
                "spec":{"gatewayClassName":"sozu","listeners":[{"name":"http","protocol":"HTTP","port":80}]}
            })).unwrap()));
            inputs.http_routes.push(Arc::new(serde_json::from_value(json!({
                "metadata":{"namespace":"demo","name":name},
                "spec":{"parentRefs":[{"name":name}],"rules":[{"backendRefs":[{"name":name,"port":80}]}]}
            })).unwrap()));
            inputs.services.push(Arc::new(
                serde_json::from_value(json!({
                    "metadata":{"namespace":"demo","name":name},"spec":{"ports":[{"port":80}]}
                }))
                .unwrap(),
            ));
        }
        inputs.ingresses.push(Arc::new(serde_json::from_value(json!({
            "metadata":{"namespace":"demo","name":"ingress"},
            "spec":{"ingressClassName":"sozu","rules":[{"host":"ingress.example.com","http":{"paths":[{
                "path":"/","pathType":"Prefix","backend":{"service":{"name":"one","port":{"number":80}}}
            }]}}]}
        })).unwrap()));
        let child = GatewayScope {
            gateway_scope: Some("demo/two".parse().unwrap()),
            ..Default::default()
        };
        let default = GatewayScope {
            exclude_gateway: vec!["demo/two".parse().unwrap()],
            ..Default::default()
        };
        let mut child_inputs = Inputs {
            ingresses: inputs.ingresses.clone(),
            gateway_classes: inputs.gateway_classes.clone(),
            gateways: inputs.gateways.clone(),
            http_routes: inputs.http_routes.clone(),
            services: inputs.services.clone(),
            ..Default::default()
        };
        child.filter_inputs(&mut child_inputs);
        default.filter_inputs(&mut inputs);
        let child_ir = build(&BuildConfig::default(), &child_inputs).ir;
        let default_ir = build(&BuildConfig::default(), &inputs).ir;
        assert_eq!(child_ir.frontends.len(), 1);
        assert_eq!(default_ir.frontends.len(), 2);
        assert_eq!(
            child_ir.frontends[0].cluster_id.as_deref(),
            Some("demo.two.80")
        );
        assert!(default_ir
            .frontends
            .iter()
            .all(|f| f.cluster_id.as_deref() == Some("demo.one.80")));
        assert!(child_inputs.ingresses.is_empty());
        child_inputs.http_routes.clear();
        let empty = build(&BuildConfig::default(), &child_inputs).ir;
        assert!(empty.frontends.is_empty());
        assert!(!sozu_gw_translator::reconcile(&child_ir, &empty)
            .unwrap()
            .is_empty());
    }
}
