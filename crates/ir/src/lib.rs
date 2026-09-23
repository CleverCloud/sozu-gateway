//! Intermediate Representation (IR) for the Sōzu gateway controller.
//!
//! Neutral, I/O-free Rust structures mapped 1:1 onto Sōzu's routing vocabulary.
//! The Builder produces this from Kubernetes objects; the Translator consumes it
//! to emit Sōzu protobuf commands. This crate depends on neither `kube` nor the
//! command socket, so it is unit-testable in isolation.
//!
//! Listeners are intentionally **not** modelled here: in Phase 1 they are
//! declared statically in Sōzu's `config.toml` and activated at boot, so the
//! controller only manages clusters / frontends / backends / certificates.
#![forbid(unsafe_code)]

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Load-balancing algorithm for a cluster (the subset meaningful in Phase 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LbAlgorithm {
    #[default]
    RoundRobin,
    Random,
    LeastLoaded,
    PowerOfTwo,
}

/// How an Ingress path is matched, mapped from Kubernetes `pathType`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PathMatch {
    /// `pathType: Prefix` (Ingress) / `PathPrefix` (Gateway API), with
    /// Kubernetes' **element-boundary** semantics: `/foo` matches `/foo`,
    /// `/foo?q=1` and `/foo/bar` but never `/foobar`. That is narrower than a
    /// raw string prefix, so the translator compiles a non-root prefix to an
    /// anchored regex rather than to Sōzu's own `Prefix` rule.
    ///
    /// The trailing slash is insignificant (`/foo/` ≡ `/foo`) and the builder
    /// canonicalises it away, so one path has one spelling here.
    Prefix(String),
    /// `pathType: Exact`
    Exact(String),
    /// `pathType: ImplementationSpecific` → regex
    Regex(String),
}

/// The Sōzu path rule a [`PathMatch`] compiles to: what the translator emits,
/// and the rule half of the key Sōzu stores a route under
/// ([`Frontend::sozu_route_key`]) — one definition, so the builder's collision
/// arbitration and the translator can never disagree about what collides.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SozuPathRule {
    /// A plain string prefix — only ever the root `/`.
    Prefix(String),
    /// An anchored regex, Sōzu's `PathRuleKind::Regex`.
    Regex(String),
}

impl PathMatch {
    /// Compile to the Sōzu rule with **Kubernetes** semantics. Two measured
    /// facts about Sōzu 2.2.x drive the shape: path rules are matched against
    /// the request target with the query string still attached, and a `Regex`
    /// rule is `is_match`ed unanchored. Sōzu's next router (unreleased, `main`
    /// at 47eb07c) instead wraps every `Regex` value in `\A(?:…)\z`, so a rule
    /// must span the whole target. Every pattern emitted here is therefore
    /// written **full-span** — anchored at both ends by us, with an explicit
    /// tail consuming the remainder — so it matches the same targets under
    /// both routers and the data-plane upgrade needs no translator change
    /// (see `sozu_rules_match_the_same_targets_unanchored_and_anchored`).
    ///
    /// The tail is `(?-u:.*)`: any byte but LF, not any *UTF-8 character*.
    /// Sōzu compiles with `regex::bytes`, whose `.` in the default Unicode
    /// mode skips a byte that is not valid UTF-8, and a request target may
    /// carry one (kawa's `tolerant-http1-parser` admits `0xA0`–`0xFF`). The
    /// previous patterns stopped at the boundary and so never looked at the
    /// remainder; the tail must not be narrower than that. LF is the one byte
    /// it still excludes, and neither HTTP parser lets one into a target.
    ///
    /// - `Prefix`: element-boundary matching. `/foo` covers `/foo`, `/foo?q`
    ///   and `/foo/bar` but never `/foobar`, so a non-root prefix is a regex
    ///   whose remainder starts with `/` or `?` — Sōzu's own `Prefix` is a raw
    ///   `starts_with`. The root `/` stays a plain prefix (every target starts
    ///   with it, and it is cheaper). A trailing slash is insignificant and
    ///   trimmed, so `/foo/` ≡ `/foo`.
    /// - `Exact`: the whole path, query string allowed. Sōzu's `Equals` compares
    ///   the query-bearing target literally, so `/get?x=1` would not match
    ///   `Equals("/get")` — and Sōzu 2.2.x cannot remove an `Equals` rule it
    ///   holds (its rule equality has no `Equals` arm; fixed upstream after
    ///   2.2.1), so a route once added as `Equals` kept matching after its
    ///   removal was acknowledged. A regex has neither problem. The trailing
    ///   slash is kept literal: Exact means exact.
    /// - `Regex`: the user's own pattern, verbatim (validated by the builder).
    ///   Its meaning *does* change with the router: unanchored today,
    ///   full-span tomorrow. That is documented for users, not papered over.
    pub fn sozu_rule(&self) -> SozuPathRule {
        match self {
            PathMatch::Regex(v) => SozuPathRule::Regex(v.clone()),
            PathMatch::Exact(v) => {
                SozuPathRule::Regex(format!("^{}(?:\\?{REMAINDER})?$", regex_escape(v)))
            }
            PathMatch::Prefix(v) => {
                let trimmed = v.trim_end_matches('/');
                if trimmed.is_empty() {
                    SozuPathRule::Prefix("/".to_string())
                } else {
                    SozuPathRule::Regex(format!("^{}(?:[/?]{REMAINDER})?$", regex_escape(trimmed)))
                }
            }
        }
    }
}

