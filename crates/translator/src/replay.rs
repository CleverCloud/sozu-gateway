//! The baseline after a batch failed part-way: the previous applied IR with
//! the requests Sōzu acknowledged replayed onto it, adopted only when it
//! describes exactly what those requests left in Sōzu.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use sozu_command_lib::proto::command::{
    request::RequestType, ListenerType, Request, RequestHttpFrontend, RequestTcpFrontend,
};
use sozu_command_lib::state::{ConfigState, StateError};
use sozu_gw_ir as ir;

use crate::{hex, plain, routing_state};

/// [`apply_acknowledged`], when the IR it returns describes exactly the
/// routing state Sōzu holds after `acknowledged`; `None` when it cannot be
/// shown to, and the caller keeps `previous`.
///
/// Some states an acknowledged head leaves have no IR spelling, and the
/// replay then misdescribes them. The translator derives a cluster's UDP
/// affinity from the layer-4 frontends naming it, so a cluster upsert
/// acknowledged before the removal of its UDP frontend leaves Sōzu holding
/// the cluster without affinity next to that frontend, while the replayed IR
/// spells affinity back in: a desired state that then wants the frontend
/// again equals the baseline, and the affinity stays lost. A layer-4 listener
/// is spelled only through a frontend on it (see [`apply_acknowledged`]).
/// From such a baseline some desired state diffs wrongly, whichever IR is
/// picked. The previous one is what the controller diffed from before it
/// replayed anything, so falling back to it is never worse than that was;
/// it does give up what the replay records of the rest of the head (a
/// cluster removal, say, and on Sōzu 2.3 the refused `AddBackend` that
/// comes with diffing as if the cluster were still there).
///
/// The check: the main process's routing state is simulated from
/// `previous`'s ([`routing_state`], the state the translator diffs), each
/// acknowledged request dispatched to it the way the agent got it
/// acknowledged, and compared with the replayed IR's. It proves the replay
/// against `previous`, not against Sōzu: drift `previous` already missed is
/// carried over, and so is anything the request a batch stopped on did. The
/// simulation runs on the linked `sozu-command-lib`, so it models a data
/// plane of that version's family; a request whose acknowledgement it cannot
/// explain makes it give up rather than guess, which keeps `previous`.
///
/// Certificates are not part of a routing state, and a certificate request
/// is left out of the simulation; the replay keys them as Sōzu does. An
/// `AddCertificate` of a fingerprint Sōzu already holds is a no-op there
/// (names and material kept, on 2.2.1 as on 2.3.2) while the replay records
/// the desired certificate, so over a drift `previous` missed the result
/// lists names Sōzu does not hold. `previous` does no better there: its
/// retry sends the same add, the same no-op.
pub fn replay_if_exact(
    previous: &ir::Ir,
    desired: &ir::Ir,
    acknowledged: &[Request],
) -> Option<ir::Ir> {
    if acknowledged.is_empty() {
        return Some(previous.clone());
    }
    let replayed = apply_acknowledged(previous, desired, acknowledged);
    let simulated = simulate(previous, acknowledged)?;
    let described = comparable(routing_state(&replayed).ok()?);
    (described == simulated).then_some(replayed)
}

