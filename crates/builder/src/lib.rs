//! Builder: Kubernetes objects → IR.
//!
//! Pure and I/O-free: it operates on already-fetched typed objects (the
//! controller fills [`Inputs`] from its reflector caches), resolves references
//! (Service → EndpointSlice pod IPs, TLS Secret → certificate), validates, and
//! produces the [`ir::Ir`] plus a per-Ingress [`IngressResult`] for status
//! reporting. No `kube` client, no socket — so it is fully unit-testable.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use k8s_openapi::api::core::v1::{Namespace, Secret, Service, ServicePort};
use k8s_openapi::api::discovery::v1::{EndpointPort, EndpointSlice};
use k8s_openapi::api::networking::v1::Ingress;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::PrivateKeyDer;
use serde::Serialize;
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute, ReferenceGrant, TcpRoute, UdpRoute};
use sozu_gw_ir as ir;

mod gateway;
mod selector;
pub use gateway::{
    GatewayClassResult, GatewayResult, ListenerStatus, RouteKind, RouteParentResult, RouteResult,
};

const SERVICE_NAME_LABEL: &str = "kubernetes.io/service-name";
const LEGACY_CLASS_ANNOTATION: &str = "kubernetes.io/ingress.class";
/// Service annotation selecting the cluster's load-balancing algorithm:
/// `round-robin` (default) | `random` | `least-loaded` | `power-of-two`.
const LB_ANNOTATION: &str = "sozu.io/load-balancing";
/// Service annotation enabling sticky sessions when set to `"true"`.
const STICKY_ANNOTATION: &str = "sozu.io/sticky-sessions";
/// Service annotation capping simultaneous connections per source IP (u64).
const MAX_CONN_PER_IP_ANNOTATION: &str = "sozu.io/max-connections-per-ip";
/// Service annotation setting the `Retry-After` seconds on the cap's `429` (u32).
const RETRY_AFTER_ANNOTATION: &str = "sozu.io/retry-after";
/// Service annotation turning the cluster's active health check on (`tcp`) or
/// off (`off`). Absent, [`BuildConfig::health_check`] decides.
const HEALTH_CHECK_ANNOTATION: &str = "sozu.io/health-check";
/// Service annotations tuning a check that is on, one field each: seconds for
/// the interval and timeout, probe counts for the thresholds, a port number for
/// the port.
const HEALTH_CHECK_INTERVAL_ANNOTATION: &str = "sozu.io/health-check-interval";
const HEALTH_CHECK_TIMEOUT_ANNOTATION: &str = "sozu.io/health-check-timeout";
const HEALTH_CHECK_HEALTHY_ANNOTATION: &str = "sozu.io/health-check-healthy-threshold";
const HEALTH_CHECK_UNHEALTHY_ANNOTATION: &str = "sozu.io/health-check-unhealthy-threshold";
const HEALTH_CHECK_PORT_ANNOTATION: &str = "sozu.io/health-check-port";
/// Ingress annotation to opt out of the automatic HTTP→HTTPS redirect. The
/// redirect is on by default for any host that has a loaded TLS cert; set this
/// to `"false"` to keep serving plain HTTP.
const SSL_REDIRECT_ANNOTATION: &str = "sozu.io/ssl-redirect";

/// The Gateway API listener protocol one exposed port serves.
///
/// Distinct from the Service port's transport protocol: a Kubernetes Service
/// only knows TCP/UDP/SCTP and the apiserver rejects `protocol: HTTP`, so the
/// chart carries both and this is the one routing cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, serde::Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ExposedProtocol {
    Http,
    Https,
    Tcp,
    Udp,
    Tls,
}

/// One port this gateway exposes.
///
/// The two ports are not interchangeable and both have to be written down.
/// `port` is what a `Gateway` listener declares and what a client dials — the
/// Service's client-facing port. `bind` is where Sōzu actually listens inside
/// the Pod, which cannot be a privileged port: both containers run as uid 1000
/// with every capability dropped, so 80 and 443 are unreachable and the chart's
/// default maps them to 8080 and 8443.
///
/// Deriving one from the other was considered and rejected: a fixed offset
/// reads naturally for 80 → 8080 and absurdly for 5432, and the collision check
/// that matters operates on `bind`, so it must be a value the operator can see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ExposedPort {
    /// Service port name.
    pub name: String,
    /// Advertised port: what a Gateway listener declares, what clients dial.
    pub port: u16,
    /// Port Sōzu listens on inside the Pod. A port, not an address: the bind
    /// interface is always the unspecified one, and the Service's `targetPort`
    /// needs the bare number anyway.
    pub bind: u16,
    pub protocol: ExposedProtocol,
    /// The Service port's transport protocol. Carried so the chart's values and
    /// this struct are the same shape; routing never reads it — `protocol` is
    /// what decides where a listener attaches.
    #[serde(default)]
    pub transport: Option<String>,
    /// Namespace allowed to claim this port. `None` means shared, which is how
    /// HTTP and HTTPS work — they multiplex on hostname, so any number of
    /// Gateways can serve on one. A layer-4 port carries exactly one route and
    /// has no hostname to arbitrate with, so it names its owner instead of
    /// letting two Gateways race for it.
    #[serde(default)]
    pub owner: Option<String>,
}

/// The exposed ports and class identity the build is parameterised over.
#[derive(Debug, Clone)]
pub struct BuildConfig {
    pub class_name: String,
    pub class_is_default: bool,
    /// GatewayClass `controllerName` we own (Gateway API).
    pub controller_name: String,
    /// Every port the gateway exposes, in the operator's declared order.
    ///
    /// This replaces the four scalars that used to encode exactly two ports.
    /// The rule a user meets moves with it: from "your listener's port must be
    /// 80 or 443" to "your `(port, protocol)` must be one this gateway
    /// exposes", which is strictly more permissive.
    pub exposure: Vec<ExposedPort>,
    /// The health check a Service gets when it does not choose one with
    /// `sozu.io/health-check`, and the timing a check starts from.
    pub health_check: HealthCheckDefault,
}

/// The controller-wide health-check default: the `--default-health-check` and
/// `--health-check-*` flags, rendered by the chart from `sozu.healthCheck`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheckDefault {
    /// Whether a Service without `sozu.io/health-check` gets a check.
    pub enabled: bool,
    /// The timing every check starts from, whether the default or the
    /// Service's own annotation turned it on; each `sozu.io/health-check-*`
    /// annotation overrides one field. Never carries a port: a probe port is
    /// a property of one workload, not of the gateway.
    pub check: ir::HealthCheck,
}

impl HealthCheckDefault {
    /// The documented default timing. Measured on 2.3.2
    /// (`docs/probes/health-check_sozu-2.3.2_2026-10-19.txt`): in the recorded
    /// runs a backend that stopped answering under a running check was out of
    /// rotation 6.3 to 9.4 s later, depending on where in the probe cycle it
    /// stopped, and one already black-holed when the check was applied 5.2 to
    /// 5.8 s later.
    ///
    /// `timeout < interval` on purpose. Sōzu starts no probe of a backend
    /// while its previous one is in flight, and decides which probes are due
    /// before it reaps the timed-out ones (v2.3.2 `lib/src/health_check.rs`,
    /// `initiate_checks` before `progress_checks`). With the two equal, a dead
    /// backend's timed-out probe is reaped after its next one was due unless
    /// the cluster's interval jitter (a hash of its id, up to a fifth of the
    /// interval) leaves room, and the backend then waits a whole extra
    /// interval: on a cluster whose jitter is 3 ms, 2/2 took a dead backend
    /// down at +4.2 s on one worker and +6.2 s on the other, twice, where 3/2
    /// took it down at +5.2 s on both.
    pub const DEFAULT_CHECK: ir::HealthCheck = ir::HealthCheck {
        interval_secs: 3,
        timeout_secs: 2,
        healthy_threshold: 2,
        unhealthy_threshold: 2,
        port: None,
    };
}

impl Default for HealthCheckDefault {
    /// Off: every TCP probe is a connection each worker of each replica opens
    /// to each backend per interval, and a backend speaking another protocol
    /// may log one line for each (MariaDB logs `Aborted connection`), so the
    /// operator opts in.
    fn default() -> Self {
        Self {
            enabled: false,
            check: Self::DEFAULT_CHECK,
        }
    }
}

/// Why Sōzu would refuse `check` on an `AddCluster`, if it would.
///
/// Sōzu 2.3.2's `ConfigState::add_cluster` validates an inline check with
/// `validate_health_check_config` and refuses the whole request on failure
/// (`command/src/state.rs`), and the translator folds every `AddCluster` into a
/// `ConfigState` before anything reaches the socket: one invalid check would
/// fail every reconcile of the shared instance. So the check is validated
/// here, by that same function on the config the translator emits for it
/// (`sozu_gw_translator::health_check_config`, which this crate cannot call;
/// `tests/health_checks.rs` checks that the two agree). Checks enter the IR
/// only through here, and the controller refuses at startup a default this
/// refuses.
pub fn health_check_refusal(check: &ir::HealthCheck) -> Option<&'static str> {
    use sozu_command_lib::proto::command::{HealthCheckConfig, HealthCheckMode};
    let config = HealthCheckConfig {
        uri: "/".to_string(),
        interval: check.interval_secs,
        timeout: check.timeout_secs,
        healthy_threshold: check.healthy_threshold,
        unhealthy_threshold: check.unhealthy_threshold,
        expected_status: 0,
        mode: HealthCheckMode::Tcp as i32,
        accepted_statuses: vec![],
        port: check.port.map(u32::from),
        headers: vec![],
    };
    sozu_command_lib::config::validate_health_check_config(&config).err()
}

impl BuildConfig {
    /// The bind address serving `protocol`, if any port exposes it.
    pub fn bind_for(&self, protocol: ExposedProtocol) -> Option<SocketAddr> {
        self.exposure
            .iter()
            .find(|e| e.protocol == protocol)
            .map(|e| Self::bind_addr(e.bind))
    }

    /// A bind port as the address Sōzu listens on.
    pub fn bind_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), port)
    }

    /// The advertised port for `protocol` — what a Gateway listener must
    /// declare to be served on it.
    pub fn advertised_for(&self, protocol: ExposedProtocol) -> Option<u16> {
        self.exposure
            .iter()
            .find(|e| e.protocol == protocol)
            .map(|e| e.port)
    }

    /// The entry serving `(protocol, port)`, i.e. the one a Gateway listener
    /// declaring that pair attaches to.
    pub fn exposed(&self, protocol: ExposedProtocol, port: u16) -> Option<&ExposedPort> {
        self.exposure
            .iter()
            .find(|e| e.protocol == protocol && e.port == port)
    }

    /// The advertised ports on offer for `protocol`, for a message that tells
    /// the user what they could have declared instead.
    pub fn advertised_ports(&self, protocol: ExposedProtocol) -> Vec<u16> {
        self.exposure
            .iter()
            .filter(|e| e.protocol == protocol)
            .map(|e| e.port)
            .collect()
    }

    /// Bind ports already taken by an exposed entry. A layer-4 route may not
    /// land on one: it would resolve to an address Sōzu already listens on,
    /// `AddTcpListener` is rejected, and since the translation is
    /// all-or-nothing the whole reconcile fails with it.
    pub fn reserved_binds(&self) -> BTreeSet<u16> {
        self.exposure.iter().map(|e| e.bind).collect()
    }

    /// Pairs of entries that would land on the same bind address.
    ///
    /// The check that matters is on `bind`, not on the advertised
    /// `(protocol, port)` key: two entries can differ in everything a user
    /// reads and still resolve to one socket. A TCP entry advertising 8080 does
    /// not collide with `(HTTP, 80)` on any key, yet both bind `0.0.0.0:8080`.
    pub fn colliding_binds(&self) -> Vec<(&ExposedPort, &ExposedPort)> {
        let mut out = Vec::new();
        for (i, a) in self.exposure.iter().enumerate() {
            for b in self.exposure.iter().skip(i + 1) {
                if a.bind == b.bind {
                    out.push((a, b));
                }
            }
        }
        out
    }
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            class_name: "sozu".to_string(),
            class_is_default: false,
            controller_name: "sozu.io/gateway-controller".to_string(),
            // The chart's own defaults: 80/443 advertised, bound unprivileged.
            exposure: vec![
                ExposedPort {
                    name: "http".to_string(),
                    port: 80,
                    bind: 80,
                    protocol: ExposedProtocol::Http,
                    transport: None,
                    owner: None,
                },
                ExposedPort {
                    name: "https".to_string(),
                    port: 443,
                    bind: 443,
                    protocol: ExposedProtocol::Https,
                    transport: None,
                    owner: None,
                },
            ],
            health_check: HealthCheckDefault::default(),
        }
    }
}

/// Already-fetched cluster objects. The controller pushes everything it watches;
/// `build` indexes and resolves. Order-independent.
///
/// The collections hold `Arc`s so the controller can hand its reflector-cache
/// entries straight through (kube-rs stores yield `Arc<K>`): the build only
/// reads them, and deep-cloning every cached object on every reconcile would
/// cost tens of MB per pass on a busy cluster. `Arc` is plain shared memory —
/// the crate stays I/O-free.
#[derive(Default)]
pub struct Inputs {
    pub ingresses: Vec<Arc<Ingress>>,
    /// Watched only for their labels, which is what
    /// `allowedRoutes.namespaces.selector` selects on.
    pub namespaces: Vec<Arc<Namespace>>,
    pub services: Vec<Arc<Service>>,
    pub endpointslices: Vec<Arc<EndpointSlice>>,
    pub secrets: Vec<Arc<Secret>>,
    // Gateway API (Phase 2). Empty when only Ingress is in use.
    pub gateway_classes: Vec<Arc<GatewayClass>>,
    pub gateways: Vec<Arc<Gateway>>,
    pub http_routes: Vec<Arc<HttpRoute>>,
    pub reference_grants: Vec<Arc<ReferenceGrant>>,
    // Layer-4 route kinds. Optional even when the Gateway API is installed:
    // the controller probes for them separately and leaves these empty when
    // their CRDs are absent.
    pub tcp_routes: Vec<Arc<TcpRoute>>,
    pub udp_routes: Vec<Arc<UdpRoute>>,
}