/// The rest of a request target after a matched boundary: any bytes but LF,
/// in Sōzu's `regex::bytes` flavour (`-u` so `.` is one byte, not one UTF-8
/// character; see [`PathMatch::sozu_rule`]).
const REMAINDER: &str = "(?-u:.*)";

/// Escape every regex metacharacter so a literal path is matched literally.
fn regex_escape(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    for c in literal.chars() {
        if r"\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A routing target: one Sōzu cluster, typically one per Service:port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cluster {
    pub id: String,
    pub load_balancing: LbAlgorithm,
    pub sticky_session: bool,
    pub https_redirect: bool,
    /// Max simultaneous connections from one source IP to this cluster; `None`
    /// uses Sōzu's global default. This is a connection cap, not an RPS quota.
    #[serde(default)]
    pub max_connections_per_ip: Option<u64>,
    /// `Retry-After` header (seconds) sent on the `429` when the cap is hit.
    #[serde(default)]
    pub retry_after: Option<u32>,
}

/// One backend endpoint: a **pod IP:port** (never a ClusterIP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backend {
    pub cluster_id: String,
    /// Stable id per endpoint so add/remove are idempotent across resyncs.
    pub backend_id: String,
    pub address: SocketAddr,
    /// Optional weight; `None` means equal weighting (Sōzu default).
    pub weight: Option<i32>,
}

/// A route: hostname + path (+ method) → cluster, on the HTTP or HTTPS listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frontend {
    pub hostname: String,
    pub path: PathMatch,
    pub method: Option<String>,
    /// Target cluster, or `None` for a redirect-only frontend (no backend).
    pub cluster_id: Option<String>,
    /// `true` => HTTPS listener (`AddHttpsFrontend`), `false` => HTTP.
    pub tls: bool,
    /// The listener address this frontend attaches to (e.g. `0.0.0.0:80`).
    pub listener: SocketAddr,
    /// Per-route filters (Phase 3): header edits, redirect, rewrite.
    #[serde(default)]
    pub filters: FrontendFilters,
}

/// The identity Sōzu stores an HTTP(S) frontend under: which frontend map it
/// lands in (`https`, one map per protocol) and the map key itself.
///
/// Sōzu 2.2.1 keys the map on `RequestHttpFrontend`'s `Display`, the
/// **unescaped** string `{address};{hostname};{kind}{rule}[;{method}]`, and
/// rejects an add on an occupied key. Because the rule and method are joined
/// with a bare `;`, two routes that differ as tuples can share one key: a regex
/// `/x;GET` with no method and a regex `/x` restricted to `GET` both key as
/// `…;R/x;GET`. Anything that decides whether two frontends collide must
/// therefore compare this, never a tuple of their fields. The translator's
/// tests pin it to `RequestHttpFrontend::to_string()`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SozuRouteKey {
    /// `true` for Sōzu's HTTPS frontend map, `false` for the HTTP one.
    pub https: bool,
    /// The map key, spelled exactly as Sōzu spells it.
    pub key: String,
}