/// `previous` with `acknowledged` replayed onto it, in order: the baseline to
/// diff from after the batch `reconcile(previous, desired)` returned failed
/// part-way, once Sōzu had acknowledged `acknowledged` (its head).
///
/// Sōzu holds what it acknowledged, and a baseline that ignores any of it
/// diffs wrongly from then on:
/// - A removal left in the baseline makes a desired state that wants the
///   object again diff to nothing for it. The route silently stays missing
///   on Sōzu 2.2.1. On 2.3, after a `RemoveCluster`, an `AddBackend` to the
///   unchanged cluster goes out without its `AddCluster` and is refused
///   `ClusterNotFound` on every reconcile, which blocks unrelated changes.
/// - An add left out makes a desired state that drops the object again
///   diff to nothing for it, and Sōzu keeps it. A route re-pointed from one
///   live cluster to another, its removal and its add both acknowledged,
///   would keep serving the second cluster once deleted.
///
/// Each request is replayed under the key the translator identifies its
/// object by, which is the key Sōzu stores it under:
/// - `AddCluster` sets the clusters with that id to the desired ones (an
///   upsert takes the desired config). `RemoveCluster` drops the cluster and,
///   with it, the backends and frontends naming it that were there before it:
///   Sōzu 2.3 removes those with the cluster, and on 2.2.1 the batch's own
///   explicit removals come first (frontends and backends are tiered before
///   clusters), so the result is the same.
/// - `AddBackend` and `RemoveBackend`: the backends with that cluster, id and
///   address.
/// - `AddHttpFrontend`/`AddHttpsFrontend` and their removals: the frontends
///   of that protocol stored under the payload's route key
///   ([`ir::Frontend::sozu_route_key`], the payload's `Display`), whatever
///   their cluster. Sōzu holds one route per key, the agent repairs an add on
///   an occupied key by evicting its holder, and it evicts a route another
///   cluster holds under a key it removes.
/// - `AddTcpFrontend`/`AddUdpFrontend` and their removals: the layer-4
///   frontends of that protocol on that address (one cluster per address,
///   evicted alike).
/// - `AddCertificate` and `RemoveCertificate`: the certificates at that
///   listener with that fingerprint. `ReplaceCertificate` drops the old
///   fingerprint, unless it is the new one (a rename), and sets the new.
///   (Sōzu takes an add of a fingerprint it already holds as a no-op, so
///   over a drift the add records names it may not hold; see
///   [`replay_if_exact`].)
///
/// An add sets what `desired` holds under its key, never an object rebuilt
/// from the payload: the batch was diffed from `desired`, so its source is
/// there, and a payload cannot give back its IR spelling (a path match it
/// compiled to a regex, say). An add whose key `desired` does not hold cannot
/// come from that diff, and leaves the baseline as it was.
///
/// The IR holds a layer-4 listener only through a frontend on it, and two
/// states of a listener have no IR spelling. Both are left as an unchanged
/// baseline leaves them:
/// - A frontend whose removal was acknowledged while its listener stays held
///   remains until that listener's `RemoveListener` is acknowledged too.
///   Dropped earlier, it would take the listener out of the baseline, and a
///   retry would never take the listener down. The price: a desired state
///   that wants that frontend back before then diffs to nothing for it.
/// - A listener whose add was acknowledged, but not its frontend's, stays
///   out. A retry that still wants it re-sends the add, which the agent
///   settles; one that no longer wants it leaves the listener in Sōzu.
///
/// Those, and a cluster whose UDP affinity no longer matches its frontends,
/// make the result misdescribe Sōzu: [`replay_if_exact`] is the baseline to
/// adopt.
pub fn apply_acknowledged(previous: &ir::Ir, desired: &ir::Ir, acknowledged: &[Request]) -> ir::Ir {
    if acknowledged.is_empty() {
        return previous.clone();
    }
    let certificates = acknowledged.iter().any(names_a_certificate);
    let mut replay = Replay::new(previous, desired, certificates);
    for (index, request) in acknowledged.iter().enumerate() {
        replay.apply(index + 1, request);
    }
    replay.finish()
}

type BackendKey = (String, String, SocketAddr);
type L4Key = (ir::L4Protocol, SocketAddr);
/// `None` for a certificate the translator cannot fingerprint: it never sent
/// one, so no request names it.
type CertificateKey = Option<(SocketAddr, String)>;

/// One kind of object, grouped under the key the translator identifies it by.
struct Keyed<K, T> {
    groups: BTreeMap<K, Group<T>>,
    /// The next output position: `previous`'s objects keep theirs, and what
    /// the replay sets goes after them.
    next: usize,
}

struct Group<T> {
    /// The step of the request that set the group, 0 for `previous`'s.
    since: usize,
    /// The objects, each with its output position.
    objects: Vec<(usize, T)>,
}

impl<K: Ord, T: Clone> Keyed<K, T> {
    fn new(objects: &[T], key: impl Fn(&T) -> K) -> Self {
        let mut groups: BTreeMap<K, Group<T>> = BTreeMap::new();
        for (position, object) in objects.iter().enumerate() {
            groups
                .entry(key(object))
                .or_insert_with(|| Group {
                    since: 0,
                    objects: Vec::new(),
                })
                .objects
                .push((position, object.clone()));
        }
        Self {
            groups,
            next: objects.len(),
        }
    }

