//! Kubernetes Events for reported problems.
//!
//! The builder's honesty rule reports every feature gap and broken reference
//! as a [`Problem`], but until now those only reached controller logs — which
//! the users who own the Ingress/Route usually cannot read. Events are the
//! self-service channel: `kubectl describe` on the object shows *why* it is
//! not (fully) served, in the owner's namespace, with no cluster access
//! needed.
//!
//! **Spam control:** a global reconcile re-derives every problem on every
//! pass (debounce, resync), so publishing unconditionally would flood etcd.
//! Problems are therefore diffed against the previous pass and only *new*
//! ones publish an Event; an unchanged problem set publishes nothing. (The
//! kube Recorder additionally aggregates identical events into series
//! server-side, but only within a short TTL — the diff is the primary
//! control.) A problem that clears and later reappears re-publishes: that is
//! a new occurrence, users should see it again.
//!
//! **Best-effort:** like status writes, a publish failure is logged and
//! swallowed — events can never break routing.
//!
//! Every problem an *object* owns is published on that object, whatever its
//! kind: the route kinds carry theirs on the route, which is why the owner's
//! kind travels in the build result instead of being a literal here. The one
//! exception is the `tcp/udp-services` ConfigMap path, whose "owner" is a map
//! entry rather than an object anybody can describe — those problems reach
//! controller logs only.

use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::core::v1::ObjectReference;
use kube::runtime::events::{Event, EventType, Recorder, Reporter};
use kube::Client;
use sozu_gw_builder::{BuildOutput, Problem};
use tracing::debug;

const GW_API_VERSION: &str = "gateway.networking.k8s.io/v1";

/// A namespaced object that owns problems (Ingress, Gateway or a route).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Owner {
    api_version: &'static str,
    kind: &'static str,
    namespace: String,
    name: String,
    /// The object's `metadata.uid`. `kubectl describe` looks an object's events
    /// up with a field selector that includes `involvedObject.uid`, so an Event
    /// published without it exists and is queryable by name yet never shows up
    /// where its owner would look. `None` only when the source object somehow
    /// carried no uid; the Event is still published, just without it.
    uid: Option<String>,
}

impl Owner {
    fn reference(&self) -> ObjectReference {
        ObjectReference {
            api_version: Some(self.api_version.to_string()),
            kind: Some(self.kind.to_string()),
            namespace: Some(self.namespace.clone()),
            name: Some(self.name.clone()),
            uid: self.uid.clone(),
            ..Default::default()
        }
    }
}

/// One problem, rendered for publication: (Event reason, human message).
type Rendered = (String, String);

/// Publisher with the memory of the previous pass (the spam control).
pub struct ProblemEvents {
    recorder: Recorder,
    seen: BTreeMap<Owner, BTreeSet<Rendered>>,
}

impl ProblemEvents {
    pub fn new(client: Client) -> Self {
        let reporter = Reporter {
            controller: "sozu-gateway-controller".into(),
            instance: None,
        };
        Self {
            recorder: Recorder::new(client, reporter),
            seen: BTreeMap::new(),
        }
    }

    /// Publish a Warning Event for every problem that is new since the
    /// previous pass, then remember the current pass as the baseline.
    pub async fn publish_new(&mut self, out: &BuildOutput) {
        let desired = collect_problems(out);
        for (owner, problems) in &desired {
            let previously = self.seen.get(owner);
            for (reason, note) in problems {
                if previously.is_some_and(|seen| seen.contains(&(reason.clone(), note.clone()))) {
                    continue;
                }
                let event = Event {
                    type_: EventType::Warning,
                    reason: reason.clone(),
                    note: Some(note.clone()),
                    action: "Reconcile".into(),
                    secondary: None,
                };
                if let Err(e) = self.recorder.publish(&event, &owner.reference()).await {
                    debug!(
                        kind = owner.kind,
                        namespace = %owner.namespace,
                        name = %owner.name,
                        error = %e,
                        "failed to publish problem event (best-effort; needs events.k8s.io create/patch RBAC)"
                    );
                }
            }
        }
        self.seen = desired;
    }
}