/// A problem found while building one Ingress. Surfaced for status + logging;
/// non-fatal problems still let the rest of the Ingress translate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Problem {
    SecretNotFound {
        secret: String,
    },
    TlsEntryWithoutSecret,
    /// An Ingress TLS entry names a Secret but no hosts. SNI names come from
    /// `tls.hosts`, so there is nothing to bind the certificate to and no
    /// rule host would turn HTTPS: loading the cert would be half-applied
    /// material serving nobody. Reported, and the entry is skipped entirely.
    TlsEntryWithoutHosts {
        secret: String,
    },
    InvalidCertificate {
        secret: String,
        reason: String,
    },
    /// Objects created earlier already claim these SNI names at the same
    /// HTTPS listener with other certificates. Sōzu serves a name from
    /// whichever of its certificates expires last, so a tenant could otherwise
    /// take a hostname's TLS identity with a far-future certificate of its
    /// own. The claimant whose *object* is oldest keeps the name
    /// (`creationTimestamp`, then `namespace/name`, as for
    /// [`Problem::RouteCollision`]) — the object's age, not the claim's, so
    /// editing an older object to add a name wins that name. The names are
    /// dropped from the certificate loaded from `secret`, which is not loaded
    /// at all when none is left.
    ///
    /// One report per certificate an object asked for, however many names it
    /// lost: a tenant controls that count, and every report is an Event.
    /// `listener` names the Gateway listener that asked for the certificate;
    /// it is `None` on an Ingress.
    CertificateNameClaimed {
        secret: String,
        listener: Option<String>,
        /// Sorted by name; the rendered message shows the first few.
        names: Vec<ClaimedName>,
    },
    NonServiceBackend,
    /// `spec.defaultBackend` is not mapped onto Sōzu routing: mapping it onto
    /// Sōzu's catch-all semantics is unverified, so per the honesty rule it
    /// is reported, never approximated. `spec.rules` still translate.
    DefaultBackendUnsupported,
    ServiceNotFound {
        service: String,
    },
    ServicePortNotFound {
        service: String,
        port: String,
    },
    NoReadyEndpoints {
        service: String,
    },
    /// An EndpointSlice with `addressType: FQDN` backs this Service. Backends
    /// are pod `IP:port` (Sōzu dials addresses, not hostnames), so the slice
    /// is skipped and reported — silently dropping its addresses would
    /// surface only as a misleading `NoReadyEndpoints`. IPv4/IPv6 slices for
    /// the same Service keep resolving.
    FqdnEndpointsUnsupported {
        service: String,
    },
    /// The Service's `sozu.io/health-check*` annotations do not describe a
    /// check Sōzu accepts: a mode other than `tcp`/`off`, a value that is not
    /// a whole number, or timing Sōzu refuses (a zero interval, timeout or
    /// threshold, port 0). Its cluster gets **no** check — not the default,
    /// which the Service set out to change — and every object routing to it
    /// is told. The older `sozu.io/*` annotations ignore an unusable value in
    /// silence; this one cannot, since the value never reaches Sōzu and the
    /// author would otherwise believe the backends are being checked.
    InvalidHealthCheck {
        service: String,
        reason: String,
    },
    /// The health check the Service asks for, or the default would give it,
    /// is not applied because its cluster carries UDP. One Service port is one
    /// cluster whatever the route kind, the translator marks a cluster with a
    /// UDP frontend as UDP, and Sōzu's TCP health check is never attached to a
    /// UDP cluster (it has a separate UDP health config). Reported on every
    /// TCPRoute, Ingress and HTTPRoute that loses its check to a UDPRoute on
    /// the same Service port, and on the UDPRoute when the cluster also
    /// carries TCP or HTTP traffic or the Service asks for the check itself.
    HealthCheckNotApplied {
        service: String,
        reason: String,
    },
    /// Another route already holds the key Sōzu would store this one under
    /// (listener + host + path + method, joined with `;`) with a different
    /// match or effect. This route is dropped — never silently applied on top
    /// — in favour of `winner` (a cluster id, or `"<redirect>"` for a
    /// cluster-less redirect frontend). Applies to Ingress and Gateway API
    /// routes alike.
    RouteCollision {
        hostname: String,
        path: String,
        winner: String,
        /// `true` when the dropped match is *not* the winner's match — it
        /// covers other requests and only Sōzu's `;`-joined key spelling
        /// merges the two (a regex `/x;GET` against `/x` restricted to `GET`),
        /// so those requests are served by nobody rather than by `winner`.
        key_only: bool,
    },
    /// A user-supplied regex path (Ingress `ImplementationSpecific`, HTTPRoute
    /// `RegularExpression`) that Sōzu cannot compile. Sōzu compiles the
    /// pattern when the frontend is added and rejects the whole request on
    /// failure — translation is all-or-nothing, so one such path would fail
    /// every reconcile of the shared instance, for every tenant, until the
    /// object is removed. The path is skipped (fail closed) and reported; the
    /// rest of the object still programs. Applies to Ingress and Gateway API
    /// routes alike.
    InvalidPathRegex {
        path: String,
        reason: String,
    },
    /// A hostname Sōzu's router cannot parse. Kubernetes validates hostnames
    /// against the RFC 1123 label grammar only, so an A-label that fails IDNA
    /// (UTS #46) processing, such as `xn--a.example.com`, is admitted by the
    /// apiserver and then refused by Sōzu when the frontend is added. Emitting
    /// it would fail every reconcile of the shared instance, as with
    /// [`Problem::InvalidPathRegex`]. The frontends that would carry it are
    /// skipped (fail closed); the rest of the object still programs.
    /// `listener` names the Gateway listener whose own `hostname` it is; it is
    /// `None` on an Ingress rule or an HTTPRoute.
    InvalidHostname {
        hostname: String,
        reason: &'static str,
        listener: Option<String>,
    },
    // Gateway API (Phase 2) — features Sōzu or this phase does not cover yet.
    UnsupportedTlsMode {
        mode: String,
    },
    UnsupportedProtocol {
        protocol: String,
    },
    /// An HTTP route's backend Service port declares an `appProtocol` Sōzu
    /// cannot speak to it (Gateway API GEP-1911): `kubernetes.io/wss`, which
    /// needs TLS to the backend, and Sōzu has none. Reported only: the route is
    /// still programmed as before, cleartext HTTP/1.1 to that port (see
    /// [`HttpBackendProtocol::Unsupported`] for why). An HTTPRoute's parent
    /// reads `ResolvedRefs: False` / `UnsupportedProtocol`; an Ingress, which
    /// has no condition for it, gets the Warning Event alone.
    UnsupportedAppProtocol {
        service: String,
        port: i32,
        app_protocol: String,
    },
    /// A listener declares a `(port, protocol)` pair this gateway does not
    /// expose. The port is the client-visible one — the Service's port, not the
    /// pod bind — and only the exposed set can carry traffic, because the
    /// Service's ports are fixed by the chart and Sōzu's HTTP(S) listeners are
    /// fixed at boot. The listener is reported and NOT programmed: landing its
    /// routes on some other port would silently serve them somewhere the
    /// Gateway never declared.
    ///
    /// `exposed` carries the ports actually on offer for that protocol, so the
    /// message can say what the author could have written instead.
    PortNotExposed {
        listener: String,
        declared: i32,
        protocol: String,
        exposed: Vec<u16>,
    },
    /// A Gateway declares a layer-4 listener on a port the exposure table
    /// reserves for another namespace. A layer-4 socket carries exactly one
    /// route and has no hostname to arbitrate with, so the table names who may
    /// claim it rather than letting two Gateways race for it.
    ListenerPortNotOwned {
        listener: String,
        port: u16,
        owner: String,
        claimed_by: String,
    },
    /// Two layer-4 routes claim one listen socket for different Services. A
    /// socket carries exactly one route, so the older one (by
    /// `creationTimestamp`, then `namespace/name`) keeps it. This route stays
    /// accepted but does not receive traffic. Settled here rather than in the
    /// translator on purpose: a
    /// translator error fails the whole reconcile, so one tenant's second route
    /// would stop routing for every other tenant.
    L4RouteConflict {
        port: u16,
        protocol: &'static str,
        winner: String,
    },
    WeightedBackendsUnsupported,
    /// A single `backendRef` with `weight: 0` (the standard drain pattern)
    /// must receive no traffic; with every weight zero the spec even calls
    /// for a 500 on matching requests. Sōzu can neither weight backends nor
    /// synthesize that 500, so the rule is reported and skipped (fail
    /// closed) instead of serving the drained backend 100% of the traffic.
    ZeroWeightBackendUnsupported {
        service: String,
    },
    /// `rule.timeouts` has no Sōzu equivalent; the rule still routes,
    /// without the timeout, and the gap is reported.
    TimeoutsUnsupported,
    /// An HTTPRoute match with a header or query condition. Only that match is
    /// skipped; the rule's other matches still program.
    HeaderOrQueryMatchUnsupported,
    /// A listener's `allowedRoutes.namespaces.selector` cannot be evaluated —
    /// an operator this build does not know, a malformed expression, or
    /// `from: Selector` with no selector at all. The listener fails CLOSED: it
    /// admits no routes, rather than guessing at a control the Gateway owner
    /// set precisely to restrict admission.
    NamespaceSelectorInvalid {
        listener: String,
        reason: String,
    },
    FilterUnsupported {
        kind: String,
    },
    BackendRefNotPermitted {
        reference: String,
    },
    /// A `Gateway` spec field this controller does not honour. Reported rather
    /// than ignored: several of these carry security intent (client-certificate
    /// validation, which namespaces may contribute listeners), and a field that
    /// is silently dropped reads to its author as a field that was applied.
    GatewaySpecUnsupported {
        field: &'static str,
    },
    /// No infrastructure parameter kinds are supported. Reject the Gateway
    /// instead of claiming to apply configuration that was never interpreted.
    GatewayParametersUnsupported {
        reference: String,
    },
}

impl Problem {
    /// Machine-readable CamelCase reason (the variant name) — the shape
    /// Kubernetes wants for Event reasons and condition reasons.
    pub fn reason(&self) -> &'static str {
        match self {
            Problem::SecretNotFound { .. } => "SecretNotFound",
            Problem::TlsEntryWithoutSecret => "TlsEntryWithoutSecret",
            Problem::TlsEntryWithoutHosts { .. } => "TlsEntryWithoutHosts",
            Problem::InvalidCertificate { .. } => "InvalidCertificate",
            Problem::CertificateNameClaimed { .. } => "CertificateNameClaimed",
            Problem::NonServiceBackend => "NonServiceBackend",
            Problem::DefaultBackendUnsupported => "DefaultBackendUnsupported",
            Problem::ServiceNotFound { .. } => "ServiceNotFound",
            Problem::ServicePortNotFound { .. } => "ServicePortNotFound",
            Problem::NoReadyEndpoints { .. } => "NoReadyEndpoints",
            Problem::FqdnEndpointsUnsupported { .. } => "FqdnEndpointsUnsupported",
            Problem::InvalidHealthCheck { .. } => "InvalidHealthCheck",
            Problem::HealthCheckNotApplied { .. } => "HealthCheckNotApplied",
            Problem::RouteCollision { .. } => "RouteCollision",
            Problem::InvalidPathRegex { .. } => "InvalidPathRegex",
            Problem::InvalidHostname { .. } => "InvalidHostname",
            Problem::UnsupportedTlsMode { .. } => "UnsupportedTlsMode",
            Problem::UnsupportedProtocol { .. } => "UnsupportedProtocol",
            Problem::UnsupportedAppProtocol { .. } => "UnsupportedAppProtocol",
            Problem::PortNotExposed { .. } => "PortNotExposed",
            Problem::ListenerPortNotOwned { .. } => "ListenerPortNotOwned",
            Problem::L4RouteConflict { .. } => "L4RouteConflict",
            Problem::WeightedBackendsUnsupported => "WeightedBackendsUnsupported",
            Problem::ZeroWeightBackendUnsupported { .. } => "ZeroWeightBackendUnsupported",
            Problem::TimeoutsUnsupported => "TimeoutsUnsupported",
            Problem::HeaderOrQueryMatchUnsupported => "HeaderOrQueryMatchUnsupported",
            Problem::NamespaceSelectorInvalid { .. } => "NamespaceSelectorInvalid",
            Problem::FilterUnsupported { .. } => "FilterUnsupported",
            Problem::BackendRefNotPermitted { .. } => "BackendRefNotPermitted",
            Problem::GatewaySpecUnsupported { .. } => "GatewaySpecUnsupported",
            Problem::GatewayParametersUnsupported { .. } => "GatewayParametersUnsupported",
        }
    }

    /// The listener this problem is scoped to, when the variant carries one —
    /// lets per-listener status conditions cite their own problems.
    pub fn listener(&self) -> Option<&str> {
        match self {
            Problem::PortNotExposed { listener, .. }
            | Problem::ListenerPortNotOwned { listener, .. }
            | Problem::NamespaceSelectorInvalid { listener, .. } => Some(listener),
            Problem::InvalidHostname { listener, .. }
            | Problem::CertificateNameClaimed { listener, .. } => listener.as_deref(),
            _ => None,
        }
    }
}

/// One SNI name a certificate lost ([`Problem::CertificateNameClaimed`]), and
/// the object that keeps it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ClaimedName {
    /// As this object spelled it.
    pub name: String,
    /// `Ingress` or `Gateway`.
    pub owner_kind: &'static str,
    /// `namespace/name`.
    pub owner: String,
}

/// The most a [`Problem::CertificateNameClaimed`] message takes, in bytes.
///
/// The message is an Event note, which the apiserver refuses past 1 kB
/// (kube-runtime's `Event::note`), and the controller records a refused
/// publication as seen, so it is never retried: an over-long message is a
/// report lost for good. Its pieces are tenant-chosen and long — a name
/// inferred from a certificate takes up to 4096 bytes, an owner's
/// `namespace/name` up to 317, a Secret name up to 253 — so the budget is
/// enforced rather than assumed, with room to spare under the limit.
const CLAIMED_NAMES_MESSAGE_BUDGET: usize = 900;

/// `s` cut to at most `max` bytes at a character boundary, `…` marking the
/// cut, so a cut never splits a UTF-8 sequence and is never silent.
fn clip(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    const MARK: &str = "…";
    if s.len() <= max {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut end = max.saturating_sub(MARK.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!("{}{MARK}", &s[..end]))
}

/// The [`Problem::CertificateNameClaimed`] message, within
/// [`CLAIMED_NAMES_MESSAGE_BUDGET`]: each tenant-chosen piece is clipped so no
/// one of them crowds out the rest, a few names are listed, only while they
/// fit, and the others are counted.
fn claimed_names_message(secret: &str, names: &[ClaimedName]) -> String {
    /// A Secret name, a claimed name or an owner, each; with the fixed text,
    /// the header and one listed name always fit the budget.
    const PIECE: usize = 200;
    /// Names listed at most, the others counted.
    const SHOWN: usize = 3;
    let claimant = |c: &ClaimedName| {
        format!(
            "{:?} ({} {})",
            clip(&c.name, PIECE),
            c.owner_kind,
            clip(&c.owner, PIECE)
        )
    };
    let secret = clip(secret, PIECE);
    let message = match names {
        [one] => format!(
            "SNI name {:?} is claimed at this listener by {} {}, created earlier (or first by \
             namespace/name); the certificate from Secret {secret:?} does not serve it",
            clip(&one.name, PIECE),
            one.owner_kind,
            clip(&one.owner, PIECE)
        ),
        _ => {
            let mut message = format!(
                "{} SNI names are claimed at this listener by objects created earlier (or first \
                 by namespace/name); the certificate from Secret {secret:?} does not serve them: ",
                names.len()
            );
            // Room kept for the count, at its longest.
            let tail = format!(" and {} more", names.len()).len();
            let mut shown = 0;
            for claimed in names.iter().take(SHOWN) {
                let sep = if shown == 0 { "" } else { ", " };
                let item = claimant(claimed);
                if shown > 0
                    && message.len() + sep.len() + item.len() + tail > CLAIMED_NAMES_MESSAGE_BUDGET
                {
                    break;
                }
                message.push_str(sep);
                message.push_str(&item);
                shown += 1;
            }
            if names.len() > shown {
                message.push_str(&format!(" and {} more", names.len() - shown));
            }
            message
        }
    };
    // A no-op with the pieces clipped above, unless escaping one for `{:?}`
    // grew it past its clip: the bound holds whatever the input.
    clip(&message, CLAIMED_NAMES_MESSAGE_BUDGET).into_owned()
}

/// Human one-liner carrying the *detail* (which Secret, which Service, which
/// port) — what status condition messages and Events show to users, so they
/// can self-diagnose without controller log access.
impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::SecretNotFound { secret } => write!(f, "TLS Secret {secret:?} not found"),
            Problem::TlsEntryWithoutSecret => {
                write!(f, "Ingress TLS entry has no secretName; its hosts stay plain HTTP")
            }
            Problem::TlsEntryWithoutHosts { secret } => {
                write!(f, "Ingress TLS entry for Secret {secret:?} lists no hosts; the certificate was skipped and the rule hosts stay plain HTTP")
            }
            Problem::InvalidCertificate { secret, reason } => {
                write!(f, "TLS Secret {secret:?} holds unusable material: {reason}")
            }
            Problem::CertificateNameClaimed { secret, names, .. } => {
                f.write_str(&claimed_names_message(secret, names))
            }
            Problem::NonServiceBackend => write!(f, "only Service backends are supported"),
            Problem::DefaultBackendUnsupported => {
                write!(f, "spec.defaultBackend is not supported and was ignored (rules still apply)")
            }
            Problem::ServiceNotFound { service } => {
                write!(f, "backend Service {service:?} not found")
            }
            Problem::ServicePortNotFound { service, port } => {
                write!(f, "port {port:?} not found on Service {service:?}")
            }
            Problem::NoReadyEndpoints { service } => {
                write!(f, "Service {service:?} has no ready endpoints")
            }
            Problem::FqdnEndpointsUnsupported { service } => write!(
                f,
                "Service {service:?} has an EndpointSlice of addressType \"FQDN\", which is not supported (backends must be pod IPs); the slice was ignored"
            ),
            Problem::InvalidHealthCheck { service, reason } => write!(
                f,
                "Service {service:?} asks for a health check Sōzu cannot run ({reason}); its \
                 backends are not health-checked"
            ),
            Problem::HealthCheckNotApplied { service, reason } => write!(
                f,
                "Service {service:?} gets no health check: {reason}"
            ),
            Problem::RouteCollision {
                hostname,
                path,
                winner,
                key_only: false,
            } => write!(
                f,
                "host+path {hostname}{path} is already served by {winner}; this route was dropped"
            ),
            Problem::RouteCollision {
                hostname,
                path,
                winner,
                key_only: true,
            } => write!(
                f,
                "the match on host+path {hostname}{path} shares Sōzu's route key with the \
                 frontend of {winner} and was dropped"
            ),
            Problem::InvalidPathRegex { path, reason } => write!(
                f,
                "path {path:?} is not a regular expression Sōzu can compile ({reason}); no \
                 route was programmed for it"
            ),
            Problem::InvalidHostname {
                hostname,
                reason,
                listener: None,
            } => write!(
                f,
                "hostname {hostname:?} cannot be parsed by Sōzu ({reason}); no route was \
                 programmed for it"
            ),
            Problem::InvalidHostname {
                hostname,
                reason,
                listener: Some(listener),
            } => write!(
                f,
                "listener {listener} hostname {hostname:?} cannot be parsed by Sōzu ({reason}); \
                 the listener serves no routes"
            ),
            Problem::UnsupportedTlsMode { mode } => {
                write!(f, "TLS mode {mode:?} is not supported (Terminate only)")
            }
            Problem::UnsupportedProtocol { protocol } => write!(
                f,
                "listener protocol {protocol:?} is not supported (HTTP, HTTPS, TCP and UDP are)"
            ),
            Problem::UnsupportedAppProtocol {
                service,
                port,
                app_protocol,
            } => write!(
                f,
                "Service {service:?} port {port} declares appProtocol {app_protocol:?}, which \
                 needs TLS to the backend, and Sōzu cannot open TLS to a backend; the route \
                 still sends it cleartext HTTP/1.1"
            ),
            Problem::PortNotExposed {
                listener,
                declared,
                protocol,
                exposed,
            } => write!(
                f,
                "listener {listener} declares port {declared}, which this gateway does not expose \
                 for {protocol}; exposed: {exposed:?}"
            ),
            Problem::ListenerPortNotOwned {
                listener,
                port,
                owner,
                claimed_by,
            } => write!(
                f,
                "listener {listener} declares L4 port {port}, which is reserved for namespace \
                 {owner}; this Gateway is in {claimed_by}"
            ),
            Problem::L4RouteConflict {
                port,
                protocol,
                winner,
            } => write!(
                f,
                "{protocol} port {port} is already claimed by route {winner} (older, or first by \
                 name); this route remains accepted but does not receive traffic"
            ),
            Problem::WeightedBackendsUnsupported => write!(
                f,
                "multiple backendRefs (weighted split) are not supported"
            ),
            Problem::ZeroWeightBackendUnsupported { service } => write!(
                f,
                "backendRef {service:?} with weight 0 cannot be honoured (Sōzu cannot drain by weight); the rule was skipped"
            ),
            Problem::TimeoutsUnsupported => {
                write!(f, "rule.timeouts has no Sōzu equivalent; the rule routes without it")
            }
            Problem::HeaderOrQueryMatchUnsupported => write!(
                f,
                "header matches are not wired yet (Sōzu 2.3.2 has header predicates) and query \
                 matches are not supported by Sōzu; only the match carrying one was skipped"
            ),
            Problem::NamespaceSelectorInvalid { listener, reason } => write!(
                f,
                "listener {listener:?} has an allowedRoutes.namespaces.selector this controller \
                 cannot evaluate ({reason}); the listener admits no routes (fail closed)"
            ),
            Problem::FilterUnsupported { kind } => write!(f, "unsupported filter: {kind}"),
            Problem::BackendRefNotPermitted { reference } => write!(
                f,
                "cross-namespace reference {reference} is not permitted (no ReferenceGrant covers it)"
            ),
            Problem::GatewaySpecUnsupported { field } => write!(
                f,
                "Gateway field {field} is not honoured by this controller and is ignored"
            ),
            Problem::GatewayParametersUnsupported { reference } => write!(
                f,
                "Gateway infrastructure.parametersRef {reference} is not supported by this \
                 controller; the Gateway cannot be configured"
            ),
        }
    }
}

