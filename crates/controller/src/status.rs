//! Gateway API status reporting (Phase 2).
//!
//! Writes Accepted/Programmed (Gateway, GatewayClass) and Accepted/ResolvedRefs
//! (routes, per parent) conditions back to the objects.
//!
//! **Loop-safe:** it reads the current status, reuses `lastTransitionTime` for
//! conditions whose (status, reason, message) are unchanged, and skips the PATCH
//! entirely when nothing changed — so the controller's own status writes never
//! re-trigger a reconcile. **Best-effort:** every failure is logged, never
//! propagated, so status reporting can never break routing.
//!
//! The route writer is generic over the route kind. Every Gateway API route
//! carries the same `status.parents[]` shape, but kopium generates a separate
//! `…Status` / `…StatusParents` / `…StatusParentsParentRef` triple per kind with
//! nothing in common — they are foreign types, so the trait tying them together
//! ([`RouteParents`]) is declared here and implemented once per kind.

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::networking::v1::{Ingress, IngressLoadBalancerIngress};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::api::{Patch, PatchParams};
use kube::core::NamespaceResourceScope;
use kube::{Api, Client, Resource};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, warn};

use sozu_gw_builder::{
    GatewayClassResult, GatewayResult, IngressResult, Problem, RouteKind, RouteResult,
};
use sozu_gw_gateway_api::gateway::{
    GatewayStatusAddresses, GatewayStatusListeners, GatewayStatusListenersSupportedKinds,
};
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute, TcpRoute, UdpRoute};

const GW_GROUP: &str = "gateway.networking.k8s.io";

/// One desired condition before timestamping.
struct Desired {
    type_: &'static str,
    status: bool,
    reason: &'static str,
    message: String,
}

/// Compose a condition message from problem details, so `kubectl describe`
/// shows *which* Secret/Service/port is wrong instead of a generic sentence
/// (the detail otherwise only reaches controller logs). Sorted and deduped —
/// the message participates in `lastTransitionTime` reuse, so it must be
/// deterministic across reconciles — and capped so a pathological object
/// cannot bloat its own status.
fn problems_message(problems: &[&Problem], fallback: &str) -> String {
    if problems.is_empty() {
        return fallback.to_string();
    }
    let mut lines: Vec<String> = problems.iter().map(|p| p.to_string()).collect();
    lines.sort();
    lines.dedup();
    const MAX_SHOWN: usize = 5;
    let extra = lines.len().saturating_sub(MAX_SHOWN);
    let mut msg = lines[..lines.len().min(MAX_SHOWN)].join("; ");
    if extra > 0 {
        msg.push_str(&format!(" (+{extra} more)"));
    }
    msg
}

pub async fn write_status(
    client: &Client,
    controller_name: &str,
    gateway_classes: &[GatewayClassResult],
    gateways: &[GatewayResult],
    routes: &[RouteResult],
    gateway_addresses: &[GatewayStatusAddresses],
) {
    for gc in gateway_classes.iter().filter(|gc| gc.accepted) {
        if let Err(e) = write_gatewayclass(client, gc).await {
            warn!(name = %gc.name, error = %e, "failed to write GatewayClass status");
        }
    }
    for gw in gateways {
        if let Err(e) = write_gateway(client, gw, gateway_addresses).await {
            warn!(namespace = %gw.namespace, name = %gw.name, error = %e, "failed to write Gateway status");
        }
    }
    for route in routes {
        // One arm per route kind: the writer is generic, but `Api<K>` needs a
        // concrete type, so the kind carried by the build result picks it.
        let written = match route.kind {
            RouteKind::HttpRoute => write_route::<HttpRoute>(client, controller_name, route).await,
            RouteKind::TcpRoute => write_route::<TcpRoute>(client, controller_name, route).await,
            RouteKind::UdpRoute => write_route::<UdpRoute>(client, controller_name, route).await,
        };
        if let Err(e) = written {
            warn!(kind = route.kind.as_str(), namespace = %route.namespace, name = %route.name, error = %e, "failed to write route status");
        }
    }
}

fn now() -> Time {
    Time(k8s_openapi::jiff::Timestamp::now())
}