    /// The group under `key` becomes `wanted`, set at `step`. Without
    /// anything wanted there is nothing to record, and the group stays.
    fn set(&mut self, key: K, wanted: Option<&Vec<&T>>, step: usize) {
        let Some(wanted) = wanted else {
            return;
        };
        let mut objects = Vec::with_capacity(wanted.len());
        for object in wanted {
            objects.push((self.next, (*object).clone()));
            self.next += 1;
        }
        self.groups.insert(
            key,
            Group {
                since: step,
                objects,
            },
        );
    }

    fn remove(&mut self, key: &K) {
        self.groups.remove(key);
    }

    /// The objects `keep` returns for each group, in output order.
    fn into_objects(
        self,
        mut keep: impl FnMut(&K, usize, Vec<(usize, T)>) -> Vec<(usize, T)>,
    ) -> Vec<T> {
        let mut out: Vec<(usize, T)> = self
            .groups
            .into_iter()
            .flat_map(|(key, group)| keep(&key, group.since, group.objects))
            .collect();
        out.sort_by_key(|(position, _)| *position);
        out.into_iter().map(|(_, object)| object).collect()
    }
}

/// What `desired` holds under each key.
fn wanted<K: Ord, T>(objects: &[T], key: impl Fn(&T) -> K) -> BTreeMap<K, Vec<&T>> {
    let mut out: BTreeMap<K, Vec<&T>> = BTreeMap::new();
    for object in objects {
        out.entry(key(object)).or_default().push(object);
    }
    out
}

fn backend_key(b: &ir::Backend) -> BackendKey {
    (b.cluster_id.clone(), b.backend_id.clone(), plain(b.address))
}

fn l4_key(f: &ir::L4Frontend) -> L4Key {
    (f.protocol, plain(f.listener))
}

fn route_key(https: bool, payload: &RequestHttpFrontend) -> ir::SozuRouteKey {
    ir::SozuRouteKey {
        https,
        key: payload.to_string(),
    }
}

/// A leaf's fingerprint, as [`crate::fingerprint`] spells it.
fn fingerprint_of(pem: &str) -> Option<String> {
    sozu_command_lib::certificate::calculate_fingerprint(pem.as_bytes())
        .ok()
        .map(|bytes| hex(&bytes))
}

fn certificate_key(c: &ir::Certificate) -> CertificateKey {
    fingerprint_of(&c.certificate).map(|fp| (plain(c.listener), fp))
}

fn names_a_certificate(request: &Request) -> bool {
    matches!(
        request.request_type,
        Some(
            RequestType::AddCertificate(_)
                | RequestType::RemoveCertificate(_)
                | RequestType::ReplaceCertificate(_)
        )
    )
}

struct Replay<'a> {
    clusters: Keyed<String, ir::Cluster>,
    backends: Keyed<BackendKey, ir::Backend>,
    frontends: Keyed<ir::SozuRouteKey, ir::Frontend>,
    l4: Keyed<L4Key, ir::L4Frontend>,
    certificates: Keyed<CertificateKey, ir::Certificate>,
    wanted_clusters: BTreeMap<String, Vec<&'a ir::Cluster>>,
    wanted_backends: BTreeMap<BackendKey, Vec<&'a ir::Backend>>,
    wanted_frontends: BTreeMap<ir::SozuRouteKey, Vec<&'a ir::Frontend>>,
    wanted_l4: BTreeMap<L4Key, Vec<&'a ir::L4Frontend>>,
    wanted_certificates: BTreeMap<CertificateKey, Vec<&'a ir::Certificate>>,
    /// The step of each cluster's last acknowledged `RemoveCluster`.
    removed_clusters: BTreeMap<String, usize>,
    /// The step of the last acknowledged layer-4 frontend removal per address.
    released: BTreeMap<L4Key, usize>,
    /// The step of the last acknowledged layer-4 listener add per address.
    listener_added: BTreeMap<L4Key, usize>,
    /// The step of the last acknowledged layer-4 `RemoveListener` per address.
    listener_removed: BTreeMap<L4Key, usize>,
}