impl Frontend {
    /// The key Sōzu stores this frontend under; see [`SozuRouteKey`].
    pub fn sozu_route_key(&self) -> SozuRouteKey {
        // Sōzu's wire address carries only the IP and port, so its `Display`
        // never shows an IPv6 flow label or scope id; drop them here too.
        let address = SocketAddr::new(self.listener.ip(), self.listener.port());
        let (kind, rule) = match self.path.sozu_rule() {
            SozuPathRule::Prefix(v) => ('P', v),
            SozuPathRule::Regex(v) => ('R', v),
        };
        let mut key = format!("{address};{};{kind}{rule}", self.hostname);
        if let Some(method) = &self.method {
            key.push(';');
            key.push_str(method);
        }
        SozuRouteKey {
            https: self.tls,
            key,
        }
    }
}

/// Request/response transformations applied to a frontend (Phase 3). Maps onto
/// Sōzu's per-frontend filter fields. Empty by default.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FrontendFilters {
    pub header_mods: Vec<HeaderMod>,
    pub redirect: Option<Redirect>,
    pub rewrite: Option<Rewrite>,
}

/// Append (`value: Some(non_empty)`) or delete (`value: None`) a header on the
/// request or the response. Replacement is a delete followed by an append on
/// the same frontend. An empty value also means delete in Sōzu's legacy wire
/// encoding, so the builder rejects empty Gateway `set` and `add` values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderMod {
    pub on: HeaderTarget,
    pub key: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HeaderTarget {
    Request,
    Response,
}

/// A redirect: the `Location` a matching request is answered with.
///
/// Every field is a *target* — what to replace in the request's own URL. A
/// `None` keeps the request's value, which is Sōzu's `USE_SAME` for the scheme
/// and its behaviour for an unset `rewrite_*`. At least one has to be `Some`,
/// or the `Location` echoes the request and the client loops; the builder
/// refuses that shape rather than programming it.
///
/// The fields are additive, so an older controller reading a shadow that
/// carries them ignores what it does not know — unlike a new enum *variant*,
/// which would fail the whole parse (see `docs/UPGRADING.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redirect {
    pub scheme: Option<Scheme>,
    pub status: RedirectStatus,
    /// Replaces the authority. Sōzu's `rewrite_host`.
    #[serde(default)]
    pub hostname: Option<String>,
    /// Replaces the whole path. Sōzu's `rewrite_path`.
    #[serde(default)]
    pub path: Option<String>,
    /// An explicit port in the `Location`. Sōzu's `rewrite_port`.
    #[serde(default)]
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scheme {
    Http,
    Https,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RedirectStatus {
    /// HTTP 301
    MovedPermanently,
    /// HTTP 302
    Found,
    /// HTTP 308. Unlike 301 it forbids a client rewriting the method to GET,
    /// which is the whole reason an author picks it.
    PermanentRedirect,
}

/// Rewrite the request's host and/or full path before proxying.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rewrite {
    pub hostname: Option<String>,
    pub path: Option<String>,
}

/// A TLS certificate loaded onto the HTTPS listener (from a K8s TLS Secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    /// The HTTPS listener address the cert is bound to (e.g. `0.0.0.0:443`).
    pub listener: SocketAddr,
    /// Leaf certificate, PEM.
    pub certificate: String,
    /// Intermediate chain, PEM (empty for self-signed).
    pub chain: Vec<String>,
    /// Private key, PEM.
    pub key: String,
    /// SNI names to serve this cert for: explicit hostnames, or inferred CN/SAN
    /// names for Gateway listeners without a hostname. An empty list in an
    /// older shadow still delegates inference to Sōzu.
    pub names: Vec<String>,
}

/// Layer-4 protocol for a raw TCP/UDP passthrough route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum L4Protocol {
    Tcp,
    Udp,
}

/// A raw layer-4 route: a listen address forwarded to a cluster's backends with
/// no HTTP parsing. One listen address maps to exactly one cluster — there is no
/// SNI/host multiplexing at L4. The cluster and backends are the same kind as
/// for HTTP (a Service resolved to pod IPs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct L4Frontend {
    pub protocol: L4Protocol,
    /// Address Sōzu listens on for this route (e.g. `0.0.0.0:5432`).
    pub listener: SocketAddr,
    pub cluster_id: String,
}