/// Build conditions, reusing the previous `lastTransitionTime` when a condition's
/// observable fields are unchanged (so repeated writes are byte-identical).
///
/// `observed_generation` is set to the object's `metadata.generation`: the Gateway
/// API requires every condition to carry it, and conformance checks that it tracks
/// the latest generation. `lastTransitionTime` still only moves when `status`
/// flips — a generation bump alone updates `observedGeneration` without resetting
/// the transition time.
fn build_conditions(
    desired: &[Desired],
    current: Option<&[Condition]>,
    generation: Option<i64>,
) -> Vec<Condition> {
    desired
        .iter()
        .map(|d| {
            let status = if d.status { "True" } else { "False" }.to_string();
            let previous = current.and_then(|cs| cs.iter().find(|c| c.type_ == d.type_));
            let last_transition_time = match previous {
                Some(p) if p.status == status && p.reason == d.reason && p.message == d.message => {
                    p.last_transition_time.clone()
                }
                _ => now(),
            };
            Condition {
                type_: d.type_.to_string(),
                status,
                reason: d.reason.to_string(),
                message: d.message.clone(),
                last_transition_time,
                observed_generation: generation,
            }
        })
        .collect()
}

fn conditions_equal(a: &[Condition], b: &[Condition]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

async fn write_gatewayclass(client: &Client, gc: &GatewayClassResult) -> Result<(), kube::Error> {
    let api: Api<GatewayClass> = Api::all(client.clone());
    let current = api.get(&gc.name).await?;
    let cur = current
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref());
    let desired = build_conditions(
        &[Desired {
            type_: "Accepted",
            status: true,
            reason: "Accepted",
            message: "Accepted by sozu-gateway".to_string(),
        }],
        cur,
        current.metadata.generation,
    );
    if cur.is_some_and(|c| conditions_equal(&desired, c)) {
        return Ok(());
    }
    let patch = json!({ "status": { "conditions": desired } });
    api.patch_status(&gc.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(name = %gc.name, "GatewayClass status updated");
    Ok(())
}

/// Build `Gateway.status.listeners[]`, reusing each listener condition's previous
/// `lastTransitionTime` (matched by listener name) so repeated writes are stable.
fn build_listeners_status(
    gw: &GatewayResult,
    current: &Gateway,
    generation: Option<i64>,
) -> Vec<GatewayStatusListeners> {
    let cur_listeners = current
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_deref())
        .unwrap_or_default();
    gw.listeners
        .iter()
        .map(|l| {
            let prev = cur_listeners
                .iter()
                .find(|cl| cl.name == l.name)
                .map(|cl| cl.conditions.as_slice());
            // Problems that name this listener carry the user-facing detail
            // for its False conditions.
            let listener_problems: Vec<&Problem> = gw
                .problems
                .iter()
                .filter(|p| p.listener() == Some(l.name.as_str()))
                .collect();
            let conditions = build_conditions(
                &[
                    Desired {
                        type_: "Accepted",
                        status: l.accepted,
                        reason: l.accepted_reason,
                        message: if l.accepted {
                            "Listener accepted by sozu-gateway".to_string()
                        } else {
                            problems_message(
                                &listener_problems,
                                "Listener cannot be accepted as declared",
                            )
                        },
                    },
                    Desired {
                        type_: "Programmed",
                        status: l.programmed,
                        reason: l.programmed_reason,
                        message: if l.programmed {
                            "Listener programmed into Sōzu".to_string()
                        } else {
                            problems_message(
                                &listener_problems,
                                "Listener could not be programmed into Sōzu",
                            )
                        },
                    },
                    Desired {
                        type_: "ResolvedRefs",
                        status: l.resolved_refs,
                        reason: l.resolved_refs_reason,
                        message: if l.resolved_refs {
                            "Listener references resolved".to_string()
                        } else {
                            problems_message(
                                &listener_problems,
                                "Listener references could not be resolved",
                            )
                        },
                    },
                ],
                prev,
                generation,
            );
            GatewayStatusListeners {
                name: l.name.clone(),
                supported_kinds: Some(
                    l.supported_kinds
                        .iter()
                        .map(|k| GatewayStatusListenersSupportedKinds {
                            group: Some(GW_GROUP.to_string()),
                            kind: k.clone(),
                        })
                        .collect(),
                ),
                attached_routes: l.attached_routes,
                conditions,
            }
        })
        .collect()
}

