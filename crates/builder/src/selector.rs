//! Label-selector evaluation for `allowedRoutes.namespaces.selector`.
//!
//! Pure and hand-rolled on purpose. The builder has no `kube` dependency — that
//! is the purity boundary the whole crate layout exists to keep — and a
//! `metav1.LabelSelector` is small enough that reimplementing it costs less than
//! the dependency would.
//!
//! The semantics are Kubernetes', not an approximation of them:
//!  - `matchLabels` and `matchExpressions` are **ANDed**; every term must hold.
//!  - An **empty** selector matches every namespace. That is not a degenerate
//!    case to guard against, it is the documented meaning.
//!  - `In`/`NotIn` need a non-empty `values`; `Exists`/`DoesNotExist` must not
//!    carry one.
//!
//! An operator this build does not know — `operator` is a bare string in the
//! CRD, so a newer Gateway API can introduce one — makes the selector
//! **unevaluable**, and an unevaluable selector fails closed. Guessing at an
//! admission control the Gateway owner set precisely to restrict admission is
//! the one thing that must never happen here.

use std::collections::{BTreeMap, BTreeSet};

use sozu_gw_gateway_api::gateway::{
    GatewayListenersAllowedRoutesNamespacesSelector as ApiSelector,
    GatewayListenersAllowedRoutesNamespacesSelectorMatchExpressions as ApiExpression,
};

/// One requirement a namespace's labels must satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Term {
    Equals {
        key: String,
        value: String,
    },
    In {
        key: String,
        values: BTreeSet<String>,
    },
    NotIn {
        key: String,
        values: BTreeSet<String>,
    },
    Exists {
        key: String,
    },
    DoesNotExist {
        key: String,
    },
}

/// A parsed selector, or the reason it cannot be evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NamespaceSelector {
    /// Every term must hold. Empty means "every namespace".
    Terms(Vec<Term>),
    /// Fails closed, carrying a reason the user can act on.
    Unevaluable(String),
}

impl NamespaceSelector {
    /// Parse a listener's `allowedRoutes.namespaces.selector`.
    ///
    /// `None` is itself unevaluable: the Gateway API requires a selector when
    /// `from: Selector`, so its absence is a Gateway that asked for a
    /// restriction and did not say which.
    pub(crate) fn parse(selector: Option<&ApiSelector>) -> Self {
        let Some(selector) = selector else {
            return NamespaceSelector::Unevaluable(
                "allowedRoutes.namespaces.from is Selector but no selector is set".to_string(),
            );
        };
        let mut terms = Vec::new();
        // A BTreeMap, so the term order is already deterministic.
        for (key, value) in selector.match_labels.iter().flatten() {
            terms.push(Term::Equals {
                key: key.clone(),
                value: value.clone(),
            });
        }
        for expr in selector.match_expressions.iter().flatten() {
            match parse_expression(expr) {
                Ok(term) => terms.push(term),
                Err(reason) => return NamespaceSelector::Unevaluable(reason),
            }
        }
        NamespaceSelector::Terms(terms)
    }

    /// The reason this selector cannot be evaluated, if it cannot.
    pub(crate) fn unevaluable(&self) -> Option<&str> {
        match self {
            NamespaceSelector::Unevaluable(reason) => Some(reason),
            NamespaceSelector::Terms(_) => None,
        }
    }

    /// Do these namespace labels satisfy the selector?
    pub(crate) fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        match self {
            // Fail closed: admit nothing rather than guess at the restriction.
            NamespaceSelector::Unevaluable(_) => false,
            NamespaceSelector::Terms(terms) => terms.iter().all(|term| match term {
                Term::Equals { key, value } => labels.get(key) == Some(value),
                Term::In { key, values } => labels.get(key).is_some_and(|v| values.contains(v)),
                // Kubernetes' NotIn is satisfied by an absent key too: the
                // requirement is "not one of these values", and a namespace
                // without the label holds none of them.
                Term::NotIn { key, values } => labels.get(key).is_none_or(|v| !values.contains(v)),
                Term::Exists { key } => labels.contains_key(key),
                Term::DoesNotExist { key } => !labels.contains_key(key),
            }),
        }
    }
}