/// The complete desired routing state compiled from all our Ingress objects.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Ir {
    pub clusters: Vec<Cluster>,
    pub frontends: Vec<Frontend>,
    pub backends: Vec<Backend>,
    pub certificates: Vec<Certificate>,
    /// Raw TCP/UDP routes (L4), from TCPRoute/UDPRoute. Empty otherwise.
    #[serde(default)]
    pub l4_frontends: Vec<L4Frontend>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_matches_compile_to_their_sozu_rules() {
        let regex = |s: &str| SozuPathRule::Regex(s.to_string());
        assert_eq!(
            PathMatch::Prefix("/".into()).sozu_rule(),
            SozuPathRule::Prefix("/".into())
        );
        assert_eq!(
            PathMatch::Prefix("/foo".into()).sozu_rule(),
            regex("^/foo(?:[/?](?-u:.*))?$")
        );
        assert_eq!(
            PathMatch::Prefix("/foo/".into()).sozu_rule(),
            regex("^/foo(?:[/?](?-u:.*))?$")
        );
        assert_eq!(
            PathMatch::Exact("/foo".into()).sozu_rule(),
            regex("^/foo(?:\\?(?-u:.*))?$")
        );
        assert_eq!(
            PathMatch::Exact("/foo/".into()).sozu_rule(),
            regex("^/foo/(?:\\?(?-u:.*))?$")
        );
        assert_eq!(
            PathMatch::Exact("/".into()).sozu_rule(),
            regex("^/(?:\\?(?-u:.*))?$")
        );
        assert_eq!(PathMatch::Regex("^/x$".into()).sozu_rule(), regex("^/x$"));
        assert_eq!(
            PathMatch::Exact("/a.b(c)".into()).sozu_rule(),
            regex("^/a\\.b\\(c\\)(?:\\?(?-u:.*))?$")
        );
    }

    /// The two routers a generated rule must satisfy: Sōzu 2.2.1 runs
    /// `regex::bytes::Regex::new(value).is_match(target)` unanchored, and its
    /// successor wraps the value as `\A(?:value)\z` first (47eb07c). Each
    /// pattern is compiled both ways, on the `regex` version Sōzu builds
    /// against, and must give the same answer on every target — including a
    /// remainder that is not valid UTF-8, which Sōzu's HTTP/1 parser can admit.
    #[test]
    fn sozu_rules_match_the_same_targets_unanchored_and_anchored() {
        /// A request target and whether the rule must match it.
        type Expectation = (&'static [u8], bool);
        let cases: &[(PathMatch, &[Expectation])] = &[
            (
                PathMatch::Prefix("/foo".into()),
                &[
                    (b"/foo", true),
                    (b"/foo/", true),
                    (b"/foo/bar", true),
                    (b"/foo?q=1", true),
                    (b"/foo/bar?q=1", true),
                    (b"/foo/\xff\xfe", true),
                    (b"/foo?\xff", true),
                    (b"/foobar", false),
                    (b"/fo", false),
                    (b"/", false),
                    (b"/x/foo", false),
                    (b"/foo.bar", false),
                ],
            ),
            (
                PathMatch::Exact("/v1".into()),
                &[
                    (b"/v1", true),
                    (b"/v1?x=1", true),
                    (b"/v1?\xff", true),
                    (b"/v1/", false),
                    (b"/v1/x", false),
                    (b"/v1x", false),
                    (b"/v10", false),
                    (b"/", false),
                    (b"/x/v1", false),
                ],
            ),
            (
                PathMatch::Exact("/v1/".into()),
                &[(b"/v1/", true), (b"/v1/?a", true), (b"/v1", false)],
            ),
            (
                PathMatch::Exact("/a.b(c)".into()),
                &[(b"/a.b(c)", true), (b"/aXb(c)", false), (b"/a.bc", false)],
            ),
        ];
        for (path, targets) in cases {
            let SozuPathRule::Regex(value) = path.sozu_rule() else {
                panic!("{path:?} must compile to a regex");
            };
            let unanchored = regex::bytes::Regex::new(&value).expect("2.2.1 accepts the rule");
            let anchored = regex::bytes::Regex::new(&format!("\\A(?:{value})\\z"))
                .expect("the anchored router accepts the rule");
            for (target, expected) in *targets {
                let shown = String::from_utf8_lossy(target);
                assert_eq!(
                    unanchored.is_match(target),
                    *expected,
                    "{path:?} on {shown:?}, unanchored (Sōzu 2.2.1)"
                );
                assert_eq!(
                    anchored.is_match(target),
                    *expected,
                    "{path:?} on {shown:?}, anchored (Sōzu main)"
                );
            }
        }
    }

    #[test]
    fn an_exact_and_a_prefix_on_one_path_are_distinct_rules() {
        assert_ne!(
            PathMatch::Exact("/foo".into()).sozu_rule(),
            PathMatch::Prefix("/foo".into()).sozu_rule()
        );
    }
}