/// Per-Ingress build result for status reporting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IngressResult {
    pub namespace: String,
    pub name: String,
    /// `metadata.uid` of the source object. Kubernetes references an object by
    /// uid as well as by name, so an Event that omits it is not shown by
    /// `kubectl describe` — see [`crate::obj_uid`].
    pub uid: Option<String>,
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildOutput {
    pub ir: ir::Ir,
    /// Per-Ingress results (status/logging).
    pub results: Vec<IngressResult>,
    /// Gateway API results (Phase 2).
    pub gateway_classes: Vec<GatewayClassResult>,
    pub gateways: Vec<GatewayResult>,
    pub routes: Vec<RouteResult>,
    /// `namespace/name` of every Service some route referenced — whether or not
    /// it resolved. Failed targets
    /// (`ServiceNotFound`, `NoReadyEndpoints`, …) are included on purpose: an
    /// EndpointSlice appearing later for a still-broken backend must count as
    /// relevant. The controller uses this set to ignore EndpointSlice churn
    /// from workloads no route cares about.
    pub referenced_services: BTreeSet<String>,
}

/// A reference to a Service port from a route's backend: by number or by name.
pub(crate) enum PortRef {
    /// A number, as a TCPRoute or UDPRoute names it: the first Service port
    /// carrying it, whatever its protocol.
    Number(i32),
    /// A number, as an HTTP route (Ingress, HTTPRoute) names it. Sōzu reaches
    /// an HTTP backend over TCP, so when the Service exposes the number over
    /// TCP and over another protocol, the TCP entry is the one meant, wherever
    /// it is listed. Without a TCP entry, the first one carrying the number,
    /// as [`PortRef::Number`].
    HttpNumber(i32),
    /// A name, unique within a Service.
    Name(String),
}

// ----------------------------------------------------------------------------

/// `metadata.uid` of an object, when the apiserver has assigned one.
///
/// Carried through the build results so Events can reference their owner the
/// way Kubernetes does. `kubectl describe` looks events up with a field
/// selector that includes `involvedObject.uid`, so a reference without it
/// produces an Event that exists and is queryable by name, yet never appears
/// where the object's owner would look for it.
pub(crate) fn obj_uid(
    meta: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> Option<String> {
    meta.uid.clone()
}

/// `metadata.creationTimestamp` as its RFC 3339 spelling (fixed-width, UTC,
/// `Z`-suffixed), which sorts chronologically as a plain string.
pub(crate) fn creation_key(
    meta: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> Option<String> {
    meta.creation_timestamp.as_ref().map(|t| t.0.to_string())
}

pub(crate) fn meta_nn(namespace: &Option<String>, name: &Option<String>) -> (String, String) {
    (
        namespace.clone().unwrap_or_else(|| "default".to_string()),
        name.clone().unwrap_or_default(),
    )
}

/// Does this Ingress belong to our IngressClass?
pub fn is_ours(ingress: &Ingress, cfg: &BuildConfig) -> bool {
    if let Some(spec) = &ingress.spec {
        if let Some(class) = &spec.ingress_class_name {
            return class == &cfg.class_name;
        }
    }
    if let Some(annotations) = &ingress.metadata.annotations {
        if let Some(class) = annotations.get(LEGACY_CLASS_ANNOTATION) {
            return class == &cfg.class_name;
        }
    }
    // No class set anywhere: ours only if our IngressClass is the cluster default.
    cfg.class_is_default
}

/// Map a Kubernetes `pathType` + `path` to an IR path match. `Err` is a regex
/// path Sōzu would refuse; the caller skips that path.
fn path_match(path_type: &str, path: Option<&str>) -> Result<ir::PathMatch, Problem> {
    let value = match path {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => "/".to_string(),
    };
    admit_path(match path_type {
        "Exact" => ir::PathMatch::Exact(value),
        "ImplementationSpecific" => ir::PathMatch::Regex(value),
        // "Prefix" and anything unknown default to Prefix — element-boundary
        // matching, which is narrower than a raw string prefix and never routes
        // more than the author asked for.
        //
        // The trailing slash is insignificant in Kubernetes (`/foo/` ≡ `/foo`),
        // so it is canonicalised away here rather than in the translator: the IR
        // is what route-collision detection compares, and two Ingresses spelling
        // one path differently are a collision to report, not two routes.
        _ => ir::PathMatch::Prefix(canonical_prefix(&value)),
    })
}

/// Admit a path match into the IR only if the Sōzu rule it compiles to is one
/// Sōzu can load.
///
/// Every variant is checked, not only a user regex: a non-root `Prefix` and an
/// `Exact` compile to anchored regexes too (`ir::PathMatch::sozu_rule`), and
/// the `regex` crate refuses a compiled program above its 10 MiB size limit —
/// reached by a literal path of a few hundred kilobytes, which Kubernetes
/// accepts on an Ingress (no length bound on `path`). The rule reaches Sōzu
/// verbatim and Sōzu 2.3.2 compiles it with `regex::bytes::Regex::new` —
/// default flags, no `RegexBuilder` limits — twice, bare and anchored (below),
/// in the main process before the frontend is dispatched, failing the whole
/// request on error and, since translation is all-or-nothing, every reconcile
/// of the shared instance. These are the same calls against the same `regex`
/// version (pinned, see `tests/sozu_lock_pins.rs`), so what compiles here
/// compiles there; the compiled regexes are discarded. Paths enter the IR only
/// through here, which is why the check lives here and not in the translator.
pub(crate) fn admit_path(path: ir::PathMatch) -> Result<ir::PathMatch, Problem> {
    let pattern = match path.sozu_rule() {
        // The root `/` is a plain prefix rule: nothing to compile.
        ir::SozuPathRule::Prefix(_) => return Ok(path),
        ir::SozuPathRule::Regex(pattern) => pattern,
    };
    // Sōzu 2.3.2 compiles a `Regex` rule twice (`PathRule::anchored_regex`):
    // as written, then wrapped in `\A(?:…)\z`, and refuses the frontend if
    // either fails. The wrapper alone can fail: an `(?x)` comment swallows
    // the closing group, the wrapper's two extra nesting levels (a group and
    // a concatenation) push a pattern nested 248 or 249 deep past the
    // parser's limit of 250, and the size limit trips a few bytes earlier.
    let anchored = format!("\\A(?:{pattern})\\z");
    match regex::bytes::Regex::new(&pattern).and_then(|_| regex::bytes::Regex::new(&anchored)) {
        Ok(_) => Ok(path),
        Err(e) => Err(Problem::InvalidPathRegex {
            path: display_path(path_value(&path)),
            // A syntax error renders as a multi-line diagnostic with a caret
            // line; condition messages and Events are single lines joined by
            // `; `, so fold it onto one.
            reason: e
                .to_string()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        }),
    }
}

/// Sōzu's bound on a frontend hostname (`router::MAX_HOSTNAME_LENGTH`), checked
/// before anything parses it.
const MAX_HOSTNAME_LENGTH: usize = 4096;

/// Admit a frontend hostname into the IR only if Sōzu's router can parse it.
///
/// Every hostname but the bare `*` catch-all goes into the route table's tree,
/// where Sōzu 2.3.2 (like 2.2.1) parses it twice when the frontend is added: as a
/// `DomainRule` (`*` is any host, a `/` makes a regex domain, a `*` is only
/// accepted in front) and through `idna::domain_to_ascii`, whose ASCII form is
/// inserted into a trie that refuses an empty leading label. Any of these
/// failures rejects the whole request ("parsing hostname failed", or "Could
/// not add route" from the trie) and, since translation is all-or-nothing,
/// every reconcile of the shared instance. These are the same
/// checks on the same `idna` version and Unicode data (pinned, see
/// `tests/sozu_lock_pins.rs`), so what passes here parses there; the one
/// deliberate difference is a '/', which is refused rather than compiled. The outcome is not a per-label property: in
/// a domain with a right-to-left label the bidi rule applies to every label,
/// so `a.xn--4gbrim.example.com` parses and `*.xn--4gbrim.example.com` does
/// not. Hostnames enter the IR only through here, which is why the check lives
/// here and not in the translator.
pub(crate) fn admit_hostname(hostname: &str) -> Result<(), Problem> {
    match hostname_refusal(hostname) {
        None => Ok(()),
        Some(reason) => Err(Problem::InvalidHostname {
            hostname: display_path(hostname),
            reason,
            listener: None,
        }),
    }
}

/// Why Sōzu's router would refuse `hostname` as a frontend hostname, if it
/// would. See [`admit_hostname`].
pub(crate) fn hostname_refusal(hostname: &str) -> Option<&'static str> {
    if hostname.len() > MAX_HOSTNAME_LENGTH {
        return Some("longer than the 4096 bytes Sōzu accepts");
    }
    if hostname == "*" {
        return None;
    }
    // Refused outright rather than compiled the way Sōzu would: a host field
    // is never a regex domain, and the apiserver refuses the character anyway.
    if hostname.contains('/') {
        return Some("a '/' makes Sōzu read it as a regular-expression domain");
    }
    if hostname.contains('*') && !hostname.starts_with('*') {
        return Some("a '*' is only accepted as the leading label");
    }
    match idna::domain_to_ascii(hostname) {
        Err(_) => Some("not a valid internationalized domain name under IDNA (UTS #46)"),
        // The trie consumes labels from the right and refuses only an empty
        // leftmost one: measured, `a..b.com` and `example.com.` are added.
        Ok(ascii) if ascii.is_empty() || ascii.starts_with('.') => Some("empty leading label"),
        Ok(_) => None,
    }
}

/// A path as it may appear in a condition message or an Event: the apiserver
/// caps those at about a kilobyte, and the path that trips the size limit is
/// hundreds of kilobytes long. Keep the head, say how long it really was.
fn display_path(path: &str) -> String {
    const KEEP: usize = 64;
    if path.chars().count() <= KEEP {
        return path.to_string();
    }
    let head: String = path.chars().take(KEEP).collect();
    format!("{head}… ({} bytes)", path.len())
}

/// Drop a prefix path's insignificant trailing slash, keeping the root `/`.
fn canonical_prefix(value: &str) -> String {
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Does a set of TLS hosts (possibly wildcards) cover `host`?
fn tls_covers(tls_hosts: &BTreeSet<String>, host: &str) -> bool {
    if tls_hosts.contains(host) {
        return true;
    }
    // A `*.example.com` wildcard covers exactly one extra label: `a.example.com`
    // but NOT `a.b.example.com` and not the bare apex `example.com`.
    tls_hosts.iter().any(|pattern| {
        pattern.strip_prefix("*.").is_some_and(|suffix| {
            host.strip_suffix(suffix)
                .and_then(|prefix| prefix.strip_suffix('.'))
                .is_some_and(|label| !label.is_empty() && !label.contains('.'))
        })
    })
}

/// The `(namespace, service)` an EndpointSlice feeds, per its
/// `kubernetes.io/service-name` label — the only linkage the build uses.
/// `None` when the label is absent: such a slice is invisible to the build.
fn slice_service(slice: &EndpointSlice) -> Option<(String, String)> {
    let svc = slice
        .metadata
        .labels
        .as_ref()?
        .get(SERVICE_NAME_LABEL)?
        .clone();
    let ns = slice
        .metadata
        .namespace
        .clone()
        .unwrap_or_else(|| "default".to_string());
    Some((ns, svc))
}

/// The `namespace/name` key of the Service an EndpointSlice feeds, in the
/// spelling of [`BuildOutput::referenced_services`]. `None` when the
/// `kubernetes.io/service-name` label is absent — the build never reads such
/// a slice, so it can never affect the output.
pub fn slice_service_key(slice: &EndpointSlice) -> Option<String> {
    slice_service(slice).map(|(ns, svc)| format!("{ns}/{svc}"))
}

/// Inferred names reach Sōzu's SNI trie directly, including its regex grammar.
/// Validate before merging so an invalid inferred name cannot invalidate a
/// certificate also used by a listener with an explicit hostname — and keep
/// the **normalised** spelling Sōzu's validator returns, which lowercases.
/// rustls lowercases the SNI before the resolver sees it, while Sōzu 2.2.1
/// stored a certificate's names verbatim, so a SAN written `MiXeD.Example.COM`
/// was reachable by no handshake at all and every one for it fell back to the
/// default certificate. Sōzu 2.3.0 lowercases names at load by the same fold
/// (704b60d), but only in the worker's resolver: the main process keeps the
/// `AddCertificate` names as sent, so lowercasing here also keeps that record
/// in the spelling the resolver uses. Explicit names never need this: an
/// Ingress `tls.hosts` entry and a Gateway listener `hostname` are lowercase by
/// apiserver validation.
fn validate_inferred_certificate_names(names: &[String]) -> Result<Vec<String>, String> {
    if names.is_empty() {
        return Err("tls.crt has no DNS SAN or common name to infer for the listener".into());
    }
    let mut normalised = Vec::with_capacity(names.len());
    for name in names {
        // Sōzu's TLS resolver caps names at MAX_HOSTNAME_LENGTH (4096 bytes).
        // Its public SNI validator excludes malformed labels, regex syntax
        // and any `/` (a hostname cannot carry one, and the trie would read
        // `/…/` as a regex segment); the bare wildcard is also supported by
        // certificate name inference.
        if name.len() > 4096 {
            return Err("tls.crt contains an unsupported inferred SNI name".into());
        }
        if name == "*" {
            normalised.push(name.clone());
            continue;
        }
        match sozu_command_lib::config::validate_sni_pattern(name) {
            Ok(valid) => normalised.push(valid),
            Err(_) => return Err("tls.crt contains an unsupported inferred SNI name".into()),
        }
    }
    Ok(normalised)
}

/// Extract **and validate** leaf + chain + key PEM from a TLS Secret
/// (`tls.crt` / `tls.key`). An empty name override uses Sōzu's CN/SAN inference
/// before certificates are merged, so an explicit override on another listener
/// cannot discard the inferred names of a listener without a hostname.
///
/// `split_certificate_chain` is purely textual (it scans for the BEGIN/END
/// markers) and never decodes the base64 body, so everything is parsed here,
/// *before* it enters the IR: a corrupt certificate would otherwise abort the
/// translator's whole diff (freezing convergence for every namespace), and a
/// corrupt key would make Sōzu reject the `AddCertificate` at apply time —
/// certificates tier before frontends, so that blocks every frontend add.
/// The key is therefore parsed and loaded exactly as Sōzu loads it, and then
/// paired with the leaf exactly as Sōzu 2.3.2 pairs it before committing the
/// certificate (2.2.x did not check the pair at all: a key that belonged to
/// another certificate loaded fine and failed every handshake).
pub(crate) fn extract_cert(
    secret: &Secret,
    listener: SocketAddr,
    mut names: Vec<String>,
) -> Result<FingerprintedCert, String> {
    let data = secret
        .data
        .as_ref()
        .ok_or_else(|| "secret has no data".to_string())?;
    let crt = data
        .get("tls.crt")
        .ok_or_else(|| "missing tls.crt".to_string())?;
    let key = data
        .get("tls.key")
        .ok_or_else(|| "missing tls.key".to_string())?;
    let crt = String::from_utf8(crt.0.clone()).map_err(|_| "tls.crt is not UTF-8".to_string())?;
    let key = String::from_utf8(key.0.clone()).map_err(|_| "tls.key is not UTF-8".to_string())?;

    let mut chain = sozu_command_lib::certificate::split_certificate_chain(crt);
    if chain.is_empty() {
        return Err("tls.crt contains no PEM blocks".to_string());
    }
    let leaf = chain.remove(0);

    // Fully parse the leaf — PEM framing, base64 body AND the decoded DER —
    // with the same helpers Sōzu applies to an `AddCertificate` (`parse_pem` +
    // `parse_x509`, its X509 parse for CN/SAN extraction). Hashing the decoded
    // bytes alone would wave through valid-base64 garbage that Sōzu then
    // rejects at apply time, aborting the whole batch on every cycle. The
    // fingerprint is the SHA-256 of the DER, the translator's identity.
    let leaf_pem = sozu_command_lib::certificate::parse_pem(leaf.as_bytes())
        .map_err(|e| format!("invalid certificate in tls.crt: {e}"))?;
    let x509 = sozu_command_lib::certificate::parse_x509(&leaf_pem.contents)
        .map_err(|e| format!("invalid certificate in tls.crt: {e}"))?;
    if names.is_empty() {
        let inferred = sozu_command_lib::certificate::get_cn_and_san_attributes(&x509);
        names = validate_inferred_certificate_names(&inferred)?;
    }
    let fingerprint =
        sozu_command_lib::certificate::calculate_fingerprint_from_der(&leaf_pem.contents);
    // The intermediates ride in the same AddCertificate, so a corrupt one
    // would equally be rejected by Sōzu at apply time.
    for (i, c) in chain.iter().enumerate() {
        let pem = sozu_command_lib::certificate::parse_pem(c.as_bytes())
            .map_err(|e| format!("invalid chain certificate #{} in tls.crt: {e}", i + 1))?;
        sozu_command_lib::certificate::parse_x509(&pem.contents)
            .map_err(|e| format!("invalid chain certificate #{} in tls.crt: {e}", i + 1))?;
    }
    // The key, the way Sōzu loads it when the AddCertificate is applied:
    // `PrivateKeyDer::from_pem_slice` (PKCS#8, PKCS#1 or SEC1 — any other PEM
    // label is "no key found"), then the ring provider's `load_private_key`
    // (RSA, ECDSA P-256/P-384, Ed25519). A well-framed block whose body is
    // not one of those was measured to pass a label check and be rejected by
    // Sōzu with "failed to parse private key as RSA, ECDSA, or EdDSA".
    //
    // Loading an ECDSA key draws from OS randomness (ring seeds the signer).
    // That is not socket or kube I/O: it stays within this crate's purity
    // boundary, and the outcome is a pure function of the input.
    let key_der = PrivateKeyDer::from_pem_slice(key.as_bytes())
        .map_err(|e| format!("invalid private key in tls.key: {e}"))?;
    let signing_key = rustls::crypto::ring::default_provider()
        .key_provider
        .load_private_key(key_der)
        .map_err(|e| format!("invalid private key in tls.key: {e}"))?;
    // Sōzu 2.3.2 refuses the pair before committing it
    // (`tls::build_validated_certificate`) with `CertifiedKey::keys_match`:
    // webpki parses the leaf, and the SubjectPublicKeyInfo the provider
    // derives from the key must equal the leaf's byte for byte. Only
    // `InconsistentKeys::Unknown` (a provider that cannot expose the public
    // key; ring always can) is let through. Same call on the same rustls,
    // webpki and ring (pinned, see `tests/sozu_lock_pins.rs`).
    let certified = rustls::sign::CertifiedKey::new(
        vec![rustls::pki_types::CertificateDer::from(
            leaf_pem.contents.clone(),
        )],
        signing_key,
    );
    match certified.keys_match() {
        Ok(()) | Err(rustls::Error::InconsistentKeys(rustls::InconsistentKeys::Unknown)) => {}
        Err(rustls::Error::InconsistentKeys(_)) => {
            return Err("tls.key does not match tls.crt".to_string())
        }
        Err(e) => return Err(format!("tls.crt cannot be paired with tls.key: {e}")),
    }

    Ok(FingerprintedCert {
        fingerprint,
        cert: ir::Certificate {
            listener,
            certificate: leaf,
            chain,
            key,
            names,
        },
    })
}

// ----------------------------------------------------------------------------

pub(crate) struct Index<'a> {
    pub(crate) services: BTreeMap<(String, String), &'a Service>,
    /// Namespace name -> its labels, for `allowedRoutes.namespaces.selector`.
    /// A namespace absent here cannot be matched, so a selector listener
    /// admits nothing from it — the same fail-closed answer as an unevaluable
    /// selector, and only reachable in the window before the cache syncs.
    pub(crate) namespace_labels: BTreeMap<String, BTreeMap<String, String>>,
    pub(crate) secrets: BTreeMap<(String, String), &'a Secret>,
    /// (namespace, service-name) -> slices
    pub(crate) slices: BTreeMap<(String, String), Vec<&'a EndpointSlice>>,
    /// The controller-wide health-check default, read wherever a Service's
    /// cluster settings are ([`add_service_route`]).
    pub(crate) health_check: &'a HealthCheckDefault,
}

impl<'a> Index<'a> {
    pub(crate) fn build(inputs: &'a Inputs, health_check: &'a HealthCheckDefault) -> Self {
        let mut services = BTreeMap::new();
        for svc in &inputs.services {
            services.insert(
                meta_nn(&svc.metadata.namespace, &svc.metadata.name),
                svc.as_ref(),
            );
        }
        let mut secrets = BTreeMap::new();
        for secret in &inputs.secrets {
            secrets.insert(
                meta_nn(&secret.metadata.namespace, &secret.metadata.name),
                secret.as_ref(),
            );
        }
        let mut namespace_labels = BTreeMap::new();
        for ns in &inputs.namespaces {
            if let Some(name) = ns.metadata.name.clone() {
                namespace_labels.insert(name, ns.metadata.labels.clone().unwrap_or_default());
            }
        }
        let mut slices: BTreeMap<(String, String), Vec<&EndpointSlice>> = BTreeMap::new();
        for slice in &inputs.endpointslices {
            if let Some(key) = slice_service(slice) {
                slices.entry(key).or_default().push(slice.as_ref());
            }
        }
        Self {
            services,
            namespace_labels,
            secrets,
            slices,
            health_check,
        }
    }
}

/// A Service port's protocol, `TCP` when unset (the apiserver's default).
fn port_protocol(port: &ServicePort) -> &str {
    port.protocol.as_deref().unwrap_or("TCP")
}

/// The Service port a backend reference names, and the id of the cluster it
/// maps to.
///
/// The id is `namespace.service.port` — except that a Service may expose one
/// number twice, over two protocols (`53/UDP` and `53/TCP`), each entry with
/// its own `targetPort` and `appProtocol`. One cluster for both would pool
/// the two target ports' pod addresses into the backends of every route to
/// either, and an HTTP route (which takes the TCP entry) and a UDPRoute (which
/// takes the first) to one such number would each send part of their traffic
/// to the other's port. So the first entry carrying a number keeps the bare id,
/// which every reference by number resolved to before HTTP routes preferred
/// TCP, and a later entry gets its protocol appended: `namespace.service.53.tcp`.
fn service_port<'a>(
    index: &Index<'a>,
    namespace: &str,
    service: &str,
    port_ref: &PortRef,
) -> Result<(&'a ServicePort, String), Problem> {
    let svc = index
        .services
        .get(&(namespace.to_string(), service.to_string()))
        .ok_or_else(|| Problem::ServiceNotFound {
            service: service.to_string(),
        })?;

    let ports: &'a [ServicePort] = svc
        .spec
        .as_ref()
        .and_then(|s| s.ports.as_deref())
        .unwrap_or_default();
    let first_with = |n: i32| ports.iter().find(|p| p.port == n);
    let port = match port_ref {
        PortRef::Number(n) => first_with(*n),
        PortRef::HttpNumber(n) => ports
            .iter()
            .find(|p| p.port == *n && port_protocol(p) == "TCP")
            .or_else(|| first_with(*n)),
        PortRef::Name(name) => ports
            .iter()
            .find(|p| p.name.as_deref() == Some(name.as_str())),
    };
    let port = port.ok_or_else(|| Problem::ServicePortNotFound {
        service: service.to_string(),
        port: match port_ref {
            PortRef::Number(n) | PortRef::HttpNumber(n) => n.to_string(),
            PortRef::Name(n) => n.clone(),
        },
    })?;
    // `(port, protocol)` is unique within a Service, so comparing protocols
    // tells whether `port` is the first entry carrying its number.
    let cluster_id = if first_with(port.port).map(port_protocol) == Some(port_protocol(port)) {
        format!("{namespace}.{service}.{}", port.port)
    } else {
        format!(
            "{namespace}.{service}.{}.{}",
            port.port,
            port_protocol(port).to_ascii_lowercase()
        )
    };
    Ok((port, cluster_id))
}

