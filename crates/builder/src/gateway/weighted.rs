//! Resolve Gateway backendRefs without confusing Service shares with pod counts.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use sha2::{Digest, Sha256};
use sozu_gw_ir as ir;

use super::{fail_ref, reference_granted, BackendRefView, GW_GROUP};
use crate::{add_service_route, resolve_backends, Index, Inputs, PortRef, Problem};

struct ServiceGroup {
    weight: u64,
    addresses: Vec<SocketAddr>,
}

/// Content identity is independent of reference order, route/rule position and
/// endpoint membership. Duplicate references contribute their combined weight.
/// Keep zero and invalid references in the identity: an empty/drained cluster
/// must never alias a different rule that can attach forwarding backends.
fn cluster_id(namespace: &str, kind: &str, refs: &[BackendRefView]) -> String {
    let mut canonical = BTreeMap::new();
    for br in refs {
        let key = (
            br.group.as_deref().unwrap_or(""),
            br.kind.as_deref().unwrap_or("Service"),
            br.namespace.as_deref().unwrap_or(namespace),
            br.name.as_str(),
            br.port.unwrap_or(-1),
        );
        *canonical.entry(key).or_insert(0_i128) += i128::from(br.weight.unwrap_or(1));
    }
    let mut digest = Sha256::new();
    // ReferenceGrants authorize the source namespace and route kind. Identical
    // destination refs in different authorization scopes must not share state.
    for value in [namespace, kind] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    for ((group, kind, namespace, name, port), weight) in canonical {
        for value in [group, kind, namespace, name] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        digest.update(port.to_be_bytes());
        digest.update(weight.to_be_bytes());
    }
    let encoded: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("gateway-weighted.{encoded}")
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Apportion a bounded integer budget to Services first, then their endpoints.
/// Largest remainders keep a Service's share within one budget unit regardless
/// of how many pods it has. Each pod gets an equal share, within one unit.
/// Finally combine shared socket addresses and reduce the weights by their GCD.
/// The final sum cannot exceed i32::MAX, so Random cannot overflow its
/// WeightedIndex and silently fall back to uniform selection.
fn normalize(groups: &[ServiceGroup]) -> Result<BTreeMap<SocketAddr, i32>, String> {
    let active: Vec<_> = groups
        .iter()
        .filter(|g| g.weight > 0 && !g.addresses.is_empty())
        .collect();
    if active.is_empty() {
        return Ok(BTreeMap::new());
    }
    let budget = i32::MAX as u64;
    let total: u128 = active.iter().map(|g| u128::from(g.weight)).sum();
    let mut quotas = Vec::with_capacity(active.len());
    let mut remainders = Vec::with_capacity(active.len());
    for (i, group) in active.iter().enumerate() {
        let numerator = u128::from(budget) * u128::from(group.weight);
        quotas.push((numerator / total) as u64);
        remainders.push((numerator % total, i));
    }
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let spare = budget - quotas.iter().sum::<u64>();
    for (_, i) in remainders.into_iter().take(spare as usize) {
        quotas[i] += 1;
    }

    let mut combined = BTreeMap::new();
    for (group, quota) in active.into_iter().zip(quotas) {
        let count = group.addresses.len() as u64;
        let per_endpoint = quota / count;
        if per_endpoint == 0 {
            return Err(
                "a positive Service share is too small for its ready endpoint count".into(),
            );
        }
        let extra = quota % count;
        for (i, address) in group.addresses.iter().enumerate() {
            let weight = per_endpoint + u64::from((i as u64) < extra);
            *combined.entry(*address).or_insert(0_u64) += weight;
        }
    }
    let divisor = combined.values().copied().fold(0, gcd);
    combined
        .into_iter()
        .map(|(address, weight)| {
            i32::try_from(weight / divisor)
                .map(|weight| (address, weight))
                .map_err(|_| "normalized backend weight exceeds i32::MAX".into())
        })
        .collect()
}

/// A rule with a single backendRef of positive weight retains the Service cluster and
/// annotations. Composite clusters always use Random with no sticky session or
/// per-Service policy: those settings cannot describe a collection of Services.
/// Invalid refs still fail ResolvedRefs; ready valid refs absorb their share.
/// A content-isolated empty cluster keeps an all-zero rule non-forwarding and
/// prevents HTTP from falling through to a broader route (currently HTTP 503,
/// until the separate data-plane HTTP 500 support is available).
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_backend_refs(
    inputs: &Inputs,
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    referenced: &mut std::collections::BTreeSet<String>,
    namespace: &str,
    kind: &str,
    refs: &[BackendRefView],
    problems: &mut Vec<Problem>,
    resolved_refs: &mut bool,
    resolved_refs_reason: &mut &'static str,
) -> Option<String> {
    if refs.is_empty() {
        problems.push(Problem::NoReadyEndpoints {
            service: "<none>".into(),
        });
        fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
        return None;
    }
    let single = refs.len() == 1 && refs[0].weight.unwrap_or(1) > 0;
    let mut groups: BTreeMap<(String, String, i32), ServiceGroup> = BTreeMap::new();
    for br in refs {
        let weight = br.weight.unwrap_or(1);
        if !(0..=1_000_000).contains(&weight) {
            problems.push(Problem::WeightedBackendsInvalid {
                reason: format!("Service {} has out-of-range weight {weight}", br.name),
            });
            fail_ref(resolved_refs, resolved_refs_reason, "UnsupportedValue");
            continue;
        }
        let is_service = br.group.as_deref().unwrap_or("").is_empty()
            && br.kind.as_deref().unwrap_or("Service") == "Service";
        if !is_service {
            problems.push(Problem::NonServiceBackend);
            fail_ref(resolved_refs, resolved_refs_reason, "InvalidKind");
            continue;
        }
        let backend_ns = br.namespace.as_deref().unwrap_or(namespace);
        if backend_ns != namespace
            && !reference_granted(
                inputs, backend_ns, "", "Service", &br.name, namespace, GW_GROUP, kind,
            )
        {
            problems.push(Problem::BackendRefNotPermitted {
                reference: format!("Service {backend_ns}/{}", br.name),
            });
            fail_ref(resolved_refs, resolved_refs_reason, "RefNotPermitted");
            continue;
        }
        let Some(port) = br.port else {
            problems.push(Problem::ServicePortNotFound {
                service: br.name.clone(),
                port: "<unspecified>".into(),
            });
            fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
            continue;
        };
        if single {
            return match add_service_route(
                index,
                clusters,
                backends,
                referenced,
                backend_ns,
                &br.name,
                &PortRef::Number(port),
                problems,
            ) {
                Ok((id, has_endpoints)) => {
                    if !has_endpoints {
                        problems.push(Problem::NoReadyEndpoints {
                            service: br.name.clone(),
                        });
                    }
                    Some(id)
                }
                Err(problem) => {
                    problems.push(problem);
                    fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
                    None
                }
            };
        }
        // Validate even drained refs: weight zero does not waive namespace
        // grants, Service existence, or port validation.
        referenced.insert(format!("{backend_ns}/{}", br.name));
        match resolve_backends(
            index,
            backend_ns,
            &br.name,
            &PortRef::Number(port),
            problems,
        ) {
            Err(problem) => {
                problems.push(problem);
                fail_ref(resolved_refs, resolved_refs_reason, "BackendNotFound");
            }
            Ok((_, _, addresses)) => {
                if weight == 0 {
                    continue;
                }
                if addresses.is_empty() {
                    problems.push(Problem::NoReadyEndpoints {
                        service: br.name.clone(),
                    });
                }
                let group = groups
                    .entry((backend_ns.into(), br.name.clone(), port))
                    .or_insert(ServiceGroup {
                        weight: 0,
                        addresses,
                    });
                group.weight += weight as u64;
            }
        }
    }
    // Preserve the existing rejection behavior for an invalid single ref.
    if single {
        return None;
    }
    if refs.iter().all(|br| br.weight == Some(0)) {
        problems.push(Problem::NoPositiveBackendWeight);
    }
    let normalized = if refs.len() > 16 {
        Err("more than 16 backendRefs are not supported".into())
    } else {
        normalize(&groups.into_values().collect::<Vec<_>>())
    };
    let weighted = match normalized {
        Ok(weights) => weights,
        Err(reason) => {
            problems.push(Problem::WeightedBackendsInvalid { reason });
            fail_ref(resolved_refs, resolved_refs_reason, "UnsupportedValue");
            BTreeMap::new()
        }
    };
    let id = cluster_id(namespace, kind, refs);
    clusters.entry(id.clone()).or_insert(ir::Cluster {
        id: id.clone(),
        load_balancing: ir::LbAlgorithm::Random,
        sticky_session: false,
        https_redirect: false,
        max_connections_per_ip: None,
        retry_after: None,
    });
    for (address, weight) in weighted {
        let backend_id = format!("{id}#{address}");
        backends.entry(backend_id.clone()).or_insert(ir::Backend {
            cluster_id: id.clone(),
            backend_id,
            address,
            weight: Some(weight),
        });
    }
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addresses(start: u16, count: u16) -> Vec<SocketAddr> {
        (start..start + count)
            .map(|port| SocketAddr::from(([127, 0, 0, 1], port)))
            .collect()
    }

    #[test]
    fn two_thousand_endpoints_do_not_outweigh_one_endpoint() {
        let weights = normalize(&[
            ServiceGroup {
                weight: 1,
                addresses: addresses(1000, 2000),
            },
            ServiceGroup {
                weight: 1,
                addresses: addresses(9000, 1),
            },
        ])
        .unwrap();
        let many: i64 = weights
            .iter()
            .filter(|(a, _)| a.port() < 9000)
            .map(|(_, w)| i64::from(*w))
            .sum();
        let one = i64::from(weights[&addresses(9000, 1)[0]]);
        assert!((many - one).abs() <= 1);
        assert_eq!(weights.len(), 2001);
        assert!(weights.values().all(|w| *w > 0));
    }

    #[test]
    fn maximum_api_weights_keep_the_total_inside_i32() {
        let groups: Vec<_> = (0..16)
            .map(|i| ServiceGroup {
                weight: 1_000_000,
                addresses: addresses(1000 + i, 1),
            })
            .collect();
        let weights = normalize(&groups).unwrap();
        let total: i64 = weights.values().map(|w| i64::from(*w)).sum();
        assert!(total <= i64::from(i32::MAX));
        assert!(weights.values().max().unwrap() - weights.values().min().unwrap() <= 1);
    }

    #[test]
    fn shared_addresses_are_summed_before_gcd_reduction() {
        let weights = normalize(&[
            ServiceGroup {
                weight: 70,
                addresses: addresses(1000, 1),
            },
            ServiceGroup {
                weight: 30,
                addresses: addresses(1000, 1),
            },
        ])
        .unwrap();
        assert_eq!(weights, BTreeMap::from([(addresses(1000, 1)[0], 1)]));
        assert_eq!(gcd(16_000_000, 2_000_000), 2_000_000);
    }

    #[test]
    fn unrepresentable_positive_shares_are_refused_instead_of_zeroed() {
        assert!(normalize(&[
            ServiceGroup {
                weight: 1,
                addresses: addresses(1000, 2000)
            },
            ServiceGroup {
                weight: 15_000_000,
                addresses: addresses(9000, 1)
            },
        ])
        .is_err());
    }

    #[test]
    fn zero_weights_and_empty_endpoint_groups_are_not_sampled() {
        let weights = normalize(&[
            ServiceGroup {
                weight: 0,
                addresses: addresses(1000, 1),
            },
            ServiceGroup {
                weight: 70,
                addresses: vec![],
            },
            ServiceGroup {
                weight: 30,
                addresses: addresses(2000, 1),
            },
        ])
        .unwrap();
        assert_eq!(weights, BTreeMap::from([(addresses(2000, 1)[0], 1)]));
        assert!(normalize(&[]).unwrap().is_empty());
    }
}