async fn write_gateway(
    client: &Client,
    gw: &GatewayResult,
    addresses: &[GatewayStatusAddresses],
) -> Result<(), kube::Error> {
    let api: Api<Gateway> = Api::namespaced(client.clone(), &gw.namespace);
    let current = api.get(&gw.name).await?;
    let cur = current
        .status
        .as_ref()
        .and_then(|s| s.conditions.as_deref());
    let all_problems: Vec<&Problem> = gw.problems.iter().collect();
    let desired = build_conditions(
        &[
            Desired {
                type_: "Accepted",
                status: gw.accepted,
                reason: if gw.accepted { "Accepted" } else { "Invalid" },
                message: if gw.accepted {
                    "Accepted by sozu-gateway".to_string()
                } else {
                    problems_message(&all_problems, "Gateway rejected")
                },
            },
            Desired {
                type_: "Programmed",
                status: gw.programmed,
                reason: if gw.programmed {
                    "Programmed"
                } else {
                    "Invalid"
                },
                message: if gw.programmed {
                    "Listeners programmed into Sōzu".to_string()
                } else {
                    problems_message(&all_problems, "No listeners could be programmed")
                },
            },
        ],
        cur,
        current.metadata.generation,
    );
    let listeners = build_listeners_status(gw, &current, current.metadata.generation);
    // Publish the LoadBalancer address into the Gateway's status (what
    // external-dns's gateway-httproute source reads). Skipped when there is no
    // address yet, so a pending LB never clears it.
    let cur_addresses = current
        .status
        .as_ref()
        .and_then(|s| s.addresses.as_deref())
        .unwrap_or_default();
    let cur_listeners = current
        .status
        .as_ref()
        .and_then(|s| s.listeners.as_deref())
        .unwrap_or_default();
    let addresses_unchanged = addresses.is_empty()
        || serde_json::to_value(cur_addresses).ok() == serde_json::to_value(addresses).ok();
    let listeners_unchanged =
        serde_json::to_value(cur_listeners).ok() == serde_json::to_value(&listeners).ok();
    let conditions_unchanged = cur.is_some_and(|c| conditions_equal(&desired, c));
    if conditions_unchanged && addresses_unchanged && listeners_unchanged {
        return Ok(());
    }
    let mut status = serde_json::Map::new();
    status.insert("conditions".to_string(), json!(desired));
    status.insert("listeners".to_string(), json!(listeners));
    if !addresses.is_empty() {
        status.insert("addresses".to_string(), json!(addresses));
    }
    let patch = json!({ "status": status });
    api.patch_status(&gw.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(namespace = %gw.namespace, name = %gw.name, "Gateway status updated");
    Ok(())
}

/// Map the publish Service's load-balancer address(es) to Gateway status
/// addresses (`IPAddress` for an IP, `Hostname` otherwise).
pub(crate) fn gateway_addresses(svc: &Service) -> Vec<GatewayStatusAddresses> {
    lb_points(svc)
        .into_iter()
        .filter_map(|p| {
            if let Some(ip) = p.ip {
                Some(GatewayStatusAddresses {
                    r#type: Some("IPAddress".to_string()),
                    value: ip,
                })
            } else {
                p.hostname.map(|h| GatewayStatusAddresses {
                    r#type: Some("Hostname".to_string()),
                    value: h,
                })
            }
        })
        .collect()
}

/// One entry of a route's `status.parents[]`, in the shape **every** Gateway
/// API route kind shares.
///
/// kopium emits a distinct struct per kind with no trait in common, so this is
/// the controller-side neutral one. It serialises to byte-identical JSON (same
/// field names, same `skip_serializing_if`), which is what makes it usable both
/// as what we read the current status into and as what we patch back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteParentStatus {
    pub conditions: Vec<Condition>,
    #[serde(rename = "controllerName")]
    pub controller_name: String,
    #[serde(rename = "parentRef")]
    pub parent_ref: RouteParentRef,
}

/// The `parentRef` a status entry answers for. Its identity is the **whole**
/// reference, `sectionName` and `port` included: a route may legally name the
/// same Gateway several times, once per listener, and each of those parentRefs
/// gets its own entry. Matching on `(name, namespace)` alone collapses them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteParentRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "sectionName"
    )]
    pub section_name: Option<String>,
}

/// A Gateway API route kind that reports per-parent status.
///
/// The one thing a route object must offer the writer is its current
/// `status.parents[]` — everything else (condition building, the loop-safety
/// comparison, the patch) is kind-independent. Implementations are mechanical
/// field copies rather than a serde round-trip so the compiler, not a runtime
/// shape mismatch, catches a generated struct that drifts.
pub trait RouteParents {
    fn route_parents(&self) -> Vec<RouteParentStatus>;
}

