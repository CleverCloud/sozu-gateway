//! Sōzu gateway controller binary.
//!
//! A singleton controller: it maintains reflector caches for Ingress,
//! IngressClass, Namespace, Service, EndpointSlice and Secret; any change (or a
//! periodic resync) triggers one **global** reconcile that rebuilds
//! the whole desired state from the caches, diffs it against the last-applied
//! shadow `ConfigState`, and pushes only the minimal mutations to Sōzu.
//! Referenced EndpointSlice changes bypass the debounce used by other watches.
//!
//! The pure crates do the work: `builder` (objects → IR), `translator`
//! (IR → diff → commands), `sozu-agent` (socket I/O). This file is just the
//! kube-rs wiring and the reconcile loop.

use std::collections::BTreeSet;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::{Ingress, IngressClass};
use kube::api::ListParams;
use kube::runtime::reflector::{
    store::{Writer, WriterDropped},
    Lookup, Store,
};
use kube::runtime::{reflector, watcher, WatchStreamExt};
use kube::{Api, Client, Resource};
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use sozu_gw_agent::SozuAgentHandle;
use sozu_gw_builder::{build, BuildConfig, ExposedPort, Inputs};
use sozu_gw_gateway_api::{Gateway, GatewayClass, HttpRoute, ReferenceGrant, TcpRoute, UdpRoute};
use sozu_gw_translator as tr;

mod changes;
mod events;
mod health;
mod metrics;
mod provision;
mod scope;
mod shadow;
mod status;

const DEFAULT_CLASS_ANNOTATION: &str = "ingressclass.kubernetes.io/is-default-class";

/// Default `--watch-timeout-secs`. Measured, not chosen: see the flag's doc.
const DEFAULT_WATCH_TIMEOUT_SECS: u32 = 60;

/// kube-core's own ceiling on a watch `timeoutSeconds` (`WatchParams::timeout
/// must be < 295s`, from the apiserver's watch limits). It is enforced on every
/// watch *start*, after the initial LIST has already filled the cache: the
/// refused watch surfaces as a retried `WatchStartFailed`, which the watch loop
/// only warns about, so every cache freezes on its LIST snapshot with `/readyz`
/// green. That is the blindness the flag exists to bound, so it is refused
/// here instead.
const MAX_WATCH_TIMEOUT_SECS: u32 = 295;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "sozu-gw-controller",
    about = "Sōzu-based Ingress + Gateway API controller"
)]
struct Args {
    #[command(flatten)]
    gateway_scope: scope::GatewayScope,
    /// Run only infrastructure provisioning, using a Helm-rendered JSON template.
    #[arg(long, env = "SOZU_GW_PROVISION_TEMPLATE", conflicts_with_all = ["gateway_scope", "gateway_uid", "ingress_only", "exclude_gateway"])]
    provision_template: Option<String>,
    /// Pin an automatically provisioned worker to one Gateway incarnation.
    #[arg(long, env = "SOZU_GW_GATEWAY_UID", requires = "gateway_scope")]
    gateway_uid: Option<String>,
    /// IngressClass name we own.
    #[arg(long, env = "SOZU_GW_CLASS", default_value = "sozu")]
    class_name: String,
    /// GatewayClass controllerName we own (Gateway API).
    #[arg(
        long,
        env = "SOZU_GW_CONTROLLER",
        default_value = "sozu.io/gateway-controller"
    )]
    controller_name: String,
    /// Path to the Sōzu command socket.
    #[arg(long, env = "SOZU_GW_SOCKET", default_value = "/run/sozu/sozu.sock")]
    socket: String,
    /// Ports this gateway exposes, as JSON — the single source of what a
    /// Gateway listener may declare and where Sōzu listens for it. Rendered by
    /// the chart from its `exposure` values, and the reason this is not four
    /// scalars any more: a gateway serving arbitrary layer-4 ports cannot
    /// encode them in a fixed pair.
    ///
    /// `[{"name":"http","port":80,"bind":"0.0.0.0:8080","protocol":"HTTP"}]`
    ///
    /// `port` is advertised (what a listener declares, what clients dial),
    /// `bind` is where Sōzu listens in the Pod, and `owner` — optional, layer-4
    /// only — names the namespace allowed to claim the port.
    /// Defaults to the two ports a gateway has always served, so the binary
    /// stays runnable on its own; the chart always sets it.
    #[arg(
        long,
        env = "SOZU_GW_EXPOSURE",
        default_value = r#"[{"name":"http","port":80,"bind":8080,"protocol":"HTTP","transport":"TCP"},{"name":"https","port":443,"bind":8443,"protocol":"HTTPS","transport":"TCP"}]"#
    )]
    exposure: String,
    /// Coalesce ordinary watch events before reconciling. Referenced
    /// EndpointSlice changes bypass this delay.
    #[arg(long, env = "SOZU_GW_DEBOUNCE_MS", default_value = "500")]
    debounce_ms: u64,
    /// Periodic full resync interval in seconds (self-heals any drift). `0`
    /// disables the periodic resync; Sōzu-restart detection is then left to
    /// the liveness tick (`sozu_probe_secs`) and to command-socket reconnects.
    #[arg(long, env = "SOZU_GW_RESYNC_SECS", default_value = "60")]
    resync_secs: u64,
    /// Poll Sōzu for a restart every N seconds: one `Status` round-trip on the
    /// command socket, the same generation check the resync tick runs, but
    /// with nothing rebuilt or reconciled unless it finds a change. Bounds how
    /// long a Sōzu that restarted alone — back with only its static listeners,
    /// its own TCP probe green within seconds — stays in the Service answering
    /// 404 before `/readyz` drops and the full state is re-applied. An *idle*
    /// socket never reconnects, so without this tick a quiet cluster notices
    /// only on the next watch event or resync tick. `0` disables it.
    #[arg(long, env = "SOZU_GW_SOZU_PROBE_SECS", default_value = "2")]
    sozu_probe_secs: u64,
    /// Publish this Service's LoadBalancer address into managed Ingresses'
    /// `.status` (format `namespace/name`). Unset = don't write Ingress status.
    /// Requires the `ingresses/status` RBAC (Helm `rbac.allowStatusWrites`).
    #[arg(long, env = "SOZU_GW_PUBLISH_SERVICE")]
    publish_service: Option<String>,
    /// Bind address for the health endpoints (`/healthz`, `/readyz`).
    #[arg(long, env = "SOZU_GW_HEALTH_LISTEN", default_value = "0.0.0.0:8081")]
    health_listen: SocketAddr,
    /// Bind address for the Prometheus `/metrics` endpoint (pulls Sōzu's
    /// metrics over the command socket on each scrape). Unset disables it.
    #[arg(long, env = "SOZU_GW_METRICS_LISTEN")]
    metrics_listen: Option<SocketAddr>,
    /// Also export per-cluster and per-backend Sōzu metrics. Off by default:
    /// each Sōzu worker then answers a scrape with every cluster's and
    /// backend's metrics in one message, and a message larger than Sōzu's
    /// `max_command_buffer_size` (the chart sets 1 638 400 bytes) wedges that
    /// worker for good on Sōzu 2.2.1 — no traffic, no commands, green probes.
    /// It grows with the clusters that have seen traffic: a local run with
    /// the chart's buffers wedged between 2 000 and 2 500, an order of
    /// magnitude rather than a limit. See docs/UPGRADING.md.
    #[arg(long, env = "SOZU_GW_METRICS_PER_CLUSTER")]
    metrics_per_cluster: bool,
    /// File on the shared volume where the last-applied state is persisted, so a
    /// controller-only restart resumes from it (and prunes orphaned Sōzu state)
    /// instead of re-applying everything. Empty disables persistence.
    #[arg(
        long,
        env = "SOZU_GW_SHADOW_FILE",
        default_value = "/run/sozu/shadow.json"
    )]
    shadow_file: String,
    /// Write Gateway API status (GatewayClass/Gateway/HTTPRoute conditions).
    /// On by default — conditions are the API's UX — but can be disabled for
    /// least-privilege deployments without the `*/status` RBAC grants (Helm
    /// `rbac.allowGatewayStatusWrites=false`), where every write would 403.
    #[arg(
        long,
        env = "SOZU_GW_GATEWAY_STATUS_WRITES",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    gateway_status_writes: bool,
    /// Write Ingress status when publishing a Service address. Scoped Gateway
    /// instances never write it; the default instance can disable it for RBAC.
    #[arg(long, env = "SOZU_GW_INGRESS_STATUS_WRITES", default_value_t = true, action = clap::ArgAction::Set)]
    ingress_status_writes: bool,
    /// Server-side `timeoutSeconds` on every watch, which also sets kube-rs's
    /// **client-side** idle timeout (that timeout is derived from this value
    /// plus a margin). `0` opts out and keeps kube-rs's default of 290 s;
    /// kube-rs refuses 295 and above, so that is refused at startup too.
    ///
    /// This is the bound on how long a watch can be silently dead before the
    /// client gives up and re-lists. A control plane replaced underneath us
    /// leaves the connection open and silent rather than closing it: nothing
    /// errors, no event arrives, and the reflector simply stops advancing while
    /// every reconcile still "succeeds" against a frozen cache.
    ///
    /// The default is the binary's, not the chart's: measured across three
    /// managed-cluster upgrades, the kube-rs bound cost 14.7-19.7% of fresh
    /// requests during node replacement and 60 cost 0.000%, and an install
    /// whose values predate the chart key would otherwise silently run
    /// unbounded.
    ///
    /// The bound is nominal and measures worse than it reads. Cutting
    /// controllers off from the apiserver and timing each to its first logged
    /// watch error gave 336-364 s at the kube-rs default and 117-132 s at 60.
    /// Idle expiry logs only at DEBUG, so what those figures time is the failed
    /// reconnect that follows it, and why both exceed their nominal bound is
    /// not established here — only that they do.
    #[arg(
        long,
        env = "SOZU_GW_WATCH_TIMEOUT_SECS",
        default_value_t = DEFAULT_WATCH_TIMEOUT_SECS
    )]
    watch_timeout_secs: u32,
    /// Read timeout applied to the kube client's connections. `0` keeps
    /// kube-rs's default, which is **no timeout at all**.
    ///
    /// kube's connector enables HTTP/1 only, so each watch holds its own
    /// connection and this bounds each one independently: how long it may
    /// deliver nothing before it is torn down and that watch re-lists.
    ///
    /// Prefer `watch_timeout_secs` to this. Both bound the blindness, but a
    /// watch closed by its own timeout resumes from the stored
    /// `resourceVersion`, while this tears the connection — which costs a watch
    /// error and a backoff every time it fires, on every quiet watch. Measured
    /// across two managed-cluster upgrades: the arm running this at 30 s logged
    /// tens of watch errors per window and did not come out cleaner than the
    /// arm running `watch_timeout_secs`.
    #[arg(long, env = "SOZU_GW_KUBE_READ_TIMEOUT_SECS", default_value = "0")]
    kube_read_timeout_secs: u64,
}