/// The Kubernetes standard application protocol (KEP-3726) for HTTP/2 over
/// cleartext with prior knowledge.
const APP_PROTOCOL_H2C: &str = "kubernetes.io/h2c";
/// The Kubernetes standard application protocol for WebSocket over TLS.
const APP_PROTOCOL_WSS: &str = "kubernetes.io/wss";

/// What a Service port's `appProtocol` asks of the connection an **HTTP**
/// route (Ingress, HTTPRoute) opens to its backends — Gateway API GEP-1911,
/// which requires the `kubernetes.io/*` constants to be recognised and a
/// backend the route cannot speak to to be reported.
///
/// Only the two constants Sōzu has an answer for are acted on. Every other
/// value — unset, `http`, `kubernetes.io/ws`, and the free-form `grpc`,
/// `http2`, `tcp`, `https` or mesh-specific spellings Services carry in front
/// of backends that also speak HTTP/1.1 — keeps the HTTP/1.1 every route got
/// before; refusing them would un-program routes that work. `https` would need
/// backend TLS too, but it is not a Kubernetes constant and is left alone.
///
/// Deliberately **not** consulted by TCPRoute/UDPRoute: a layer-4 route
/// forwards bytes, so a TLS port behind one is plain passthrough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HttpBackendProtocol {
    /// HTTP/1.1, Sōzu's default towards a backend.
    Http1,
    /// `kubernetes.io/h2c`: the cluster gets Sōzu's `http2`. On Sōzu 2.3.2 an
    /// HTTP/2 client (HTTPS listener, ALPN h2) and an HTTP/1.1 client alike
    /// then reach the backend as HTTP/2 — measured, see
    /// `docs/probes/h2c-backend_sozu-2.3.2_2026-10-19.txt`.
    H2c,
    /// `kubernetes.io/wss`: WebSocket over TLS. Sōzu 2.3.2 has no TLS towards
    /// a backend (`AddBackend` carries an address, never a TLS setting), so
    /// the route cannot speak the declared protocol.
    ///
    /// Reported ([`Self::problem`]) and still programmed as before: cleartext
    /// HTTP/1.1 to the TLS port, which fails there. Taking the rule out would
    /// let another rule answer its requests instead — `/ws` to such a port
    /// next to `/` to another would send `/ws` to the `/` backend. Nor is it
    /// pointed at an empty cluster for Sōzu's `503`: every other invalid
    /// `backendRef` drops its match today, and an error answer of Sōzu's own,
    /// for all of them, is #78.
    Unsupported { port: i32, app_protocol: String },
}

impl HttpBackendProtocol {
    /// Classify the port a backend reference names; an HTTP route names a
    /// number as [`PortRef::HttpNumber`], so this is the TCP entry when the
    /// number is also exposed over UDP or SCTP. A reference that does not resolve is
    /// `Http1` here: [`add_service_route`] reports why.
    pub(crate) fn of(index: &Index, namespace: &str, service: &str, port_ref: &PortRef) -> Self {
        let Ok((port, _)) = service_port(index, namespace, service, port_ref) else {
            return Self::Http1;
        };
        match port.app_protocol.as_deref() {
            Some(APP_PROTOCOL_H2C) => Self::H2c,
            Some(APP_PROTOCOL_WSS) => Self::Unsupported {
                port: port.port,
                app_protocol: APP_PROTOCOL_WSS.to_string(),
            },
            _ => Self::Http1,
        }
    }

    /// What to report about a route to `service`'s port: an `appProtocol`
    /// Sōzu cannot speak. Never a reason to leave the route out.
    pub(crate) fn problem(&self, service: &str) -> Option<Problem> {
        match self {
            Self::Unsupported { port, app_protocol } => Some(Problem::UnsupportedAppProtocol {
                service: service.to_string(),
                port: *port,
                app_protocol: app_protocol.clone(),
            }),
            Self::Http1 | Self::H2c => None,
        }
    }

    /// Apply the protocol to the cluster an HTTP route resolved to.
    ///
    /// Only ever turns `http2` on; [`add_service_route`] creates the cluster
    /// with it off. A cluster is one Service port entry ([`service_port`]
    /// gives a TCP/UDP twin its own id), and `appProtocol` belongs to that
    /// entry, so every HTTP route reaching one cluster classifies it the same
    /// way and the order routes are built in cannot change the outcome. A
    /// layer-4 route resolving to the same entry shares the cluster but never
    /// touches the flag, and Sōzu's TCP and UDP proxies never read it (2.3.2:
    /// `cluster.http2` reaches only the HTTP router and health checks,
    /// `lib/src/server.rs:3289`, and `ConfigState::add_cluster` does not
    /// validate it), so a cluster both kinds share is served correctly by each.
    pub(crate) fn configure(&self, clusters: &mut BTreeMap<String, ir::Cluster>, cluster_id: &str) {
        if *self == Self::H2c {
            if let Some(cluster) = clusters.get_mut(cluster_id) {
                cluster.http2 = true;
            }
        }
    }
}

/// The port of one EndpointSlice that carries `svc_port`'s pods, if any.
///
/// Matched by name, the Service port's: the EndpointSlice controller writes
/// each slice port with its Service port's name and protocol, an unnamed one
/// as `""` where the Service has no name at all, so the two spellings are one.
/// A slice whose only port does not match falls back to it, as long as it
/// could be this port: unnamed, and of this port's protocol. Never `first()`
/// of several, which would route to the wrong container port. And never a
/// sole port named for another Service port or carrying another protocol:
/// the controller groups pods by the ports they resolve, so pods exposing only
/// one port's named `targetPort` get a slice of their own carrying that port
/// alone, and falling back to it would dial it for every Service port — both
/// entries of a TCP/UDP twin ([`service_port`]) included.
fn slice_port<'s>(ports: &'s [EndpointPort], svc_port: &ServicePort) -> Option<&'s EndpointPort> {
    fn name(name: &Option<String>) -> &str {
        name.as_deref().unwrap_or("")
    }
    if let Some(named) = ports.iter().find(|p| name(&p.name) == name(&svc_port.name)) {
        return Some(named);
    }
    match ports {
        // An EndpointSlice port's protocol is `TCP` when unset too.
        [sole]
            if name(&sole.name).is_empty()
                && sole.protocol.as_deref().unwrap_or("TCP") == port_protocol(svc_port) =>
        {
            Some(sole)
        }
        _ => None,
    }
}