impl<'a> Replay<'a> {
    /// `certificates` says whether any request names a certificate; when
    /// none does, they are not fingerprinted at all.
    fn new(previous: &ir::Ir, desired: &'a ir::Ir, certificates: bool) -> Self {
        let cert_key = |c: &ir::Certificate| certificates.then(|| certificate_key(c)).flatten();
        Self {
            clusters: Keyed::new(&previous.clusters, |c| c.id.clone()),
            backends: Keyed::new(&previous.backends, backend_key),
            frontends: Keyed::new(&previous.frontends, ir::Frontend::sozu_route_key),
            l4: Keyed::new(&previous.l4_frontends, l4_key),
            certificates: Keyed::new(&previous.certificates, cert_key),
            wanted_clusters: wanted(&desired.clusters, |c| c.id.clone()),
            wanted_backends: wanted(&desired.backends, backend_key),
            wanted_frontends: wanted(&desired.frontends, ir::Frontend::sozu_route_key),
            wanted_l4: wanted(&desired.l4_frontends, l4_key),
            wanted_certificates: wanted(&desired.certificates, cert_key),
            removed_clusters: BTreeMap::new(),
            released: BTreeMap::new(),
            listener_added: BTreeMap::new(),
            listener_removed: BTreeMap::new(),
        }
    }

    /// Replay one acknowledged request, the `step`th of the batch (from 1).
    fn apply(&mut self, step: usize, request: &Request) {
        let Some(request) = &request.request_type else {
            return;
        };
        match request {
            RequestType::AddCluster(c) => {
                let wanted = self.wanted_clusters.get(&c.cluster_id);
                self.clusters.set(c.cluster_id.clone(), wanted, step);
            }
            RequestType::RemoveCluster(id) => {
                self.clusters.remove(id);
                self.removed_clusters.insert(id.clone(), step);
            }
            RequestType::AddBackend(b) => {
                let key = (
                    b.cluster_id.clone(),
                    b.backend_id.clone(),
                    plain(b.address.into()),
                );
                let wanted = self.wanted_backends.get(&key);
                self.backends.set(key, wanted, step);
            }
            RequestType::RemoveBackend(b) => {
                let key = (
                    b.cluster_id.clone(),
                    b.backend_id.clone(),
                    plain(b.address.into()),
                );
                self.backends.remove(&key);
            }
            RequestType::AddHttpFrontend(f) | RequestType::AddHttpsFrontend(f) => {
                let https = matches!(request, RequestType::AddHttpsFrontend(_));
                let key = route_key(https, f);
                let wanted = self.wanted_frontends.get(&key);
                self.frontends.set(key, wanted, step);
            }
            RequestType::RemoveHttpFrontend(f) => self.frontends.remove(&route_key(false, f)),
            RequestType::RemoveHttpsFrontend(f) => self.frontends.remove(&route_key(true, f)),
            RequestType::AddTcpFrontend(f) => {
                self.set_l4((ir::L4Protocol::Tcp, plain(f.address.into())), step)
            }
            RequestType::AddUdpFrontend(f) => {
                self.set_l4((ir::L4Protocol::Udp, plain(f.address.into())), step)
            }
            RequestType::RemoveTcpFrontend(f) => {
                let key = (ir::L4Protocol::Tcp, plain(f.address.into()));
                self.released.insert(key, step);
            }
            RequestType::RemoveUdpFrontend(f) => {
                let key = (ir::L4Protocol::Udp, plain(f.address.into()));
                self.released.insert(key, step);
            }
            RequestType::AddTcpListener(l) => {
                let key = (ir::L4Protocol::Tcp, plain(l.address.into()));
                self.listener_added.insert(key, step);
            }
            RequestType::AddUdpListener(l) => {
                let key = (ir::L4Protocol::Udp, plain(l.address.into()));
                self.listener_added.insert(key, step);
            }
            RequestType::RemoveListener(l) => {
                let protocol = match ListenerType::try_from(l.proxy) {
                    Ok(ListenerType::Tcp) => ir::L4Protocol::Tcp,
                    Ok(ListenerType::Udp) => ir::L4Protocol::Udp,
                    // HTTP/HTTPS listeners are static, never in the IR.
                    _ => return,
                };
                self.listener_removed
                    .insert((protocol, plain(l.address.into())), step);
            }
            RequestType::AddCertificate(add) => {
                if let Some(fp) = fingerprint_of(&add.certificate.certificate) {
                    self.set_certificate(Some((plain(add.address.into()), fp)), step);
                }
            }
            RequestType::RemoveCertificate(remove) => {
                let fp = remove.fingerprint.to_ascii_lowercase();
                self.certificates
                    .remove(&Some((plain(remove.address.into()), fp)));
            }
            RequestType::ReplaceCertificate(replace) => {
                // Sōzu parsed the new certificate to acknowledge the replace,
                // so this cannot fail; if it did, keep the old one rather
                // than guess.
                let Some(new) = fingerprint_of(&replace.new_certificate.certificate) else {
                    return;
                };
                let address = plain(replace.address.into());
                let old = replace.old_fingerprint.to_ascii_lowercase();
                if old != new {
                    self.certificates.remove(&Some((address, old)));
                }
                self.set_certificate(Some((address, new)), step);
            }
            // Listener activation has no IR spelling, and anything else
            // changes no routing object.
            _ => {}
        }
    }