/// Reflector read handles for every watched resource type.
struct Stores {
    ingresses: Store<Ingress>,
    namespaces: Store<Namespace>,
    ingress_classes: Store<IngressClass>,
    services: Store<Service>,
    endpointslices: Store<EndpointSlice>,
    secrets: Store<Secret>,
    // Gateway API (Phase 2).
    gateway_classes: Store<GatewayClass>,
    gateways: Store<Gateway>,
    http_routes: Store<HttpRoute>,
    reference_grants: Store<ReferenceGrant>,
    tcp_routes: Store<TcpRoute>,
    udp_routes: Store<UdpRoute>,
}

/// Spawn a watcher+reflector that keeps `writer`'s store fresh and pings `tx`
/// on every event.
fn spawn_watch<K>(
    api: Api<K>,
    cfg: watcher::Config,
    writer: Writer<K>,
    tx: mpsc::Sender<()>,
    kind: &'static str,
) where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
    K::DynamicType: Default + Eq + Hash + Clone + std::fmt::Debug + Unpin,
{
    spawn_watch_notified(api, cfg, writer, kind, move |event: &watcher::Event<K>| {
        if matches!(
            event,
            watcher::Event::Apply(_) | watcher::Event::Delete(_) | watcher::Event::InitDone
        ) {
            let _ = tx.try_send(());
        }
    })
}

/// Keep the reflector cache fresh for every event, then notify the reconcile
/// loop. The EndpointSlice callback selects the wakeup priority and ignores
/// unrelated workloads; it never filters the objects entering the cache.
fn spawn_watch_notified<K, F>(
    api: Api<K>,
    cfg: watcher::Config,
    writer: Writer<K>,
    kind: &'static str,
    notify: F,
) where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
    K::DynamicType: Default + Eq + Hash + Clone + std::fmt::Debug + Unpin,
    F: Fn(&watcher::Event<K>) + Send + 'static,
{
    // Keep InitDone: a relist publishes its complete cache only at that
    // event, and an empty relist still has to withdraw the previous objects.
    let stream = watcher(api, cfg).default_backoff().reflect(writer);
    tokio::spawn(async move {
        futures::pin_mut!(stream);
        loop {
            match stream.next().await {
                Some(Ok(obj)) => notify(&obj),
                Some(Err(e)) => warn!(watch = kind, error = %e, "watch error (will retry)"),
                None => {
                    // The watcher's own backoff means a healthy stream never
                    // ends; if it does, fail fast so Kubernetes restarts us
                    // rather than silently going blind to this resource.
                    error!(
                        watch = kind,
                        "watch stream ended unexpectedly; exiting for restart"
                    );
                    std::process::exit(1);
                }
            }
        }
    });
}

/// Should an EndpointSlice event wake the reconcile loop? Only when its
/// Service (`kubernetes.io/service-name` label + namespace) is one the last
/// build referenced — resolved or not. Endpoint churn from unrelated
/// workloads is the dominant wakeup source on a busy cluster, and every
/// wakeup is a full rebuild.
///
/// An empty set passes everything: before the first build has populated it,
/// a missed wakeup is an outage risk while a spurious one only costs CPU
/// (and a cluster whose build genuinely references nothing rebuilds an empty
/// state, which is cheap). A slice without the service-name label never
/// pings — the builder cannot attribute it to any Service, so it can never
/// change the build output.
fn slice_pings(referenced: &BTreeSet<String>, slice: &EndpointSlice) -> bool {
    if referenced.is_empty() {
        return true;
    }
    sozu_gw_builder::slice_service_key(slice).is_some_and(|key| referenced.contains(&key))
}

fn notify_slice(
    referenced: &BTreeSet<String>,
    slice: &EndpointSlice,
    changes: &mpsc::Sender<()>,
    endpoints: &mpsc::Sender<()>,
) {
    if slice_pings(referenced, slice) {
        // An empty index is the conservative startup/no-routes fallback. It
        // must not make unrelated cluster churn trigger immediate rebuilds.
        let tx = if referenced.is_empty() {
            changes
        } else {
            endpoints
        };
        let _ = tx.try_send(());
    }
}

fn notify_slice_event(
    referenced: &BTreeSet<String>,
    event: &watcher::Event<EndpointSlice>,
    changes: &mpsc::Sender<()>,
    endpoints: &mpsc::Sender<()>,
) {
    match event {
        watcher::Event::Apply(slice) | watcher::Event::Delete(slice) => {
            notify_slice(referenced, slice, changes, endpoints);
        }
        watcher::Event::InitDone => {
            // The relist can remove previously referenced slices, including
            // every slice. Wake once after the new cache has been published.
            let tx = if referenced.is_empty() {
                changes
            } else {
                endpoints
            };
            let _ = tx.try_send(());
        }
        watcher::Event::Init | watcher::Event::InitApply(_) => {}
    }
}

/// Await a store's readiness only when its watcher was actually spawned.
///
/// Optional features (the Gateway API kinds) drop their `Writer` when
/// disabled, and `wait_until_ready` on such a store fails immediately; a store
/// that is never watched is trivially "ready" instead. For a watched store this
/// is the plain readiness wait, so the sync gate covers *every* cache the first
/// reconcile will read — skipping one would let a resumed shadow diff against a
/// half-built IR and tear down live routes.
async fn ready_when<K>(watched: bool, store: &Store<K>) -> Result<(), WriterDropped>
where
    K: Lookup + Clone + 'static,
    K::DynamicType: Eq + Hash + Clone,
{
    if watched {
        store.wait_until_ready().await
    } else {
        Ok(())
    }
}

/// The `/metrics` endpoint the flags ask for, if any. Per-cluster series stay
/// off unless `--metrics-per-cluster` is given: they can wedge Sōzu's workers.
fn metrics_endpoint(args: &Args) -> Option<metrics::Endpoint> {
    args.metrics_listen.map(|addr| metrics::Endpoint {
        addr,
        per_cluster: args.metrics_per_cluster,
    })
}

/// Refuse a `--watch-timeout-secs` kube-rs would refuse on every watch start,
/// so it fails the process at startup (a CrashLoopBackOff with this message)
/// rather than freezing every cache behind a green `/readyz`.
fn validate_watch_timeout(secs: u32) -> Result<()> {
    anyhow::ensure!(
        secs < MAX_WATCH_TIMEOUT_SECS,
        "--watch-timeout-secs {secs} is not below kube-rs's limit of {MAX_WATCH_TIMEOUT_SECS} \
         (`WatchParams::timeout must be < 295s`): every watch would be refused before it \
         starts and the caches would never advance past their initial LIST; use 0 to opt \
         out and keep kube-rs's default"
    );
    Ok(())
}

/// The config every watch starts from: `timeoutSeconds` bounded by
/// `--watch-timeout-secs`, or left to kube-rs's own default when that is the
/// explicit `0` opt-out. Both the worker and the provisioner read it this way.
fn watch_config(watch_timeout_secs: u32) -> watcher::Config {
    let cfg = watcher::Config::default();
    if watch_timeout_secs > 0 {
        cfg.timeout(watch_timeout_secs)
    } else {
        cfg
    }
}

/// Interpret a configured tick interval (resync, Sōzu liveness): `0` means
/// "disabled" (a zero `tokio::time::interval` would panic, and disabling the
/// tick is the only sensible reading of an explicit `SOZU_GW_RESYNC_SECS=0`
/// or `SOZU_GW_SOZU_PROBE_SECS=0`).
fn optional_period(secs: u64) -> Option<Duration> {
    (secs != 0).then(|| Duration::from_secs(secs))
}

/// Build a periodic tick. Unlike a raw `interval()`, whose first tick
/// completes immediately (which would re-run a redundant reconcile right
/// after the initial one), the first tick lands one full period after startup.
fn delayed_interval(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.reset();
    interval
}

/// Tick the optional resync interval; when resync is disabled, pend forever so
/// the `select!` arm simply never fires.
async fn maybe_tick(interval: Option<&mut tokio::time::Interval>) {
    match interval {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Is our IngressClass marked as the cluster default?
fn class_is_default(stores: &Stores, class_name: &str) -> bool {
    stores.ingress_classes.state().iter().any(|ic| {
        ic.metadata.name.as_deref() == Some(class_name)
            && ic
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(DEFAULT_CLASS_ANNOTATION))
                .map(|v| v == "true")
                .unwrap_or(false)
    })
}

/// Latch readiness on a successful reconcile (logging the transition). A later
/// transient failure never unsets it — that would pull a still-programmed Pod
/// out of the Service's endpoints. Only [`unmark_ready`] does, when Sōzu is
/// known to hold nothing.
fn mark_ready(ready: &AtomicBool) {
    if !ready.swap(true, Ordering::Relaxed) {
        info!("controller ready: Sōzu is programmed");
    }
}

/// Drop readiness (logging the transition): Sōzu restarted with empty state,
/// so until the full re-apply lands every request it serves is a 404 or a TLS
/// failure, and the Pod must leave the Service's endpoints. Its own TCP probe
/// cannot tell — it is green within seconds of the restart. [`mark_ready`]
/// restores it after the next successful reconcile.
fn unmark_ready(ready: &AtomicBool) {
    if ready.swap(false, Ordering::Relaxed) {
        warn!("controller not ready: Sōzu restarted with empty state; re-applying the full state");
    }
}

/// Readiness follows the generation check: only a `Reset` moves it — Sōzu came
/// back empty and the shadow was reset, so the Pod leaves the Service until the
/// re-apply lands. `Unchanged` keeps whatever holds, and `ProbeFailed` decides
/// nothing: Sōzu's own probe already de-pools a dead data plane, and dropping
/// readiness on a transient socket error would pull a still-programmed Pod.
fn readiness_after_check(ready: &AtomicBool, outcome: shadow::GenerationCheck) {
    if outcome == shadow::GenerationCheck::Reset {
        unmark_ready(ready);
    }
}

/// Why the run loop woke up: decides whether the pass reconciles at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wakeup {
    /// A watch event or a self-nudge: rebuild and reconcile.
    Change,
    /// The periodic resync: generation check, then rebuild and reconcile.
    Resync,
    /// The Sōzu liveness tick: generation check only, unless it reset.
    SozuProbe,
    /// The self-scheduled retry of a failed reconcile, with both ticks off:
    /// generation check (the failure may have been a restart), then reconcile.
    Retry,
}

/// Whether a pass proceeds to a reconcile after its generation check. A
/// liveness tick that found the generation unchanged has nothing *new* to
/// apply, and proceeding would turn a 2 s probe into a full rebuild every 2 s
/// — unless a previous reconcile failed and its work is still `pending`: then
/// Sōzu answering again, same generation, is exactly the moment to retry it.
/// Without that, a transient socket failure during an apply left the desired
/// state unapplied until an unrelated Kubernetes event or the resync tick —
/// never, with the resync disabled. A tick that could not reach Sōzu at all
/// does not retry (the apply would fail the same way; the next tick will).
/// Every other wakeup reconciles regardless of the check, as before; a reset
/// always does — that reconcile *is* the re-apply.
fn reconciles(wakeup: Wakeup, check: Option<shadow::GenerationCheck>, pending: bool) -> bool {
    wakeup != Wakeup::SozuProbe
        || check == Some(shadow::GenerationCheck::Reset)
        || (pending && check == Some(shadow::GenerationCheck::Unchanged))
}