/// Resolve an Ingress backend (service + port) to (cluster_id, pod addresses).
/// Non-fatal findings (an ignored FQDN slice) are appended to `problems`;
/// the `Err` variant is reserved for failures that leave nothing to program.
fn resolve_backends(
    index: &Index,
    namespace: &str,
    service: &str,
    port_ref: &PortRef,
    problems: &mut Vec<Problem>,
) -> Result<(String, i32, Vec<SocketAddr>), Problem> {
    let (svc_port, cluster_id) = service_port(index, namespace, service, port_ref)?;

    let mut addrs = Vec::new();
    if let Some(slices) = index
        .slices
        .get(&(namespace.to_string(), service.to_string()))
    {
        for slice in slices {
            // An FQDN-type slice carries hostnames, not IPs: every address
            // would fail the IpAddr parse below and vanish, surfacing only as
            // a misleading NoReadyEndpoints. Report it and skip the slice;
            // IPv4/IPv6 slices for the same Service keep resolving.
            if slice.address_type == "FQDN" {
                let problem = Problem::FqdnEndpointsUnsupported {
                    service: service.to_string(),
                };
                if !problems.contains(&problem) {
                    problems.push(problem);
                }
                continue;
            }
            let pod_port = slice
                .ports
                .as_deref()
                .and_then(|ports| slice_port(ports, svc_port))
                .and_then(|p| p.port);
            // The wire type is i32; a value outside u16 cannot be a real port
            // (API validation forbids it), but truncating with `as` would
            // silently rewrite it into a different port — skip it instead.
            let Some(pod_port) = pod_port.and_then(|p| u16::try_from(p).ok()) else {
                continue;
            };

            for endpoint in slice.endpoints.iter().flatten() {
                // Treat ready=None (unknown) as ready; exclude only ready=Some(false).
                let ready = endpoint
                    .conditions
                    .as_ref()
                    .and_then(|c| c.ready)
                    .unwrap_or(true);
                if !ready {
                    continue;
                }
                for addr in &endpoint.addresses {
                    if let Ok(ip) = addr.parse::<IpAddr>() {
                        addrs.push(SocketAddr::new(ip, pod_port));
                    }
                }
            }
        }
    }

    addrs.sort();
    addrs.dedup();
    Ok((cluster_id, svc_port.port, addrs))
}

/// Parse the `sozu.io/load-balancing` annotation value. Unknown values (and the
/// absence of the annotation) fall back to round-robin — a missing/typo'd value
/// keeps a valid, predictable default rather than failing the whole Service.
fn parse_lb_algorithm(value: &str) -> ir::LbAlgorithm {
    match value
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '_'], "-")
        .as_str()
    {
        "random" => ir::LbAlgorithm::Random,
        "least-loaded" => ir::LbAlgorithm::LeastLoaded,
        "power-of-two" => ir::LbAlgorithm::PowerOfTwo,
        _ => ir::LbAlgorithm::RoundRobin,
    }
}

/// Cluster-level settings read from the backing Service's annotations. The
/// cluster is 1:1 with a Service, so these live on the Service (not the route) —
/// both Ingress and Gateway refs to one Service then agree on one cluster
/// config, with no cross-route conflict.
struct ClusterSettings {
    load_balancing: ir::LbAlgorithm,
    sticky_session: bool,
    max_connections_per_ip: Option<u64>,
    retry_after: Option<u32>,
    /// `Err` when the Service's health-check annotations are unusable: the
    /// cluster then runs no check and the problem is reported.
    health_check: Result<Option<ir::HealthCheck>, Problem>,
}

fn cluster_settings(
    namespace: &str,
    service: &str,
    svc: Option<&Service>,
    health_check: &HealthCheckDefault,
) -> ClusterSettings {
    let annotations = svc.and_then(|s| s.metadata.annotations.as_ref());
    let get = |key: &str| annotations.and_then(|a| a.get(key)).map(|v| v.trim());
    ClusterSettings {
        load_balancing: get(LB_ANNOTATION)
            .map(parse_lb_algorithm)
            .unwrap_or_default(),
        sticky_session: get(STICKY_ANNOTATION).is_some_and(|v| v.eq_ignore_ascii_case("true")),
        // A non-numeric value is ignored (falls back to the global default)
        // rather than failing the Service.
        max_connections_per_ip: get(MAX_CONN_PER_IP_ANNOTATION).and_then(|v| v.parse().ok()),
        retry_after: get(RETRY_AFTER_ANNOTATION).and_then(|v| v.parse().ok()),
        health_check: service_health_check(&get, health_check).map_err(|reason| {
            Problem::InvalidHealthCheck {
                service: format!("{namespace}/{service}"),
                reason,
            }
        }),
    }
}

/// Whether the Service turns its health check on itself
/// (`sozu.io/health-check: tcp`), as opposed to inheriting the default.
pub(crate) fn service_asks_for_health_check(svc: Option<&Service>) -> bool {
    svc.and_then(|s| s.metadata.annotations.as_ref())
        .and_then(|a| a.get(HEALTH_CHECK_ANNOTATION))
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("tcp"))
}

/// The health check a Service's cluster runs, from its `sozu.io/health-check`
/// annotation (`tcp` | `off`) or, absent, the controller-wide default. A check
/// that is on starts from the default timing and each
/// `sozu.io/health-check-*` annotation overrides one field; they are read
/// only when it is on, so tuning a check that is off changes nothing.
///
/// `Err` carries why the result is unusable — an unknown mode, a value that
/// is not a whole number, or one Sōzu refuses ([`health_check_refusal`]).
/// The caller then programs **no** check rather than the default: the Service
/// set out to change the default, and silently running something else would
/// read as the change having taken.
fn service_health_check<'v>(
    get: &impl Fn(&str) -> Option<&'v str>,
    default: &HealthCheckDefault,
) -> Result<Option<ir::HealthCheck>, String> {
    let enabled = match get(HEALTH_CHECK_ANNOTATION) {
        None => default.enabled,
        Some(mode) if mode.eq_ignore_ascii_case("tcp") => true,
        Some(mode) if mode.eq_ignore_ascii_case("off") => false,
        Some(mode) => {
            return Err(format!(
                "{HEALTH_CHECK_ANNOTATION} is {:?}; expected \"tcp\" or \"off\"",
                display_path(mode)
            ))
        }
    };
    if !enabled {
        return Ok(None);
    }
    let whole = |key: &str, field: &mut u32| -> Result<(), String> {
        if let Some(value) = get(key) {
            *field = value
                .parse()
                .map_err(|_| format!("{key} is {:?}, not a whole number", display_path(value)))?;
        }
        Ok(())
    };
    let mut check = default.check.clone();
    whole(HEALTH_CHECK_INTERVAL_ANNOTATION, &mut check.interval_secs)?;
    whole(HEALTH_CHECK_TIMEOUT_ANNOTATION, &mut check.timeout_secs)?;
    whole(
        HEALTH_CHECK_HEALTHY_ANNOTATION,
        &mut check.healthy_threshold,
    )?;
    whole(
        HEALTH_CHECK_UNHEALTHY_ANNOTATION,
        &mut check.unhealthy_threshold,
    )?;
    if let Some(value) = get(HEALTH_CHECK_PORT_ANNOTATION) {
        // A number only: the probe dials the backend's IP, and a named port
        // would need the Pod spec, which the controller does not watch.
        check.port = Some(value.parse().map_err(|_| {
            format!(
                "{HEALTH_CHECK_PORT_ANNOTATION} is {:?}, not a port number",
                display_path(value)
            )
        })?);
    }
    match health_check_refusal(&check) {
        None => Ok(Some(check)),
        Some(reason) => Err(format!("Sōzu refuses it: {reason}")),
    }
}

/// Resolve a Service+port and upsert its cluster + pod-IP backends into the
/// shared accumulators. Shared by the Ingress and Gateway API mappers so both
/// feed the *same* IR. Returns `(cluster_id, has_ready_endpoints)`; non-fatal
/// findings (an ignored FQDN slice) are appended to `problems`.
// The two accumulator sets (`referenced`, `problems`) pushed it past the
// lint's limit; bundling them would just move the noise to every call site.
#[allow(clippy::too_many_arguments)]
pub(crate) fn add_service_route(
    index: &Index,
    clusters: &mut BTreeMap<String, ir::Cluster>,
    backends: &mut BTreeMap<String, ir::Backend>,
    referenced: &mut BTreeSet<String>,
    namespace: &str,
    service: &str,
    port_ref: &PortRef,
    problems: &mut Vec<Problem>,
) -> Result<(String, bool), Problem> {
    // Recorded before resolution, success or failure alike: an EndpointSlice
    // appearing later for a still-broken target must read as relevant.
    referenced.insert(format!("{namespace}/{service}"));
    let (cluster_id, _port, addrs) =
        resolve_backends(index, namespace, service, port_ref, problems)?;
    let svc = index
        .services
        .get(&(namespace.to_string(), service.to_string()))
        .copied();
    let s = cluster_settings(namespace, service, svc, index.health_check);
    // Every object routing to the Service is told, not only the first: each
    // of them reads as served by an unchecked cluster.
    let health_check = s.health_check.unwrap_or_else(|problem| {
        if !problems.contains(&problem) {
            problems.push(problem);
        }
        None
    });
    clusters.entry(cluster_id.clone()).or_insert(ir::Cluster {
        id: cluster_id.clone(),
        load_balancing: s.load_balancing,
        sticky_session: s.sticky_session,
        https_redirect: false,
        max_connections_per_ip: s.max_connections_per_ip,
        retry_after: s.retry_after,
        // Set by an HTTP route only (`HttpBackendProtocol::configure`): a
        // layer-4 route ignores `appProtocol`.
        http2: false,
        health_check,
    });
    let backend_ids: Vec<String> = addrs
        .iter()
        .map(|addr| format!("{cluster_id}#{addr}"))
        .collect();
    // Keyed on the cluster's own setting, not this call's: the first reference
    // to a cluster fixes it (`or_insert` above), and a sticky cluster needs a
    // sticky id on every backend.
    let sticky = clusters
        .get(&cluster_id)
        .is_some_and(|cluster| cluster.sticky_session);
    let service_uid = svc.and_then(|s| s.metadata.uid.as_deref()).unwrap_or("");
    let mut sticky_ids = sticky
        .then(|| sticky_ids(service_uid, &backend_ids))
        .map(Vec::into_iter);
    for (addr, backend_id) in addrs.iter().zip(backend_ids) {
        let sticky_id = sticky_ids.as_mut().and_then(Iterator::next);
        backends.entry(backend_id.clone()).or_insert(ir::Backend {
            cluster_id: cluster_id.clone(),
            backend_id,
            address: *addr,
            weight: None,
            sticky_id,
        });
    }
    Ok((cluster_id, !addrs.is_empty()))
}

/// Bytes of the HMAC kept in a sticky id (16 hex characters).
const STICKY_ID_BYTES: usize = 8;

/// Sticky-session cookie values for one cluster's backends, in input order.
///
/// Sōzu hands the value to every client of a sticky cluster, so it must not be
/// the backend id, which spells out namespace, Service and pod IP. It is an
/// HMAC-SHA256 of the backend id keyed by the Service UID: every controller
/// replica and every restart derives the same value from the same objects — a
/// client pinned by one replica stays pinned through another — while a client,
/// who never sees the UID, cannot enumerate pod IPs against it. A recreated
/// Service gets a new UID and so new cookies, which only re-pins its clients
/// once.
///
/// Values are unique within the call: a truncated HMAC can collide, and Sōzu
/// resolves a cookie to the *first* backend carrying it, so a duplicate would
/// silently merge two backends' sessions. On a clash the later backend (input
/// order, which the caller sorts by address) re-derives with a counter. That
/// makes an id depend on its colliding peers: a newcomer sorting first takes an
/// existing backend's id, and removing a backend that won a clash hands its id
/// back to the one that lost it, re-pinning that backend's clients either way.
/// At 64 bits a clash is odds of about n²/2⁶⁵ per cluster, so that cost is
/// theoretical; without a clash, pods joining or leaving move no other id.
fn sticky_ids(service_uid: &str, backend_ids: &[String]) -> Vec<String> {
    unique_truncated_hmacs(service_uid, backend_ids, STICKY_ID_BYTES)
}

/// [`sticky_ids`] at any truncation, so a test can force collisions. `bytes`
/// must leave room for every input: 256^bytes distinct values.
fn unique_truncated_hmacs(key: &str, backend_ids: &[String], bytes: usize) -> Vec<String> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key.as_bytes());
    let mut taken: BTreeSet<String> = BTreeSet::new();
    backend_ids
        .iter()
        .map(|backend_id| {
            let mut attempt: u32 = 0;
            loop {
                let mut ctx = ring::hmac::Context::with_key(&key);
                ctx.update(backend_id.as_bytes());
                if attempt > 0 {
                    // NUL never occurs in a backend id: no retry input can
                    // spell another backend's first one.
                    ctx.update(&[0]);
                    ctx.update(&attempt.to_be_bytes());
                }
                let tag = ctx.sign();
                let candidate: String = tag
                    .as_ref()
                    .iter()
                    .take(bytes)
                    .map(|b| format!("{b:02x}"))
                    .collect();
                if taken.insert(candidate.clone()) {
                    break candidate;
                }
                attempt += 1;
            }
        })
        .collect()
}

/// Where an HTTP(S) frontend came from, so a route-key collision can be
/// reported on the losing object's own result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrontendSource {
    Ingress {
        namespace: String,
        name: String,
    },
    /// An HTTPRoute rule, attributed to one (route, parentRef) pair —
    /// collision problems land on that parent's [`RouteParentResult`]. The
    /// parentRef's `sectionName`/`port` are part of the identity: a route may
    /// carry several parentRefs to the *same* Gateway, and the collision must
    /// land on the one that produced the losing frontend.
    HttpRoute {
        namespace: String,
        name: String,
        gateway_namespace: String,
        gateway_name: String,
        section_name: Option<String>,
        port: Option<i32>,
    },
}

/// An IR frontend paired with its source, carried until collision resolution.
pub(crate) struct SourcedFrontend {
    pub(crate) frontend: ir::Frontend,
    pub(crate) source: FrontendSource,
}

/// The route identity Sōzu keys on — the exact string its frontend map uses
/// ([`ir::SozuRouteKey`]), built from the *compiled* path rule. So an `Exact`
/// and a user regex that spell the same anchored pattern collide here exactly
/// as they do in Sōzu, and so do two routes that differ as tuples but not as
/// Sōzu's unescaped `;`-joined string (a regex `/x;GET` and a regex `/x` with
/// `method: GET`). The loser is reported rather than left for Sōzu to refuse,
/// which would fail every reconcile of the shared instance. The target cluster
/// is *not* part of the key.
type RouteKey = ir::SozuRouteKey;

/// The raw path value of a match, for problem context.
fn path_value(p: &ir::PathMatch) -> &str {
    match p {
        ir::PathMatch::Prefix(v) | ir::PathMatch::Exact(v) | ir::PathMatch::Regex(v) => v,
    }
}

fn route_key(frontend: &ir::Frontend) -> RouteKey {
    frontend.sozu_route_key()
}

/// Whether two frontends sharing a [`RouteKey`] also *match* the same requests:
/// same listener, hostname, compiled rule and method. Only then is the loser a
/// true duplicate; otherwise it is a different route that Sōzu's key spelling
/// cannot hold alongside the winner, and dropping it loses traffic.
fn same_match(a: &ir::Frontend, b: &ir::Frontend) -> bool {
    a.tls == b.tls
        && a.listener == b.listener
        && a.hostname == b.hostname
        && a.path.sozu_rule() == b.path.sozu_rule()
        && a.method == b.method
}

