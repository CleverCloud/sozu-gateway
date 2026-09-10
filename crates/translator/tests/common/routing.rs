//! Command replay and lookup semantics from Sōzu 2.2.1 router/mod.rs.
use sozu_command_lib::proto::command::{
    request::RequestType, PathRuleKind, Request, RequestHttpFrontend, RulePosition,
};

#[derive(Default)]
pub struct RoutingTable(Vec<RequestHttpFrontend>);

impl RoutingTable {
    pub fn apply(&mut self, requests: Vec<Request>) {
        for request in requests {
            match request.request_type {
                Some(RequestType::AddHttpFrontend(f) | RequestType::AddHttpsFrontend(f)) => {
                    assert!(
                        !self.0.iter().any(|old| same_key(old, &f)),
                        "duplicate frontend"
                    );
                    self.0.push(f);
                }
                Some(RequestType::RemoveHttpFrontend(f) | RequestType::RemoveHttpsFrontend(f)) => {
                    let index = self
                        .0
                        .iter()
                        .position(|old| same_key(old, &f))
                        .expect("existing frontend");
                    self.0.remove(index);
                }
                _ => {}
            }
        }
    }

    pub fn backend(&self, path: &str, method: &str) -> &str {
        self.backend_for(None, path, method)
    }

    pub fn backend_for(&self, hostname: Option<&str>, path: &str, method: &str) -> &str {
        // TREE picks a hostname before considering paths, preferring exact
        // names to its single-label wildcard. POST is scanned only afterward.
        let tree_host = self
            .0
            .iter()
            .filter(|f| f.position == RulePosition::Tree as i32)
            .filter(|f| hostname.is_none_or(|h| hostname_matches(&f.hostname, h)))
            .max_by_key(|f| (!f.hostname.starts_with("*."), f.hostname.len()))
            .map(|f| f.hostname.as_str());
        let tree = self.0.iter().filter(|f| {
            f.position == RulePosition::Tree as i32 && Some(f.hostname.as_str()) == tree_host
        });
        let post = self.0.iter().filter(|f| {
            f.position == RulePosition::Post as i32
                && hostname.is_none_or(|h| hostname_matches(&f.hostname, h))
        });
        let mut matched = None;
        let mut prefix_length = 0;
        for f in tree.chain(post) {
            if f.method.as_deref().is_some_and(|m| m != method) {
                continue;
            }
            let matches = match f.path.kind() {
                PathRuleKind::Equals => path == f.path.value,
                PathRuleKind::Prefix => path.starts_with(&f.path.value),
                PathRuleKind::Regex => regex::Regex::new(&f.path.value).unwrap().is_match(path),
            };
            if !matches {
                continue;
            }
            if f.position == RulePosition::Post as i32 {
                return matched.unwrap_or_else(|| f.cluster_id.as_deref().unwrap());
            }
            match f.path.kind() {
                PathRuleKind::Equals | PathRuleKind::Regex => {
                    if f.method.is_some() {
                        return f.cluster_id.as_deref().unwrap();
                    }
                    prefix_length = path.len();
                    matched = f.cluster_id.as_deref();
                }
                PathRuleKind::Prefix if f.path.value.len() >= prefix_length => {
                    prefix_length = f.path.value.len();
                    matched = f.cluster_id.as_deref();
                }
                _ => {}
            }
        }
        matched.expect("matching frontend")
    }
}

fn same_key(a: &RequestHttpFrontend, b: &RequestHttpFrontend) -> bool {
    // Sōzu 2.2.1 PathRule::eq omits Equals: a worker can never remove it.
    a.path.kind() != PathRuleKind::Equals
        && a.address == b.address
        && a.hostname == b.hostname
        && a.path == b.path
        && a.method == b.method
}

// Sōzu DomainRule regex syntax wraps the full expression in slashes and
// anchors it with \A and \z. Plain *.names use its single-label wildcard.
fn hostname_matches(pattern: &str, hostname: &str) -> bool {
    if pattern == "*" {
        true
    } else if let Some(regex) = pattern.strip_prefix('/').and_then(|p| p.strip_suffix('/')) {
        regex::Regex::new(&format!(r"\A{regex}\z"))
            .unwrap()
            .is_match(hostname)
    } else if let Some(suffix) = pattern.strip_prefix('*') {
        hostname
            .strip_suffix(suffix)
            .is_some_and(|p| !p.is_empty() && !p.contains('.'))
    } else {
        pattern == hostname
    }
}