/// Whether a failed generation probe needs a channel nudge to be retried: only
/// without the liveness tick, where the nudge is the sole retry path with
/// resync disabled and no watch traffic. With the tick, the retry is its job —
/// a nudge would make every failed probe a full rebuild — and it retries
/// quietly, at debug, after the check's first warning.
fn retry_by_nudge(outcome: shadow::GenerationCheck, has_probe_tick: bool) -> bool {
    outcome == shadow::GenerationCheck::ProbeFailed && !has_probe_tick
}

/// Whether a failed reconcile must schedule its own retry. The liveness tick
/// retries pending work on its next successful probe and the resync tick
/// rebuilds regardless, so with either one running the retry is theirs. With
/// both disabled nothing else ever runs it again on a quiet cluster: a
/// transient socket failure during one apply would leave the desired state
/// unapplied until an unrelated watch event — possibly never.
fn retry_reconcile_by_nudge(has_probe_tick: bool, has_resync: bool) -> bool {
    !has_probe_tick && !has_resync
}

/// How long a self-scheduled reconcile retry waits. A failed apply is retried
/// as a full rebuild, so this is not immediate: a data plane that just refused
/// a batch is not asked again the same instant. It is the same order as the
/// probe tick's own retry cadence, which is what it stands in for.
const RECONCILE_RETRY_DELAY: Duration = Duration::from_secs(5);

/// A pending self-scheduled retry, held as one `select!` arm rather than a
/// spawned nudge: scheduling again *replaces* it, so however many failures
/// land in one window there is ever one retry outstanding, and it goes away
/// with the loop.
type RetryTimer = std::pin::Pin<Box<tokio::time::Sleep>>;

fn scheduled_retry() -> RetryTimer {
    Box::pin(tokio::time::sleep(RECONCILE_RETRY_DELAY))
}

/// Await the optional retry timer; with none scheduled, pend forever so the
/// `select!` arm simply never fires.
async fn maybe_retry(timer: Option<&mut RetryTimer>) {
    match timer {
        Some(timer) => timer.await,
        None => std::future::pending::<()>().await,
    }
}

/// The Gateway API kinds gateway mode *requires*. All of them: watching a
/// missing kind would never sync its cache, and the sync gate would kill the
/// process — a partial install must run Ingress-only, not crash-loop.
///
/// GatewayClass belongs here and is not optional: without it `our_classes` is
/// empty and every Gateway is skipped, so "core installed, GatewayClass
/// missing" is not a degraded mode, it is no mode at all.
const REQUIRED_GATEWAY_API_KINDS: [&str; 4] =
    ["GatewayClass", "Gateway", "HTTPRoute", "ReferenceGrant"];

/// Route kinds that are genuinely optional: their absence costs layer-4
/// routing and nothing else. Probed independently of the required set, so a
/// cluster with "core yes, layer 4 no" does not read as "Gateway API not
/// installed".
const OPTIONAL_GATEWAY_API_KINDS: [&str; 2] = ["TCPRoute", "UDPRoute"];

/// Which Gateway API kinds this cluster serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct GatewayApiSupport {
    /// Every required kind is served: the Gateway API path can run at all.
    core: bool,
    tcp_routes: bool,
    udp_routes: bool,
}

impl GatewayApiSupport {
    /// Everything served — nothing left for a re-probe to discover.
    fn full() -> Self {
        Self {
            core: true,
            tcp_routes: true,
            udp_routes: true,
        }
    }
}

/// Are the Gateway API CRDs installed? Probed by a tiny list against **every**
/// kind the controller watches — a partial install (e.g. GatewayClass served
/// but ReferenceGrant absent) is a real cluster state and must read as
/// Ingress-only (with a warning naming the missing kinds), not as gateway
/// mode. Only a 404 from the apiserver (the CRD's group/kind is not served,
/// see [`gateway_crds_absent`]) means "absent"; any other failure — an
/// apiserver hiccup, RBAC not yet propagated — is propagated so startup fails
/// fast and
/// Kubernetes restarts us, instead of silently locking the whole process into
/// Ingress-only mode for its lifetime.
///
/// The optional kinds are probed too, and a **403 on them is fatal, not
/// "absent"** — the v1.6.1 standard-channel bundle ships tcproutes/udproutes,
/// so a ClusterRole that predates this grant answers 403 where the CRD is
/// plainly installed. Reading that as absence would silently drop layer-4
/// routing on exactly the clusters that have it.
async fn gateway_api_available(client: &Client) -> Result<GatewayApiSupport> {
    let served = [
        crd_served::<GatewayClass>(client).await?,
        crd_served::<Gateway>(client).await?,
        crd_served::<HttpRoute>(client).await?,
        crd_served::<ReferenceGrant>(client).await?,
    ];
    let missing = missing_gateway_crds(served);
    if !missing.is_empty() {
        if missing.len() == REQUIRED_GATEWAY_API_KINDS.len() {
            // No Gateway API at all: the ordinary Ingress-only cluster.
            debug!("Gateway API CRDs not installed");
        } else {
            warn!(
                ?missing,
                "partial Gateway API install: some required CRDs are not served; \
                 running in Ingress-only mode until all of them are installed"
            );
        }
        return Ok(GatewayApiSupport::default());
    }
    let support = GatewayApiSupport {
        core: true,
        tcp_routes: crd_served::<TcpRoute>(client).await?,
        udp_routes: crd_served::<UdpRoute>(client).await?,
    };
    let absent: Vec<&str> = OPTIONAL_GATEWAY_API_KINDS
        .iter()
        .zip([support.tcp_routes, support.udp_routes])
        .filter_map(|(kind, served)| (!served).then_some(*kind))
        .collect();
    if !absent.is_empty() {
        info!(
            ?absent,
            "Gateway API layer-4 route CRDs are not installed; TCP/UDP routing via the \
             Gateway API is unavailable (the rest of the Gateway API is unaffected)"
        );
    }
    Ok(support)
}

/// Probe one watched kind with a tiny list: `Ok(true)` = served, `Ok(false)` =
/// the apiserver does not serve it (`NotFound`), `Err` = anything else (fail
/// fast).
async fn crd_served<K>(client: &Client) -> Result<bool>
where
    K: Resource + Clone + DeserializeOwned + std::fmt::Debug,
    K::DynamicType: Default,
{
    let api: Api<K> = Api::all(client.clone());
    match api.list(&ListParams::default().limit(1)).await {
        Ok(_) => Ok(true),
        Err(e) if gateway_crds_absent(&e) => Ok(false),
        // A 403 lands here, not in the absent branch, and it must: reading
        // "forbidden" as "not installed" would quietly drop the whole Gateway
        // API because someone tightened a ClusterRole. Fail fast, but name the
        // kind and the likely cause — the alternative is a crash-loop whose log
        // says only "probe Gateway API availability".
        Err(e) => {
            let kind = std::any::type_name::<K>()
                .rsplit("::")
                .next()
                .unwrap_or("?");
            Err(e).with_context(|| {
                format!(
                    "probing whether {kind} is served (a 403 here means the ClusterRole is \
                     missing list/watch on it, not that the CRD is absent)"
                )
            })
        }
    }
}

/// Pure classifier: which *required* Gateway API kinds are missing, given the
/// per-kind probe results (in [`REQUIRED_GATEWAY_API_KINDS`] order). Any
/// missing one forces Ingress-only mode.
fn missing_gateway_crds(served: [bool; 4]) -> Vec<&'static str> {
    REQUIRED_GATEWAY_API_KINDS
        .iter()
        .zip(served)
        .filter_map(|(kind, served)| (!served).then_some(*kind))
        .collect()
}

/// Clamp a kube `Config` to one-shot-call timeouts (see the `ops_client`
/// construction in `main`): connecting or waiting minutes on a single
/// GET/PATCH is never right for best-effort calls made inline in the
/// reconcile loop.
fn bound_ops_config(cfg: &mut kube::Config) {
    cfg.connect_timeout = Some(Duration::from_secs(10));
    cfg.read_timeout = Some(Duration::from_secs(30));
    cfg.write_timeout = Some(Duration::from_secs(30));
}

/// Classify the probe error: a 404 from the apiserver (what the list returns
/// when the CRD's group/kind is not served) means the CRD is absent. Matched
/// by HTTP code, not only by the parsed `NotFound` reason: managed clusters
/// that front the apiserver with an HTTP router can answer an unserved
/// group's path with a plain-text `404 page not found` body, which
/// kube-client cannot parse into a typed `Status` — `reason` is then a
/// synthetic parse-failure marker but `code` is still 404. A 404 on a
/// collection list is never transient, so everything else stays fail-fast.
fn gateway_crds_absent(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(status) if status.is_not_found() || status.code == 404)
}

/// Run one restart-generation check (see [`shadow::check_restart_generation`]),
/// consuming the pending reconnect signal only when the probe *succeeds*: on a
/// probe error `acked_reconnects` stays behind the agent's epoch, so the
/// reconnect remains visible and the check is retried instead of silently
/// dropped. A reset drops readiness right here, before any caller decides
/// what to do next: the Pod must leave the Service the moment Sōzu is known
/// to be empty, not after the re-apply has been scheduled.
async fn probe_sozu_generation(
    agent: &SozuAgentHandle,
    acked_reconnects: &mut u64,
    shadow: &mut shadow::Shadow,
    shadow_file: &str,
    self_metrics: &metrics::SelfMetrics,
    ready: &AtomicBool,
) -> shadow::GenerationCheck {
    // Read the epoch *before* the probe: a reconnect landing mid-probe stays
    // pending and triggers one more (cheap, idempotent) check.
    let pending = agent.reconnect_epoch();
    let outcome = shadow::check_restart_generation(agent, shadow).await;
    if outcome != shadow::GenerationCheck::ProbeFailed {
        *acked_reconnects = pending;
    }
    if outcome == shadow::GenerationCheck::Reset {
        self_metrics.record_shadow_reset();
        // The file must never outlive the state it describes: a controller
        // restart before the full re-apply lands would otherwise resume the
        // stale IR against a Sōzu that holds nothing of it.
        shadow::persist(shadow_file, shadow);
    }
    readiness_after_check(ready, outcome);
    outcome
}

fn parse_publish_service(value: &str) -> Option<(&str, &str)> {
    value.split_once('/').filter(|(namespace, name)| {
        !namespace.is_empty() && !name.is_empty() && !name.contains('/')
    })
}

/// Names can be reused after deletion. An old Pod must never program the
/// replacement Gateway or publish its old Service address into its status.
async fn verify_gateway_identity(args: &Args, client: &Client) -> Result<()> {
    let Some(uid) = &args.gateway_uid else {
        return Ok(());
    };
    let scope = args
        .gateway_scope
        .gateway_scope
        .as_ref()
        .context("--gateway-uid requires --gateway-scope")?;
    let gateway = Api::<Gateway>::namespaced(client.clone(), &scope.namespace)
        .get_opt(&scope.name)
        .await
        .context("verify the scoped Gateway identity")?
        .context("the scoped Gateway no longer exists")?;
    anyhow::ensure!(
        gateway.metadata.uid.as_ref() == Some(uid) && gateway.metadata.deletion_timestamp.is_none(),
        "the scoped Gateway was replaced or is being deleted"
    );
    Ok(())
}