/// Implement [`RouteParents`] for one kopium-generated route kind. The bodies
/// are identical; only the types differ, and they have no trait in common to
/// abstract over.
macro_rules! impl_route_parents {
    ($kind:ty) => {
        impl RouteParents for $kind {
            fn route_parents(&self) -> Vec<RouteParentStatus> {
                self.status
                    .iter()
                    .flat_map(|s| s.parents.iter())
                    .map(|p| RouteParentStatus {
                        conditions: p.conditions.clone(),
                        controller_name: p.controller_name.clone(),
                        parent_ref: RouteParentRef {
                            group: p.parent_ref.group.clone(),
                            kind: p.parent_ref.kind.clone(),
                            name: p.parent_ref.name.clone(),
                            namespace: p.parent_ref.namespace.clone(),
                            port: p.parent_ref.port,
                            section_name: p.parent_ref.section_name.clone(),
                        },
                    })
                    .collect()
            }
        }
    };
}

impl_route_parents!(HttpRoute);
impl_route_parents!(TcpRoute);
impl_route_parents!(UdpRoute);

/// The `status.parents[]` we want on a route: every entry owned by another
/// controller kept verbatim, followed by one entry per parentRef we resolved.
///
/// Pure, so the loop-safety property is testable without an apiserver: feeding
/// this function its own output must be a fixed point, or the controller
/// re-patches on every reconcile.
fn route_parents(
    controller_name: &str,
    route: &RouteResult,
    current: &[RouteParentStatus],
    generation: Option<i64>,
) -> Vec<RouteParentStatus> {
    let mut parents: Vec<RouteParentStatus> = current
        .iter()
        .filter(|p| p.controller_name != controller_name)
        .cloned()
        .collect();

    for parent in &route.parents {
        let parent_ref = RouteParentRef {
            group: Some(GW_GROUP.to_string()),
            kind: Some("Gateway".to_string()),
            name: parent.gateway_name.clone(),
            namespace: Some(parent.gateway_namespace.clone()),
            port: parent.port,
            section_name: parent.section_name.clone(),
        };
        // Matched on the full reference. Keying on (name, namespace) made two
        // parentRefs to one Gateway that differ only by `sectionName` share a
        // single entry: each pass rebuilt the second one against the first
        // one's conditions, `lastTransitionTime` moved, the no-op guard never
        // held, and the controller re-patched forever.
        let existing = current
            .iter()
            .find(|p| p.controller_name == controller_name && p.parent_ref == parent_ref);
        let parent_problems: Vec<&Problem> = parent.problems.iter().collect();
        let conditions = build_conditions(
            &[
                Desired {
                    type_: "Accepted",
                    status: parent.accepted,
                    reason: parent.accepted_reason,
                    message: if parent.accepted {
                        "Route accepted by sozu-gateway".to_string()
                    } else {
                        problems_message(&parent_problems, "Route does not bind to this parent")
                    },
                },
                Desired {
                    type_: "ResolvedRefs",
                    status: parent.resolved_refs,
                    reason: parent.resolved_refs_reason,
                    message: if parent.resolved_refs {
                        "All backend references resolved".to_string()
                    } else {
                        problems_message(
                            &parent_problems,
                            "One or more backend references could not be resolved",
                        )
                    },
                },
            ],
            existing.map(|p| p.conditions.as_slice()),
            generation,
        );
        parents.push(RouteParentStatus {
            conditions,
            controller_name: controller_name.to_string(),
            parent_ref,
        });
    }
    parents
}

async fn write_route<K>(
    client: &Client,
    controller_name: &str,
    route: &RouteResult,
) -> Result<(), kube::Error>
where
    K: RouteParents + Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned,
    K: std::fmt::Debug,
    K::DynamicType: Default,
{
    let api: Api<K> = Api::namespaced(client.clone(), &route.namespace);
    let current = api.get(&route.name).await?;
    let generation = current.meta().generation;
    let current_parents = current.route_parents();
    let parents = route_parents(controller_name, route, &current_parents, generation);

    // Skip the write when the full parents list is unchanged (loop-safety).
    if current_parents == parents {
        return Ok(());
    }
    let patch = json!({ "status": { "parents": parents } });
    api.patch_status(&route.name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(kind = route.kind.as_str(), namespace = %route.namespace, name = %route.name, "route status updated");
    Ok(())
}

// ---- Ingress status (.status.loadBalancer.ingress) -------------------------

/// Map the publish Service's load-balancer address(es) into the shape an Ingress
/// status expects. Pure, so it is unit-tested without a cluster.
///
/// The result is sorted by `(ip, hostname)` so the order is independent of the
/// Service status's array order. The loop-safety guard in [`write_one_ingress`]
/// compares element-wise, so without this a provider that re-orders its
/// `loadBalancer.ingress` between reads would cause endless no-op re-patches.
pub(crate) fn lb_points(svc: &Service) -> Vec<IngressLoadBalancerIngress> {
    let mut points: Vec<IngressLoadBalancerIngress> = svc
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_ref())
        .map(|points| {
            points
                .iter()
                .map(|p| IngressLoadBalancerIngress {
                    hostname: p.hostname.clone(),
                    ip: p.ip.clone(),
                    ports: None,
                })
                .collect()
        })
        .unwrap_or_default();
    points.sort_by(|a, b| (&a.ip, &a.hostname).cmp(&(&b.ip, &b.hostname)));
    points
}