    fn set_l4(&mut self, key: L4Key, step: usize) {
        let wanted = self.wanted_l4.get(&key);
        self.l4.set(key, wanted, step);
    }

    fn set_certificate(&mut self, key: CertificateKey, step: usize) {
        let wanted = self.wanted_certificates.get(&key);
        self.certificates.set(key, wanted, step);
    }

    fn finish(self) -> ir::Ir {
        let Self {
            clusters,
            backends,
            frontends,
            l4,
            certificates,
            removed_clusters,
            released,
            listener_added,
            listener_removed,
            ..
        } = self;
        // Whether `cluster_id` was removed after the group that names it was set.
        let removed_since = |cluster_id: &str, since: usize| {
            removed_clusters
                .get(cluster_id)
                .is_some_and(|&at| at > since)
        };
        let listener_gone = |key: &L4Key| match (listener_removed.get(key), listener_added.get(key))
        {
            (Some(removed), Some(added)) => removed > added,
            (Some(_), None) => true,
            (None, _) => false,
        };
        ir::Ir {
            clusters: clusters.into_objects(|_, _, objects| objects),
            frontends: frontends.into_objects(|_, since, mut objects| {
                objects.retain(|(_, f)| {
                    !f.cluster_id
                        .as_deref()
                        .is_some_and(|id| removed_since(id, since))
                });
                objects
            }),
            backends: backends.into_objects(|_, since, mut objects| {
                objects.retain(|(_, b)| !removed_since(&b.cluster_id, since));
                objects
            }),
            certificates: certificates.into_objects(|_, _, objects| objects),
            l4_frontends: l4.into_objects(|key, since, objects| {
                let released = released.get(key).is_some_and(|&at| at > since);
                let held: Vec<(usize, ir::L4Frontend)> = objects
                    .iter()
                    .filter(|(_, f)| !released && !removed_since(&f.cluster_id, since))
                    .cloned()
                    .collect();
                if !held.is_empty() || listener_gone(key) {
                    held
                } else {
                    // Sōzu no longer holds these frontends but still holds
                    // their listener, which the IR spells only through them.
                    objects
                }
            }),
        }
    }
}

// ----------------------------------------------------------------------------
// The routing state an acknowledged head leaves
// ----------------------------------------------------------------------------

/// The main process's routing state once the agent got `acknowledged`
/// acknowledged from one holding `previous`'s, in the form [`comparable`]
/// gives it. `None` when that state cannot be built or a request's
/// acknowledgement cannot be explained.
fn simulate(previous: &ir::Ir, acknowledged: &[Request]) -> Option<ConfigState> {
    let mut state = routing_state(previous).ok()?;
    for request in acknowledged {
        if names_a_certificate(request) {
            continue;
        }
        if !settle(&mut state, request)? {
            return None;
        }
    }
    Some(comparable(state))
}

/// Dispatch one acknowledged request to `state` the way the agent got it
/// acknowledged (see `sozu_gw_agent::SozuAgent::apply`): whether that
/// explains the acknowledgement, `None` for a request that carries nothing.
/// - A teardown of something `state` does not hold (`NotFound`, `NoChange`)
///   was settled as already gone. For a frontend, the agent then evicts
///   whatever another cluster still stores under the key; Sōzu 2.3 answers
///   that case `Ok` ("nothing to remove") where the library refuses it, and
///   2.2.1 removes an HTTP route by key alone: the same end state.
/// - A listener add `state` already holds (`Exists`) was settled as applied.
/// - A frontend add refused while `state` holds its key (`Exists`; for UDP
///   also 2.3's refusal of an address another cluster holds) was repaired:
///   the holder evicted, the add sent again.
///
/// Anything else that `state` refuses was not something the agent settles.
fn settle(state: &mut ConfigState, request: &Request) -> Option<bool> {
    let kind = request.request_type.as_ref()?;
    let refusal = match state.dispatch(request) {
        Ok(()) => return Some(true),
        Err(refusal) => refusal,
    };
    let settled = match (&refusal, kind) {
        (StateError::NotFound { .. } | StateError::NoChange, _) if is_teardown(kind) => {
            evict(state, kind);
            true
        }
        (
            StateError::Exists { .. },
            RequestType::AddTcpListener(_) | RequestType::AddUdpListener(_),
        ) => true,
        (
            StateError::Exists { .. },
            RequestType::AddHttpFrontend(_)
            | RequestType::AddHttpsFrontend(_)
            | RequestType::AddTcpFrontend(_),
        )
        | (_, RequestType::AddUdpFrontend(_)) => {
            evict(state, kind) && state.dispatch(request).is_ok()
        }
        _ => false,
    };
    Some(settled)
}