/// A source object's claim on a hostname, for arbitrating who wins a contested
/// route key or SNI name: oldest `creationTimestamp` first (absent sorts last),
/// then `namespace/name`. This is the Gateway API's own tie-break, applied
/// uniformly to Ingress and HTTPRoute so no tenant can take a host+path from an
/// older one by having a "smaller" backend name — the previous policy, which
/// sorted on the cluster id (`{ns}.{svc}.{port}`) and so followed the *backend*
/// namespace's spelling. `creationTimestamp` is compared as its fixed-width,
/// UTC, `Z`-suffixed RFC 3339 string, which sorts chronologically. The
/// namespace and name are compared as the single `namespace/name` spelling,
/// not as a `(namespace, name)` tuple: `-` sorts before `/`, so `a-b/z` must
/// beat `a/a`, which a tuple comparison reverses.
type OwnerClaim = (bool, Option<String>, String);

/// The [`OwnerClaim`] of the object `meta` describes.
fn owner_claim(
    namespace: &str,
    name: &str,
    meta: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> OwnerClaim {
    let ts = creation_key(meta);
    (ts.is_none(), ts, format!("{namespace}/{name}"))
}

/// `(kind, namespace, name)` — a source object's identity, kind distinguishing
/// an Ingress from an HTTPRoute or a Gateway so two of different kinds sharing
/// a name are still separate owners. The kind is the last tie-break, after an
/// identical claim.
type OwnerIdent = (u8, String, String);

const INGRESS_OWNER: u8 = 0;
const HTTP_ROUTE_OWNER: u8 = 1;
const GATEWAY_OWNER: u8 = 2;

/// The Kubernetes kind an [`OwnerIdent`] stands for, as a report names it.
fn owner_kind(ident: &OwnerIdent) -> &'static str {
    match ident.0 {
        INGRESS_OWNER => "Ingress",
        HTTP_ROUTE_OWNER => "HTTPRoute",
        _ => "Gateway",
    }
}

fn owner_ident(source: &FrontendSource) -> OwnerIdent {
    match source {
        FrontendSource::Ingress { namespace, name } => {
            (INGRESS_OWNER, namespace.clone(), name.clone())
        }
        FrontendSource::HttpRoute {
            namespace, name, ..
        } => (HTTP_ROUTE_OWNER, namespace.clone(), name.clone()),
    }
}

/// The claim recorded for a source, or a last-place fallback (absent timestamp,
/// its own namespace/name) when the caller supplied no map entry — only tests
/// build frontends with no backing object.
fn claim_of(source: &FrontendSource, claims: &BTreeMap<OwnerIdent, OwnerClaim>) -> OwnerClaim {
    claim_for(&owner_ident(source), claims)
}

fn claim_for(ident: &OwnerIdent, claims: &BTreeMap<OwnerIdent, OwnerClaim>) -> OwnerClaim {
    claims
        .get(ident)
        .cloned()
        .unwrap_or_else(|| (true, None, format!("{}/{}", ident.1, ident.2)))
}

/// Keep exactly one frontend per Sōzu route key and report the losers.
///
/// The winner is the **oldest claimant** of the contested key (`creationTimestamp`,
/// then `namespace/name`; see [`OwnerClaim`]), Ingress and HTTPRoute alike, with
/// emission order breaking a tie between two frontends of the *same* object (an
/// HTTPRoute's own rules, in rule order). Backend names, redirects and rejection
/// backends never determine the winner.
///
/// Sōzu's key is the `;`-joined string of listener, hostname, path kind/value
/// and method ([`RouteKey`]), never the target cluster. Prefixes must already
/// have their trailing slash canonicalised. A repeated match from the *same*
/// object is legal input (an HTTPRoute's repeated matches, an Ingress's own
/// rules) and stays benign; so does an identical match with an identical
/// effect from another object. Anything else sharing the key is a collision
/// on the losing object's own source and parentRef — including two different
/// matches of one object that only Sōzu's key spelling merges.
///
/// The second list is the frontends folded into a kept one because they match
/// the same requests with the same effect (cluster and filters) from another
/// source — another object, or another parentRef of the same one. Sōzu holds
/// one route for them, but its traffic is theirs too: whatever is said about
/// the cluster it reaches (`settle_health_checks`) concerns their owners as
/// much as the kept one's.
fn resolve_frontend_collisions(
    frontends: Vec<SourcedFrontend>,
    claims: &BTreeMap<OwnerIdent, OwnerClaim>,
) -> (
    Vec<SourcedFrontend>,
    Vec<SourcedFrontend>,
    Vec<(FrontendSource, Problem)>,
) {
    // Sort by the owner's claim, then by emission order so an object's own
    // frontends keep their rule order. The tuple comparator is transitive, so
    // one policy covers every Ingress/HTTPRoute pairing.
    let mut frontends: Vec<_> = frontends.into_iter().enumerate().collect();
    frontends.sort_by(|(a_order, a), (b_order, b)| {
        claim_of(&a.source, claims)
            .cmp(&claim_of(&b.source, claims))
            .then(a_order.cmp(b_order))
    });

    let mut kept: Vec<SourcedFrontend> = Vec::new();
    let mut winners: BTreeMap<RouteKey, usize> = BTreeMap::new();
    for (_, sf) in &frontends {
        let key = route_key(&sf.frontend);
        if let std::collections::btree_map::Entry::Vacant(entry) = winners.entry(key) {
            entry.insert(kept.len());
            kept.push(SourcedFrontend {
                frontend: sf.frontend.clone(),
                source: sf.source.clone(),
            });
        }
    }

    // Resolve all candidates against the final winner. Events must name the
    // actual winning backend.
    let mut collisions = Vec::new();
    let mut folded = Vec::new();
    for (_, sf) in frontends {
        let index = winners[&route_key(&sf.frontend)];
        let winner = &kept[index].frontend;
        // A repeated match from the same object is its own rules/matches,
        // resolved by emission order — legal API input, never a collision. An
        // identical match with an identical effect is benign whoever owns it.
        // A *different* match that only shares Sōzu's key spelling is always
        // reported, even within one object: it is not served.
        let same_match = same_match(winner, &sf.frontend);
        let same_effect =
            winner.cluster_id == sf.frontend.cluster_id && winner.filters == sf.frontend.filters;
        if same_match
            && (owner_ident(&kept[index].source) == owner_ident(&sf.source) || same_effect)
        {
            // The kept frontend itself, and a repeat from its own source, add
            // nothing; a same-object repeat with another effect is shadowed by
            // emission order and serves nothing.
            if same_effect && sf.source != kept[index].source {
                folded.push(sf);
            }
            continue;
        }
        collisions.push((
            sf.source,
            Problem::RouteCollision {
                hostname: sf.frontend.hostname.clone(),
                path: path_value(&sf.frontend.path).to_string(),
                winner: winner
                    .cluster_id
                    .clone()
                    .unwrap_or_else(|| "<redirect>".to_string()),
                key_only: !same_match,
            },
        ));
    }
    (kept, folded, collisions)
}

/// Leave a health check only on a cluster Sōzu would run it for: one that a
/// programmed HTTP(S) frontend proxies to or a TCP layer-4 frontend routes
/// to, and that no UDP frontend routes to.
///
/// [`add_service_route`] creates a cluster, check included, as soon as a
/// route resolves its Service — before route-key arbitration and before a
/// layer-4 socket is settled — and a cluster no frontend names is still sent
/// to Sōzu, whose every worker on every replica would then probe backends
/// nothing is routed to: the loser of a route-key collision, a UDPRoute that
/// lost its socket (its datagram-only backends then get TCP probes), a rule
/// whose every match was skipped. A redirect frontend sends nothing to its
/// cluster, so it keeps none alive.
///
/// UDP: the translator marks a cluster UDP as soon as one UDP frontend names
/// it and then leaves the check out (Sōzu does not refuse one there, and has
/// a separate UDP health config for datagrams). One Service port is one
/// cluster whatever the route kind, so a TCPRoute, an Ingress or an HTTPRoute
/// to a Service port a UDPRoute also routes to — DNS on 53/TCP and 53/UDP —
/// loses its check. Dropped here rather than left to the translator so the
/// IR, and the shadow written from it, say what Sōzu runs.
///
/// A check lost to UDP is reported (`HealthCheckNotApplied`) on every
/// TCPRoute parent, Ingress and HTTPRoute parent routing to the shared
/// cluster, once each; and on the UDPRoute when the cluster also carries TCP
/// or HTTP traffic, or when its Service turns the check on itself. "Every"
/// includes the owners of benign duplicates: route-key arbitration keeps one
/// of several identical HTTP(S) frontends (`folded` holds the others) and a
/// layer-4 socket one of several identical claims (each still an
/// [`gateway::L4Carrier`]), yet each of those owners is served through the
/// shared cluster and reads as checked unless told. A UDP-only
/// Service under the controller-wide default is not reported: excluding
/// datagrams is how the default is documented. Nor is a cluster nothing
/// routes to: no traffic goes unchecked, and its owner is already told why
/// its route serves nothing.
fn settle_health_checks(
    clusters: &mut BTreeMap<String, ir::Cluster>,
    frontends: &[SourcedFrontend],
    folded: &[SourcedFrontend],
    l4: &[gateway::L4Carrier],
    results: &mut [IngressResult],
    routes: &mut [RouteResult],
) {
    // Cluster id -> the first UDPRoute (in tie-break order) routing to it,
    // and its Service: one cluster id is one Service port, so every route to
    // that cluster names the same Service.
    let mut udp: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    let mut tcp: BTreeSet<&str> = BTreeSet::new();
    for carrier in l4 {
        match carrier.protocol {
            ir::L4Protocol::Udp => {
                udp.entry(carrier.cluster_id.as_str())
                    .or_insert((carrier.route_name.as_str(), carrier.service.as_str()));
            }
            ir::L4Protocol::Tcp => {
                tcp.insert(carrier.cluster_id.as_str());
            }
        }
    }
    // Every owner an HTTP(S) frontend proxies for: the kept frontends, and
    // the identical ones arbitration folded into them, whose owners Sōzu
    // serves through the same route.
    let proxied: Vec<(&str, &FrontendSource)> = frontends
        .iter()
        .chain(folded)
        .filter(|sf| sf.frontend.filters.redirect.is_none())
        .filter_map(|sf| Some((sf.frontend.cluster_id.as_deref()?, &sf.source)))
        .collect();
    let http: BTreeSet<&str> = proxied.iter().map(|(cluster_id, _)| *cluster_id).collect();

    let has_check = |cluster_id: &str| {
        clusters
            .get(cluster_id)
            .is_some_and(|cluster| cluster.health_check.is_some())
    };
    let lost_to = |udp_route: &str| {
        format!(
            "UDPRoute {udp_route} routes the same Service port over UDP, so Sōzu holds one \
             UDP cluster for both and runs no TCP health check on it"
        )
    };
    let push_once = |problems: &mut Vec<Problem>, problem: Problem| {
        if !problems.contains(&problem) {
            problems.push(problem);
        }
    };

    for carrier in l4 {
        let cluster_id = carrier.cluster_id.as_str();
        let Some((udp_route, _)) = udp.get(cluster_id) else {
            continue;
        };
        if !has_check(cluster_id) {
            continue;
        }
        let reason = match carrier.protocol {
            ir::L4Protocol::Tcp => lost_to(udp_route),
            ir::L4Protocol::Udp if tcp.contains(cluster_id) || http.contains(cluster_id) => {
                "this route makes the Service port's cluster a UDP one, which runs no TCP \
                 health check, so its TCP or HTTP traffic goes unchecked too"
                    .to_string()
            }
            ir::L4Protocol::Udp if carrier.asks_for_health_check => {
                "Sōzu's TCP health check does not run on UDP traffic".to_string()
            }
            ir::L4Protocol::Udp => continue,
        };
        push_once(
            &mut routes[carrier.route].parents[carrier.parent].problems,
            Problem::HealthCheckNotApplied {
                service: carrier.service.clone(),
                reason,
            },
        );
    }
    for (cluster_id, source) in &proxied {
        let Some((udp_route, service)) = udp.get(cluster_id) else {
            continue;
        };
        if !has_check(cluster_id) {
            continue;
        }
        let problem = Problem::HealthCheckNotApplied {
            service: service.to_string(),
            reason: lost_to(udp_route),
        };
        // Deduplicated per owner: an Ingress's HTTP and HTTPS frontends, its
        // hosts and paths, all lose the same check.
        match source {
            FrontendSource::Ingress { namespace, name } => {
                if let Some(result) = results
                    .iter_mut()
                    .find(|r| &r.namespace == namespace && &r.name == name)
                {
                    push_once(&mut result.problems, problem);
                }
            }
            FrontendSource::HttpRoute {
                namespace,
                name,
                gateway_namespace,
                gateway_name,
                section_name,
                port,
            } => {
                if let Some(parent) = routes
                    .iter_mut()
                    .filter(|r| r.kind == RouteKind::HttpRoute)
                    .find(|r| &r.namespace == namespace && &r.name == name)
                    .and_then(|r| {
                        r.parents.iter_mut().find(|p| {
                            &p.gateway_namespace == gateway_namespace
                                && &p.gateway_name == gateway_name
                                && &p.section_name == section_name
                                && &p.port == port
                        })
                    })
                {
                    push_once(&mut parent.problems, problem);
                }
            }
        }
    }

    for (id, cluster) in clusters.iter_mut() {
        let carries_tcp = http.contains(id.as_str()) || tcp.contains(id.as_str());
        if !carries_tcp || udp.contains_key(id.as_str()) {
            cluster.health_check = None;
        }
    }
}

/// An IR certificate paired with its leaf fingerprint (SHA-256 of the DER, as
/// computed by [`extract_cert`]) — Sōzu's identity for a loaded certificate.
/// Carried until [`merge_certificates`] so the merge keys on the DER identity,
/// not on PEM bytes.
pub(crate) struct FingerprintedCert {
    pub(crate) fingerprint: Vec<u8>,
    pub(crate) cert: ir::Certificate,
}

/// The object that asked for a certificate: the claimant of its SNI names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CertificateSource {
    Ingress {
        namespace: String,
        name: String,
    },
    /// One listener of a Gateway; the Gateway is the claimant, the listener
    /// is where its status reports a lost name.
    Gateway {
        namespace: String,
        name: String,
        listener: String,
    },
}

impl CertificateSource {
    fn owner_ident(&self) -> OwnerIdent {
        match self {
            CertificateSource::Ingress { namespace, name } => {
                (INGRESS_OWNER, namespace.clone(), name.clone())
            }
            CertificateSource::Gateway {
                namespace, name, ..
            } => (GATEWAY_OWNER, namespace.clone(), name.clone()),
        }
    }

    fn listener(&self) -> Option<String> {
        match self {
            CertificateSource::Ingress { .. } => None,
            CertificateSource::Gateway { listener, .. } => Some(listener.clone()),
        }
    }
}

/// A loaded certificate, carried with who asked for it until its names are
/// arbitrated ([`arbitrate_certificate_names`]).
pub(crate) struct ClaimedCert {
    pub(crate) cert: FingerprintedCert,
    /// The Secret it was read from, named in the report of a lost name.
    pub(crate) secret: String,
    pub(crate) source: CertificateSource,
}