fn parse_expression(expr: &ApiExpression) -> Result<Term, String> {
    let values: BTreeSet<String> = expr.values.iter().flatten().cloned().collect();
    let key = expr.key.clone();
    match expr.operator.as_str() {
        "In" | "NotIn" if values.is_empty() => Err(format!(
            "matchExpressions[{key}] operator {:?} needs a non-empty values list",
            expr.operator
        )),
        "In" => Ok(Term::In { key, values }),
        "NotIn" => Ok(Term::NotIn { key, values }),
        "Exists" | "DoesNotExist" if !values.is_empty() => Err(format!(
            "matchExpressions[{key}] operator {:?} must not carry values",
            expr.operator
        )),
        "Exists" => Ok(Term::Exists { key }),
        "DoesNotExist" => Ok(Term::DoesNotExist { key }),
        // `operator` is a bare string in the CRD, so a newer Gateway API can
        // add one. Unknown means unevaluable, which means fail closed.
        other => Err(format!(
            "matchExpressions[{key}] uses operator {other:?}, which this controller does not know"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn selector(v: serde_json::Value) -> ApiSelector {
        serde_json::from_value(v).expect("valid selector json")
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn match_labels_are_anded() {
        let s = NamespaceSelector::parse(Some(&selector(
            json!({ "matchLabels": { "a": "1", "b": "2" } }),
        )));
        assert!(s.matches(&labels(&[("a", "1"), ("b", "2"), ("c", "3")])));
        assert!(!s.matches(&labels(&[("a", "1")])), "every term must hold");
        assert!(!s.matches(&labels(&[("a", "1"), ("b", "x")])));
    }

    /// The case the conformance suite actually uses, and the one the
    /// fail-closed stance used to reject.
    #[test]
    fn the_conformance_shape_matches_by_metadata_name() {
        let s = NamespaceSelector::parse(Some(&selector(
            json!({ "matchLabels": { "gateway-conformance": "backend" } }),
        )));
        assert!(s.matches(&labels(&[
            (
                "kubernetes.io/metadata.name",
                "gateway-conformance-app-backend"
            ),
            ("gateway-conformance", "backend"),
        ])));
        assert!(!s.matches(&labels(&[(
            "kubernetes.io/metadata.name",
            "gateway-conformance-infra"
        )])));
    }

    /// An empty selector selects everything. This is the documented meaning,
    /// not a hole — reading it as "matches nothing" would silently strand every
    /// route on such a listener.
    #[test]
    fn an_empty_selector_matches_every_namespace() {
        for empty in [json!({}), json!({ "matchLabels": {} })] {
            let s = NamespaceSelector::parse(Some(&selector(empty)));
            assert!(s.matches(&labels(&[])));
            assert!(s.matches(&labels(&[("anything", "at-all")])));
        }
    }

    #[test]
    fn set_operators_follow_kubernetes_semantics() {
        let in_ = NamespaceSelector::parse(Some(&selector(json!({
            "matchExpressions": [{ "key": "env", "operator": "In", "values": ["prod", "stage"] }]
        }))));
        assert!(in_.matches(&labels(&[("env", "prod")])));
        assert!(!in_.matches(&labels(&[("env", "dev")])));
        assert!(!in_.matches(&labels(&[])), "In needs the key present");

        let not_in = NamespaceSelector::parse(Some(&selector(json!({
            "matchExpressions": [{ "key": "env", "operator": "NotIn", "values": ["prod"] }]
        }))));
        assert!(not_in.matches(&labels(&[("env", "dev")])));
        assert!(!not_in.matches(&labels(&[("env", "prod")])));
        // An absent key holds none of the values, so NotIn is satisfied.
        assert!(not_in.matches(&labels(&[])));

        let exists = NamespaceSelector::parse(Some(&selector(json!({
            "matchExpressions": [{ "key": "env", "operator": "Exists" }]
        }))));
        assert!(exists.matches(&labels(&[("env", "")])), "any value counts");
        assert!(!exists.matches(&labels(&[])));

        let absent = NamespaceSelector::parse(Some(&selector(json!({
            "matchExpressions": [{ "key": "env", "operator": "DoesNotExist" }]
        }))));
        assert!(absent.matches(&labels(&[])));
        assert!(!absent.matches(&labels(&[("env", "prod")])));
    }

    #[test]
    fn labels_and_expressions_are_anded_together() {
        let s = NamespaceSelector::parse(Some(&selector(json!({
            "matchLabels": { "tier": "web" },
            "matchExpressions": [{ "key": "env", "operator": "In", "values": ["prod"] }]
        }))));
        assert!(s.matches(&labels(&[("tier", "web"), ("env", "prod")])));
        assert!(!s.matches(&labels(&[("tier", "web"), ("env", "dev")])));
        assert!(!s.matches(&labels(&[("env", "prod")])));
    }

    /// Anything we cannot evaluate admits nothing and says why. Guessing at an
    /// admission control the Gateway owner set to *restrict* admission is the
    /// one outcome that must never happen.
    #[test]
    fn what_cannot_be_evaluated_fails_closed_with_a_reason() {
        let cases = [
            // `operator` is a bare string in the CRD: a newer API can add one.
            json!({ "matchExpressions": [{ "key": "env", "operator": "Sorta", "values": ["x"] }] }),
            json!({ "matchExpressions": [{ "key": "env", "operator": "In" }] }),
            json!({ "matchExpressions": [{ "key": "env", "operator": "In", "values": [] }] }),
            json!({ "matchExpressions": [{ "key": "env", "operator": "Exists", "values": ["x"] }] }),
        ];
        for case in cases {
            let s = NamespaceSelector::parse(Some(&selector(case.clone())));
            assert!(s.unevaluable().is_some(), "{case} should be unevaluable");
            assert!(
                !s.matches(&labels(&[("env", "x")])),
                "{case} must admit nothing"
            );
        }

        // `from: Selector` with no selector at all: a restriction was asked for
        // without saying which.
        let missing = NamespaceSelector::parse(None);
        assert!(missing.unevaluable().is_some());
        assert!(!missing.matches(&labels(&[])));
    }
}