fn is_teardown(kind: &RequestType) -> bool {
    matches!(
        kind,
        RequestType::RemoveCluster(_)
            | RequestType::RemoveBackend(_)
            | RequestType::RemoveHttpFrontend(_)
            | RequestType::RemoveHttpsFrontend(_)
            | RequestType::RemoveTcpFrontend(_)
            | RequestType::RemoveUdpFrontend(_)
            | RequestType::RemoveListener(_)
            | RequestType::RemoveCertificate(_)
            | RequestType::DeactivateListener(_)
    )
}

/// Remove whatever `state` stores under this frontend request's key,
/// whichever cluster it names, as the agent's eviction does: the route key
/// for HTTP/HTTPS, the address with the same SNI and ALPN claim for TCP, the
/// address for UDP. Whether anything was there; `false` for any other
/// request.
fn evict(state: &mut ConfigState, kind: &RequestType) -> bool {
    match kind {
        RequestType::AddHttpFrontend(f) | RequestType::RemoveHttpFrontend(f) => {
            state.http_fronts.remove(&f.to_string()).is_some()
        }
        RequestType::AddHttpsFrontend(f) | RequestType::RemoveHttpsFrontend(f) => {
            state.https_fronts.remove(&f.to_string()).is_some()
        }
        RequestType::AddTcpFrontend(f) | RequestType::RemoveTcpFrontend(f) => {
            let address: SocketAddr = f.address.into();
            let claim = tcp_claim(f);
            let mut evicted = false;
            for fronts in state.tcp_fronts.values_mut() {
                fronts.retain(|held| {
                    let holds = held.address == address
                        && (held.sni.as_deref(), held.alpn.as_slice())
                            == (claim.0.as_deref(), claim.1.as_slice());
                    evicted |= holds;
                    !holds
                });
            }
            evicted
        }
        RequestType::AddUdpFrontend(f) | RequestType::RemoveUdpFrontend(f) => {
            let address: SocketAddr = f.address.into();
            let mut evicted = false;
            for fronts in state.udp_fronts.values_mut() {
                fronts.retain(|held| {
                    let holds = held.address == address;
                    evicted |= holds;
                    !holds
                });
            }
            evicted
        }
        _ => false,
    }
}

/// A TCP frontend's SNI and ALPN as Sōzu stores them: SNI lowercased, ALPN
/// sorted and deduplicated.
fn tcp_claim(front: &RequestTcpFrontend) -> (Option<String>, Vec<String>) {
    let mut alpn = front.alpn.clone();
    alpn.sort();
    alpn.dedup();
    (front.sni.as_deref().map(str::to_ascii_lowercase), alpn)
}

/// `state` reduced to what it routes with, so that two states holding the
/// same objects compare equal however they got there: no request census, no
/// emptied per-cluster bucket, each bucket in one order.
fn comparable(mut state: ConfigState) -> ConfigState {
    state.request_counts.clear();
    state.backends.retain(|_, held| !held.is_empty());
    state.tcp_fronts.retain(|_, held| !held.is_empty());
    state.udp_fronts.retain(|_, held| !held.is_empty());
    for held in state.backends.values_mut() {
        held.sort_by_cached_key(|b| serde_json::to_string(b).unwrap_or_default());
    }
    for held in state.tcp_fronts.values_mut() {
        held.sort_by_cached_key(|f| serde_json::to_string(f).unwrap_or_default());
    }
    for held in state.udp_fronts.values_mut() {
        held.sort_by_cached_key(|f| serde_json::to_string(f).unwrap_or_default());
    }
    state
}