/// Give every SNI name at an HTTPS listener to one claimant and drop it from
/// every other claimant's certificate.
///
/// For an SNI several of its certificates carry, Sōzu serves the one that
/// expires last, the most recently inserted on a tie (`lib/src/tls.rs`, the
/// resolver sorts the candidates by expiration and takes the last). Left to that,
/// a tenant in any namespace takes another's hostname by loading a
/// later-expiring certificate that names it — a substitution proven live on
/// 2026-09-21. The claimants of a name are the objects that declared it with
/// a certificate (an Ingress `tls[].hosts` entry, a Gateway listener's
/// `hostname`, or the names inferred for a listener without one), and the
/// oldest wins: the [`OwnerClaim`] that settles a contested route key, so
/// both arbitrations name the same owner.
///
/// Oldest means the *object's* `creationTimestamp`, not when it first named
/// the host: a tenant owning an Ingress or Gateway older than the victim's
/// takes the name by adding it to that object, as it takes a route key. The
/// substitution is closed only against tenants whose objects are all newer
/// than the victim's; by expiry, any tenant could win.
///
/// Ownership is per name, never per certificate. Two objects naming one
/// SNI with the same certificate (same fingerprint) do not conflict, but a
/// newer object sharing an older one's leaf gains no seniority for names the
/// older object never claimed. Every certificate the winning object loaded
/// for the name keeps it (an RSA and an ECDSA certificate, say); every other
/// claim is reported on its object, one report per certificate that object
/// asked for.
///
/// Only names are settled here. Every entry comes back, an entry stripped of
/// all its names included, because the material a leaf is served with is
/// chosen by [`merge_certificates`] among *every* entry carrying that leaf:
/// an object that lost all its names may still hold the fullest chain for a
/// leaf an owner serves, and dropping its entry here would serve the owner's
/// name with a shorter chain. The merge drops a leaf whose entries keep no
/// name at all, rather than emitting `names: []`, which Sōzu reads as "infer
/// them from the CN/SAN" and so would claim the lost names again.
///
/// Every step is a map lookup per name, never a scan: a tenant lists as many
/// hosts as its objects hold, and this runs inline in the global reconcile.
///
/// Stripping a name from a certificate that stays loaded is a
/// same-fingerprint `ReplaceCertificate`, which Sōzu applies only since 2.3.2
/// (0bc160b7, #81); 2.2.1 acknowledged it and kept serving the name.
///
/// Names are compared as Sōzu 2.3.2 stores them, ASCII-lowercased at load. A
/// wildcard and an exact name are different names here, and Sōzu's trie
/// prefers the exact one, so a `*.example.com` owner does not stop a newer
/// `app.example.com` claim; that overlap is not arbitrated.
fn arbitrate_certificate_names(
    certs: Vec<ClaimedCert>,
    claims: &BTreeMap<OwnerIdent, OwnerClaim>,
) -> (Vec<FingerprintedCert>, Vec<(CertificateSource, Problem)>) {
    type NameKey = (SocketAddr, String);
    let key = |c: &ClaimedCert, name: &str| (c.cert.cert.listener, name.to_ascii_lowercase());
    let owners: Vec<OwnerIdent> = certs.iter().map(|c| c.source.owner_ident()).collect();
    let ranks: Vec<OwnerClaim> = owners.iter().map(|o| claim_for(o, claims)).collect();
    // The ident breaks a tie between two kinds sharing a claim, so the winner
    // never follows input order.
    let rank = |i: usize| (&ranks[i], &owners[i]);

    // The oldest claimant of every (listener, name), kept as the index of
    // one of its certificates.
    let mut winners: BTreeMap<NameKey, usize> = BTreeMap::new();
    for (i, c) in certs.iter().enumerate() {
        for name in &c.cert.cert.names {
            let best = winners.entry(key(c, name)).or_insert(i);
            if rank(i) < rank(*best) {
                *best = i;
            }
        }
    }
    // Every certificate the winner loaded for each of its names. Another
    // object's copy of one of them is the same certificate, not a rival.
    let mut winning: BTreeSet<(NameKey, &[u8])> = BTreeSet::new();
    for (i, c) in certs.iter().enumerate() {
        for name in &c.cert.cert.names {
            let k = key(c, name);
            if winners.get(&k).is_some_and(|&w| owners[w] == owners[i]) {
                winning.insert((k, c.cert.fingerprint.as_slice()));
            }
        }
    }

    // What each object lost, grouped per certificate it asked for: one
    // Secret listed twice, or loaded for two entries, is still one report.
    let mut lost: BTreeMap<(&CertificateSource, &str), BTreeSet<ClaimedName>> = BTreeMap::new();
    let mut kept_names: Vec<Vec<String>> = Vec::with_capacity(certs.len());
    for c in &certs {
        let mut names = Vec::with_capacity(c.cert.cert.names.len());
        let mut lost_here = Vec::new();
        for name in &c.cert.cert.names {
            let k = key(c, name);
            let Some(&winner) = winners.get(&k) else {
                names.push(name.clone());
                continue;
            };
            if winning.contains(&(k, c.cert.fingerprint.as_slice())) {
                names.push(name.clone());
                continue;
            }
            lost_here.push(ClaimedName {
                name: name.clone(),
                owner_kind: owner_kind(&owners[winner]),
                owner: ranks[winner].2.clone(),
            });
        }
        if !lost_here.is_empty() {
            lost.entry((&c.source, c.secret.as_str()))
                .or_default()
                .extend(lost_here);
        }
        kept_names.push(names);
    }
    let lost = lost
        .into_iter()
        .map(|((source, secret), names)| {
            let problem = Problem::CertificateNameClaimed {
                secret: secret.to_string(),
                listener: source.listener(),
                names: names.into_iter().collect(),
            };
            (source.clone(), problem)
        })
        .collect();

    let kept = certs
        .into_iter()
        .zip(kept_names)
        .map(|(mut c, names)| {
            c.cert.cert.names = names;
            c.cert
        })
        .collect();
    (kept, lost)
}

/// Merge certificates that share Sōzu's identity at a listener — same leaf
/// `(listener, fingerprint)` — into one entry, unioning their SNI names. The
/// same certificate routinely backs several routes with different hostnames
/// (e.g. an Ingress and a Gateway listener); Sōzu stores exactly one
/// certificate per `(listener, fingerprint)`, so the IR must present one too.
/// Otherwise the translator would emit a `ReplaceCertificate` on every
/// reconcile, forever flipping between the conflicting name sets. Keying on
/// the fingerprint (not the PEM text) matters: two Secrets holding the same
/// DER re-encoded with different line wrapping (cert-manager vs hand-made)
/// are still one certificate to Sōzu. The leaf is the same DER, but the
/// Secrets may disagree on the rest — one carries the intermediates, another
/// not — and Sōzu serves one chain per certificate: the entry with the most
/// chain links wins (an incomplete chain breaks clients that lack the
/// intermediate), then the smallest material, so the order the Secrets are
/// read in never decides what is served (otherwise a changed read order would
/// replace the certificate). Empty overrides are resolved from CN/SAN by
/// [`extract_cert`] before reaching this union, so an empty name set here is
/// one [`arbitrate_certificate_names`] stripped. The material is chosen among
/// every entry for the leaf, those left with no name included, so who keeps
/// the names never decides the chain a name is served with; then a leaf whose
/// entries kept no name at all is dropped, never emitted with `names: []`.
fn merge_certificates(certs: Vec<FingerprintedCert>) -> Vec<ir::Certificate> {
    let mut merged: BTreeMap<(SocketAddr, Vec<u8>), (ir::Certificate, BTreeSet<String>)> =
        BTreeMap::new();
    for FingerprintedCert { fingerprint, cert } in certs {
        match merged.entry((cert.listener, fingerprint)) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let (served, names) = entry.get_mut();
                names.extend(cert.names.iter().cloned());
                if served_first(&cert, served) {
                    *served = cert;
                }
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                let names = cert.names.iter().cloned().collect();
                entry.insert((cert, names));
            }
        }
    }
    merged
        .into_values()
        .filter(|(_, names)| !names.is_empty())
        .map(|(mut cert, names)| {
            cert.names = names.into_iter().collect();
            cert
        })
        .collect()
}

/// Whether `candidate`'s material should be served rather than `current`'s
/// when both carry the same leaf: more chain links first, then the smallest
/// chain, key and PEM, a total order independent of input order.
fn served_first(candidate: &ir::Certificate, current: &ir::Certificate) -> bool {
    let rank = |c: &ir::Certificate| {
        (
            std::cmp::Reverse(c.chain.len()),
            c.chain.clone(),
            c.key.clone(),
            c.certificate.clone(),
        )
    };
    rank(candidate) < rank(current)
}

/// Compile all our-class Ingresses (+ resolved deps) into the IR.
pub fn build(cfg: &BuildConfig, inputs: &Inputs) -> BuildOutput {
    let index = Index::build(inputs, &cfg.health_check);

    let mut clusters: BTreeMap<String, ir::Cluster> = BTreeMap::new();
    let mut backends: BTreeMap<String, ir::Backend> = BTreeMap::new();
    let mut frontends: Vec<SourcedFrontend> = Vec::new();
    let mut certificates: Vec<ClaimedCert> = Vec::new();
    let mut results: Vec<IngressResult> = Vec::new();
    let mut referenced: BTreeSet<String> = BTreeSet::new();

    for ingress in &inputs.ingresses {
        if !is_ours(ingress, cfg) {
            continue;
        }
        let (namespace, name) = meta_nn(&ingress.metadata.namespace, &ingress.metadata.name);
        let uid = obj_uid(&ingress.metadata);
        let mut problems: Vec<Problem> = Vec::new();
        let Some(spec) = &ingress.spec else {
            results.push(IngressResult {
                namespace,
                name,
                uid,
                problems,
            });
            continue;
        };

        let source = FrontendSource::Ingress {
            namespace: namespace.clone(),
            name: name.clone(),
        };

        // spec.defaultBackend has no verified Sōzu equivalent; an Ingress
        // relying on it would otherwise build to nothing while reading
        // accepted-with-no-problems (its requests just 404). Report it and
        // translate the rules as usual.
        if spec.default_backend.is_some() {
            problems.push(Problem::DefaultBackendUnsupported);
        }

        // Automatic HTTP→HTTPS redirect: on by default, opt out per Ingress.
        let ssl_redirect = ingress
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(SSL_REDIRECT_ANNOTATION))
            .map(|v| !v.trim().eq_ignore_ascii_case("false"))
            .unwrap_or(true);

        // ---- TLS: load certs; only hosts with a *successfully loaded* cert
        // become HTTPS-enabled (a frontend without a cert can't handshake) ----
        let mut tls_ready_hosts: BTreeSet<String> = BTreeSet::new();
        for tls in spec.tls.iter().flatten() {
            let hosts: Vec<String> = tls.hosts.clone().unwrap_or_default();
            let Some(secret_name) = &tls.secret_name else {
                problems.push(Problem::TlsEntryWithoutSecret);
                continue;
            };
            // No hosts means nothing to derive SNI names from and no rule
            // host would turn HTTPS: the cert would load with an empty name
            // set while the Ingress keeps serving plain HTTP — half-applied
            // material. Report and skip the entry entirely.
            if hosts.is_empty() {
                problems.push(Problem::TlsEntryWithoutHosts {
                    secret: secret_name.clone(),
                });
                continue;
            }
            match index.secrets.get(&(namespace.clone(), secret_name.clone())) {
                None => problems.push(Problem::SecretNotFound {
                    secret: secret_name.clone(),
                }),
                Some(secret) => match extract_cert(
                    secret,
                    cfg.bind_for(ExposedProtocol::Https)
                        .expect("HTTPS is always exposed"),
                    hosts.clone(),
                ) {
                    Ok(cert) => {
                        // A host whose name goes to an older claimant's
                        // certificate stays HTTPS: the name is served, by
                        // that certificate.
                        for h in &hosts {
                            tls_ready_hosts.insert(h.clone());
                        }
                        certificates.push(ClaimedCert {
                            cert,
                            secret: secret_name.clone(),
                            source: CertificateSource::Ingress {
                                namespace: namespace.clone(),
                                name: name.clone(),
                            },
                        });
                    }
                    Err(reason) => problems.push(Problem::InvalidCertificate {
                        secret: secret_name.clone(),
                        reason,
                    }),
                },
            }
        }

        // ---- Rules: clusters + backends + frontends ----
        for rule in spec.rules.iter().flatten() {
            let Some(http) = &rule.http else { continue };
            // A hostless rule becomes a catch-all (`*`) frontend, which Sōzu
            // routes as DomainRule::Any. `tls_covers` returns false for `*`, so
            // it stays plain-HTTP (no `*`-named cert frontend).
            let host = rule.host.clone().unwrap_or_else(|| "*".to_string());
            // A host Sōzu cannot parse takes out the whole rule before any
            // backend is resolved: every one of its paths would carry it.
            if let Err(problem) = admit_hostname(&host) {
                if !problems.contains(&problem) {
                    problems.push(problem);
                }
                continue;
            }
            for path in &http.paths {
                // Settled before the backend is touched: a path Sōzu would
                // refuse programs nothing, so its Service is neither resolved
                // nor recorded as referenced. The sibling paths still are.
                let pm = match path_match(&path.path_type, path.path.as_deref()) {
                    Ok(pm) => pm,
                    Err(problem) => {
                        problems.push(problem);
                        continue;
                    }
                };
                let Some(svc_backend) = &path.backend.service else {
                    problems.push(Problem::NonServiceBackend);
                    continue;
                };
                let port_ref = match &svc_backend.port {
                    Some(p) if p.number.is_some() => {
                        PortRef::HttpNumber(p.number.unwrap_or_default())
                    }
                    Some(p) if p.name.is_some() => {
                        PortRef::Name(p.name.clone().unwrap_or_default())
                    }
                    _ => {
                        problems.push(Problem::ServicePortNotFound {
                            service: svc_backend.name.clone(),
                            port: "<unspecified>".to_string(),
                        });
                        continue;
                    }
                };

                // GEP-1911 is Gateway API; an Ingress has no condition to put
                // it in. A port Sōzu cannot speak to is warned about and
                // still programmed as before (cleartext HTTP/1.1), as on an
                // HTTPRoute: taking the path out would hand its requests to
                // another path, or a 404.
                let protocol =
                    HttpBackendProtocol::of(&index, &namespace, &svc_backend.name, &port_ref);
                if let Some(problem) = protocol.problem(&svc_backend.name) {
                    if !problems.contains(&problem) {
                        problems.push(problem);
                    }
                }

                match add_service_route(
                    &index,
                    &mut clusters,
                    &mut backends,
                    &mut referenced,
                    &namespace,
                    &svc_backend.name,
                    &port_ref,
                    &mut problems,
                ) {
                    Err(problem) => problems.push(problem),
                    Ok((cluster_id, has_endpoints)) => {
                        protocol.configure(&mut clusters, &cluster_id);
                        if !has_endpoints {
                            problems.push(Problem::NoReadyEndpoints {
                                service: svc_backend.name.clone(),
                            });
                        }

                        let host_has_tls = tls_covers(&tls_ready_hosts, &host);
                        // The plain-HTTP frontend redirects to HTTPS when the host
                        // has a cert and the redirect isn't opted out; otherwise it
                        // proxies. The redirect wins over the cluster, so keeping
                        // `cluster_id` set is harmless.
                        let http_filters = if host_has_tls && ssl_redirect {
                            ir::FrontendFilters {
                                redirect: Some(ir::Redirect {
                                    scheme: Some(ir::Scheme::Https),
                                    status: ir::RedirectStatus::MovedPermanently,
                                    // Scheme only: the host and path are the
                                    // request's own, which is the point.
                                    hostname: None,
                                    path: None,
                                    port: None,
                                }),
                                ..Default::default()
                            }
                        } else {
                            ir::FrontendFilters::default()
                        };
                        frontends.push(SourcedFrontend {
                            frontend: ir::Frontend {
                                hostname: host.clone(),
                                path: pm.clone(),
                                method: None,
                                cluster_id: Some(cluster_id.clone()),
                                tls: false,
                                listener: cfg
                                    .bind_for(ExposedProtocol::Http)
                                    .expect("HTTP is always exposed"),
                                filters: http_filters,
                            },
                            source: source.clone(),
                        });
                        if host_has_tls {
                            frontends.push(SourcedFrontend {
                                frontend: ir::Frontend {
                                    hostname: host.clone(),
                                    path: pm,
                                    method: None,
                                    cluster_id: Some(cluster_id),
                                    tls: true,
                                    listener: cfg
                                        .bind_for(ExposedProtocol::Https)
                                        .expect("HTTPS is always exposed"),
                                    filters: ir::FrontendFilters::default(),
                                },
                                source: source.clone(),
                            });
                        }
                    }
                }
            }
        }

        results.push(IngressResult {
            namespace,
            name,
            uid,
            problems,
        });
    }

    // Gateway API (Phase 2): same accumulators, same IR.
    let mut gw = gateway::build_gateway(
        cfg,
        inputs,
        &index,
        &mut clusters,
        &mut backends,
        &mut referenced,
        &mut frontends,
        &mut certificates,
    );

    // Deterministic frontend ordering (the Translator re-canonicalises anyway)
    // + one frontend per Sōzu route key: a route-key collision with a
    // different effect is reported on the losing owner's result instead of
    // silently letting the winner steal the traffic.
    //
    // Every object that produced a frontend or a certificate gets a claim
    // (oldest `creationTimestamp`, then `namespace/name`), so arbitration
    // follows who asked first — Ingress, HTTPRoute and Gateway alike — not
    // the backend's spelling or the certificate's expiry.
    let mut owner_claims: BTreeMap<OwnerIdent, OwnerClaim> = BTreeMap::new();
    for ingress in &inputs.ingresses {
        if !is_ours(ingress, cfg) {
            continue;
        }
        let (ns, name) = meta_nn(&ingress.metadata.namespace, &ingress.metadata.name);
        let claim = owner_claim(&ns, &name, &ingress.metadata);
        owner_claims.insert((INGRESS_OWNER, ns, name), claim);
    }
    for route in &inputs.http_routes {
        let (ns, name) = meta_nn(&route.metadata.namespace, &route.metadata.name);
        let claim = owner_claim(&ns, &name, &route.metadata);
        owner_claims.insert((HTTP_ROUTE_OWNER, ns, name), claim);
    }
    for gateway in &inputs.gateways {
        let (ns, name) = meta_nn(&gateway.metadata.namespace, &gateway.metadata.name);
        let claim = owner_claim(&ns, &name, &gateway.metadata);
        owner_claims.insert((GATEWAY_OWNER, ns, name), claim);
    }
    let (frontends, folded, collisions) = resolve_frontend_collisions(frontends, &owner_claims);
    for (source, problem) in collisions {
        // Dedup: the HTTP and HTTPS frontends of one host+path lose as a pair
        // to the same winner — one problem carries the same information.
        match source {
            // No Ingress status machinery exists, so a losing Ingress keeps
            // problem-only reporting.
            FrontendSource::Ingress { namespace, name } => {
                if let Some(r) = results
                    .iter_mut()
                    .find(|r| r.namespace == namespace && r.name == name)
                {
                    if !r.problems.contains(&problem) {
                        r.problems.push(problem);
                    }
                }
            }
            FrontendSource::HttpRoute {
                namespace,
                name,
                gateway_namespace,
                gateway_name,
                section_name,
                port,
            } => {
                if let Some(p) = gw
                    .routes
                    .iter_mut()
                    .find(|r| r.namespace == namespace && r.name == name)
                    .and_then(|r| {
                        // The parentRef identity includes sectionName/port: a
                        // route may hold several parentRefs to one Gateway,
                        // and the loser must be the parent that produced the
                        // colliding frontend, not the first match.
                        r.parents.iter_mut().find(|p| {
                            p.gateway_namespace == gateway_namespace
                                && p.gateway_name == gateway_name
                                && p.section_name == section_name
                                && p.port == port
                        })
                    })
                {
                    // A losing route must not read fully healthy: downgrade
                    // Accepted with an implementation-specific reason so the
                    // condition (not just the log) carries the collision.
                    p.accepted = false;
                    p.accepted_reason = "RouteCollision";
                    if !p.problems.contains(&problem) {
                        p.problems.push(problem);
                    }
                }
            }
        }
    }

    // One claimant per SNI name and listener, settled before the merge so a
    // name is attributed to the object that asked for it, never to whichever
    // object shares its certificate.
    let (certificates, lost_names) = arbitrate_certificate_names(certificates, &owner_claims);
    // Grouped per object, then attached in one pass over the results: a
    // search of every result per report would grow with their product. The
    // reports are distinct already (one per certificate an object asked for).
    let mut ingress_lost: BTreeMap<(String, String), Vec<Problem>> = BTreeMap::new();
    let mut gateway_lost: BTreeMap<(String, String), Vec<Problem>> = BTreeMap::new();
    for (source, problem) in lost_names {
        match source {
            CertificateSource::Ingress { namespace, name } => {
                ingress_lost
                    .entry((namespace, name))
                    .or_default()
                    .push(problem);
            }
            CertificateSource::Gateway {
                namespace, name, ..
            } => gateway_lost
                .entry((namespace, name))
                .or_default()
                .push(problem),
        }
    }
    // An Ingress has no status conditions: the Warning Event is its only
    // report.
    for r in results.iter_mut() {
        if ingress_lost.is_empty() {
            break;
        }
        if let Some(lost) = ingress_lost.remove(&(r.namespace.clone(), r.name.clone())) {
            r.problems.extend(lost);
        }
    }
    // Listener-scoped through `Problem::listener`, so the listener's own
    // status cites it.
    for g in gw.gateways.iter_mut() {
        if gateway_lost.is_empty() {
            break;
        }
        if let Some(lost) = gateway_lost.remove(&(g.namespace.clone(), g.name.clone())) {
            g.problems.extend(lost);
        }
    }
    // Only now is it known which clusters carry traffic, and of which kind.
    settle_health_checks(
        &mut clusters,
        &frontends,
        &folded,
        &gw.l4_carriers,
        &mut results,
        &mut gw.routes,
    );
    let frontends: Vec<ir::Frontend> = frontends.into_iter().map(|sf| sf.frontend).collect();

    // Merge certs that share Sōzu's (listener, fingerprint) identity, unioning
    // their names, then order deterministically. This subsumes exact-duplicate
    // dedup and prevents a perpetual ReplaceCertificate diff.
    let mut certificates = merge_certificates(certificates);
    certificates.sort_by(|a, b| {
        (&a.listener, &a.names, &a.certificate).cmp(&(&b.listener, &b.names, &b.certificate))
    });

    BuildOutput {
        ir: ir::Ir {
            clusters: clusters.into_values().collect(),
            backends: backends.into_values().collect(),
            frontends,
            certificates,
            // Layer 4 has one source now: TCPRoute/UDPRoute, with their port
            // conflicts already settled per route.
            l4_frontends: gw.l4_frontends,
        },
        results,
        gateway_classes: gw.classes,
        gateways: gw.gateways,
        routes: gw.routes,
        referenced_services: referenced,
    }
}