/// Publish the gateway's external address into each managed Ingress's
/// `.status.loadBalancer.ingress`. Loop-safe (skips no-op patches) and
/// best-effort. Does nothing when there is no address yet, so a still-pending
/// LoadBalancer never clears an Ingress's status.
pub async fn write_ingress_status(
    client: &Client,
    ingresses: &[IngressResult],
    points: &[IngressLoadBalancerIngress],
) {
    if points.is_empty() {
        return;
    }
    for r in ingresses {
        if let Err(e) = write_one_ingress(client, &r.namespace, &r.name, points).await {
            warn!(namespace = %r.namespace, name = %r.name, error = %e, "failed to write Ingress status");
        }
    }
}

async fn write_one_ingress(
    client: &Client,
    namespace: &str,
    name: &str,
    points: &[IngressLoadBalancerIngress],
) -> Result<(), kube::Error> {
    let api: Api<Ingress> = Api::namespaced(client.clone(), namespace);
    let current = api.get(name).await?;
    let cur = current
        .status
        .as_ref()
        .and_then(|s| s.load_balancer.as_ref())
        .and_then(|lb| lb.ingress.as_deref())
        .unwrap_or_default();
    if cur == points {
        return Ok(()); // already published — skip to stay loop-safe
    }
    let patch = json!({ "status": { "loadBalancer": { "ingress": points } } });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    debug!(namespace = %namespace, name = %name, "Ingress status updated");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sozu_gw_builder::RouteParentResult;

    fn parent(section: Option<&str>, accepted: bool) -> RouteParentResult {
        RouteParentResult {
            gateway_namespace: "sozu-system".to_string(),
            gateway_name: "gw".to_string(),
            section_name: section.map(str::to_string),
            port: None,
            accepted,
            accepted_reason: if accepted {
                "Accepted"
            } else {
                "NoMatchingParent"
            },
            resolved_refs: true,
            resolved_refs_reason: "ResolvedRefs",
            problems: vec![],
        }
    }

    fn route(parents: Vec<RouteParentResult>) -> RouteResult {
        RouteResult {
            kind: RouteKind::HttpRoute,
            namespace: "demo".to_string(),
            name: "web".to_string(),
            uid: Some("11111111-2222-3333-4444-555555555555".to_string()),
            parents,
        }
    }

    /// Two parentRefs to one Gateway that differ only by `sectionName` must
    /// produce two *distinguishable* status entries, or neither can be matched
    /// back to its own conditions on the next pass. This is the normal shape
    /// for a layer-4 route, where a Gateway may declare several listeners on
    /// one port and `sectionName` is the only way to pick one.
    #[test]
    fn parent_entries_carry_section_name_and_port() {
        let mut with_port = parent(Some("b"), true);
        with_port.port = Some(8443);
        let r = route(vec![parent(Some("a"), true), with_port]);
        let parents = route_parents("sozu.io/gateway-controller", &r, &[], Some(3));

        assert_eq!(parents.len(), 2);
        assert_eq!(parents[0].parent_ref.section_name.as_deref(), Some("a"));
        assert_eq!(parents[1].parent_ref.section_name.as_deref(), Some("b"));
        assert_eq!(parents[1].parent_ref.port, Some(8443));
        assert_ne!(parents[0].parent_ref, parents[1].parent_ref);
    }

    /// The loop-safety contract, mechanically: feeding the writer its own
    /// output must change nothing, so the PATCH is skipped. With two parents
    /// differing only by `sectionName` and *different* conditions this used to
    /// fail — both matched the first stored entry, the second one's
    /// `lastTransitionTime` moved every pass, and the controller re-patched on
    /// every reconcile forever.
    #[test]
    fn rebuilding_from_our_own_status_is_a_fixed_point() {
        let controller = "sozu.io/gateway-controller";
        let r = route(vec![parent(Some("a"), true), parent(Some("b"), false)]);
        let first = route_parents(controller, &r, &[], Some(3));
        let second = route_parents(controller, &r, &first, Some(3));
        assert_eq!(first, second, "a second pass must be a no-op");
    }

    /// Entries written by another controller are carried through untouched:
    /// a route may be attached to somebody else's Gateway as well as ours.
    #[test]
    fn other_controllers_entries_are_preserved() {
        let controller = "sozu.io/gateway-controller";
        let theirs = RouteParentStatus {
            conditions: vec![],
            controller_name: "example.net/other".to_string(),
            parent_ref: RouteParentRef {
                group: Some(GW_GROUP.to_string()),
                kind: Some("Gateway".to_string()),
                name: "other-gw".to_string(),
                namespace: Some("other".to_string()),
                port: None,
                section_name: None,
            },
        };
        let parents = route_parents(
            controller,
            &route(vec![parent(None, true)]),
            std::slice::from_ref(&theirs),
            None,
        );
        assert_eq!(parents.len(), 2);
        assert!(parents.contains(&theirs));
    }

    #[test]
    fn problems_message_is_deterministic_deduped_and_capped() {
        assert_eq!(problems_message(&[], "fallback"), "fallback");

        // Order-insensitive and deduped: the message participates in
        // lastTransitionTime reuse, so it must not flap across reconciles.
        let a = Problem::ServiceNotFound {
            service: "z".into(),
        };
        let b = Problem::ServiceNotFound {
            service: "a".into(),
        };
        let one = problems_message(&[&a, &b, &a], "");
        let two = problems_message(&[&b, &a, &b], "");
        assert_eq!(one, two);
        assert_eq!(one.matches("\"z\"").count(), 1, "duplicates collapse");

        let many: Vec<Problem> = (0..8)
            .map(|i| Problem::ServiceNotFound {
                service: format!("s{i}"),
            })
            .collect();
        let refs: Vec<&Problem> = many.iter().collect();
        assert!(problems_message(&refs, "").ends_with("(+3 more)"));
    }

    fn svc_with_ips(ips: &[&str]) -> Service {
        let ingress: Vec<_> = ips.iter().map(|ip| json!({ "ip": ip })).collect();
        serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" },
            "status": { "loadBalancer": { "ingress": ingress } }
        }))
        .unwrap()
    }

    #[test]
    fn lb_points_extracts_ip_and_hostname() {
        let svc: Service = serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" },
            "status": { "loadBalancer": { "ingress": [
                { "ip": "1.2.3.4" },
                { "hostname": "lb.example.com" }
            ] } }
        }))
        .unwrap();
        let pts = lb_points(&svc);
        assert_eq!(pts.len(), 2);
        assert!(pts.iter().any(|p| p.ip.as_deref() == Some("1.2.3.4")));
        assert!(pts
            .iter()
            .any(|p| p.hostname.as_deref() == Some("lb.example.com")));
    }

    #[test]
    fn lb_points_order_is_canonical() {
        // Same address set in two different Service orders must map to the same
        // (sorted) Vec, so the loop-safety comparison never flips on reorder.
        let a = lb_points(&svc_with_ips(&["10.0.0.2", "10.0.0.1"]));
        let b = lb_points(&svc_with_ips(&["10.0.0.1", "10.0.0.2"]));
        assert_eq!(a, b);
        assert_eq!(a[0].ip.as_deref(), Some("10.0.0.1"));
    }

    #[test]
    fn lb_points_empty_when_no_loadbalancer_status() {
        let svc: Service = serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" }
        }))
        .unwrap();
        assert!(lb_points(&svc).is_empty());
    }

    #[test]
    fn gateway_addresses_typed_from_lb() {
        let svc: Service = serde_json::from_value(json!({
            "metadata": { "name": "gw", "namespace": "sozu-system" },
            "status": { "loadBalancer": { "ingress": [
                { "ip": "1.2.3.4" },
                { "hostname": "lb.example.com" }
            ] } }
        }))
        .unwrap();
        let addrs = gateway_addresses(&svc);
        assert_eq!(addrs.len(), 2);
        assert!(addrs
            .iter()
            .any(|a| a.r#type.as_deref() == Some("IPAddress") && a.value == "1.2.3.4"));
        assert!(addrs
            .iter()
            .any(|a| a.r#type.as_deref() == Some("Hostname") && a.value == "lb.example.com"));
    }
}