/// The desired (owner → problems) map for one build output. Pure, so the
/// diff-not-flood behaviour is testable without an apiserver.
fn collect_problems(out: &BuildOutput) -> BTreeMap<Owner, BTreeSet<Rendered>> {
    let mut map: BTreeMap<Owner, BTreeSet<Rendered>> = BTreeMap::new();
    let mut add = |owner: Owner, problems: &[Problem]| {
        if problems.is_empty() {
            return;
        }
        let rendered = problems
            .iter()
            .map(|p| (p.reason().to_string(), p.to_string()));
        map.entry(owner).or_default().extend(rendered);
    };

    for r in &out.results {
        add(
            Owner {
                api_version: "networking.k8s.io/v1",
                kind: "Ingress",
                namespace: r.namespace.clone(),
                name: r.name.clone(),
                uid: r.uid.clone(),
            },
            &r.problems,
        );
    }
    for gw in &out.gateways {
        add(
            Owner {
                api_version: GW_API_VERSION,
                kind: "Gateway",
                namespace: gw.namespace.clone(),
                name: gw.name.clone(),
                uid: gw.uid.clone(),
            },
            &gw.problems,
        );
    }
    for route in &out.routes {
        for parent in &route.parents {
            add(
                Owner {
                    api_version: GW_API_VERSION,
                    kind: route.kind.as_str(),
                    namespace: route.namespace.clone(),
                    name: route.name.clone(),
                    uid: route.uid.clone(),
                },
                &parent.problems,
            );
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use sozu_gw_builder::IngressResult;

    fn out_with_ingress_problem(service: &str) -> BuildOutput {
        BuildOutput {
            ir: Default::default(),
            results: vec![IngressResult {
                namespace: "demo".into(),
                name: "web".into(),
                uid: Some("11111111-2222-3333-4444-555555555555".into()),
                problems: vec![Problem::ServiceNotFound {
                    service: service.into(),
                }],
            }],
            gateway_classes: vec![],
            gateways: vec![],
            routes: vec![],
            l4_results: vec![],
            referenced_services: Default::default(),
        }
    }

    #[test]
    fn problems_map_onto_their_owning_object() {
        let map = collect_problems(&out_with_ingress_problem("demo/web"));
        assert_eq!(map.len(), 1);
        let (owner, problems) = map.first_key_value().expect("one owner");
        assert_eq!(owner.kind, "Ingress");
        assert_eq!(owner.namespace, "demo");
        let (reason, note) = problems.first().expect("one problem");
        assert_eq!(reason, "ServiceNotFound");
        assert!(note.contains("demo/web"), "the note carries the detail");
    }

    /// `kubectl describe` selects an object's events on `involvedObject.uid`
    /// as well as its name, so an Event published without the uid never shows
    /// up on the object it is about.
    #[test]
    fn event_reference_carries_the_object_uid() {
        let map = collect_problems(&out_with_ingress_problem("demo/web"));
        let (owner, _) = map.first_key_value().expect("one owner");
        let reference = owner.reference();

        assert_eq!(
            reference.uid.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(reference.kind.as_deref(), Some("Ingress"));
        assert_eq!(
            reference.api_version.as_deref(),
            Some("networking.k8s.io/v1")
        );
        assert_eq!(reference.namespace.as_deref(), Some("demo"));
        assert_eq!(reference.name.as_deref(), Some("web"));
    }

    /// An object without a uid is unusual but must not stop the Event: the
    /// reference is published without it rather than dropped.
    #[test]
    fn missing_uid_still_publishes_a_reference() {
        let mut out = out_with_ingress_problem("demo/web");
        out.results[0].uid = None;
        let map = collect_problems(&out);
        let (owner, _) = map.first_key_value().expect("one owner");
        let reference = owner.reference();

        assert!(reference.uid.is_none());
        assert_eq!(reference.name.as_deref(), Some("web"));
    }

    #[test]
    fn problem_free_objects_produce_no_entries() {
        let mut out = out_with_ingress_problem("demo/web");
        out.results[0].problems.clear();
        assert!(collect_problems(&out).is_empty());
    }

    /// The diff-not-flood contract: an unchanged problem set must publish
    /// nothing on the next pass; a new problem must publish exactly itself.
    /// Exercised through the pure map + the `seen` baseline the way
    /// `publish_new` consumes them.
    #[test]
    fn only_new_problems_survive_the_baseline_diff() {
        let pass1 = collect_problems(&out_with_ingress_problem("demo/web"));
        // Same problems next pass: everything is filtered by the baseline.
        let pass2 = collect_problems(&out_with_ingress_problem("demo/web"));
        for (owner, problems) in &pass2 {
            let seen = pass1.get(owner).expect("same owner");
            assert!(problems.iter().all(|p| seen.contains(p)));
        }
        // A different problem is new against the same baseline.
        let pass3 = collect_problems(&out_with_ingress_problem("demo/other"));
        let (owner, problems) = pass3.first_key_value().expect("one owner");
        let seen = pass1.get(owner).expect("same owner");
        assert!(problems.iter().any(|p| !seen.contains(p)));
    }
}