#[cfg(test)]
mod sticky_id_tests {
    use super::{sticky_ids, unique_truncated_hmacs};
    use std::collections::BTreeSet;

    fn backend_ids(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("demo.web.80#10.244.{}.{}:8080", i / 256, i % 256))
            .collect()
    }

    #[test]
    fn colliding_truncations_are_redrawn_until_unique() {
        // One byte leaves 256 values, so 200 backends collide many times over:
        // every clash must be re-derived, never handed out twice.
        let ids = unique_truncated_hmacs("uid", &backend_ids(200), 1);
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), 200);
        assert!(ids.iter().all(|id| id.len() == 2));
        // ...and deterministically, so every replica redraws alike.
        assert_eq!(ids, unique_truncated_hmacs("uid", &backend_ids(200), 1));
    }

    /// `10.244.<a>.<b>` in the backend-id format, so a test can order by
    /// address exactly as `add_service_route` sorts its input.
    fn backend_id(a: u8, b: u8) -> String {
        format!("demo.web.80#10.244.{a}.{b}:8080")
    }

    /// The id a backend draws when nothing else in the cluster holds it.
    fn first_draw(backend_id: &str, bytes: usize) -> String {
        unique_truncated_hmacs("uid", &[backend_id.to_owned()], bytes).remove(0)
    }

    /// A backend and one sorting before it whose first draws clash at one
    /// byte. The HMAC is fixed, so the search always finds the same pair.
    fn colliding_pair() -> (String, String) {
        let later = backend_id(200, 0);
        let taken = first_draw(&later, 1);
        let earlier = (0..200u8)
            .flat_map(|a| (0..=255u8).map(move |b| backend_id(a, b)))
            .find(|id| first_draw(id, 1) == taken)
            .expect("256 values cannot dodge 51200 draws");
        (earlier, later)
    }

    #[test]
    fn a_colliding_newcomer_that_sorts_first_takes_the_existing_id() {
        // Characterises the uniqueness rule under membership change: the
        // input order decides who keeps a clashing value, not seniority.
        let (earlier, later) = colliding_pair();
        let alone = unique_truncated_hmacs("uid", std::slice::from_ref(&later), 1);
        let both = unique_truncated_hmacs("uid", &[earlier.clone(), later.clone()], 1);
        assert_eq!(both[0], alone[0], "the newcomer draws the clashing value");
        assert_ne!(both[1], alone[0], "the existing backend is redrawn");
        // Removing the winner hands the value back to the backend it displaced.
        assert_eq!(unique_truncated_hmacs("uid", &[later], 1), alone);
    }

    #[test]
    fn a_colliding_newcomer_that_sorts_last_is_the_one_redrawn() {
        let (earlier, later) = colliding_pair();
        let alone = unique_truncated_hmacs("uid", std::slice::from_ref(&earlier), 1);
        let both = unique_truncated_hmacs("uid", &[earlier.clone(), later.clone()], 1);
        assert_eq!(both[0], alone[0], "the existing backend keeps its id");
        assert_ne!(both[1], alone[0]);
        assert_eq!(unique_truncated_hmacs("uid", &[earlier], 1), alone);
    }

    #[test]
    fn without_a_clash_membership_and_order_move_no_id() {
        let ids = backend_ids(20);
        let by_backend = |input: &[String]| {
            input
                .iter()
                .cloned()
                .zip(sticky_ids("uid", input))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let base = by_backend(&ids);
        // An earlier address joins, the earliest leaves, the input is permuted.
        let mut joined = vec!["demo.web.80#10.243.0.1:8080".to_owned()];
        joined.extend(ids.iter().cloned());
        let left = ids[1..].to_vec();
        let reversed: Vec<String> = ids.iter().rev().cloned().collect();
        for variant in [&joined, &left, &reversed] {
            for (backend, id) in by_backend(variant) {
                if let Some(before) = base.get(&backend) {
                    assert_eq!(&id, before, "{backend} was re-pinned");
                }
            }
        }
    }

    #[test]
    fn production_ids_are_sixteen_hex_characters() {
        let ids = sticky_ids("uid", &backend_ids(3));
        assert!(ids
            .iter()
            .all(|id| id.len() == 16 && id.bytes().all(|c| c.is_ascii_hexdigit())));
    }
}

#[cfg(test)]
mod certificate_name_tests {
    use super::validate_inferred_certificate_names;

    #[test]
    fn inferred_names_reject_trie_syntax_and_unusable_identities() {
        for name in [
            "",
            ".example.org",
            "example..org",
            "example.org/",
            "/example.*/",
            "a/b.example.org",
            "*.",
            "a.*.org",
            "éxample.org",
        ] {
            assert!(
                validate_inferred_certificate_names(&["valid.example.org".into(), name.into()])
                    .is_err(),
                "invalid inferred name {name:?} must not poison a shared certificate"
            );
        }
        assert!(validate_inferred_certificate_names(&[]).is_err());
        assert!(validate_inferred_certificate_names(&["a".repeat(4097)]).is_err());
    }

    #[test]
    fn inferred_names_keep_certificate_wildcards_and_the_resolver_length_bound() {
        let names = [
            "*",
            "*.org",
            "*.wildcard.org",
            "cn-only.example.org",
            "xn--xample-9ua.org",
        ]
        .map(String::from);
        assert!(validate_inferred_certificate_names(&names).is_ok());
        assert!(validate_inferred_certificate_names(&["a".repeat(4096)]).is_ok());
    }

    #[test]
    fn inferred_names_are_lowercased_the_way_a_handshake_presents_them() {
        // rustls lowercases the SNI before Sōzu's resolver sees it; a name
        // stored as the certificate spells it would never be selected.
        let names = ["MiXeD.Example.COM", "*.Wild.ORG", "*", "plain.example.org"].map(String::from);
        assert_eq!(
            validate_inferred_certificate_names(&names).unwrap(),
            ["mixed.example.com", "*.wild.org", "*", "plain.example.org"].map(String::from)
        );
    }
}

#[cfg(test)]
mod hostname_tests {
    use super::hostname_refusal;

    /// Each shape was added to (or refused by) a live Sōzu 2.2.1 with
    /// `AddHttpFrontend`. Most of them never reach the builder, because the
    /// apiserver refuses them first, but none may be refused here while Sōzu
    /// adds it — with one deliberate exception: Sōzu reads a `/` as a regex
    /// domain (`/a+/.example.com` is added), which a host field never means.
    #[test]
    fn hostnames_are_refused_exactly_where_sozu_refuses_them() {
        assert!(hostname_refusal("/a+/.example.com").is_some());
        for refused in [
            "xn--a.example.com",
            "xn--.example.com",
            "*.xn--a.example.com",
            "0.xn--4gbrim.example.com",
            "*.xn--4gbrim.example.com",
            "",
            ".",
            ".example.com",
            "..a.com",
            "a.com/",
            "x.*",
        ] {
            assert!(hostname_refusal(refused).is_some(), "{refused:?}");
        }
        for accepted in [
            "*",
            "*.example.com",
            "good.example.com",
            "xn--bcher-kva.example.com",
            "a.xn--4gbrim.example.com",
            "a..b.com",
            "a...b.com",
            "example.com.",
            "com..",
            "*.",
            "**.foo",
        ] {
            assert_eq!(hostname_refusal(accepted), None, "{accepted:?}");
        }
        assert!(hostname_refusal(&"a".repeat(4097)).is_some());
        assert_eq!(hostname_refusal(&"a".repeat(4096)), None);
    }
}

#[cfg(test)]
mod http_route_collisions {
    use super::*;

    fn route(name: &str, cluster: &str) -> SourcedFrontend {
        SourcedFrontend {
            frontend: ir::Frontend {
                hostname: "app.example.com".into(),
                path: ir::PathMatch::Prefix("/".into()),
                method: None,
                cluster_id: Some(cluster.into()),
                tls: false,
                listener: "0.0.0.0:8080".parse().unwrap(),
                filters: Default::default(),
            },
            source: FrontendSource::HttpRoute {
                namespace: "demo".into(),
                name: name.into(),
                gateway_namespace: "demo".into(),
                gateway_name: "gw".into(),
                section_name: Some("http".into()),
                port: Some(80),
            },
        }
    }

    /// A claims map with explicit ages, so a test states who is older rather
    /// than relying on emission order. `(kind, namespace, name, timestamp)`;
    /// kind 0 = Ingress, 1 = HTTPRoute.
    fn claims(entries: &[(u8, &str, &str, &str)]) -> BTreeMap<OwnerIdent, OwnerClaim> {
        entries
            .iter()
            .map(|(kind, ns, name, ts)| {
                (
                    (*kind, ns.to_string(), name.to_string()),
                    (false, Some(ts.to_string()), format!("{ns}/{name}")),
                )
            })
            .collect()
    }

    #[test]
    fn a_rejection_backend_has_no_special_collision_priority() {
        // Gateway emission order already encodes route/rule precedence. A
        // future invalid-backend frontend must obey exactly the same order.
        for (older, newer) in [
            ("zzzz", "sozu-gateway/http-error"),
            ("sozu-gateway/http-error", "aaaa"),
        ] {
            let first = route("older", older);
            let second = route("newer", newer);
            let loser = second.source.clone();
            // "older" is the older object; the backend string never decides.
            let claims = claims(&[
                (1, "demo", "older", "2020-01-01T00:00:00Z"),
                (1, "demo", "newer", "2021-01-01T00:00:00Z"),
            ]);
            let (kept, _, collisions) = resolve_frontend_collisions(vec![first, second], &claims);
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].frontend.cluster_id.as_deref(), Some(older));
            assert_eq!(collisions.len(), 1);
            assert_eq!(collisions[0].0, loser);
            assert!(
                matches!(&collisions[0].1, Problem::RouteCollision { winner, .. } if winner == older)
            );
        }
    }

    #[test]
    fn ingress_competes_with_the_selected_http_route_and_events_name_the_final_winner() {
        let mut ingress = route("unused", "middle");
        ingress.source = FrontendSource::Ingress {
            namespace: "demo".into(),
            name: "ingress".into(),
        };
        // The Ingress is the oldest claimant, so it wins over both HTTPRoutes
        // regardless of their (smaller) backend cluster ids.
        let claims = claims(&[
            (0, "demo", "ingress", "2019-01-01T00:00:00Z"),
            (1, "demo", "older", "2020-01-01T00:00:00Z"),
            (1, "demo", "newer", "2021-01-01T00:00:00Z"),
        ]);
        let (kept, _, collisions) = resolve_frontend_collisions(
            vec![ingress, route("older", "zzzz"), route("newer", "aaaa")],
            &claims,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].frontend.cluster_id.as_deref(), Some("middle"));
        assert_eq!(collisions.len(), 2);
        assert!(collisions.iter().all(|(source, problem)| {
            matches!(source, FrontendSource::HttpRoute { .. })
                && matches!(problem, Problem::RouteCollision { winner, .. } if winner == "middle")
        }));
    }

    #[test]
    fn independent_protocol_address_method_and_path_matches_remain_programmed() {
        let mut frontends = vec![route("root", "root")];
        let mut tls = route("tls", "tls");
        tls.frontend.tls = true;
        frontends.push(tls);
        let mut address = route("address", "address");
        address.frontend.listener = "127.0.0.1:8080".parse().unwrap();
        frontends.push(address);
        for method in ["GET", "POST"] {
            let mut frontend = route(method, method);
            frontend.frontend.method = Some(method.into());
            frontends.push(frontend);
        }
        let mut exact = route("exact", "exact");
        exact.frontend.path = ir::PathMatch::Exact("/".into());
        frontends.push(exact);
        let (kept, _, collisions) = resolve_frontend_collisions(frontends, &BTreeMap::new());
        assert_eq!(kept.len(), 6);
        assert!(collisions.is_empty());
    }
}