/// Infrastructure provisioning is separate from socket reconciliation. One
/// failed Service or rollout must not stall already serving proxy workers.
async fn run_provisioner(
    args: &Args,
    path: &str,
    watch_client: Client,
    ops_client: Client,
    ready: Arc<AtomicBool>,
) -> Result<()> {
    let config: provision::ProvisionConfig =
        serde_json::from_slice(&std::fs::read(path).context("read the provisioning template")?)
            .context("parse the provisioning template")?;
    let provisioner = provision::Provisioner::new(ops_client, config).await?;
    let (tx, mut rx) = mpsc::channel(64);
    let (gateways, writer) = reflector::store();
    spawn_watch(
        Api::<Gateway>::all(watch_client.clone()),
        watch_config(args.watch_timeout_secs),
        writer,
        tx.clone(),
        "provisioner Gateway",
    );
    let (classes, writer) = reflector::store();
    spawn_watch(
        Api::<GatewayClass>::all(watch_client),
        watch_config(args.watch_timeout_secs),
        writer,
        tx,
        "provisioner GatewayClass",
    );
    tokio::time::timeout(Duration::from_secs(120), async {
        tokio::try_join!(gateways.wait_until_ready(), classes.wait_until_ready())
    })
    .await
    .context("timed out waiting for provisioning caches; check Gateway API CRDs and RBAC")?
    .context("provisioning cache writer stopped")?;

    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;
    // Watch events drive provisioning immediately. A slower resync repairs
    // infrastructure drift, while failed operations get a short retry delay.
    let mut resync = optional_period(args.resync_secs).map(delayed_interval);
    loop {
        let failed = match provisioner
            .reconcile(&gateways.state(), &classes.state(), &args.controller_name)
            .await
        {
            Ok(outcome) => {
                for (uid, failure) in &outcome.failures {
                    warn!(gateway_uid = %uid, error = %failure, "Gateway provisioning failed; will retry");
                }
                mark_ready(&ready);
                !outcome.failures.is_empty()
            }
            Err(error) => {
                ready.store(false, Ordering::Relaxed);
                warn!(error = %error, "provisioning failed; will retry");
                true
            }
        };
        tokio::select! {
            event = changes::debounced(&mut rx, Duration::from_millis(args.debounce_ms)) => {
                if event.is_none() {
                    anyhow::bail!("provisioning watch channel closed");
                }
            }
            _ = maybe_tick(resync.as_mut()) => {}
            _ = tokio::time::sleep(Duration::from_secs(5)), if failed => {}
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }
    Ok(())
}

/// One global reconcile: caches → IR → diff → apply. Updates `shadow` (the
/// last-applied IR) only on a successful apply, so a failed push is retried from
/// the same baseline.
#[allow(clippy::too_many_arguments)]
async fn reconcile(
    args: &Args,
    client: &Client,
    stores: &Stores,
    agent: &SozuAgentHandle,
    shadow: &mut shadow::Shadow,
    problem_events: &mut events::ProblemEvents,
    referenced_services: &RwLock<BTreeSet<String>>,
    exposure: &[ExposedPort],
) -> Result<()> {
    verify_gateway_identity(args, client).await?;
    let cfg = BuildConfig {
        class_name: args.class_name.clone(),
        class_is_default: class_is_default(stores, &args.class_name),
        controller_name: args.controller_name.clone(),
        exposure: exposure.to_vec(),
    };
    // The stores hand out `Arc`s to the cached objects; the builder borrows
    // them as-is, so a reconcile never deep-clones the whole cluster state.
    let mut inputs = Inputs {
        ingresses: stores.ingresses.state(),
        namespaces: stores.namespaces.state(),
        services: stores.services.state(),
        endpointslices: stores.endpointslices.state(),
        secrets: stores.secrets.state(),
        gateway_classes: stores.gateway_classes.state(),
        gateways: stores.gateways.state(),
        http_routes: stores.http_routes.state(),
        reference_grants: stores.reference_grants.state(),
        tcp_routes: stores.tcp_routes.state(),
        udp_routes: stores.udp_routes.state(),
    };

    args.gateway_scope.filter_inputs(&mut inputs);
    if let Some(uid) = &args.gateway_uid {
        anyhow::ensure!(
            inputs.gateways.len() == 1 && inputs.gateways[0].metadata.uid.as_ref() == Some(uid),
            "waiting for the scoped Gateway incarnation in the reflector cache"
        );
    }
    let out = build(&cfg, &inputs);

    // Publish the Services this build referenced (resolved or not) for the
    // EndpointSlice ping filter — before and independent of the apply:
    // relevance follows the *desired* state, not whether the socket push
    // succeeds.
    *referenced_services
        .write()
        .unwrap_or_else(|e| e.into_inner()) = out.referenced_services.clone();

    for r in &out.results {
        if !r.problems.is_empty() {
            warn!(namespace = %r.namespace, name = %r.name, problems = ?r.problems, "ingress has problems");
        }
    }
    for g in &out.gateways {
        if !g.problems.is_empty() {
            warn!(namespace = %g.namespace, name = %g.name, problems = ?g.problems, "gateway has problems");
        }
    }
    for route in &out.routes {
        for parent in &route.parents {
            if !parent.problems.is_empty() {
                warn!(namespace = %route.namespace, name = %route.name, gateway = %parent.gateway_name, problems = ?parent.problems, "httproute has problems");
            }
        }
    }

    // Surface the problems on their owning objects (kubectl describe), before
    // and independent of the apply: a broken Secret must be visible to its
    // owner even when the socket push fails. Best-effort, diffed against the
    // previous pass so resyncs do not flood etcd with duplicate events.
    problem_events.publish_new(&out).await;

    let requests = tr::reconcile(&shadow.ir, &out.ir).context("translate IR to commands")?;
    if requests.is_empty() {
        debug!("reconcile: no socket changes");
    } else {
        info!(
            clusters = out.ir.clusters.len(),
            backends = out.ir.backends.len(),
            frontends = out.ir.frontends.len(),
            certificates = out.ir.certificates.len(),
            requests = requests.len(),
            "applying changes to sozu"
        );
        // Bound the apply so a wedged Sōzu socket surfaces as a retryable error
        // instead of stalling the reconcile loop indefinitely.
        tokio::time::timeout(Duration::from_secs(60), agent.apply(requests))
            .await
            .context("timed out applying requests to sozu")?
            .context("apply requests to sozu")?;
        // The shadow advances the moment the socket apply succeeds, before any
        // apiserver call below can fail: Sōzu now holds `out.ir`, and a
        // baseline that lags behind it would re-emit the same delta next pass —
        // every frontend add answered `Exists` and "repaired" with a remove +
        // re-add, a routing gap per route, for as long as the apiserver is
        // unhappy. Persisting here also narrows the window in which a
        // controller restart finds a file that predates the apply.
        //
        // Only a successful apply advances it. On failure it stays at the
        // previous applied IR; the emitted requests are not all idempotent, so
        // re-diffing from the unchanged shadow converges thanks to Sōzu's
        // upsert semantics for clusters/backends plus the agent's handling of
        // the rest: already-gone teardowns are tolerated, duplicate frontend
        // adds repaired (remove + re-add on the same route key).
        shadow.ir = out.ir.clone();
        shadow::persist(&args.shadow_file, shadow);
    }

    // Fence every pass, including one that applied nothing: `publish_new` above
    // makes apiserver calls, so the pre-build check is not "a moment ago", and
    // `write_route` guards only the *route*'s identity — nothing downstream
    // notices that our Gateway was replaced. Worse, its 409 path re-merges and
    // retries, so a stale writer wins the race instead of losing it.
    verify_gateway_identity(args, client).await?;

    // Report Gateway API status (best-effort; never fails the reconcile). It is
    // loop-safe: a no-op patch is skipped, so our own writes don't re-trigger.
    // Resolve our own LoadBalancer Service once: its address is published into
    // both Ingress `.status` and Gateway `.status.addresses` (what external-dns
    // consumes). Best-effort + loop-safe (writes skipped when already current).
    let publish_reference = args
        .publish_service
        .as_deref()
        .and_then(parse_publish_service);
    let publish_svc = publish_reference.and_then(|(ns, name)| {
        inputs.services.iter().find(|s| {
            s.metadata.namespace.as_deref() == Some(ns) && s.metadata.name.as_deref() == Some(name)
        })
    });
    let published = status::publication(
        publish_reference.is_some(),
        publish_svc.map(|svc| svc.as_ref()),
    );

    // Skippable for least-privilege deployments running without the
    // gateways/status RBAC grants, where every write would 403.
    if args.gateway_status_writes {
        let route_updates = status::route_updates(
            &out.routes,
            &inputs,
            &args.controller_name,
            &args.gateway_scope,
        );
        status::write_status(
            client,
            &args.controller_name,
            &out.gateway_classes,
            &out.gateways,
            &route_updates,
            &published,
            &args.gateway_scope,
        )
        .await;
    } else {
        debug!("gateway status writes disabled");
    }

    let lb_points = publish_svc
        .map(|s| status::lb_points(s))
        .unwrap_or_default();
    if args.gateway_scope.is_default() && args.ingress_status_writes {
        status::write_ingress_status(client, &out.results, &lb_points).await;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    args.gateway_scope.validate().map_err(anyhow::Error::msg)?;
    validate_watch_timeout(args.watch_timeout_secs)?;
    info!(?args, "starting sozu gateway controller");

    // The exposure table decides both what a Gateway listener may declare and
    // where Sōzu listens for it, so a malformed or self-contradicting one is a
    // startup error, not something to discover on the first reconcile.
    let exposure: Vec<ExposedPort> =
        serde_json::from_str(&args.exposure).context("parsing --exposure")?;
    if exposure.is_empty() {
        anyhow::bail!("--exposure is empty: the gateway would serve nothing");
    }
    {
        // Injectivity on `bind`, not on the advertised key: two entries can
        // differ in everything a user reads and still resolve to one socket,
        // and the second listener add would fail the whole reconcile.
        let probe = BuildConfig {
            exposure: exposure.clone(),
            ..Default::default()
        };
        let clashes = probe.colliding_binds();
        if let Some((a, b)) = clashes.first() {
            anyhow::bail!(
                "exposure entries {:?} and {:?} both bind {} — one socket cannot serve two",
                a.name,
                b.name,
                a.bind
            );
        }
    }
    info!(
        ports = ?exposure.iter().map(|e| format!("{}={}->{}", e.name, e.port, e.bind)).collect::<Vec<_>>(),
        "exposed ports"
    );

    if let Some(ps) = &args.publish_service {
        if parse_publish_service(ps).is_none() {
            warn!(publish_service = %ps, "--publish-service must be namespace/name; Ingress status will not be written");
        }
    }

    // Health endpoints come up immediately: /healthz (liveness) is green now,
    // while /readyz (readiness) stays 503 until the first reconcile has
    // programmed Sōzu, so the Pod takes no traffic during the cold-start gap.
    let ready = Arc::new(AtomicBool::new(false));
    health::spawn(args.health_listen, ready.clone());

    let client = {
        let mut cfg = kube::Config::infer()
            .await
            .context("create kube client (in-cluster or kubeconfig)")?;
        if args.kube_read_timeout_secs > 0 {
            cfg.read_timeout = Some(Duration::from_secs(args.kube_read_timeout_secs));
        }
        Client::try_from(cfg).context("create kube client (in-cluster or kubeconfig)")?
    };
    // Second client for one-shot calls (status writes, Events, probes), with
    // tight timeouts. The default client has **no read timeout at all** —
    // `kube::Config` sets `read_timeout: None` in every constructor, and the
    // ~295 s often attributed to it is the *watcher's* idle timeout
    // (`timeoutSeconds` + a 5 s margin), which guards watch streams and nothing
    // else. So a single status write on a black-holed connection would park the
    // singleton reconcile loop indefinitely, and routing and status would starve
    // together (observed live under conformance-suite churn). One-shot calls get
    // seconds; a slow write fails fast and stays best-effort.
    let ops_client = {
        let mut cfg = kube::Config::infer()
            .await
            .context("infer kube config for the bounded ops client")?;
        bound_ops_config(&mut cfg);
        Client::try_from(cfg).context("create bounded ops client")?
    };
    if let Some(path) = &args.provision_template {
        return run_provisioner(&args, path, client, ops_client, ready).await;
    }
    let agent = SozuAgentHandle::spawn(&args.socket).context("spawn sozu-agent")?;

    // Optional Prometheus `/metrics`: each scrape pulls Sōzu's aggregated
    // metrics over the same command socket and renders them. Best-effort and
    // independent of routing — a bind failure never affects reconciliation.
    // Self-metrics are recorded unconditionally (cheap atomics); the endpoint
    // below only decides whether anyone can scrape them.
    let self_metrics = Arc::new(metrics::SelfMetrics::default());
    if let Some(endpoint) = metrics_endpoint(&args) {
        metrics::spawn(endpoint, agent.clone(), self_metrics.clone());
    }

    // Ordinary changes coalesce for the debounce period. EndpointSlice
    // changes have their own one-pending signal, so a full ordinary queue
    // cannot delay a newly ready backend or a withdrawn endpoint.
    let (tx, mut rx) = mpsc::channel::<()>(64);
    let (endpoint_tx, mut endpoint_rx) = mpsc::channel::<()>(1);

    // `namespace/name` of the Services the last build referenced, shared with
    // the EndpointSlice watcher so endpoint churn from unrelated workloads —
    // the dominant wakeup source on a busy cluster — stops triggering full
    // rebuilds. Empty until the first build; `slice_pings` then passes
    // everything, since a missed wakeup is an outage risk while a spurious
    // one only costs CPU.
    let referenced_services: Arc<RwLock<BTreeSet<String>>> = Arc::new(RwLock::new(BTreeSet::new()));

    // Every watch carries the configured `timeoutSeconds`, which is also what
    // kube-rs derives its client-side idle timeout from — the only bound on how
    // long a silently-dead watch can keep a reflector frozen.
    let watch_timeout_secs = args.watch_timeout_secs;
    let watch_all = move || watch_config(watch_timeout_secs);
    // A scoped instance serves one Gateway and `GatewayScope::filter_inputs`
    // discards every Ingress, so watching them cluster-wide would only hold a
    // cache it never reads and wake this worker on changes it must ignore.
    let serves_ingresses = args.gateway_scope.is_default();
    let (ingresses, w) = reflector::store();
    if serves_ingresses {
        spawn_watch::<Ingress>(
            Api::all(client.clone()),
            watch_all(),
            w,
            tx.clone(),
            "ingress",
        );
    }
    // Namespaces are watched for their **labels**: that is what
    // `allowedRoutes.namespaces.selector` selects on, and a label edit changes
    // which routes a listener admits, so it has to wake the loop.
    let (namespaces, w) = reflector::store();
    spawn_watch::<Namespace>(
        Api::all(client.clone()),
        watch_all(),
        w,
        tx.clone(),
        "namespace",
    );
    let (ingress_classes, w) = reflector::store();
    if serves_ingresses {
        spawn_watch::<IngressClass>(
            Api::all(client.clone()),
            watch_all(),
            w,
            tx.clone(),
            "ingressclass",
        );
    }
    let (services, w) = reflector::store();
    spawn_watch::<Service>(
        Api::all(client.clone()),
        watch_all(),
        w,
        tx.clone(),
        "service",
    );
    let (endpointslices, w) = reflector::store();
    let ping_set = referenced_services.clone();
    let slice_tx = tx.clone();
    let slice_endpoint_tx = endpoint_tx.clone();
    spawn_watch_notified::<EndpointSlice, _>(
        Api::all(client.clone()),
        watch_all(),
        w,
        "endpointslice",
        move |event| {
            let set = ping_set.read().unwrap_or_else(|e| e.into_inner());
            notify_slice_event(&set, event, &slice_tx, &slice_endpoint_tx);
        },
    );
    // Only TLS Secrets are of any use to the builder; watching every Secret in
    // the cluster (SA tokens, Helm release blobs, application secrets) would
    // cache them all in this process for nothing — maximal memory cost and
    // maximal blast radius. The field selector bounds both.
    let (secrets, w) = reflector::store();
    spawn_watch::<Secret>(
        Api::all(client.clone()),
        watch_all().fields("type=kubernetes.io/tls"),
        w,
        tx.clone(),
        "secret",
    );

    // Gateway API CRDs are optional. Only watch them when they are installed, so
    // an Ingress-only cluster runs cleanly instead of logging watch errors.
    let (gateway_classes, gc_w) = reflector::store();
    let (gateways, gw_w) = reflector::store();
    let (http_routes, hr_w) = reflector::store();
    let (reference_grants, rg_w) = reflector::store();
    let (tcp_routes, tcp_w) = reflector::store();
    let (udp_routes, udp_w) = reflector::store();
    let gw_api = gateway_api_available(&ops_client).await?;
    let gateway_api_enabled = gw_api.core;
    if gateway_api_enabled {
        info!("Gateway API detected; watching gateway.networking.k8s.io resources");
        spawn_watch::<GatewayClass>(
            Api::all(client.clone()),
            watch_all(),
            gc_w,
            tx.clone(),
            "gatewayclass",
        );
        spawn_watch::<Gateway>(
            Api::all(client.clone()),
            watch_all(),
            gw_w,
            tx.clone(),
            "gateway",
        );
        spawn_watch::<HttpRoute>(
            Api::all(client.clone()),
            watch_all(),
            hr_w,
            tx.clone(),
            "httproute",
        );
        spawn_watch::<ReferenceGrant>(
            Api::all(client.clone()),
            watch_all(),
            rg_w,
            tx.clone(),
            "referencegrant",
        );
        // Layer-4 route kinds, each watched only when its CRD is served.
        if gw_api.tcp_routes {
            spawn_watch::<TcpRoute>(
                Api::all(client.clone()),
                watch_all(),
                tcp_w,
                tx.clone(),
                "tcproute",
            );
        } else {
            drop(tcp_w);
        }
        if gw_api.udp_routes {
            spawn_watch::<UdpRoute>(
                Api::all(client.clone()),
                watch_all(),
                udp_w,
                tx.clone(),
                "udproute",
            );
        } else {
            drop(udp_w);
        }
    } else {
        info!("Gateway API CRDs not found; running in Ingress-only mode");
        drop((gc_w, gw_w, hr_w, rg_w, tcp_w, udp_w));
    }

    let stores = Stores {
        ingresses,
        namespaces,
        ingress_classes,
        services,
        endpointslices,
        secrets,
        gateway_classes,
        gateways,
        http_routes,
        reference_grants,
        tcp_routes,
        udp_routes,
    };

    // Wait for the caches to fill so the first reconcile sees a complete picture.
    // Every spawned watcher is gated, including the optional Gateway API ones:
    // a resumed shadow holds Gateway routes and L4 listeners,
    // so reconciling before those caches finish their initial LIST would diff
    // them away (a live-traffic flap). Bounded so a wedged/permission-denied
    // watcher surfaces as a clear failure (CrashLoopBackOff) instead of hanging
    // forever.
    info!("waiting for informer caches to sync...");
    let sync = async {
        tokio::try_join!(
            ready_when(serves_ingresses, &stores.ingresses),
            stores.namespaces.wait_until_ready(),
            ready_when(serves_ingresses, &stores.ingress_classes),
            stores.services.wait_until_ready(),
            stores.endpointslices.wait_until_ready(),
            stores.secrets.wait_until_ready(),
            ready_when(gateway_api_enabled, &stores.gateway_classes),
            ready_when(gateway_api_enabled, &stores.gateways),
            ready_when(gateway_api_enabled, &stores.http_routes),
            ready_when(gateway_api_enabled, &stores.reference_grants),
            ready_when(gateway_api_enabled && gw_api.tcp_routes, &stores.tcp_routes),
            ready_when(gateway_api_enabled && gw_api.udp_routes, &stores.udp_routes),
        )
    };
    tokio::time::timeout(Duration::from_secs(120), sync)
        .await
        .context(
            "timed out waiting for informer caches to sync \
             (check RBAC, and that every watched CRD is installed and served)",
        )?
        .context("informer cache writer dropped before becoming ready")?;
    info!("caches synced");

    // Read the reconnect epoch *before* asking for the generation: a reconnect
    // landing during that await must leave the epoch ahead of this baseline so
    // the conditional probe below fires, instead of acknowledging it unseen and
    // trusting a generation from a socket that has since gone.
    let mut acked_reconnects = agent.reconnect_epoch();
    // Capture the generation, then resume the persisted shadow only if it was
    // written against the same command socket: a Sōzu that restarted while the
    // controller was down recreated its socket and holds nothing the file
    // describes. A failed capture is no proof, so nothing is resumed.
    let generation = match agent.generation().await {
        Ok(generation) => Some(generation),
        Err(e) => {
            warn!(error = %e, "could not capture Sōzu's generation baseline; ignoring any persisted shadow, will re-apply");
            None
        }
    };
    let mut shadow = shadow::load_initial(&args.shadow_file, generation.as_ref());
    if agent.reconnect_epoch() != acked_reconnects
        && probe_sozu_generation(
            &agent,
            &mut acked_reconnects,
            &mut shadow,
            &args.shadow_file,
            &self_metrics,
            &ready,
        )
        .await
            == shadow::GenerationCheck::ProbeFailed
    {
        warn!("could not verify Sōzu's generation after loading the shadow; will re-apply");
        shadow = shadow::Shadow::empty(None);
    }

    let debounce = Duration::from_millis(args.debounce_ms);
    let mut resync = optional_period(args.resync_secs).map(delayed_interval);
    if resync.is_none() {
        info!("periodic resync disabled (resync-secs = 0)");
    }
    // The Sōzu liveness tick: a `Status` round-trip per period, nothing more
    // unless it finds a restart. Without it an idle socket never reconnects and
    // a restarted-empty Sōzu waits for the next event or resync tick.
    let mut sozu_probe = optional_period(args.sozu_probe_secs).map(delayed_interval);
    let has_probe_tick = sozu_probe.is_some();
    if !has_probe_tick {
        info!("Sōzu liveness probe disabled (sozu-probe-secs = 0)");
    }
    // With neither tick, a failed reconcile schedules its own delayed retry;
    // otherwise the ticks retry it and a timer would only double them.
    let self_retry = retry_reconcile_by_nudge(has_probe_tick, resync.is_some());
    let mut retry: Option<RetryTimer> = None;
    // Set by a failed generation probe, cleared by the next one that answers.
    // While it holds, the liveness tick retries at debug: the check warns on
    // every failure, and a dead data plane is not news every two seconds.
    let mut sozu_unreachable = false;
    // Work a failed reconcile left behind, to be retried by the next successful
    // probe rather than waiting for an unrelated event.
    let mut pending_reconcile = false;

    // Graceful shutdown on the signals Kubernetes uses on Pod termination, so we
    // stop cleanly within the grace period instead of being SIGKILLed.
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;

    // Event publisher for reported problems, with its diff baseline.
    let mut problem_events = events::ProblemEvents::new(ops_client.clone());

    // Initial reconcile (full apply). Readiness latches once this succeeds.
    let started = std::time::Instant::now();
    match reconcile(
        &args,
        &ops_client,
        &stores,
        &agent,
        &mut shadow,
        &mut problem_events,
        &referenced_services,
        &exposure,
    )
    .await
    {
        Ok(()) => {
            self_metrics.record_reconcile(started.elapsed(), true);
            mark_ready(&ready);
        }
        Err(e) => {
            self_metrics.record_reconcile(started.elapsed(), false);
            error!(error = ?e, "initial reconcile failed; will retry");
            pending_reconcile = true;
            if self_retry {
                retry = Some(scheduled_retry());
            }
        }
    }
    // A reconnect can land *during* that first apply (Sōzu restarting
    // mid-batch): probe right away and nudge the loop when a full re-apply or
    // a probe retry is due, instead of waiting for a watch event.
    if agent.reconnect_epoch() != acked_reconnects {
        let outcome = probe_sozu_generation(
            &agent,
            &mut acked_reconnects,
            &mut shadow,
            &args.shadow_file,
            &self_metrics,
            &ready,
        )
        .await;
        sozu_unreachable = outcome == shadow::GenerationCheck::ProbeFailed;
        if outcome == shadow::GenerationCheck::Reset || retry_by_nudge(outcome, has_probe_tick) {
            let _ = tx.try_send(());
        }
    }

    loop {
        // Wait for a change signal, the resync tick or the Sōzu liveness tick.
        // The change receivers are awaited raw here, not through
        // `changes::next`, because `mpsc::Receiver::recv` is cancellation-safe:
        // when the Sōzu-liveness or resync tick wins this race it removes no
        // notice. A consumed change is debounced by `changes::settle` *after*
        // the select! has resolved — past the race — so a tick can never drop a
        // pending change (which an unchanged liveness tick would then never
        // reconcile). Defensive `is_none`: this loop holds its own tx clone, so
        // the channel cannot actually close; if that invariant ever breaks,
        // exit loudly rather than spin on a dead channel.
        let wakeup = tokio::select! {
            changed = endpoint_rx.recv() => {
                if changed.is_none() { warn!("change channel closed (should be unreachable); exiting"); break; }
                changes::settle(&mut rx, &mut endpoint_rx, debounce, true).await;
                Wakeup::Change
            }
            changed = rx.recv() => {
                if changed.is_none() { warn!("change channel closed (should be unreachable); exiting"); break; }
                changes::settle(&mut rx, &mut endpoint_rx, debounce, false).await;
                Wakeup::Change
            }
            _ = maybe_tick(resync.as_mut()) => {
                debug!("periodic resync");
                // The CRD probe runs once, at startup, because the reflectors
                // for the Gateway API kinds are wired there — a store whose
                // writer was dropped cannot be attached to later. So a cluster
                // that installs the CRDs under a running controller would stay
                // Ingress-only for the process's whole life, with nothing said
                // about it after the one startup line.
                //
                // Re-probing here and exiting on a change hands the problem to
                // the thing that already solves it: Kubernetes restarts us, and
                // the fresh process wires the watches. Same reflex as a watch
                // stream ending — fail fast rather than run blind.
                //
                // This covers the layer-4 kinds as well as the core: installing
                // TCPRoute under a live controller must not leave it blind to
                // the kind for the rest of the process's life either.
                if gw_api != GatewayApiSupport::full() {
                    match gateway_api_available(&ops_client).await {
                        Ok(now) if now != gw_api => {
                            info!(
                                before = ?gw_api,
                                after = ?now,
                                "Gateway API CRDs have appeared since startup; exiting so the \
                                 restarted process watches them"
                            );
                            std::process::exit(0);
                        }
                        Ok(_) => {}
                        // A probe failure here is not fatal: we are already
                        // serving Ingress, and the next tick retries.
                        Err(e) => debug!(error = ?e, "Gateway API re-probe failed; will retry"),
                    }
                }
                Wakeup::Resync
            }
            _ = maybe_tick(sozu_probe.as_mut()) => Wakeup::SozuProbe,
            _ = maybe_retry(retry.as_mut()) => {
                retry = None;
                Wakeup::Retry
            }
            _ = sigterm.recv() => { info!("SIGTERM received; shutting down"); break; }
            _ = sigint.recv() => { info!("SIGINT received; shutting down"); break; }
        };

        // A liveness tick while Sōzu was last seen unreachable: a plain
        // `Status` first, so a data plane that is still down costs one debug
        // line per tick rather than the generation check's warning. The
        // authoritative check below runs once the socket answers again.
        if wakeup == Wakeup::SozuProbe && sozu_unreachable {
            match agent.generation().await {
                Ok(_) => info!("Sōzu is answering the command socket again"),
                Err(e) => {
                    debug!(error = %e, "Sōzu still unreachable; retrying on the next liveness tick");
                    continue;
                }
            }
        }

        // If Sōzu restarted under us, the agent reconnects transparently and
        // the diff against the stale shadow stays empty — every request would
        // 404 forever. Check Sōzu's socket/worker generation on every resync
        // and liveness tick and whenever a reconnect is pending, resetting the
        // shadow (and dropping readiness) on a change so the reconcile below
        // re-applies the full state. The reconnect signal is consumed only by
        // a *successful* probe: on a failure it stays pending, and the check is
        // retried by the liveness tick — or, without one, by nudging the loop,
        // so it is retried promptly even with resync disabled and no watch
        // traffic.
        let check = if wakeup != Wakeup::Change || agent.reconnect_epoch() != acked_reconnects {
            let outcome = probe_sozu_generation(
                &agent,
                &mut acked_reconnects,
                &mut shadow,
                &args.shadow_file,
                &self_metrics,
                &ready,
            )
            .await;
            sozu_unreachable = outcome == shadow::GenerationCheck::ProbeFailed;
            if retry_by_nudge(outcome, has_probe_tick) {
                let _ = tx.try_send(());
            }
            Some(outcome)
        } else {
            None
        };
        // A liveness tick that found nothing changed ends here: no rebuild, no
        // reconcile. Only a reset (a full re-apply is due) goes on.
        if !reconciles(wakeup, check, pending_reconcile) {
            continue;
        }

        let started = std::time::Instant::now();
        match reconcile(
            &args,
            &ops_client,
            &stores,
            &agent,
            &mut shadow,
            &mut problem_events,
            &referenced_services,
            &exposure,
        )
        .await
        {
            Ok(()) => {
                self_metrics.record_reconcile(started.elapsed(), true);
                mark_ready(&ready);
                pending_reconcile = false;
                // A watch event that reconciled successfully inside the retry
                // window has done the retry's work: disarm it, or it would run
                // one redundant full rebuild.
                retry = None;
            }
            Err(e) => {
                self_metrics.record_reconcile(started.elapsed(), false);
                if self_retry {
                    error!(
                        error = ?e,
                        retry_in_secs = RECONCILE_RETRY_DELAY.as_secs(),
                        "reconcile failed; will retry"
                    );
                    retry = Some(scheduled_retry());
                } else {
                    error!(error = ?e, "reconcile failed; will retry on the next liveness tick, event or resync");
                }
                pending_reconcile = true;
            }
        }

        // The emptiness race, closed: a reconnect landing *mid-apply* means
        // the rest of the batch was applied to a freshly restarted Sōzu — it
        // is no longer empty, but it only holds that delta. Probe immediately
        // after the apply instead of waiting for the next event; on a reset (a
        // full re-apply is due) nudge the channel so the next pass runs
        // promptly, and on a failed probe leave the retry to the liveness tick
        // (or nudge, without one).
        if agent.reconnect_epoch() != acked_reconnects {
            let outcome = probe_sozu_generation(
                &agent,
                &mut acked_reconnects,
                &mut shadow,
                &args.shadow_file,
                &self_metrics,
                &ready,
            )
            .await;
            sozu_unreachable = outcome == shadow::GenerationCheck::ProbeFailed;
            if outcome == shadow::GenerationCheck::Reset || retry_by_nudge(outcome, has_probe_tick)
            {
                let _ = tx.try_send(());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only as a stand-in resource type for the reflector-readiness tests.
    use k8s_openapi::api::core::v1::ConfigMap;

    #[test]
    fn provisioning_and_worker_modes_cannot_be_combined() {
        for arguments in [
            vec!["controller", "--gateway-uid", "uid"],
            vec!["controller", "--ingress-only", "--gateway-scope", "demo/gw"],
            vec![
                "controller",
                "--provision-template",
                "/template",
                "--ingress-only",
            ],
            vec![
                "controller",
                "--provision-template",
                "/template",
                "--gateway-scope",
                "demo/gw",
            ],
        ] {
            assert!(Args::try_parse_from(arguments).is_err());
        }
        assert!(Args::try_parse_from([
            "controller",
            "--gateway-scope",
            "demo/gw",
            "--gateway-uid",
            "uid"
        ])
        .is_ok());
    }

    /// Per-cluster metrics can wedge Sōzu workers, so they are strictly opt-in,
    /// and the chart's `SOZU_GW_METRICS_PER_CLUSTER` must reach the same flag.
    /// The env binding is read from the parser's definition rather than by
    /// setting the variable: `set_var` is unsound while sibling tests parse
    /// arguments (and thus call `getenv`) on other threads.
    #[test]
    fn per_cluster_metrics_are_off_unless_asked_for() {
        use clap::CommandFactory;

        assert!(
            !Args::try_parse_from(["controller"])
                .unwrap()
                .metrics_per_cluster
        );
        assert!(
            Args::try_parse_from(["controller", "--metrics-per-cluster"])
                .unwrap()
                .metrics_per_cluster
        );
        let command = Args::command();
        let arg = command
            .get_arguments()
            .find(|a| a.get_id() == "metrics_per_cluster")
            .expect("the metrics_per_cluster argument exists");
        assert_eq!(
            arg.get_env(),
            Some(std::ffi::OsStr::new("SOZU_GW_METRICS_PER_CLUSTER"))
        );
    }

    /// What `main` starts the metrics server with, from parsed flags: the
    /// default must reach the server as process-level only. The server's side,
    /// from that switch to the query on the socket, is pinned in `metrics`.
    #[test]
    fn metrics_endpoint_carries_the_parsed_switch() {
        let addr: SocketAddr = "127.0.0.1:9102".parse().unwrap();
        let endpoint = |argv: &[&str]| metrics_endpoint(&Args::try_parse_from(argv).unwrap());

        assert_eq!(endpoint(&["controller"]), None);
        assert_eq!(
            endpoint(&["controller", "--metrics-listen", "127.0.0.1:9102"]),
            Some(metrics::Endpoint {
                addr,
                per_cluster: false
            })
        );
        assert_eq!(
            endpoint(&[
                "controller",
                "--metrics-listen",
                "127.0.0.1:9102",
                "--metrics-per-cluster"
            ]),
            Some(metrics::Endpoint {
                addr,
                per_cluster: true
            })
        );
    }

    #[tokio::test]
    async fn pinned_workers_reject_replaced_deleted_and_unreadable_gateways() {
        use kube::client::Body;
        use serde_json::json;
        let args = Args::try_parse_from([
            "controller",
            "--gateway-scope",
            "demo/gw",
            "--gateway-uid",
            "original",
        ])
        .unwrap();
        for (code, uid, deleting, expected) in [
            (200, "original", false, true),
            (200, "replacement", false, false),
            (200, "original", true, false),
            (404, "", false, false),
            (403, "", false, false),
        ] {
            let service = tower::service_fn(move |request: http::Request<Body>| async move {
                assert_eq!(request.method(), http::Method::GET);
                assert_eq!(
                    request.uri().path(),
                    "/apis/gateway.networking.k8s.io/v1/namespaces/demo/gateways/gw"
                );
                let object = if code == 200 {
                    let mut gateway = json!({
                        "apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
                        "metadata": {"namespace": "demo", "name": "gw", "uid": uid},
                        "spec": {"gatewayClassName": "sozu", "listeners": []}
                    });
                    if deleting {
                        gateway["metadata"]["deletionTimestamp"] = json!("2026-09-10T00:00:00Z");
                    }
                    gateway
                } else {
                    json!({"apiVersion": "v1", "kind": "Status", "status": "Failure",
                        "code": code, "reason": if code == 404 {"NotFound"} else {"Forbidden"}, "message": "unavailable"})
                };
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .status(code)
                        .body(Body::from(serde_json::to_vec(&object).unwrap()))
                        .unwrap(),
                )
            });
            let client = Client::new(service, "default");
            assert_eq!(
                verify_gateway_identity(&args, &client).await.is_ok(),
                expected
            );
        }
    }

    #[test]
    fn publish_service_requires_one_nonempty_namespace_and_name() {
        for (value, expected) in [
            ("", None),
            ("service", None),
            ("/service", None),
            ("namespace/", None),
            ("namespace/service/extra", None),
            ("namespace/service", Some(("namespace", "service"))),
        ] {
            assert_eq!(parse_publish_service(value), expected, "{value}");
        }
    }

    /// An EndpointSlice labelled for `svc` in `ns` (`None` omits the piece).
    fn slice(ns: Option<&str>, svc: Option<&str>) -> EndpointSlice {
        EndpointSlice {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("slice-1".to_string()),
                namespace: ns.map(str::to_string),
                labels: svc.map(|s| {
                    [("kubernetes.io/service-name".to_string(), s.to_string())]
                        .into_iter()
                        .collect()
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn set(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn slice_pings_only_for_referenced_services() {
        let referenced = set(&["demo/web", "prod/api"]);
        // The slice's service is referenced: ping.
        assert!(slice_pings(&referenced, &slice(Some("demo"), Some("web"))));
        // Same name in another namespace, or another service: no ping — this
        // is exactly the unrelated churn the filter exists to drop.
        assert!(!slice_pings(&referenced, &slice(Some("prod"), Some("web"))));
        assert!(!slice_pings(
            &referenced,
            &slice(Some("demo"), Some("other"))
        ));
        // No service-name label: the builder cannot attribute the slice to
        // any Service, so it can never change the build — no ping.
        assert!(!slice_pings(&referenced, &slice(Some("demo"), None)));
    }

    #[test]
    fn slice_pings_defaults_the_namespace_like_the_builder() {
        // A namespace-less slice must match the builder's `default` fallback,
        // or a referenced default-namespace Service would stop waking us.
        let referenced = set(&["default/web"]);
        assert!(slice_pings(&referenced, &slice(None, Some("web"))));
    }

    #[test]
    fn empty_referenced_set_passes_every_slice() {
        // Before the first build populates the set, a missed wakeup is an
        // outage risk; everything — even unattributable slices — must ping.
        let empty = BTreeSet::new();
        assert!(slice_pings(&empty, &slice(Some("demo"), Some("web"))));
        assert!(slice_pings(&empty, &slice(Some("demo"), None)));
        assert!(slice_pings(&empty, &slice(None, None)));
    }

    #[test]
    fn referenced_slices_use_the_endpoint_channel() {
        let (tx, mut rx) = mpsc::channel(64);
        let (endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        let referenced = set(&["demo/web"]);
        notify_slice(
            &referenced,
            &slice(Some("demo"), Some("web")),
            &tx,
            &endpoint_tx,
        );
        assert_eq!(endpoint_rx.try_recv(), Ok(()));
        assert!(rx.try_recv().is_err());
        notify_slice(
            &referenced,
            &slice(Some("other"), Some("web")),
            &tx,
            &endpoint_tx,
        );
        assert!(endpoint_rx.try_recv().is_err());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn empty_reference_index_keeps_the_ordinary_debounce() {
        let (tx, mut rx) = mpsc::channel(64);
        let (endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        notify_slice(
            &BTreeSet::new(),
            &slice(Some("demo"), Some("web")),
            &tx,
            &endpoint_tx,
        );
        assert_eq!(rx.try_recv(), Ok(()));
        assert!(endpoint_rx.try_recv().is_err());
    }

    #[test]
    fn endpoint_relists_wake_only_after_publishing_the_complete_cache() {
        let (store, mut writer) = reflector::store();
        let (tx, mut rx) = mpsc::channel(64);
        let (endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        let referenced = set(&["demo/web"]);
        let old = slice(Some("demo"), Some("web"));
        writer.apply_watcher_event(&watcher::Event::Apply(old));
        let mut replacement = slice(Some("demo"), Some("web"));
        replacement.metadata.name = Some("slice-2".into());

        for event in [watcher::Event::Init, watcher::Event::InitApply(replacement)] {
            writer.apply_watcher_event(&event);
            notify_slice_event(&referenced, &event, &tx, &endpoint_tx);
            assert_eq!(store.state()[0].metadata.name.as_deref(), Some("slice-1"));
            assert!(rx.try_recv().is_err());
            assert!(endpoint_rx.try_recv().is_err());
        }
        writer.apply_watcher_event(&watcher::Event::InitDone);
        notify_slice_event(&referenced, &watcher::Event::InitDone, &tx, &endpoint_tx);
        assert_eq!(store.state()[0].metadata.name.as_deref(), Some("slice-2"));
        assert_eq!(endpoint_rx.try_recv(), Ok(()));
        assert!(rx.try_recv().is_err());

        // An empty relist must also wake the loop: otherwise the old backend
        // would remain programmed despite its absence from the fresh cache.
        for event in [watcher::Event::Init, watcher::Event::InitDone] {
            writer.apply_watcher_event(&event);
            notify_slice_event(&referenced, &event, &tx, &endpoint_tx);
        }
        assert!(store.state().is_empty());
        assert_eq!(endpoint_rx.try_recv(), Ok(()));
        assert!(endpoint_rx.try_recv().is_err());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ops_config_is_bounded_to_seconds() {
        // The default kube read timeout (~295s) exists for long-lived watches;
        // a one-shot call must never be able to hold the loop that long.
        let mut cfg = kube::Config::new("http://localhost:8080".parse().unwrap());
        bound_ops_config(&mut cfg);
        for t in [cfg.connect_timeout, cfg.read_timeout, cfg.write_timeout] {
            assert!(t.expect("bounded") <= Duration::from_secs(30));
        }
    }

    #[tokio::test]
    async fn unwatched_store_is_trivially_ready() {
        // A disabled feature (no L4, no Gateway API) drops the writer without
        // spawning a watcher; the sync gate must not wait on (or fail for) it.
        let (store, writer) = reflector::store::<ConfigMap>();
        drop(writer);
        ready_when(false, &store)
            .await
            .expect("an unwatched store must not gate the sync");
    }

    #[tokio::test]
    async fn watched_store_gates_until_its_initial_list_lands() {
        let (store, mut writer) = reflector::store::<ConfigMap>();
        // Before the initial LIST completes, the gate must still be waiting —
        // this is exactly the window where reconciling would flap live routes.
        let waiting =
            tokio::time::timeout(Duration::from_millis(50), ready_when(true, &store)).await;
        assert!(waiting.is_err(), "gate must hold until the cache syncs");

        writer.apply_watcher_event(&watcher::Event::Init);
        writer.apply_watcher_event(&watcher::Event::InitDone);
        ready_when(true, &store)
            .await
            .expect("gate must open once the initial LIST is applied");
    }

    #[tokio::test]
    async fn watched_store_with_dropped_writer_fails_fast() {
        // If a watched store's writer is gone the gate must error (fail fast),
        // never report ready.
        let (store, writer) = reflector::store::<ConfigMap>();
        drop(writer);
        assert!(ready_when(true, &store).await.is_err());
    }

    #[test]
    fn only_a_404_reads_as_crds_absent() {
        use kube::core::Status;

        // What the apiserver returns when the CRD's group/kind is not served.
        let not_found = kube::Error::Api(
            Status::failure(
                "the server could not find the requested resource",
                "NotFound",
            )
            .with_code(404)
            .boxed(),
        );
        assert!(gateway_crds_absent(&not_found));

        // Managed clusters fronting the apiserver with an HTTP router answer
        // an unserved group's path with a plain-text `404 page not found`;
        // kube-client can't parse that body into a typed Status and
        // synthesizes this parse-failure reason. Still a 404 on a collection
        // list, so still "absent" — matching on the reason alone crash-looped
        // the controller on such clusters.
        let unparsed_404 = kube::Error::Api(
            Status::failure("404 page not found\n", "Failed to parse error data")
                .with_code(404)
                .boxed(),
        );
        assert!(gateway_crds_absent(&unparsed_404));

        // RBAC not yet propagated: a transient failure, never "absent".
        let forbidden = kube::Error::Api(
            Status::failure("gatewayclasses is forbidden", "Forbidden")
                .with_code(403)
                .boxed(),
        );
        assert!(!gateway_crds_absent(&forbidden));

        // An apiserver hiccup must fail fast, not lock in Ingress-only mode.
        let unavailable = kube::Error::Api(
            Status::failure("etcdserver: request timed out", "InternalError")
                .with_code(500)
                .boxed(),
        );
        assert!(!gateway_crds_absent(&unavailable));
    }

    #[test]
    fn any_missing_crd_forces_ingress_only_mode() {
        // Full install: gateway mode.
        assert!(missing_gateway_crds([true, true, true, true]).is_empty());
        // Partial install (e.g. the standard channel applied without
        // ReferenceGrant): Ingress-only, naming exactly the missing kind —
        // gateway mode would watch it, never sync, and crash-loop at the
        // cache gate.
        assert_eq!(
            missing_gateway_crds([true, true, true, false]),
            vec!["ReferenceGrant"]
        );
        assert_eq!(
            missing_gateway_crds([true, false, true, false]),
            vec!["Gateway", "ReferenceGrant"]
        );
        // Nothing installed: the ordinary Ingress-only cluster.
        assert_eq!(
            missing_gateway_crds([false, false, false, false]),
            REQUIRED_GATEWAY_API_KINDS.to_vec()
        );
    }

    /// The layer-4 kinds are optional, the rest is not. A cluster with the
    /// core installed and no TCPRoute/UDPRoute must keep serving the Gateway
    /// API — reading it as "Gateway API not installed" would take HTTPRoute
    /// down with it.
    #[test]
    fn layer4_kinds_are_optional_and_the_core_is_not() {
        let core_only = GatewayApiSupport {
            core: true,
            tcp_routes: false,
            udp_routes: false,
        };
        assert!(core_only.core, "core stays enabled without the L4 kinds");
        assert_ne!(
            core_only,
            GatewayApiSupport::full(),
            "a re-probe must still have something to discover"
        );
        assert_eq!(
            GatewayApiSupport::default(),
            GatewayApiSupport {
                core: false,
                tcp_routes: false,
                udp_routes: false
            },
            "the Ingress-only default watches nothing"
        );
    }

    #[test]
    fn gateway_status_writes_default_on_and_are_disablable() {
        // Conditions are the Gateway API's UX: the default must stay on. The
        // explicit off-switch exists for deployments without the */status
        // RBAC grants.
        let args = Args::parse_from(["sozu-gw-controller"]);
        assert!(args.gateway_status_writes);
        let args = Args::parse_from(["sozu-gw-controller", "--gateway-status-writes", "false"]);
        assert!(!args.gateway_status_writes);
    }

    #[test]
    fn watch_timeout_defaults_to_60_and_explicit_zero_opts_out() {
        // The measured bound must hold for an install whose values predate the
        // chart key: the default is the binary's, and the chart only mirrors
        // it. `0` stays the documented opt-out, in both the worker and the
        // provisioner, which used to silently map it to 60.
        let args = Args::parse_from(["sozu-gw-controller"]);
        assert_eq!(args.watch_timeout_secs, 60);
        assert_eq!(watch_config(args.watch_timeout_secs).timeout, Some(60));
        let args = Args::parse_from(["sozu-gw-controller", "--watch-timeout-secs", "0"]);
        assert_eq!(args.watch_timeout_secs, 0);
        assert_eq!(watch_config(args.watch_timeout_secs).timeout, None);
    }

    #[test]
    fn watch_timeout_at_or_past_the_kube_limit_is_refused_at_startup() {
        // kube-core rejects `timeoutSeconds >= 295` on every watch start, after
        // the LIST has filled the cache — a silent freeze, not a crash. Refusing
        // it here is what turns it into one.
        for ok in [0, 60, 294] {
            assert!(validate_watch_timeout(ok).is_ok(), "{ok} must be accepted");
        }
        for refused in [295, 600] {
            let err = validate_watch_timeout(refused).unwrap_err().to_string();
            assert!(
                err.contains("295") && err.contains(&refused.to_string()),
                "{refused} must be refused naming the limit, got: {err}"
            );
        }
    }

    #[test]
    fn zero_resync_secs_disables_the_periodic_resync() {
        // 0 must read as "disabled", never reach tokio's zero-interval panic.
        assert_eq!(optional_period(0), None);
        assert_eq!(optional_period(60), Some(Duration::from_secs(60)));
    }

    #[test]
    fn sozu_probe_defaults_to_two_seconds_and_zero_disables_it() {
        // The default bounds how long a restarted-empty Sōzu stays in the
        // Service; an explicit 0 must read as "disabled", never as a
        // zero-interval panic.
        let args = Args::parse_from(["sozu-gw-controller"]);
        assert_eq!(args.sozu_probe_secs, 2);
        assert_eq!(
            optional_period(args.sozu_probe_secs),
            Some(Duration::from_secs(2))
        );
        let args = Args::parse_from(["sozu-gw-controller", "--sozu-probe-secs", "0"]);
        assert_eq!(optional_period(args.sozu_probe_secs), None);
        let args = Args::parse_from(["sozu-gw-controller", "--sozu-probe-secs", "7"]);
        assert_eq!(
            optional_period(args.sozu_probe_secs),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn readiness_drops_on_a_reset_and_returns_after_a_successful_reconcile() {
        use shadow::GenerationCheck::{ProbeFailed, Reset, Unchanged};
        let ready = AtomicBool::new(false);
        // Before the first reconcile nothing moves it: a reset with nothing
        // applied is not a transition, and a failed probe decides nothing.
        readiness_after_check(&ready, Reset);
        readiness_after_check(&ready, ProbeFailed);
        assert!(!ready.load(Ordering::Relaxed));
        // The first successful reconcile latches it.
        mark_ready(&ready);
        assert!(ready.load(Ordering::Relaxed));
        // Unchanged keeps it; a transient probe failure must not pull a
        // still-programmed Pod out of the Service.
        readiness_after_check(&ready, Unchanged);
        readiness_after_check(&ready, ProbeFailed);
        assert!(ready.load(Ordering::Relaxed));
        // Sōzu came back empty: not ready until the re-apply lands ...
        readiness_after_check(&ready, Reset);
        assert!(!ready.load(Ordering::Relaxed));
        // ... which is the next successful reconcile.
        mark_ready(&ready);
        assert!(ready.load(Ordering::Relaxed));
    }

    #[test]
    fn a_liveness_tick_reconciles_only_after_a_reset_or_for_pending_work() {
        use shadow::GenerationCheck::{ProbeFailed, Reset, Unchanged};
        // Nothing changed, or Sōzu unreachable, and nothing pending: the tick
        // must not become a full rebuild every period.
        assert!(!reconciles(Wakeup::SozuProbe, Some(Unchanged), false));
        assert!(!reconciles(Wakeup::SozuProbe, Some(ProbeFailed), false));
        // A reset is acted on: that reconcile is the re-apply.
        assert!(reconciles(Wakeup::SozuProbe, Some(Reset), false));
        // A previous reconcile failed (a transient socket error mid-apply):
        // Sōzu answering again on the same generation is the retry moment —
        // otherwise, with the resync disabled, the desired state waits for an
        // unrelated Kubernetes event that may never come.
        assert!(reconciles(Wakeup::SozuProbe, Some(Unchanged), true));
        // ...but not while Sōzu is still unreachable: the apply would fail
        // the same way, and the next tick retries.
        assert!(!reconciles(Wakeup::SozuProbe, Some(ProbeFailed), true));
        // Every other wakeup reconciles regardless of the check, as before.
        assert!(reconciles(Wakeup::Change, None, false));
        assert!(reconciles(Wakeup::Change, Some(ProbeFailed), false));
        assert!(reconciles(Wakeup::Resync, Some(Unchanged), false));
        assert!(reconciles(Wakeup::Resync, Some(ProbeFailed), false));
        // The self-scheduled retry exists to reconcile: it always does, even
        // when its generation check could not reach Sōzu (the apply fails
        // fast and schedules the next retry).
        assert!(reconciles(Wakeup::Retry, Some(Unchanged), true));
        assert!(reconciles(Wakeup::Retry, Some(ProbeFailed), true));
    }

    #[test]
    fn a_failed_probe_is_nudged_only_without_the_liveness_tick() {
        use shadow::GenerationCheck::{ProbeFailed, Reset, Unchanged};
        // With the tick, the retry is its job; a nudge per failure would run
        // a full rebuild per failure while Sōzu is down.
        assert!(!retry_by_nudge(ProbeFailed, true));
        // Without it, the nudge is the only retry path on a quiet cluster
        // with resync disabled.
        assert!(retry_by_nudge(ProbeFailed, false));
        for outcome in [Unchanged, Reset] {
            assert!(!retry_by_nudge(outcome, true));
            assert!(!retry_by_nudge(outcome, false));
        }
    }

    #[test]
    fn a_failed_reconcile_schedules_its_own_retry_only_with_both_ticks_off() {
        // Either tick retries pending work on its own; a nudge on top would
        // run the rebuild twice per failure.
        assert!(!retry_reconcile_by_nudge(true, true));
        assert!(!retry_reconcile_by_nudge(true, false));
        assert!(!retry_reconcile_by_nudge(false, true));
        // With both off, nothing else ever retries on a quiet cluster: the
        // failed apply would stay unapplied until an unrelated watch event.
        assert!(retry_reconcile_by_nudge(false, false));
    }

    #[tokio::test(start_paused = true)]
    async fn a_scheduled_retry_fires_after_the_delay_not_before_and_never_unscheduled() {
        let mut retry = Some(scheduled_retry());
        // Paused time: the arm must not complete until the delay has elapsed…
        let early = tokio::time::timeout(RECONCILE_RETRY_DELAY / 2, maybe_retry(retry.as_mut()));
        assert!(early.await.is_err(), "the retry must not fire early");
        // …and must once it has. `timeout` auto-advances the paused clock.
        let due = tokio::time::timeout(RECONCILE_RETRY_DELAY, maybe_retry(retry.as_mut()));
        assert!(due.await.is_ok(), "the retry must fire after the delay");
        // With none scheduled the arm pends forever, like a disabled tick.
        let idle = tokio::time::timeout(Duration::from_secs(60), maybe_retry(None));
        assert!(idle.await.is_err(), "no retry scheduled must never fire");
    }

    #[tokio::test]
    async fn disabled_resync_arm_never_fires() {
        let fired = tokio::time::timeout(Duration::from_millis(50), maybe_tick(None)).await;
        assert!(fired.is_err(), "a disabled resync must pend forever");
    }

    #[tokio::test(start_paused = true)]
    async fn resync_interval_first_tick_lands_one_period_after_startup() {
        let mut interval = delayed_interval(Duration::from_secs(60));
        // A raw `interval()` would tick immediately, re-running a redundant
        // reconcile right after the initial one.
        let early =
            tokio::time::timeout(Duration::from_secs(1), maybe_tick(Some(&mut interval))).await;
        assert!(early.is_err(), "first tick must not complete immediately");
        // ... but it must still fire once a full period has elapsed.
        let due =
            tokio::time::timeout(Duration::from_secs(120), maybe_tick(Some(&mut interval))).await;
        assert!(due.is_ok(), "the interval must tick after one period");
    }
}
