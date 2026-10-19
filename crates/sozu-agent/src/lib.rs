//! sozu-agent: thin, typed wrapper around `sozu-command-lib`'s command socket.
//!
//! Owns all socket I/O: connect, send a batch request-by-request (each acked
//! through Sōzu's `Processing → Ok/Failure` reply sequence), bounded reads
//! *and writes* (no permanent hang), and reconnect-and-retry on a broken
//! channel. The requests themselves are NOT all idempotent: convergence
//! against a drifted Sōzu comes from the upsert semantics of
//! `AddCluster`/`AddBackend` plus this crate's tolerance for already-gone
//! teardowns, its repair of duplicate frontend adds, which evicts the
//! entry Sōzu actually stores under the key and re-adds, and its re-add of a
//! replacement certificate whose listener Sōzu holds no certificate for
//! (see [`SozuAgent::apply`]).
//!
//! Two layers:
//!  - [`SozuAgent`] — the synchronous core (the command socket is a blocking,
//!    single-stream protocol; this type owns it).
//!  - [`SozuAgentHandle`] — an async, cloneable handle. It runs the blocking
//!    core on a dedicated thread and serialises all access through an mpsc
//!    queue, so concurrent async callers never share the socket unsafely.
#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use sozu_command_lib::channel::Channel;
use sozu_command_lib::proto::command::{
    request::RequestType, response_content::ContentType, AddCertificate, FrontendFilters,
    ListedFrontends, ReplaceCertificate, RequestTcpFrontend, Response, ResponseContent,
    ResponseStatus, RunState, Status,
};
use thiserror::Error;
use tokio::sync::oneshot;
use tracing::{debug, warn};

// Re-exported so callers (the controller) can drive `query_metrics`, and name
// the batch they apply, without taking a direct dependency on
// `sozu-command-lib`.
pub use sozu_command_lib::proto::command::{AggregatedMetrics, QueryMetricsOptions, Request};

/// Client-side socket buffer sizes (server bounds come from `config.toml`).
const DEFAULT_BUFFER_SIZE: u64 = 1024 * 1024;
const DEFAULT_MAX_BUFFER_SIZE: u64 = 16 * 1024 * 1024;
/// Upper bound on a whole request's ack sequence, so a wedged Sōzu can't hang
/// us forever (applies across the Processing→Ok replies, not per read).
///
/// Sized so that one request's usual worst case — a read timeout, the
/// reconnect backoff, and a second read timeout on the retry — stays under the
/// controller's 60 s apply deadline: the controller must see the agent's own
/// error, not its timeout, or the real cause is lost and the job runs on
/// after the caller gave up. A Sōzu ack is normally milliseconds.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(20);
/// `SO_SNDTIMEO` on the command socket, so a Sōzu that keeps the socket open
/// but stops reading can't block the worker thread in the kernel forever once
/// the socket buffer fills (same order of magnitude as the read deadline).
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(20);
/// Small backoff before a reconnect-and-retry, so an unhealthy Sōzu is not
/// hammered with reconnect storms across reconcile cycles.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(200);

/// Whether a request is a teardown verb (`Remove*` / `DeactivateListener`).
/// Sōzu rejects removing (or deactivating) an object it no longer holds, so a
/// failed teardown *of that kind* is treated as an already-done no-op rather
/// than wedging reconciliation (see [`is_already_gone`] and [`SozuAgent::apply`]).
fn is_teardown(request: &Request) -> bool {
    matches!(
        &request.request_type,
        Some(
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
    )
}

/// Whether a failed request is an add Sōzu rejected because it already holds
/// an object under the same key (`StateError::Exists`).
///
/// Sōzu's frontend and L4-listener `Add*` verbs are NOT idempotent (unlike
/// `AddCluster`/`AddBackend`, which upsert, and `AddCertificate`, which skips a
/// cert it already holds): re-applying one returns `StateError::Exists`
/// ("… already exists") — a hard `Failure` on the wire. That happens whenever
/// Sōzu applied a batch our shadow never recorded (an ack that arrived after
/// the read deadline, a partially applied batch, a controller restart
/// mid-apply). Failing the batch would re-emit the same add every reconcile,
/// forever. What happens instead depends on the verb (see
/// [`SozuAgent::apply`]): frontend adds are *repaired* by evicting whatever
/// holds the key and re-adding (see [`stored_removal`] for why tolerance is
/// not enough), listener adds are tolerated as already-applied.
///
/// Scoped to the verbs where `Exists` is the expected duplicate outcome, and
/// to failure messages that actually carry `StateError::Exists`'s Display,
/// matched case-insensitively because the main process wraps it in its own
/// error prefix. The match is on "already exist" alone, which is the part the
/// wording has kept across versions: Sōzu 2.2.0 says "{kind:?} '{id}' already
/// exists", 2.2.1 says "{kind:?} with id_bytes={n} already exists; …". Both
/// shapes are covered by the tests, because the proxy image and the linked
/// library move independently. A UDP address held by *another* cluster's
/// frontend is refused with its own wording since Sōzu 2.3.0 ("UDP frontend
/// address … is already claimed by another frontend"), which is the same
/// stale-state duplicate and takes the same repair. `ActivateListener` is
/// *not* here: re-activation answers `Ok` (it only fails `NotFound`), and is
/// harmless since Sōzu 2.3.0 (7109e20a) — on 2.2.1, re-activating an active UDP
/// listener cut the replies of its established flows (measured).
fn is_duplicate_add(request: &Request, failure_message: &str) -> bool {
    let message = failure_message.to_lowercase();
    match &request.request_type {
        Some(RequestType::AddUdpFrontend(_)) => {
            message.contains("already exist")
                || message.contains("is already claimed by another frontend")
        }
        Some(
            RequestType::AddHttpFrontend(_)
            | RequestType::AddHttpsFrontend(_)
            | RequestType::AddTcpFrontend(_)
            | RequestType::AddTcpListener(_)
            | RequestType::AddUdpListener(_),
        ) => message.contains("already exist"),
        _ => false,
    }
}

/// The per-worker outcomes Sōzu's main process folds into one failure message:
/// `"<worker id>: <message>"` joined by `", "`, with the workers that succeeded
/// contributing `"<id>: OK"` (verified live: `1: could not add certificate: …,
/// 0: could not add certificate: …`). A message without that shape — a failure
/// raised by the main process before dispatch — is one segment. Split them back
/// out so that one worker's benign message can never hide another's real
/// failure.
fn worker_segments(message: &str) -> Vec<&str> {
    let bytes = message.as_bytes();
    let mut starts = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let at_boundary = i == 0 || (i >= 2 && &bytes[i - 2..i] == b", ");
        if at_boundary && bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if bytes[j..].starts_with(b": ") {
                starts.push((i, j + 2));
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    if starts.is_empty() {
        return vec![message];
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &(start, body)) in starts.iter().enumerate() {
        let end = starts
            .get(n + 1)
            .map(|&(next, _)| next.saturating_sub(2))
            .unwrap_or(bytes.len());
        let _ = start;
        out.push(message[body..end].trim());
    }
    out
}

/// Is a failed teardown really "already gone" — Sōzu (main process or every
/// worker) saying it no longer holds the object — rather than a removal that
/// genuinely did not happen? Only the former may be skipped: skipping the
/// latter would advance the shadow over an object Sōzu still serves.
///
/// The message families come from Sōzu itself (2.2.1, and 2.3.2 where it
/// changed the wording). The main process answers `Did not find …` / `did not
/// bring any change` for any teardown verb before dispatching; a 2.3 worker
/// refusing a frontend removal its own state cannot match says `Could not
/// execute order on config state: Did not find …`. A worker whose state
/// diverged from the main process answers `found no listener` (`… with
/// address X` on 2.2, `… at X` on 2.3) for any object addressed through a listener that is
/// gone (a certificate, a frontend, the listener itself — the main process
/// keeps accepting a valid-fingerprint certificate removal, so this one must
/// not fail the batch or it fails it forever), and per kind: listeners `the
/// listener is not activated` / `no TCP|UDP listener to remove`, HTTP routes
/// `Could not remove route`. A worker that could not deactivate a listener
/// wraps the cause in `Couldn't deactivate … listener at address …: …`, so
/// the substrings are searched anywhere in the segment. Anything else — a
/// parse error, a wrong fingerprint, a lock failure — is a real failure.
///
/// An aggregate whose every segment is `OK` is *not* benign: the main process
/// only folds the responses it received and answers `Failure` when a worker
/// timed out, so all-`OK` under a `Failure` status means a worker never
/// answered, not that the object is gone.
///
/// Settling such a teardown keeps reconciliation going; it does not always
/// prove the main process dropped the object (see [`gone_from_main_process`]).
fn is_already_gone(request: &Request, failure_message: &str) -> bool {
    if !is_teardown(request) {
        return false;
    }
    let kind_specific: &[&str] = match &request.request_type {
        Some(RequestType::RemoveListener(_)) | Some(RequestType::DeactivateListener(_)) => &[
            "the listener is not activated",
            "no tcp listener to remove",
            "no udp listener to remove",
        ],
        Some(RequestType::RemoveHttpFrontend(_)) | Some(RequestType::RemoveHttpsFrontend(_)) => {
            &["could not remove route"]
        }
        _ => &[],
    };
    let mut saw_gone = false;
    let all_benign = worker_segments(failure_message).iter().all(|segment| {
        let m = segment.to_ascii_lowercase();
        if m == "ok" {
            return true;
        }
        let gone = m.contains("did not find")
            || m.contains("did not bring any change")
            || m.contains("found no listener")
            || kind_specific.iter().any(|needle| m.contains(needle));
        saw_gone |= gone;
        gone
    });
    all_benign && saw_gone
}

/// Does a teardown refusal that [`is_already_gone`] accepts also prove that
/// Sōzu's main process does not hold the object? It does when the main
/// process refused the request itself (its own prefix: it found nothing to
/// remove and sent the request to no worker), or when a worker answered
/// `OK`: the main process applied the removal before fanning it out, and a
/// worker took it, so it is kept.
///
/// Otherwise every worker refused it, and the main process may hold the
/// object anyway: since Sōzu 2.3.0 it reverts its own state when no worker
/// acknowledges a change (`should_rollback_fanout`, the inverse of a
/// frontend removal snapshotted by `ConfigState::rollback_for`), and a worker
/// it forks later is fed that state. 2.2.1 keeps the removal, but the two
/// cannot be told apart from the answer, so such a settlement is not proof.
fn gone_from_main_process(failure_message: &str) -> bool {
    failure_message
        .to_ascii_lowercase()
        .starts_with("could not apply request on the main process state:")
        || worker_segments(failure_message)
            .iter()
            .any(|segment| segment.eq_ignore_ascii_case("ok"))
}

/// Was this `ReplaceCertificate` refused because Sōzu's main process holds no
/// certificate at all at its listener?
///
/// Since Sōzu 2.3.0 (6dd569ee, sozu#1405) removing the last certificate at an
/// address drops that address from the main process's certificate map
/// (`ConfigState::remove_certificate`), and a replace naming it is refused
/// before the old fingerprint is even looked up
/// (`ConfigState::replace_certificate`: `StateError::NotFound`, "Did not find
/// Certificate with address or id_bytes=…", which the main process prefixes
/// with "could not apply request on the main process state: "). 2.2.1 kept the
/// emptied entry and inserted the replacement. A replace on a listener that
/// holds *some* certificate, just not the old one, is still accepted on 2.3,
/// so this refusal means exactly "no certificate here at all". That is the
/// state a lagging shadow leaves: the tier-9 `RemoveCertificate` of a batch
/// landed, then the batch failed, its ack was lost or the controller died,
/// and the next desired state pairs a certificate with the same names at
/// that listener as a rotation of the one Sōzu already dropped (or renames
/// that very certificate, also sent as a replace).
///
/// Only the main process's own refusal qualifies — its prefix, nothing else
/// in the message: it refuses before dispatching to any worker, so nothing of
/// the replace was applied anywhere. No worker answers a replace with this text
/// in 2.2.1 or 2.3.2 (the worker's resolver never fails on an absent old
/// fingerprint), and requiring the prefix keeps a single worker's aggregate,
/// which `worker_segments` also reduces to one segment, out regardless. The
/// refusal proves the main process holds no certificate there; the workers
/// follow it unless a removal failed on one of them, which no path on a
/// surviving worker is known to do. The NotFound Display and the prefix are the
/// same in 2.2.1 and 2.3.2; the match is case-insensitive like the others here.
fn is_replace_into_empty_listener(request: &Request, failure_message: &str) -> bool {
    if !matches!(
        request.request_type,
        Some(RequestType::ReplaceCertificate(_))
    ) {
        return false;
    }
    let message = failure_message.to_ascii_lowercase();
    message.starts_with("could not apply request on the main process state:")
        && message.contains("did not find certificate")
}

/// The `AddCertificate` that lands a replacement on a listener holding no
/// certificate: the same address, certificate, chain, key, versions, names and
/// expiry override. With no old certificate there, adding the new one is the
/// whole of what the replace meant.
fn addition_for(replace: &ReplaceCertificate) -> Request {
    RequestType::AddCertificate(AddCertificate {
        address: replace.address,
        certificate: replace.new_certificate.clone(),
        expired_at: replace.new_expired_at,
    })
    .into()
}

/// The `Remove*` built from a frontend add's own payload: the repair for a
/// duplicate add when the stored entry cannot be looked up. It evicts an
/// identical stale entry only — see [`stored_removal`]. `None` for listener
/// adds, which are tolerated instead: a listener is keyed by its address
/// alone, so a duplicate cannot mask a different routing target, and
/// repairing one would need deactivate-before-remove ordering.
fn removal_for(request: &Request) -> Option<Request> {
    match &request.request_type {
        Some(RequestType::AddHttpFrontend(front)) => {
            Some(RequestType::RemoveHttpFrontend(front.clone()).into())
        }
        Some(RequestType::AddHttpsFrontend(front)) => {
            Some(RequestType::RemoveHttpsFrontend(front.clone()).into())
        }
        Some(RequestType::AddTcpFrontend(front)) => {
            Some(RequestType::RemoveTcpFrontend(front.clone()).into())
        }
        Some(RequestType::AddUdpFrontend(front)) => {
            Some(RequestType::RemoveUdpFrontend(front.clone()).into())
        }
        _ => None,
    }
}

/// The `ListFrontends` query that lists whatever Sōzu stores under the same
/// key as this frontend add or remove: the route's hostname for HTTP/HTTPS
/// (a substring filter, so [`stored_removal`] compares whole keys), every
/// layer-4 frontend for TCP/UDP (the listing has no UDP flag, and a `domain`
/// filter would drop them).
fn owner_query(request: &Request) -> Option<Request> {
    let filters = match &request.request_type {
        Some(RequestType::AddHttpFrontend(front) | RequestType::RemoveHttpFrontend(front)) => {
            FrontendFilters {
                http: true,
                https: false,
                tcp: false,
                domain: Some(front.hostname.clone()),
                cluster_id: None,
            }
        }
        Some(RequestType::AddHttpsFrontend(front) | RequestType::RemoveHttpsFrontend(front)) => {
            FrontendFilters {
                http: false,
                https: true,
                tcp: false,
                domain: Some(front.hostname.clone()),
                cluster_id: None,
            }
        }
        Some(
            RequestType::AddTcpFrontend(_)
            | RequestType::RemoveTcpFrontend(_)
            | RequestType::AddUdpFrontend(_)
            | RequestType::RemoveUdpFrontend(_),
        ) => FrontendFilters {
            http: false,
            https: false,
            tcp: true,
            domain: None,
            cluster_id: None,
        },
        _ => return None,
    };
    Some(RequestType::ListFrontends(filters).into())
}

/// The removal of the entry Sōzu stores under this frontend request's key,
/// built from the stored entry itself so that it names the cluster which
/// actually holds the key. `None` when the listing holds no such entry.
///
/// The key is the route key for HTTP/HTTPS (`address;hostname;path[;method]`,
/// `RequestHttpFrontend`'s `Display`, which Sōzu 2.3 extends with a frontend's
/// header predicates — none of ours carries any), the listen address — plus
/// SNI and ALPN — for TCP, the address alone for UDP (Sōzu 2.2 admits a second
/// UDP frontend on an address instead of refusing it, so there the first one
/// listed is evicted). None of them includes the cluster, so
/// when the shadow missed an applied batch, the stored entry can point at a
/// cluster the shadow never knew. A removal built from the shadow's or the
/// desired payload then names the wrong cluster, and Sōzu only honours a
/// removal that names the stored one: always for TCP/UDP, whose frontends are
/// filed per cluster, and for HTTP/HTTPS since 2.3.0 (`remove_frontend_from`:
/// a key freed by one cluster may be taken by another, and a late removal for
/// the first must not evict the second). Such an add would earn `Exists`
/// forever, and such a removal `NotFound` while the route keeps serving.
fn stored_removal(request: &Request, listed: ListedFrontends) -> Option<Request> {
    match &request.request_type {
        Some(RequestType::AddHttpFrontend(front) | RequestType::RemoveHttpFrontend(front)) => {
            let key = front.to_string();
            listed
                .http_frontends
                .into_iter()
                .find(|stored| stored.to_string() == key)
                .map(|stored| RequestType::RemoveHttpFrontend(stored).into())
        }
        Some(RequestType::AddHttpsFrontend(front) | RequestType::RemoveHttpsFrontend(front)) => {
            let key = front.to_string();
            listed
                .https_frontends
                .into_iter()
                .find(|stored| stored.to_string() == key)
                .map(|stored| RequestType::RemoveHttpsFrontend(stored).into())
        }
        Some(RequestType::AddTcpFrontend(front) | RequestType::RemoveTcpFrontend(front)) => listed
            .tcp_frontends
            .into_iter()
            .find(|stored| stored.address == front.address && tcp_claim(stored) == tcp_claim(front))
            .map(|stored| RequestType::RemoveTcpFrontend(stored).into()),
        Some(RequestType::AddUdpFrontend(front) | RequestType::RemoveUdpFrontend(front)) => listed
            .udp_frontends
            .into_iter()
            .find(|stored| stored.address == front.address)
            .map(|stored| RequestType::RemoveUdpFrontend(stored).into()),
        _ => None,
    }
}

/// What the main process holds under a frontend key once
/// [`SozuAgent::evict_stale_holder`] is done with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eviction {
    /// Another cluster's route was there, and its removal was acknowledged.
    Removed,
    /// Nothing was there.
    Nothing,
    /// A route is left there that the teardown meant to take out: the one it
    /// named, or another cluster's whose removal was refused as already gone.
    Left,
}

/// How far a batch got (see [`SozuAgent::apply_tracked`]).
#[derive(Debug, Default)]
struct Progress {
    /// Requests at the head that Sōzu acknowledged or that were settled.
    acknowledged: usize,
    /// The index of the first settled request whose effect on the main
    /// process is not established (see [`ApplyError::replayable`]).
    unproven: Option<usize>,
}

impl Progress {
    /// The request at `index` was settled without establishing what the
    /// main process holds; only the first such index is kept.
    fn unproven(&mut self, index: usize) {
        self.unproven.get_or_insert(index);
    }
}

/// What a lookup of the entry holding a frontend key found.
#[derive(Debug)]
enum Holder {
    /// The removal of the stored entry.
    Stored(Box<Request>),
    /// The listing holds nothing under the key.
    Nothing,
    /// The listing could not be read.
    Unknown,
}

/// A TCP frontend's SNI and ALPN as Sōzu files them: SNI lowercased, ALPN
/// sorted and deduplicated (`ConfigState::add_tcp_frontend`), so a stored
/// entry compares equal to any spelling of the same claim.
fn tcp_claim(front: &RequestTcpFrontend) -> (Option<String>, Vec<String>) {
    let mut alpn = front.alpn.clone();
    alpn.sort();
    alpn.dedup();
    (front.sni.as_deref().map(str::to_ascii_lowercase), alpn)
}

/// Is this the removal of a frontend? Only those can leave a route behind
/// under another cluster when Sōzu answers that it holds nothing to remove.
fn is_frontend_teardown(request: &Request) -> bool {
    matches!(
        request.request_type,
        Some(
            RequestType::RemoveHttpFrontend(_)
                | RequestType::RemoveHttpsFrontend(_)
                | RequestType::RemoveTcpFrontend(_)
                | RequestType::RemoveUdpFrontend(_)
        )
    )
}

#[derive(Debug, Error)]
pub enum SozuError {
    #[error("sozu command channel error: {0}")]
    Channel(String),
    #[error("sozu rejected the request: {0}")]
    Failure(String),
    #[error("sozu returned an unexpected response (missing the expected content)")]
    UnexpectedResponse,
    #[error("sozu-agent worker thread is gone")]
    WorkerGone,
    #[error("apply abandoned by its caller after {applied} of {total} requests")]
    Abandoned { applied: usize, total: usize },
}

/// A batch that stopped part-way: how many requests at its head Sōzu
/// acknowledged, and why it stopped.
///
/// A request counts when Sōzu answered it `Ok`, or when this crate settled
/// its failure: a teardown of an object Sōzu no longer holds, a repaired
/// duplicate frontend add, a duplicate listener add, a replacement added
/// instead (see [`SozuAgent::apply`]). The request the batch stopped on never
/// counts, whatever it left behind, because its effects are not reliably
/// established. A refusal can still have changed something: Sōzu 2.2.1's main
/// process drops a `ReplaceCertificate`'s old certificate before it parses
/// the new one, and refuses the request when that parse fails. A frontend add
/// can fail after its repair evicted the stored entry. A reply that never
/// came (a read timeout, a broken channel) says nothing either way. So the
/// first `applied` requests are exactly the ones Sōzu acknowledged.
///
/// A settled teardown does not always establish what the main process
/// holds: when every worker refused it, the main process may have reverted
/// it (see [`gone_from_main_process`]), and an eviction this crate gave up on
/// leaves a route in the main process. `replayable` stops before the first
/// such request, so a caller that replays the first `replayable` requests
/// onto its baseline records nothing the main process may not hold.
#[derive(Debug, Error)]
#[error("{cause} (after sozu acknowledged {applied} of the batch's requests)")]
pub struct ApplyError {
    /// Requests at the head of the batch that Sōzu acknowledged.
    pub applied: usize,
    /// The head of those whose effect on the main process is established:
    /// `applied`, cut before the first settlement that does not prove it.
    pub replayable: usize,
    /// Why the batch stopped.
    pub cause: SozuError,
}

/// Identity of the command socket in the shared filesystem. Container restarts
/// recreate this socket even when their new PID namespace reuses worker PIDs.
/// The change timestamp also distinguishes a reused device/inode pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SocketIdentity {
    pub device: u64,
    pub inode: u64,
    pub changed_secs: i64,
    pub changed_nanos: i64,
}

impl SocketIdentity {
    fn read(path: &str) -> Result<Self, SozuError> {
        let metadata = std::fs::metadata(path)
            .map_err(|e| SozuError::Channel(format!("stat command socket: {e}")))?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            changed_secs: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
        })
    }

    fn verify(&self, path: &str) -> Result<(), SozuError> {
        if *self != Self::read(path)? {
            return Err(SozuError::Channel(
                "command socket changed while connecting".into(),
            ));
        }
        Ok(())
    }
}

/// Restart identity: the command socket detects a container restart with reused
/// PIDs, while the live worker set also detects individual worker replacements.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SozuGeneration {
    pub socket: SocketIdentity,
    pub worker_pids: BTreeSet<i32>,
}

struct ConnectedChannel {
    channel: Channel<Request, Response>,
    socket: SocketIdentity,
}

/// Synchronous client for the Sōzu command socket. Reconnects lazily.
pub struct SozuAgent {
    path: String,
    buffer_size: u64,
    max_buffer_size: u64,
    read_timeout: Duration,
    write_timeout: Duration,
    channel: Option<ConnectedChannel>,
    /// Monotone count of reconnect attempts: bumped whenever an *established*
    /// channel broke and a reconnect-and-retry was attempted — a signal that
    /// the peer may have restarted and lost its state. Shared with
    /// [`SozuAgentHandle`], which exposes it as [`SozuAgentHandle::reconnect_epoch`];
    /// a counter (instead of a swap-off flag) lets the controller acknowledge
    /// a reconnect only once its restart probe *succeeded*, so a reconnect
    /// whose probe failed stays visible and is retried. Bumped even when the
    /// retry itself fails — the established connection is gone either way.
    /// The lazy first connect never bumps it.
    reconnects: Arc<AtomicU64>,
}

impl SozuAgent {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            buffer_size: DEFAULT_BUFFER_SIZE,
            max_buffer_size: DEFAULT_MAX_BUFFER_SIZE,
            read_timeout: DEFAULT_READ_TIMEOUT,
            write_timeout: DEFAULT_WRITE_TIMEOUT,
            channel: None,
            reconnects: Arc::new(AtomicU64::new(0)),
        }
    }

    fn connect(&mut self) -> Result<(), SozuError> {
        debug!(path = %self.path, "connecting to sozu command socket");
        // The stream is built by hand (rather than `Channel::from_path`) so a
        // write timeout can be set on it: `sozu-command-lib`'s blocking write
        // loop has no bound of its own, so a Sōzu that keeps the socket open
        // but stops reading would block this thread in the kernel forever once
        // the socket buffer fills. With `SO_SNDTIMEO` the write errors out
        // instead. No read timeout is set here: the channel's
        // `read_message_blocking_timeout` manages `SO_RCVTIMEO` itself on
        // every call (and resets it), so a base value would not compose.
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .map_err(|e| SozuError::Channel(format!("create socket: {e:?}")))?;
        // `SO_SNDTIMEO` must be set BEFORE connect (which is why socket2 is
        // used instead of `UnixStream::connect`, whose std API only exposes
        // the option on an already-connected stream): on Linux a blocking
        // AF_UNIX connect() waits for backlog room under the *send* timeout
        // (`unix_wait_for_peer` sleeps under `sock_sndtimeo`, infinite by
        // default), so against a wedged-but-alive Sōzu whose accept backlog
        // has filled with earlier reconnect attempts, connect() itself would
        // block the worker thread forever — the very hang this bound exists
        // to prevent, back via a different syscall.
        socket
            .set_write_timeout(Some(self.write_timeout))
            .map_err(|e| SozuError::Channel(format!("set write timeout: {e:?}")))?;
        let address = socket2::SockAddr::unix(&self.path)
            .map_err(|e| SozuError::Channel(format!("socket address: {e:?}")))?;
        let identity = SocketIdentity::read(&self.path)?;
        // A connect that times out surfaces as the same `Channel` error kind
        // as any other connect failure, so the reconnect path handles it.
        socket
            .connect(&address)
            .map_err(|e| SozuError::Channel(format!("connect: {e:?}")))?;
        // Bind the identity to this connection, not to a later Status probe:
        // the path could then name a new socket while the old peer still answers.
        // A replacement during connect is ambiguous, so discard the stream and
        // let the caller retry instead of acknowledging the wrong generation.
        identity.verify(&self.path)?;
        // socket2's direct Socket→UnixStream conversion is gated behind its
        // `all` feature; the io-safety OwnedFd round-trip is ungated, safe
        // and equivalent (both just move the fd).
        let stream = std::os::unix::net::UnixStream::from(std::os::fd::OwnedFd::from(socket));
        // `mio` streams are expected to be nonblocking; `Channel::blocking`
        // below flips the mode back for the synchronous request/reply
        // protocol (`SO_SNDTIMEO` is a socket option, it survives the flip).
        stream
            .set_nonblocking(true)
            .map_err(|e| SozuError::Channel(format!("set nonblocking: {e:?}")))?;
        let mut channel: Channel<Request, Response> = Channel::new(
            mio::net::UnixStream::from_std(stream),
            self.buffer_size,
            self.max_buffer_size,
        );
        // Blocking mode is required: a non-blocking `write_message` only buffers,
        // it does not flush to the socket.
        channel
            .blocking()
            .map_err(|e| SozuError::Channel(format!("set blocking: {e:?}")))?;
        self.channel = Some(ConnectedChannel {
            channel,
            socket: identity,
        });
        Ok(())
    }

    fn channel_mut(&mut self) -> Result<&mut Channel<Request, Response>, SozuError> {
        if self.channel.is_none() {
            self.connect()?;
        }
        self.channel
            .as_mut()
            .map(|connected| &mut connected.channel)
            .ok_or_else(|| SozuError::Channel("not connected".to_string()))
    }

    /// Send one request and await its terminal response, skipping interim
    /// `Processing` replies.
    fn send_one(
        channel: &mut Channel<Request, Response>,
        read_timeout: Duration,
        request: &Request,
    ) -> Result<Response, SozuError> {
        channel
            .write_message(request)
            .map_err(|e| SozuError::Channel(format!("write: {e:?}")))?;
        // One deadline for the whole Processing→Ok sequence.
        let deadline = Instant::now() + read_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SozuError::Channel(
                    "timed out waiting for a terminal response".to_string(),
                ));
            }
            let response = channel
                .read_message_blocking_timeout(Some(remaining))
                .map_err(|e| SozuError::Channel(format!("read: {e:?}")))?;
            let status = response.status;
            if status == ResponseStatus::Processing as i32 {
                continue;
            }
            if status == ResponseStatus::Ok as i32 {
                return Ok(response);
            }
            return Err(SozuError::Failure(response.message));
        }
    }

    /// Send one request and return its terminal `Response`; on a *channel* error
    /// (broken pipe, etc.) reconnect once and retry. Application-level `Failure`
    /// is returned without retrying.
    fn send_with_retry(&mut self, request: &Request) -> Result<Response, SozuError> {
        let read_timeout = self.read_timeout;
        let first = {
            let channel = self.channel_mut()?;
            Self::send_one(channel, read_timeout, request)
        };
        match first {
            Ok(response) => Ok(response),
            Err(failure @ SozuError::Failure(_)) => Err(failure),
            Err(channel_error) => {
                warn!(error = %channel_error, "sozu channel error, reconnecting and retrying");
                self.channel = None;
                // Bump the epoch *before* retrying: even if the retry also
                // fails, the established connection is gone and the peer may
                // have restarted with empty state.
                self.reconnects.fetch_add(1, Ordering::Relaxed);
                thread::sleep(RECONNECT_BACKOFF);
                let channel = self.channel_mut()?;
                let retry = Self::send_one(channel, read_timeout, request);
                // The protocol has no request ids (PROTOCOL.md §1): a channel
                // error on the retry — typically a read timeout — leaves the
                // reply of *this* request in flight, and keeping the connection
                // would hand that reply to the next request as its own ack.
                // Only an application-level `Failure` proves the channel is in
                // step; every other error means the connection cannot be
                // trusted again.
                if !matches!(retry, Ok(_) | Err(SozuError::Failure(_))) {
                    self.channel = None;
                }
                retry
            }
        }
    }

    /// Apply one request, discarding the response body (mutations only care about
    /// success/failure).
    fn apply_one(&mut self, request: &Request) -> Result<(), SozuError> {
        self.send_with_retry(request).map(|_| ())
    }

    /// Apply a batch of requests in order (the caller supplies a dependency-safe
    /// order, e.g. from the Translator). Stops at the first error — except a
    /// teardown of an object Sōzu says it no longer holds (see
    /// [`is_already_gone`]), which is tolerated unless the key still routes
    /// for another cluster, and a *duplicate add* (see [`is_duplicate_add`]):
    /// a duplicate frontend add is repaired by removing the stored entry and
    /// re-adding (see [`stored_removal`]), a duplicate L4 listener add is
    /// tolerated — and a certificate replace refused because Sōzu holds no
    /// certificate at that listener (see [`is_replace_into_empty_listener`]),
    /// which is applied as an add of the replacement instead.
    pub fn apply(&mut self, requests: &[Request]) -> Result<(), SozuError> {
        self.apply_counted(requests)
            .map_err(|failure| failure.cause)
    }

    /// [`Self::apply`], reporting on failure how many requests at the head of
    /// the batch Sōzu acknowledged (see [`ApplyError`]).
    pub fn apply_counted(&mut self, requests: &[Request]) -> Result<(), ApplyError> {
        self.apply_unless(requests, || false)
    }

    /// [`Self::apply_counted`], stopping between two requests once
    /// `abandoned()` turns true. The caller that bounds an apply with a
    /// deadline drops its side of the reply channel when it gives up; the
    /// batch must not keep landing on the socket after that, or Sōzu drifts
    /// from a shadow the caller already declared not applied and the next
    /// batch queues behind the orphan.
    fn apply_unless(
        &mut self,
        requests: &[Request],
        abandoned: impl Fn() -> bool,
    ) -> Result<(), ApplyError> {
        let mut progress = Progress::default();
        self.apply_tracked(requests, abandoned, &mut progress)
            .map_err(|cause| ApplyError {
                applied: progress.acknowledged,
                replayable: progress
                    .unproven
                    .map_or(progress.acknowledged, |at| at.min(progress.acknowledged)),
                cause,
            })
    }

    /// The loop behind [`Self::apply_unless`]. `progress.acknowledged`
    /// counts the requests settled so far; it moves past a request only once
    /// Sōzu acknowledged it or its failure was settled, so on an error it is
    /// the index of the request the batch stopped on.
    fn apply_tracked(
        &mut self,
        requests: &[Request],
        abandoned: impl Fn() -> bool,
        progress: &mut Progress,
    ) -> Result<(), SozuError> {
        for (applied, request) in requests.iter().enumerate() {
            if abandoned() {
                warn!(
                    applied,
                    total = requests.len(),
                    "apply abandoned by its caller; stopping before the next request"
                );
                return Err(SozuError::Abandoned {
                    applied,
                    total: requests.len(),
                });
            }
            match self.send_with_retry(request) {
                // Since Sōzu 2.3.0 a frontend removal that names a cluster the
                // main process no longer holds is answered `Ok` ("nothing to
                // remove: cluster … does not exist"), and a route another
                // cluster holds under that key is left in place: the same
                // stale owner as the refusal below, and it is evicted alike.
                Ok(response)
                    if is_frontend_teardown(request)
                        && response
                            .message
                            .to_ascii_lowercase()
                            .contains("nothing to remove") =>
                {
                    match self.evict_stale_holder(
                        request,
                        &response.message,
                        &abandoned,
                        (applied, requests.len()),
                    )? {
                        Eviction::Removed => {
                            warn!(message = %response.message, "sozu still routes this key for another cluster; removed the stored route");
                        }
                        Eviction::Nothing => {}
                        Eviction::Left => progress.unproven(applied),
                    }
                }
                Ok(_) => {}
                // Sōzu's teardown verbs are NOT idempotent: removing an object it
                // no longer holds (state diverged from our shadow — a partially
                // applied batch, or a worker-side drop) is a hard `Failure`. Such a
                // teardown is effectively already done, so treat it as a no-op.
                // This keeps the invariant that re-diffing from the shadow
                // converges: without it, one un-removable object wedges *all*
                // reconciliation forever (the same failing remove re-emitted
                // every cycle). Only the "no longer held" failures qualify: a
                // teardown Sōzu refused for another reason — a bad request, a
                // worker that could not act — leaves the object in place, and
                // skipping it would advance the shadow over a route Sōzu still
                // serves. A frontend removal answered "nothing to remove" while
                // the key may still route is the exception: the stored entry
                // can belong to a cluster the shadow never knew (see
                // [`stored_removal`]), so it is evicted as stored first.
                Err(SozuError::Failure(msg)) if is_already_gone(request, &msg) => {
                    let eviction = if is_frontend_teardown(request) {
                        self.evict_stale_holder(
                            request,
                            &msg,
                            &abandoned,
                            (applied, requests.len()),
                        )?
                    } else {
                        Eviction::Nothing
                    };
                    if eviction == Eviction::Removed {
                        warn!(error = %msg, "sozu still routes this key for another cluster; removed the stored route");
                    } else {
                        warn!(error = %msg, "sozu no longer holds the object this teardown targets; treating it as already-gone so reconciliation converges");
                    }
                    // Settled, but only the main process's own refusal or a
                    // worker that took the removal proves it holds nothing.
                    if eviction == Eviction::Left || !gone_from_main_process(&msg) {
                        progress.unproven(applied);
                    }
                }
                // The mirror image: an add Sōzu already holds (the shadow missed
                // an applied batch) fails with `Exists` on every re-application,
                // wedging reconciliation the same way. For a *frontend* this is
                // NOT proof the desired object is there — the key excludes
                // `cluster_id`/tags/filters, so the stored route may point at a
                // different cluster (see [`stored_removal`]). Repair it: evict
                // whatever holds the key, as stored, then re-send our add.
                Err(SozuError::Failure(msg)) if is_duplicate_add(request, &msg) => {
                    match removal_for(request) {
                        Some(own_payload) => {
                            let remove = match self.key_holder(request)? {
                                Holder::Stored(stored) => *stored,
                                Holder::Nothing | Holder::Unknown => own_payload,
                            };
                            // The lookup took a round-trip: a caller that gave
                            // up meanwhile must not see a route evicted. Nothing
                            // has changed yet, so stopping here is clean.
                            if abandoned() {
                                return Err(SozuError::Abandoned {
                                    applied,
                                    total: requests.len(),
                                });
                            }
                            warn!(error = %msg, "sozu already holds this frontend's key; repairing by removing the stored entry and re-adding");
                            match self.apply_one(&remove) {
                                Ok(()) => {}
                                // The stored entry vanished between the add and
                                // the removal, or could not be listed and the
                                // add's own payload did not reach it: the re-add
                                // decides either way.
                                Err(SozuError::Failure(remove_msg)) => {
                                    warn!(error = %remove_msg, "sozu rejected the repair remove; attempting the re-add anyway");
                                }
                                Err(channel_error) => return Err(channel_error),
                            }
                            // One repair attempt only: if the re-add fails — even
                            // with another `Exists` — fail the batch. The add is
                            // not counted as acknowledged, so the next reconcile
                            // sends it again.
                            self.apply_one(request)?;
                        }
                        // L4 listener adds: address-keyed, a duplicate cannot
                        // mask a different target — tolerate as already-applied
                        // (repair would need deactivate-before-remove ordering,
                        // out of scope here).
                        None => {
                            warn!(error = %msg, "sozu rejected a duplicate listener add; treating it as already-applied so reconciliation converges");
                        }
                    }
                }
                // A rotation onto a listener Sōzu holds no certificate for: the
                // old one is already gone (the shadow lags behind a removal that
                // landed), and since 2.3.0 the replace is refused for it on every
                // reconcile. Adding the replacement is the same end state. Any
                // other refusal of a replace stays fatal.
                Err(SozuError::Failure(msg)) if is_replace_into_empty_listener(request, &msg) => {
                    self.add_replacement(request, msg, &abandoned, (applied, requests.len()))?;
                }
                Err(e) => return Err(e),
            }
            progress.acknowledged = applied + 1;
        }
        Ok(())
    }

    /// After a frontend removal Sōzu answered with "nothing to remove",
    /// remove whatever still holds the key under another cluster (see
    /// [`stored_removal`]), and say what the main process is left holding
    /// under it. A listing that cannot be read fails: "could not tell" is not
    /// "nothing there", and tolerating it would advance the shadow over a
    /// route that may still serve.
    fn evict_stale_holder(
        &mut self,
        request: &Request,
        answer: &str,
        abandoned: &impl Fn() -> bool,
        (applied, total): (usize, usize),
    ) -> Result<Eviction, SozuError> {
        match self.key_holder(request)? {
            Holder::Stored(stored) if *stored != *request => {
                // The lookup took a round-trip: a caller that gave up
                // meanwhile must not see a route evicted.
                if abandoned() {
                    return Err(SozuError::Abandoned { applied, total });
                }
                match self.apply_one(&stored) {
                    Ok(()) => Ok(Eviction::Removed),
                    // Every worker refuses the removal as not routing the key
                    // either: the main process holds a route nothing serves.
                    // Sōzu 2.3 leaves one behind when every worker stayed
                    // silent past its timeout and the main process rolled
                    // the order back, and then rolls this removal back too,
                    // so retrying it can only fail the same way. Failing the
                    // batch would fail every reconcile for a route no request
                    // reaches; a Sōzu restart clears it.
                    Err(SozuError::Failure(refusal)) if is_already_gone(&stored, &refusal) => {
                        warn!(error = %refusal, "sozu's main process keeps a route for this key that no worker serves; leaving it until sozu restarts");
                        Ok(Eviction::Left)
                    }
                    Err(error) => Err(error),
                }
            }
            Holder::Unknown => Err(SozuError::Failure(format!(
                "{answer} (and the frontends holding this key could not be listed to tell \
                 whether it still routes)"
            ))),
            // The main process still stores the very route the removal
            // named: the removal was reverted (see `gone_from_main_process`).
            Holder::Stored(_) => Ok(Eviction::Left),
            Holder::Nothing => Ok(Eviction::Nothing),
        }
    }

    /// After a `ReplaceCertificate` Sōzu refused because its listener holds no
    /// certificate (see [`is_replace_into_empty_listener`]), send the
    /// replacement as an `AddCertificate` (see [`addition_for`]); the replace
    /// counts as applied when the add is. A refused add fails the batch with
    /// both answers, the replace not counted as acknowledged, so the next
    /// reconcile retries it.
    fn add_replacement(
        &mut self,
        request: &Request,
        refusal: String,
        abandoned: &impl Fn() -> bool,
        (applied, total): (usize, usize),
    ) -> Result<(), SozuError> {
        let Some(RequestType::ReplaceCertificate(replace)) = &request.request_type else {
            return Err(SozuError::Failure(refusal));
        };
        // The refused replace took a round-trip: a caller that gave up
        // meanwhile must not see a certificate land. The main process refused
        // it before any worker saw it, so stopping here is clean.
        if abandoned() {
            return Err(SozuError::Abandoned { applied, total });
        }
        warn!(error = %refusal, "sozu holds no certificate at this listener to replace; adding the replacement instead");
        match self.apply_one(&addition_for(replace)) {
            Ok(()) => Ok(()),
            Err(SozuError::Failure(add_refusal)) => Err(SozuError::Failure(format!(
                "{refusal} (and adding the replacement certificate instead failed: {add_refusal})"
            ))),
            Err(channel_error) => Err(channel_error),
        }
    }

    /// What Sōzu's main process stores under this frontend request's key, as
    /// the removal of that stored entry (see [`stored_removal`]). `Unknown`
    /// when the listing could not be read: an add falls back to its own
    /// payload, since the re-add decides anyway, but a teardown must not take
    /// it for "nothing there".
    fn key_holder(&mut self, request: &Request) -> Result<Holder, SozuError> {
        let Some(query) = owner_query(request) else {
            return Ok(Holder::Unknown);
        };
        match self.send_with_retry(&query) {
            Ok(Response {
                content:
                    Some(ResponseContent {
                        content_type: Some(ContentType::FrontendList(listed)),
                    }),
                ..
            }) => Ok(stored_removal(request, listed)
                .map_or(Holder::Nothing, |stored| Holder::Stored(Box::new(stored)))),
            Ok(_) | Err(SozuError::Failure(_)) => {
                warn!("sozu did not list the frontends holding this key");
                Ok(Holder::Unknown)
            }
            Err(channel_error) => Err(channel_error),
        }
    }

    /// Liveness check: `Status` round-trip.
    pub fn status(&mut self) -> Result<(), SozuError> {
        self.apply_one(&RequestType::Status(Status {}).into())
    }

    /// The PIDs of Sōzu's live workers. A `Status`
    /// round-trip: the response carries `ContentType::Workers(WorkerInfos)`
    /// (per-worker id, pid, run_state; verified live, see PROTOCOL.md). Any
    /// bounce of a worker changes this set, but a container restart can reuse
    /// the same PIDs in its new namespace. Use [`Self::generation`] to detect
    /// both cases, even when Sōzu already holds some state again. `Stopped`
    /// workers are excluded: they no longer serve, and Sōzu never prunes them
    /// from the list outside a main-process upgrade (read in 2.3.2), so a
    /// killed or re-forked worker would otherwise stay in the set for the main
    /// process's life — the killed one possibly as an unreaped zombie for a
    /// while (measured). A soft-stopping worker reads `Running` until it exits.
    pub fn worker_pids(&mut self) -> Result<BTreeSet<i32>, SozuError> {
        let request: Request = RequestType::Status(Status {}).into();
        let response = self.send_with_retry(&request)?;
        match response.content {
            Some(ResponseContent {
                content_type: Some(ContentType::Workers(infos)),
            }) => Ok(infos
                .vec
                .iter()
                .filter(|worker| worker.run_state != RunState::Stopped as i32)
                .map(|worker| worker.pid)
                .collect()),
            _ => Err(SozuError::UnexpectedResponse),
        }
    }

    /// Query the workers and the identity of the socket that answered. A retry
    /// may reconnect, so obtain the connection's identity after the Status
    /// succeeds. Never re-stat the path here: it may already name another peer.
    pub fn generation(&mut self) -> Result<SozuGeneration, SozuError> {
        let worker_pids = self.worker_pids()?;
        let connected = self
            .channel
            .as_ref()
            .ok_or_else(|| SozuError::Channel("not connected".into()))?;
        Ok(SozuGeneration {
            socket: connected.socket,
            worker_pids,
        })
    }

    /// Ask Sōzu to load its routing state from a file path (visible to Sōzu).
    pub fn load_state(&mut self, path: impl Into<String>) -> Result<(), SozuError> {
        self.apply_one(&RequestType::LoadState(path.into()).into())
    }

    /// Ask Sōzu to persist its current routing state to a file path.
    pub fn save_state(&mut self, path: impl Into<String>) -> Result<(), SozuError> {
        self.apply_one(&RequestType::SaveState(path.into()).into())
    }

    /// Pull Sōzu's aggregated metrics over the command socket (a `QueryMetrics`
    /// round-trip). Unlike a mutation, this keeps the response body and extracts
    /// the `AggregatedMetrics` from it.
    pub fn query_metrics(
        &mut self,
        options: QueryMetricsOptions,
    ) -> Result<AggregatedMetrics, SozuError> {
        let request: Request = RequestType::QueryMetrics(options).into();
        let response = self.send_with_retry(&request)?;
        match response.content {
            Some(ResponseContent {
                content_type: Some(ContentType::Metrics(metrics)),
            }) => Ok(metrics),
            _ => Err(SozuError::UnexpectedResponse),
        }
    }
}

// ----------------------------------------------------------------------------
// Async handle
// ----------------------------------------------------------------------------

enum Job {
    Apply(Vec<Request>, oneshot::Sender<Result<(), ApplyError>>),
    SaveState(String, oneshot::Sender<Result<(), SozuError>>),
    QueryMetrics(
        QueryMetricsOptions,
        oneshot::Sender<Result<AggregatedMetrics, SozuError>>,
    ),
    WorkerPids(oneshot::Sender<Result<BTreeSet<i32>, SozuError>>),
    Generation(oneshot::Sender<Result<SozuGeneration, SozuError>>),
}

impl Job {
    /// Whether the caller has already given up on this job (its `oneshot`
    /// receiver is dropped — e.g. a timed-out `/metrics` scrape). The worker
    /// skips such jobs entirely instead of running them against the socket.
    fn is_abandoned(&self) -> bool {
        match self {
            Job::Apply(_, reply) => reply.is_closed(),
            Job::SaveState(_, reply) => reply.is_closed(),
            Job::QueryMetrics(_, reply) => reply.is_closed(),
            Job::WorkerPids(reply) => reply.is_closed(),
            Job::Generation(reply) => reply.is_closed(),
        }
    }
}

/// Cloneable async handle to a single Sōzu command socket. All work runs on one
/// dedicated thread, so socket access is serialised across clones.
#[derive(Clone)]
pub struct SozuAgentHandle {
    tx: mpsc::Sender<Job>,
    reconnects: Arc<AtomicU64>,
}

impl SozuAgentHandle {
    /// Spawn the worker thread for the socket at `path`. The connection is
    /// established lazily on first use (so this never fails on a not-yet-ready
    /// Sōzu).
    pub fn spawn(path: impl Into<String>) -> std::io::Result<Self> {
        let path = path.into();
        let (tx, rx) = mpsc::channel::<Job>();
        let reconnects = Arc::new(AtomicU64::new(0));
        let epoch = reconnects.clone();
        thread::Builder::new()
            .name("sozu-agent".to_string())
            .spawn(move || {
                let mut agent = SozuAgent::new(path);
                agent.reconnects = epoch;
                // Ends when every `SozuAgentHandle` (and thus every Sender) drops.
                for job in rx {
                    // A job whose reply channel is already closed was abandoned
                    // by its caller (e.g. a timed-out /metrics scrape): skip it
                    // without touching the socket. Executing it anyway would
                    // let abandoned work queue ahead of routing applies and
                    // burn socket round-trips nobody will read.
                    if job.is_abandoned() {
                        debug!("skipping an abandoned sozu-agent job (caller gone)");
                        continue;
                    }
                    match job {
                        Job::Apply(requests, reply) => {
                            let result = agent.apply_unless(&requests, || reply.is_closed());
                            let _ = reply.send(result);
                        }
                        Job::SaveState(path, reply) => {
                            let _ = reply.send(agent.save_state(path));
                        }
                        Job::QueryMetrics(options, reply) => {
                            let _ = reply.send(agent.query_metrics(options));
                        }
                        Job::WorkerPids(reply) => {
                            let _ = reply.send(agent.worker_pids());
                        }
                        Job::Generation(reply) => {
                            let _ = reply.send(agent.generation());
                        }
                    }
                }
                debug!("sozu-agent worker thread exiting");
            })?;
        Ok(Self { tx, reconnects })
    }

    /// Monotone count of reconnect-and-retry attempts on the command socket
    /// (a reconnect after a channel error on an *established* connection —
    /// the signal that the peer may have restarted and lost its routing
    /// state). The controller records the value it has *acknowledged* — only
    /// after a successful restart probe — and treats any newer value as a
    /// pending reconnect to investigate. Deliberately not a swap-off flag:
    /// consuming the signal before the probe's outcome is known would lose a
    /// reconnect whose one probe errored.
    pub fn reconnect_epoch(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }

    /// Apply a batch of requests, awaiting Sōzu's acks. A failure reports how
    /// many requests at the head of the batch Sōzu acknowledged (see
    /// [`ApplyError`]); none when the worker thread is gone, since nothing
    /// then says how far it got.
    pub async fn apply(&self, requests: Vec<Request>) -> Result<(), ApplyError> {
        let worker_gone = || ApplyError {
            applied: 0,
            replayable: 0,
            cause: SozuError::WorkerGone,
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job::Apply(requests, reply_tx))
            .map_err(|_| worker_gone())?;
        reply_rx.await.map_err(|_| worker_gone())?
    }

    /// Ask Sōzu to dump its full routing state to `path` (a file both the
    /// controller and Sōzu can see via the shared volume).
    pub async fn save_state(&self, path: String) -> Result<(), SozuError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job::SaveState(path, reply_tx))
            .map_err(|_| SozuError::WorkerGone)?;
        reply_rx.await.map_err(|_| SozuError::WorkerGone)?
    }

    /// Pull Sōzu's aggregated metrics over the command socket.
    pub async fn query_metrics(
        &self,
        options: QueryMetricsOptions,
    ) -> Result<AggregatedMetrics, SozuError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job::QueryMetrics(options, reply_tx))
            .map_err(|_| SozuError::WorkerGone)?;
        reply_rx.await.map_err(|_| SozuError::WorkerGone)?
    }

    /// Fetch the PIDs of Sōzu's live workers (see
    /// [`SozuAgent::worker_pids`]).
    pub async fn worker_pids(&self) -> Result<BTreeSet<i32>, SozuError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job::WorkerPids(reply_tx))
            .map_err(|_| SozuError::WorkerGone)?;
        reply_rx.await.map_err(|_| SozuError::WorkerGone)?
    }

    /// Fetch Sōzu's socket identity and live workers in one serialised job.
    pub async fn generation(&self) -> Result<SozuGeneration, SozuError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job::Generation(reply_tx))
            .map_err(|_| SozuError::WorkerGone)?;
        reply_rx.await.map_err(|_| SozuError::WorkerGone)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};

    use sozu_command_lib::proto::command::{
        AddBackend, CertificateAndKey, Cluster, DeactivateListener, ListenerType, PathRule,
        PathRuleKind, RemoveBackend, RemoveCertificate, RequestHttpFrontend, RequestTcpFrontend,
        RequestUdpFrontend, SocketAddress, TcpListenerConfig, TlsVersion, WorkerInfo, WorkerInfos,
    };

    /// A failure message shaped like the real thing: the main process wraps
    /// `StateError::Exists`'s Display in its own prefix. This is Sōzu 2.2.0's
    /// wording, which is what the pinned proxy image still emits.
    const EXISTS_MESSAGE: &str =
        "executing request on the state: HttpFrontend 'lb.example.com;/' already exists";

    /// The same failure from Sōzu 2.2.1, which replaced the object's id with the
    /// byte length of it. The matcher has to survive the image being bumped
    /// under a controller that was linked against either version.
    const EXISTS_MESSAGE_2_2_1: &str = "executing request on the state: HttpFrontend with \
         id_bytes=17 already exists; remove it first, or apply the corresponding update if \
         the object supports one, instead of re-adding it";

    fn socket_address(port: u16) -> sozu_command_lib::proto::command::SocketAddress {
        std::net::SocketAddr::from(([127, 0, 0, 1], port)).into()
    }

    /// A unique, short socket path (unix socket paths are length-limited).
    fn temp_socket_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("sozu-gw-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// A fake Sōzu: accepts one connection on `path` and answers every request
    /// with the given terminal failure message, through the crate's own
    /// `Channel` so the length-prefixed prost framing is the real one.
    fn spawn_fake_sozu_failing_with(path: &Path, message: &str) -> thread::JoinHandle<()> {
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind fake sozu");
        let message = message.to_string();
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            // `mio` streams are expected to be nonblocking; `Channel::blocking`
            // flips the mode for the synchronous request/reply protocol.
            stream.set_nonblocking(true).expect("set nonblocking");
            let mut channel: Channel<Response, Request> =
                Channel::new(mio::net::UnixStream::from_std(stream), 16_384, 65_536);
            channel.blocking().expect("set blocking");
            // Serve until the client hangs up.
            while channel
                .read_message_blocking_timeout(Some(Duration::from_secs(5)))
                .is_ok()
            {
                channel
                    .write_message(&Response {
                        status: ResponseStatus::Failure as i32,
                        message: message.clone(),
                        content: None,
                    })
                    .expect("write response");
            }
        })
    }

    fn ok_response() -> Response {
        Response {
            status: ResponseStatus::Ok as i32,
            message: String::new(),
            content: None,
        }
    }

    fn failure_response(message: &str) -> Response {
        Response {
            status: ResponseStatus::Failure as i32,
            message: message.to_string(),
            content: None,
        }
    }

    /// A scripted fake Sōzu: accepts one connection on `path`, records every
    /// request it receives and answers each with the next response in `script`
    /// (failing loudly if the script runs dry). Returns the recorded requests
    /// when the client hangs up, so tests can assert the exact wire sequence.
    fn spawn_scripted_fake_sozu(
        path: &Path,
        script: Vec<Response>,
    ) -> thread::JoinHandle<Vec<Request>> {
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind fake sozu");
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            stream.set_nonblocking(true).expect("set nonblocking");
            let mut channel: Channel<Response, Request> =
                Channel::new(mio::net::UnixStream::from_std(stream), 16_384, 65_536);
            channel.blocking().expect("set blocking");
            let mut script = script.into_iter();
            let mut received = Vec::new();
            while let Ok(request) =
                channel.read_message_blocking_timeout(Some(Duration::from_secs(5)))
            {
                received.push(request);
                let response = script
                    .next()
                    .unwrap_or_else(|| failure_response("fake sozu script exhausted"));
                channel.write_message(&response).expect("write response");
            }
            received
        })
    }

    #[test]
    fn worker_segments_split_the_main_process_aggregate() {
        assert_eq!(
            worker_segments("Did not find Backend with address or id_bytes=3"),
            vec!["Did not find Backend with address or id_bytes=3"],
            "a main-process failure is a single segment"
        );
        assert_eq!(
            worker_segments(
                "1: could not add certificate: invalid private key, 0: could not add certificate: invalid private key"
            ),
            vec![
                "could not add certificate: invalid private key",
                "could not add certificate: invalid private key"
            ],
            "the live-observed two-worker aggregate splits into two segments"
        );
        assert_eq!(
            worker_segments("0: OK, 1: found no listener with address 0.0.0.0:5353"),
            vec!["OK", "found no listener with address 0.0.0.0:5353"],
            "a worker that succeeded contributes an `OK` segment"
        );
        assert_eq!(
            worker_segments("listener at 127.0.0.1:8080, port 8080: refused"),
            vec!["listener at 127.0.0.1:8080, port 8080: refused"],
            "a comma inside a plain message must not be mistaken for a worker boundary"
        );
    }

    #[test]
    fn already_gone_is_scoped_to_sozu_no_longer_holding_the_object() {
        let remove_backend: Request = RequestType::RemoveBackend(RemoveBackend {
            cluster_id: "c".into(),
            backend_id: "b".into(),
            address: socket_address(1),
        })
        .into();
        assert!(is_already_gone(
            &remove_backend,
            "Did not find Backend with address or id_bytes=1"
        ));
        assert!(is_already_gone(
            &remove_backend,
            "dispatching this request did not bring any change to the state"
        ));
        assert!(
            !is_already_gone(&remove_backend, "Wrong field value: address"),
            "a request Sōzu could not even interpret leaves the object in place"
        );

        let deactivate: Request = RequestType::DeactivateListener(DeactivateListener {
            address: socket_address(5353),
            proxy: ListenerType::Tcp as i32,
            to_scm: false,
            interface: None,
        })
        .into();
        assert!(is_already_gone(
            &deactivate,
            "1: Could not deactivate TCP listener at address 0.0.0.0:5353: the listener is not activated, 0: OK"
        ));
        assert!(is_already_gone(
            &deactivate,
            "0: found no listener with address 0.0.0.0:5353, 1: found no listener with address 0.0.0.0:5353"
        ));
        assert!(
            is_already_gone(
                &deactivate,
                "0: found no listener at 0.0.0.0:5353, 1: found no listener at 0.0.0.0:5353"
            ),
            "Sōzu 2.3 spelling"
        );
        assert!(
            !is_already_gone(
                &deactivate,
                "0: found no listener with address 0.0.0.0:5353, 1: failed to acquire the lock"
            ),
            "one worker's benign answer must not hide another worker's real failure"
        );

        let remove_front: Request =
            RequestType::RemoveHttpFrontend(RequestHttpFrontend::default()).into();
        assert!(is_already_gone(
            &remove_front,
            "could not remove frontend: Could not remove route, route_bytes=42"
        ));
        assert!(
            is_already_gone(
                &remove_front,
                "0: Could not execute order on config state: Did not find HttpFrontend with \
                 address or id_bytes=36, 1: OK"
            ),
            "a Sōzu 2.3 worker whose own state lost the route"
        );
        assert!(
            !is_already_gone(
                &remove_front,
                "could not remove frontend: Could not parse rule from frontend path, path_bytes=13"
            ),
            "a parse failure is not an absent route"
        );

        let remove_cert: Request = RequestType::RemoveCertificate(RemoveCertificate {
            address: socket_address(8443),
            fingerprint: "zz".into(),
        })
        .into();
        assert!(
            !is_already_gone(
                &remove_cert,
                "Could not remove certificate: fingerprint_bytes=2"
            ),
            "a fingerprint Sōzu could not decode was never looked up, let alone removed"
        );
        assert!(
            is_already_gone(
                &remove_cert,
                "0: found no listener with address 0.0.0.0:8443, 1: found no listener with address 0.0.0.0:8443"
            ),
            "the main process keeps accepting the removal of a cert on a listener that is gone; \
             the workers' answer must be benign or the batch fails forever"
        );
        assert!(
            !is_already_gone(&deactivate, "0: OK"),
            "all-OK under a Failure status means a worker never answered (timed out), \
             not that the object is gone"
        );
        assert!(
            !is_already_gone(&deactivate, "0: OK, 1: OK"),
            "same with every worker reporting OK"
        );

        let add: Request = RequestType::AddCluster(Cluster::default()).into();
        assert!(
            !is_already_gone(&add, "Did not find Cluster with address or id_bytes=1"),
            "only teardown verbs may be tolerated"
        );
    }

    /// A teardown Sōzu refused for a reason other than "no longer held" must
    /// fail the batch: skipping it would advance the shadow over an object
    /// Sōzu still serves.
    #[test]
    fn teardown_refused_for_another_reason_fails_the_batch() {
        let path = temp_socket_path("teardown-real-failure");
        let server = spawn_fake_sozu_failing_with(&path, "Wrong field value: address");
        let remove: Request = RequestType::RemoveCluster("c".to_string()).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent.apply(&[remove]).unwrap_err();
        assert!(matches!(err, SozuError::Failure(_)), "got {err:?}");
        drop(agent);
        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn teardown_of_an_object_sozu_no_longer_holds_is_tolerated() {
        let path = temp_socket_path("teardown-gone");
        let server =
            spawn_fake_sozu_failing_with(&path, "Did not find Cluster with address or id_bytes=1");
        let remove: Request = RequestType::RemoveCluster("c".to_string()).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent
            .apply(&[remove])
            .expect("an already-gone teardown converges");
        drop(agent);
        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    /// A failed batch reports how many requests at its head Sōzu acknowledged:
    /// those it answered `Ok` and those whose failure was settled (here an
    /// already-gone teardown). The request it stopped on is not counted, and
    /// nothing after it is sent. The controller replays exactly that many
    /// requests onto its shadow, so one too many would record a request Sōzu
    /// refused.
    #[test]
    fn a_failed_batch_counts_the_requests_sozu_acknowledged_before_it() {
        let path = temp_socket_path("partial-count");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                ok_response(),
                failure_response(
                    "could not apply request on the main process state: Did not find Cluster \
                     with address or id_bytes=4",
                ),
                failure_response(
                    "could not apply request on the main process state: Invalid character 'z' \
                     at position 0",
                ),
            ],
        );
        let batch: Vec<Request> = vec![
            RequestType::RemoveBackend(RemoveBackend::default()).into(),
            RequestType::RemoveCluster("gone".to_string()).into(),
            RequestType::RemoveCertificate(RemoveCertificate {
                address: socket_address(8443),
                fingerprint: "zz".to_string(),
            })
            .into(),
            RequestType::RemoveCluster("never-sent".to_string()).into(),
        ];
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent.apply_counted(&batch).unwrap_err();
        assert_eq!(err.applied, 2, "got {err:?}");
        assert_eq!(
            err.replayable, 2,
            "the main process refused the teardown itself: {err:?}"
        );
        assert!(
            matches!(&err.cause, SozuError::Failure(m) if m.contains("Invalid character")),
            "got {err:?}"
        );
        drop(agent);
        let received = server.join().expect("fake sozu thread");
        assert_eq!(
            received,
            batch[..3],
            "nothing may follow the failed request"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Every worker refusing a removal as already gone, in the main process's
    /// aggregate of their answers (2.2.1 and 2.3.2 alike).
    const EVERY_WORKER_GONE: &str = "0: Could not execute order on config state: Did not \
         find Cluster with address or id_bytes=1, 1: Could not execute order on config state: \
         Did not find Cluster with address or id_bytes=1";

    /// A settled teardown proves the main process holds nothing only by the
    /// main process's own refusal, or by a worker that took the removal.
    /// Every worker refusing it proves nothing: since 2.3.0 the main process
    /// then reverts its own state.
    #[test]
    fn only_the_main_process_refusal_or_a_worker_ok_proves_a_teardown_gone() {
        let remove: Request = RequestType::RemoveCluster("c".to_string()).into();
        for (message, proves) in [
            (
                "could not apply request on the main process state: Did not find Cluster with \
                 address or id_bytes=1",
                true,
            ),
            (
                "Could Not Apply Request On The Main Process State: dispatching this request \
                 did not bring any change to the state",
                true,
            ),
            (
                "0: OK, 1: Could not execute order on config state: Did not find Cluster with \
                 address or id_bytes=1",
                true,
            ),
            (
                "1: Could not execute order on config state: Did not find Cluster with address \
                 or id_bytes=1, 0: ok",
                true,
            ),
            (EVERY_WORKER_GONE, false),
            ("Did not find Cluster with address or id_bytes=1", false),
            (
                "0: found no listener for that address, 1: not ok: Did not find Cluster with \
                 address or id_bytes=1",
                false,
            ),
        ] {
            assert!(is_already_gone(&remove, message), "{message}");
            assert_eq!(gone_from_main_process(message), proves, "{message}");
        }
    }

    /// The first settlement that proves nothing cuts what the batch reports
    /// for replay, there: that request and everything after it are left out,
    /// though Sōzu acknowledged them.
    #[test]
    fn a_settlement_that_proves_nothing_cuts_the_replayable_head() {
        let path = temp_socket_path("partial-unproven");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response(
                    "0: OK, 1: Could not execute order on config state: Did not find Cluster \
                     with address or id_bytes=1",
                ),
                failure_response(EVERY_WORKER_GONE),
                failure_response(
                    "could not apply request on the main process state: Did not find Cluster \
                     with address or id_bytes=1",
                ),
                failure_response(
                    "could not apply request on the main process state: Invalid character 'z' \
                     at position 0",
                ),
            ],
        );
        let batch: Vec<Request> = vec![
            RequestType::RemoveCluster("a".to_string()).into(),
            RequestType::RemoveCluster("b".to_string()).into(),
            RequestType::RemoveCluster("c".to_string()).into(),
            RequestType::RemoveCertificate(RemoveCertificate {
                address: socket_address(8443),
                fingerprint: "zz".to_string(),
            })
            .into(),
        ];
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent.apply_counted(&batch).unwrap_err();
        assert_eq!((err.applied, err.replayable), (3, 1), "{err:?}");
        drop(agent);
        assert_eq!(server.join().expect("fake sozu thread"), batch);
        let _ = std::fs::remove_file(&path);
    }

    /// A frontend teardown whose key the main process still holds afterwards
    /// proves nothing either: the route it named is listed again (the
    /// removal was reverted), or another cluster's route whose removal every
    /// worker refused is left in place. Both after a refusal and after 2.3's
    /// "nothing to remove".
    #[test]
    fn a_route_the_main_process_still_holds_cuts_the_replayable_head() {
        let nothing_to_remove = Response {
            status: ResponseStatus::Ok as i32,
            message: "nothing to remove: cluster shadow does not exist (removed, or never \
                      added); this key belongs to cluster stale, left in place"
                .to_string(),
            content: None,
        };
        let main_gone = || {
            failure_response(
                "could not apply request on the main process state: Did not find \
                 HttpFrontend with address or id_bytes=36",
            )
        };
        let listed = |front: RequestHttpFrontend| {
            frontend_list_response(ListedFrontends {
                http_frontends: vec![front],
                ..ListedFrontends::default()
            })
        };
        let refused = failure_response(
            "could not apply request on the main process state: Invalid character 'z' at \
             position 0",
        );
        let remove: Request = RequestType::RemoveHttpFrontend(http_front("shadow", "/")).into();
        let every_worker_gone = failure_response(
            "0: Could not execute order on config state: Did not find HttpFrontend with \
             address or id_bytes=36, 1: Could not execute order on config state: Did not find \
             HttpFrontend with address or id_bytes=36",
        );
        for (name, script, replayable) in [
            (
                "evicted",
                vec![
                    main_gone(),
                    listed(http_front("stale", "/")),
                    ok_response(),
                    ok_response(),
                    refused.clone(),
                ],
                2,
            ),
            (
                "eviction-refused",
                vec![
                    main_gone(),
                    listed(http_front("stale", "/")),
                    every_worker_gone.clone(),
                    ok_response(),
                    refused.clone(),
                ],
                0,
            ),
            (
                "reverted",
                vec![
                    every_worker_gone.clone(),
                    listed(http_front("shadow", "/")),
                    ok_response(),
                    refused.clone(),
                ],
                0,
            ),
            (
                "nothing-left",
                vec![
                    nothing_to_remove.clone(),
                    listed(http_front("stale", "/")),
                    every_worker_gone.clone(),
                    ok_response(),
                    refused.clone(),
                ],
                0,
            ),
        ] {
            let path = temp_socket_path(&format!("unproven-{name}"));
            let server = spawn_scripted_fake_sozu(&path, script);
            let batch: Vec<Request> = vec![
                remove.clone(),
                RequestType::RemoveCluster("other".to_string()).into(),
                RequestType::RemoveCertificate(RemoveCertificate {
                    address: socket_address(8443),
                    fingerprint: "zz".to_string(),
                })
                .into(),
            ];
            let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
            let err = agent.apply_counted(&batch).unwrap_err();
            assert_eq!(
                (err.applied, err.replayable),
                (2, replayable),
                "{name}: {err:?}"
            );
            drop(agent);
            server.join().expect("fake sozu thread");
            let _ = std::fs::remove_file(&path);
        }
    }

    /// What the cut is for. A teardown every worker refused may have been
    /// reverted in the main process; replayed, it would leave the shadow, and
    /// no later reconcile would ask for it again. Replaying the established
    /// head keeps it, and the next reconcile sends the removal again. Sōzu
    /// 2.3.2 reverts a frontend removal, not a backend one (`rollback_for`
    /// has no inverse for it), but the answer does not say which verb it was
    /// for, so the agent cuts at both alike; the backend removal here
    /// exercises that bookkeeping.
    #[test]
    fn a_cut_replay_keeps_what_the_main_process_may_still_hold() {
        use sozu_gw_ir as ir;
        use sozu_gw_translator as tr;

        let cluster = ir::Cluster {
            id: "c".to_string(),
            load_balancing: ir::LbAlgorithm::RoundRobin,
            sticky_session: false,
            https_redirect: false,
            max_connections_per_ip: None,
            retry_after: None,
            http2: false,
            health_check: None,
        };
        let backend = |n: u8| ir::Backend {
            cluster_id: "c".to_string(),
            backend_id: format!("c-{n}"),
            address: std::net::SocketAddr::from(([10, 0, 0, n], 80)),
            weight: None,
            sticky_id: None,
        };
        let previous = ir::Ir {
            clusters: vec![cluster.clone()],
            backends: vec![backend(1), backend(2), backend(3)],
            ..ir::Ir::default()
        };
        let desired = ir::Ir {
            clusters: vec![cluster],
            backends: vec![backend(1)],
            ..ir::Ir::default()
        };
        let removals = tr::reconcile(&previous, &desired).expect("translate");
        assert_eq!(removals.len(), 2, "{removals:?}");
        let mut batch = removals.clone();
        batch.push(
            RequestType::RemoveCertificate(RemoveCertificate {
                address: socket_address(8443),
                fingerprint: "zz".to_string(),
            })
            .into(),
        );

        let path = temp_socket_path("unproven-replay");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response(EVERY_WORKER_GONE),
                ok_response(),
                failure_response(
                    "could not apply request on the main process state: Invalid character 'z' \
                     at position 0",
                ),
            ],
        );
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent.apply_counted(&batch).unwrap_err();
        assert_eq!((err.applied, err.replayable), (2, 0), "{err:?}");
        drop(agent);
        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);

        let cut = tr::replay_if_exact(&previous, &desired, &batch[..err.replayable])
            .expect("nothing replayed");
        assert_eq!(cut, previous);
        assert_eq!(
            tr::reconcile(&cut, &desired).expect("translate"),
            removals,
            "both removals are sent again"
        );
        let uncut = tr::replay_if_exact(&previous, &desired, &batch[..err.applied])
            .expect("the replay of every acknowledged request is exact");
        assert_eq!(uncut, desired);
        assert!(
            tr::reconcile(&uncut, &desired)
                .expect("translate")
                .is_empty(),
            "replayed in full, nothing would ask for the removal again"
        );
    }

    /// A request whose reply never came is indeterminate and not counted: the
    /// first request is acknowledged, the second is read and never answered,
    /// on the first connection and on the retry.
    #[test]
    fn a_request_whose_reply_never_came_is_not_counted() {
        let path = temp_socket_path("partial-unanswered");
        let read_timeout = Duration::from_millis(100);
        let server = spawn_per_connection_fake_sozu(
            &path,
            vec![
                Box::new(|mut channel: Channel<Response, Request>| {
                    if channel
                        .read_message_blocking_timeout(Some(Duration::from_secs(2)))
                        .is_ok()
                    {
                        let _ = channel.write_message(&ok_response());
                    }
                    let _ = channel.read_message_blocking_timeout(Some(Duration::from_secs(2)));
                    let _ = channel.read_message_blocking_timeout(Some(Duration::from_secs(2)));
                }),
                Box::new(|mut channel: Channel<Response, Request>| {
                    let _ = channel.read_message_blocking_timeout(Some(Duration::from_secs(2)));
                    let _ = channel.read_message_blocking_timeout(Some(Duration::from_secs(2)));
                }),
            ],
        );
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent.read_timeout = read_timeout;
        let batch: Vec<Request> = vec![
            RequestType::RemoveCluster("first".to_string()).into(),
            RequestType::RemoveCluster("unanswered".to_string()).into(),
        ];
        let err = agent.apply_counted(&batch).unwrap_err();
        assert_eq!(err.applied, 1, "got {err:?}");
        assert!(matches!(err.cause, SozuError::Channel(_)), "got {err:?}");
        drop(agent);
        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    /// The async handle hands the count back to its caller.
    #[tokio::test]
    async fn the_handle_reports_how_far_a_failed_batch_got() {
        let path = temp_socket_path("partial-handle");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![ok_response(), failure_response("refused for the test")],
        );
        let handle = SozuAgentHandle::spawn(path.to_str().expect("utf-8 path")).expect("spawn");
        let err = handle
            .apply(vec![
                RequestType::RemoveCluster("a".to_string()).into(),
                RequestType::RemoveCluster("b".to_string()).into(),
                RequestType::RemoveCluster("c".to_string()).into(),
            ])
            .await
            .unwrap_err();
        assert_eq!(err.applied, 1, "got {err:?}");
        assert!(
            matches!(&err.cause, SozuError::Failure(m) if m == "refused for the test"),
            "got {err:?}"
        );
        drop(handle);
        assert_eq!(server.join().expect("fake sozu thread").len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    /// What one accepted connection of [`spawn_per_connection_fake_sozu`] does.
    type ConnectionHandler = Box<dyn FnOnce(Channel<Response, Request>) + Send>;

    /// A fake Sōzu that serves each accepted connection on its own thread,
    /// running the handler for that connection's ordinal. Sequential accept
    /// would otherwise let a handler that deliberately lingers block the next
    /// connection the agent opens.
    fn spawn_per_connection_fake_sozu(
        path: &Path,
        handlers: Vec<ConnectionHandler>,
    ) -> thread::JoinHandle<()> {
        let listener = std::os::unix::net::UnixListener::bind(path).expect("bind fake sozu");
        thread::spawn(move || {
            let mut workers = Vec::new();
            for handler in handlers {
                let (stream, _) = listener.accept().expect("accept");
                stream.set_nonblocking(true).expect("set nonblocking");
                let mut channel: Channel<Response, Request> =
                    Channel::new(mio::net::UnixStream::from_std(stream), 16_384, 65_536);
                channel.blocking().expect("set blocking");
                workers.push(thread::spawn(move || handler(channel)));
            }
            for worker in workers {
                let _ = worker.join();
            }
        })
    }

    /// The protocol carries no request ids. After a retry that also times
    /// out, the connection must be dropped: a reply that arrives late on it
    /// would otherwise be read as the *next* request's ack. Here the late
    /// reply is a rejection; the next request must still get its own `Ok`.
    #[test]
    fn a_failed_retry_drops_the_channel_so_a_late_reply_is_not_the_next_ack() {
        let path = temp_socket_path("late-reply");
        let read_timeout = Duration::from_millis(100);
        let server = spawn_per_connection_fake_sozu(
            &path,
            vec![
                // First attempt: read the request, never answer, wait for the
                // agent to hang up.
                Box::new(|mut channel: Channel<Response, Request>| {
                    let _ = channel.read_message_blocking_timeout(Some(Duration::from_secs(2)));
                    let _ = channel.read_message_blocking_timeout(Some(Duration::from_secs(2)));
                }),
                // Retry: read the request, answer *after* the agent's read
                // deadline with a rejection, then linger so the reply stays
                // readable on this connection if the agent kept it.
                Box::new(move |mut channel: Channel<Response, Request>| {
                    let _ = channel.read_message_blocking_timeout(Some(Duration::from_secs(2)));
                    thread::sleep(read_timeout * 3);
                    let _ = channel.write_message(&failure_response("first request REJECTED"));
                    thread::sleep(Duration::from_secs(2));
                }),
                // The connection a correct agent opens for the next request.
                Box::new(|mut channel: Channel<Response, Request>| {
                    if channel
                        .read_message_blocking_timeout(Some(Duration::from_secs(2)))
                        .is_ok()
                    {
                        let _ = channel.write_message(&ok_response());
                    }
                    let _ = channel.read_message_blocking_timeout(Some(Duration::from_secs(2)));
                }),
            ],
        );

        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent.read_timeout = read_timeout;
        let first: Request = RequestType::AddCluster(Cluster::default()).into();
        let err = agent.apply(&[first]).unwrap_err();
        assert!(matches!(err, SozuError::Channel(_)), "got {err:?}");
        assert!(
            agent.channel.is_none(),
            "a connection whose reply may still be in flight must not be reused"
        );
        // Give the lingering retry connection time to write its late reply,
        // then let the healthy follow-up wait generously: the point is which
        // reply it reads, not how fast.
        thread::sleep(read_timeout * 4);
        agent.read_timeout = Duration::from_secs(5);
        let second: Request = RequestType::AddBackend(AddBackend::default()).into();
        agent
            .apply(&[second])
            .expect("the next request gets its own ack, not the first request's late rejection");
        drop(agent);
        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    /// The wiring behind [`apply_stops_between_requests_once_abandoned`]: a
    /// caller that drops its side of the reply (here, a timed-out future) must
    /// stop the batch at the next request boundary.
    #[tokio::test]
    async fn a_caller_that_times_out_stops_the_batch_at_the_next_request() {
        let path = temp_socket_path("abandon-async");
        let (count_tx, count_rx) = std::sync::mpsc::channel::<usize>();
        let server = spawn_per_connection_fake_sozu(
            &path,
            vec![Box::new(move |mut channel: Channel<Response, Request>| {
                let mut received = 0usize;
                // First request: answer late, after the caller's deadline.
                if channel
                    .read_message_blocking_timeout(Some(Duration::from_secs(5)))
                    .is_ok()
                {
                    received += 1;
                    thread::sleep(Duration::from_millis(300));
                    let _ = channel.write_message(&ok_response());
                }
                // A second request must never arrive.
                if channel
                    .read_message_blocking_timeout(Some(Duration::from_secs(1)))
                    .is_ok()
                {
                    received += 1;
                }
                let _ = count_tx.send(received);
            })],
        );
        let handle = SozuAgentHandle::spawn(path.to_str().expect("utf-8 path")).expect("spawn");
        let requests: Vec<Request> = vec![
            RequestType::AddCluster(Cluster::default()).into(),
            RequestType::AddBackend(AddBackend::default()).into(),
        ];
        let gave_up =
            tokio::time::timeout(Duration::from_millis(100), handle.apply(requests)).await;
        assert!(gave_up.is_err(), "the caller must have given up first");
        let received = count_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("fake sozu reports what it received");
        assert_eq!(received, 1, "only the first request may reach the wire");
        drop(handle);
        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    /// Once the caller has given up on a batch, the agent must stop between
    /// two requests instead of landing the rest on the socket: the caller
    /// already treats the batch as not applied.
    #[test]
    fn apply_stops_between_requests_once_abandoned() {
        let path = temp_socket_path("abandon-mid-batch");
        let server = spawn_scripted_fake_sozu(&path, vec![ok_response(), ok_response()]);
        let requests: Vec<Request> = vec![
            RequestType::AddCluster(Cluster::default()).into(),
            RequestType::AddBackend(AddBackend::default()).into(),
        ];
        let sent = std::cell::Cell::new(0usize);
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent
            .apply_unless(&requests, || {
                // Abandoned after the first request went out.
                sent.set(sent.get() + 1);
                sent.get() > 1
            })
            .unwrap_err();
        assert!(
            matches!(
                err,
                ApplyError {
                    applied: 1,
                    replayable: 1,
                    cause: SozuError::Abandoned {
                        applied: 1,
                        total: 2
                    }
                }
            ),
            "got {err:?}"
        );
        drop(agent);
        let received = server.join().expect("fake sozu thread");
        assert_eq!(
            received.len(),
            1,
            "only the first request may reach the wire"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_batch_does_not_connect() {
        // No socket exists; an empty batch must succeed without touching it.
        let mut agent = SozuAgent::new("/nonexistent/sozu.sock");
        assert!(agent.apply(&[]).is_ok());
        assert!(agent.channel.is_none());
    }

    #[test]
    fn teardowns_are_recognized_for_failure_tolerance() {
        let remove: Request = RequestType::RemoveCluster("c".to_string()).into();
        assert!(is_teardown(&remove), "RemoveCluster must be a teardown");
        let deactivate: Request = RequestType::DeactivateListener(DeactivateListener {
            address: socket_address(7777),
            proxy: ListenerType::Tcp as i32,
            to_scm: false,
            interface: None,
        })
        .into();
        assert!(
            is_teardown(&deactivate),
            "DeactivateListener must be a teardown (L4 listener teardown against a drifted Sōzu)"
        );
        let status: Request = RequestType::Status(Status {}).into();
        assert!(
            !is_teardown(&status),
            "non-teardown verbs must not be tolerated"
        );
    }

    #[test]
    fn duplicate_add_detection_is_scoped_to_exists_prone_verbs() {
        let add_frontend: Request =
            RequestType::AddHttpFrontend(RequestHttpFrontend::default()).into();
        assert!(
            is_duplicate_add(&add_frontend, EXISTS_MESSAGE),
            "a frontend add rejected as already-existing must get duplicate handling"
        );
        assert!(
            is_duplicate_add(&add_frontend, EXISTS_MESSAGE_2_2_1),
            "the same rejection from a 2.2.1 proxy, which reports the id's byte \
             length instead of the id, must still get duplicate handling: the image \
             and the linked library are bumped independently"
        );
        assert!(
            is_duplicate_add(&add_frontend, "HTTPFRONTEND 'X' ALREADY EXISTS"),
            "message matching must be case-insensitive"
        );
        assert!(
            !is_duplicate_add(&add_frontend, "wrong hostname"),
            "an unrelated add failure must still fail the batch"
        );

        let add_listener: Request =
            RequestType::AddTcpListener(TcpListenerConfig::default()).into();
        assert!(
            is_duplicate_add(&add_listener, "TcpListener '127.0.0.1:7777' already exists"),
            "an L4 listener add rejected as already-existing must get duplicate handling"
        );

        // AddCluster/AddBackend are upserts in Sōzu: an exists-looking failure
        // on them is unexpected and must not be swallowed.
        let add_cluster: Request = RequestType::AddCluster(Cluster::default()).into();
        assert!(
            !is_duplicate_add(&add_cluster, EXISTS_MESSAGE),
            "idempotent verbs must not get duplicate-add handling"
        );

        let remove: Request =
            RequestType::RemoveHttpFrontend(RequestHttpFrontend::default()).into();
        assert!(
            !is_duplicate_add(&remove, EXISTS_MESSAGE) && is_teardown(&remove),
            "removes stay on the teardown-tolerance path"
        );
    }

    #[test]
    fn removal_is_synthesized_only_for_frontend_adds() {
        let front = RequestHttpFrontend {
            cluster_id: Some("cluster-a".to_string()),
            hostname: "lb.example.com".to_string(),
            ..RequestHttpFrontend::default()
        };
        let add: Request = RequestType::AddHttpFrontend(front.clone()).into();
        let expected: Request = RequestType::RemoveHttpFrontend(front).into();
        assert_eq!(
            removal_for(&add),
            Some(expected),
            "a frontend add must repair via the remove built from its own payload"
        );

        let add_listener: Request =
            RequestType::AddTcpListener(TcpListenerConfig::default()).into();
        assert!(
            removal_for(&add_listener).is_none(),
            "listener adds are tolerated, never repaired (deactivate ordering)"
        );

        let add_cluster: Request = RequestType::AddCluster(Cluster::default()).into();
        assert!(
            removal_for(&add_cluster).is_none(),
            "upsert verbs never reach the repair path"
        );
    }

    /// The stored frontend behind an `Exists` may point at a *different*
    /// cluster (the route key excludes `cluster_id`/tags/filters), and only a
    /// removal naming that cluster evicts it on Sōzu 2.3. The agent must look
    /// the stored route up, remove it as stored, and re-send its own add.
    /// Assert that exact wire sequence.
    #[test]
    fn duplicate_frontend_add_is_repaired_by_removing_the_stored_route() {
        let path = temp_socket_path("repair-add");
        let front = RequestHttpFrontend {
            cluster_id: Some("cluster-a".to_string()),
            hostname: "lb.example.com".to_string(),
            ..RequestHttpFrontend::default()
        };
        let stored = RequestHttpFrontend {
            cluster_id: Some("cluster-b".to_string()),
            ..front.clone()
        };
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response(EXISTS_MESSAGE_2_2_1), // the add: duplicate route key
                frontend_list_response(ListedFrontends {
                    http_frontends: vec![stored.clone()],
                    ..ListedFrontends::default()
                }), // the lookup
                ok_response(),                          // the repair remove
                ok_response(),                          // the re-sent add
            ],
        );

        let add: Request = RequestType::AddHttpFrontend(front).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent
            .apply(std::slice::from_ref(&add))
            .expect("a repaired duplicate frontend add must succeed");
        drop(agent);

        let received = server.join().expect("fake sozu thread");
        let expected = vec![
            add.clone(),
            RequestType::ListFrontends(FrontendFilters {
                http: true,
                https: false,
                tcp: false,
                domain: Some("lb.example.com".to_string()),
                cluster_id: None,
            })
            .into(),
            RequestType::RemoveHttpFrontend(stored).into(),
            add,
        ];
        assert_eq!(
            received, expected,
            "the repair must remove the route as stored, then re-send the add"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// If the re-add after the repair fails hard, the batch must fail — the
    /// add is not counted and the next reconcile retries; never a silent skip.
    /// A lookup Sōzu cannot answer falls back to the add's own payload.
    #[test]
    fn failed_readd_after_repair_fails_the_batch() {
        let path = temp_socket_path("repair-fail");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response(EXISTS_MESSAGE),   // the add: duplicate route key
                failure_response("could not list"), // the lookup
                ok_response(),                      // the repair remove (own payload)
                failure_response("wrong hostname"), // the re-sent add: hard failure
            ],
        );

        let add: Request = RequestType::AddHttpFrontend(RequestHttpFrontend::default()).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent.apply(std::slice::from_ref(&add)).unwrap_err();
        assert!(matches!(err, SozuError::Failure(_)), "got {err:?}");
        drop(agent);

        let received = server.join().expect("fake sozu thread");
        assert_eq!(
            received.len(),
            4,
            "the wire must show add, lookup, repair remove, failed re-add"
        );
        assert_eq!(received[2], removal_for(&add).expect("frontend add"));
        let _ = std::fs::remove_file(&path);
    }

    /// L4 listener adds keep plain tolerance: they are keyed by address alone,
    /// so a duplicate cannot mask a different routing target — and no repair
    /// traffic may be emitted for them.
    #[test]
    fn duplicate_listener_add_is_tolerated_without_repair() {
        let path = temp_socket_path("dup-listener");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![failure_response(
                "TcpListener '127.0.0.1:7777' already exists",
            )],
        );

        let add: Request = RequestType::AddTcpListener(TcpListenerConfig {
            address: socket_address(7777),
            ..TcpListenerConfig::default()
        })
        .into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent
            .apply(&[add])
            .expect("a duplicate listener add must be tolerated so reconciliation converges");
        drop(agent);

        let received = server.join().expect("fake sozu thread");
        assert_eq!(received.len(), 1, "no repair traffic for a listener add");
        let _ = std::fs::remove_file(&path);
    }

    /// A Sōzu that keeps the socket open but never reads must not hang
    /// `apply` forever: the write timeout unblocks the kernel write, then the
    /// read deadline and the one reconnect+retry surface a channel error.
    /// Behavioral by necessity — `mio`'s stream exposes no timeout getter and
    /// the crate forbids the unsafe fd round-trip that could read it back.
    /// Without `SO_SNDTIMEO` this test hangs and trips `recv_timeout`.
    #[test]
    fn apply_does_not_hang_when_sozu_stops_reading() {
        let path = temp_socket_path("stalled");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind fake sozu");
        // A stalled Sōzu: accept the connection, hold it open, never read.
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let _held = listener.accept().expect("accept");
            let _ = hold_rx.recv();
        });

        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        // Short deadlines so the test is fast (defaults are 30s).
        agent.write_timeout = Duration::from_millis(100);
        agent.read_timeout = Duration::from_millis(100);

        // A payload far larger than a unix socket buffer, so an unbounded
        // write blocks in the kernel once the buffer fills.
        let request: Request = RequestType::LoadState("x".repeat(8 * 1024 * 1024)).into();

        let (done_tx, done_rx) = mpsc::channel();
        let apply_thread = thread::spawn(move || {
            let _ = done_tx.send(agent.apply(&[request]));
        });

        let result = done_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("apply must not hang when sozu stops reading");
        assert!(
            matches!(result, Err(SozuError::Channel(_))),
            "a stalled sozu cannot ack anything, got {result:?}"
        );

        let _ = hold_tx.send(());
        apply_thread.join().expect("apply thread");
        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    /// A wedged-but-alive Sōzu whose accept backlog is already full must not
    /// hang `connect` forever: on Linux a blocking AF_UNIX connect() waits
    /// for backlog room under `SO_SNDTIMEO`, which is why the agent sets the
    /// timeout *before* connecting (std can only set it on a connected
    /// stream). The fake listener uses the smallest backlog and never
    /// accepts; nonblocking fillers saturate the queue without leaking
    /// blocked threads (a nonblocking AF_UNIX connect fails fast instead of
    /// queueing once the backlog is full). Without the pre-connect
    /// `SO_SNDTIMEO` this test hangs and trips `recv_timeout`.
    #[test]
    fn connect_does_not_hang_when_the_listener_backlog_is_full() {
        let path = temp_socket_path("backlog");
        let address = socket2::SockAddr::unix(&path).expect("socket address");
        let listener = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .expect("create listener socket");
        listener.bind(&address).expect("bind fake sozu");
        listener.listen(0).expect("listen with minimal backlog");

        // Fill the accept queue: connects succeed instantly while there is
        // room; the first fast failure signals saturation. Kept alive so the
        // queue stays full for the agent's connect below.
        let mut fillers = Vec::new();
        for _ in 0..64 {
            let filler = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
                .expect("create filler socket");
            filler.set_nonblocking(true).expect("set nonblocking");
            match filler.connect(&address) {
                Ok(()) => fillers.push(filler),
                Err(_) => break, // backlog full (EAGAIN on Linux)
            }
        }
        assert!(
            fillers.len() < 64,
            "the accept queue never filled; cannot exercise a blocking connect"
        );

        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        // Short deadlines so the test is fast (defaults are 30s).
        agent.write_timeout = Duration::from_millis(100);
        agent.read_timeout = Duration::from_millis(100);

        let (done_tx, done_rx) = mpsc::channel();
        let connect_thread = thread::spawn(move || {
            let _ = done_tx.send(agent.status());
        });

        let result = done_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("connect must not hang when the backlog is full");
        assert!(
            matches!(result, Err(SozuError::Channel(_))),
            "a full backlog cannot yield a working channel, got {result:?}"
        );

        connect_thread.join().expect("connect thread");
        drop(fillers);
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reconnect_after_a_broken_channel_bumps_the_reconnect_epoch() {
        let path = temp_socket_path("reconnect");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind fake sozu");
        let server = thread::spawn(move || {
            // First connection: accept and drop immediately — a Sōzu that died
            // (and possibly restarted) under an established channel.
            drop(listener.accept().expect("accept first connection"));
            // Second connection: a healthy Sōzu answering Ok.
            let (stream, _) = listener.accept().expect("accept second connection");
            stream.set_nonblocking(true).expect("set nonblocking");
            let mut channel: Channel<Response, Request> =
                Channel::new(mio::net::UnixStream::from_std(stream), 16_384, 65_536);
            channel.blocking().expect("set blocking");
            while channel
                .read_message_blocking_timeout(Some(Duration::from_secs(5)))
                .is_ok()
            {
                channel
                    .write_message(&Response {
                        status: ResponseStatus::Ok as i32,
                        message: String::new(),
                        content: None,
                    })
                    .expect("write response");
            }
        });

        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent.read_timeout = Duration::from_secs(5);
        assert_eq!(
            agent.reconnects.load(Ordering::Relaxed),
            0,
            "the epoch must start at zero"
        );
        agent
            .status()
            .expect("the request must succeed after the transparent reconnect");
        assert_eq!(
            agent.reconnects.load(Ordering::Relaxed),
            1,
            "a reconnect-and-retry must bump the epoch"
        );

        drop(agent);
        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    /// The reconnect signal must survive a *failed* retry: the established
    /// connection is gone (the peer may have restarted empty) even when the
    /// reconnect attempt itself cannot reach it, so the controller's restart
    /// probe must still see a pending reconnect and retry — a signal consumed
    /// (or never raised) on the failure path would be lost forever.
    #[test]
    fn a_failed_retry_still_bumps_the_reconnect_epoch() {
        let path = temp_socket_path("reconnect-fail");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind fake sozu");
        let server = thread::spawn(move || {
            // Accept one connection and drop it, then close the listener: the
            // reconnect attempt finds nobody home (ECONNREFUSED on the stale
            // socket file) and the retry fails too.
            drop(listener.accept().expect("accept first connection"));
            drop(listener);
        });

        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        // Short deadline so a retry that connects before the listener closes
        // still fails fast on the read instead of stalling the test.
        agent.read_timeout = Duration::from_millis(200);
        let err = agent.status().unwrap_err();
        assert!(matches!(err, SozuError::Channel(_)), "got {err:?}");
        assert_eq!(
            agent.reconnects.load(Ordering::Relaxed),
            1,
            "the epoch must be bumped even when the retry fails"
        );

        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    /// `worker_pids` must parse the `Status` response's `WorkerInfos` into the
    /// live-worker PID set, excluding `Stopped` workers (Sōzu prunes those on
    /// its own schedule, which would otherwise read as a second, spurious
    /// generation change).
    #[test]
    fn worker_pids_returns_the_live_pid_set() {
        let path = temp_socket_path("worker-pids");
        let workers = WorkerInfos {
            vec: vec![
                WorkerInfo {
                    id: 0,
                    pid: 101,
                    run_state: RunState::Running as i32,
                },
                WorkerInfo {
                    id: 1,
                    pid: 102,
                    run_state: RunState::NotAnswering as i32,
                },
                WorkerInfo {
                    id: 2,
                    pid: 99,
                    run_state: RunState::Stopped as i32,
                },
            ],
        };
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![Response {
                status: ResponseStatus::Ok as i32,
                message: String::new(),
                content: Some(ResponseContent {
                    content_type: Some(ContentType::Workers(workers)),
                }),
            }],
        );

        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let pids = agent.worker_pids().expect("worker pids");
        assert_eq!(
            pids,
            BTreeSet::from([101, 102]),
            "live workers (including NotAnswering) are the generation; Stopped ones are not"
        );
        drop(agent);

        let received = server.join().expect("fake sozu thread");
        assert_eq!(
            received,
            vec![RequestType::Status(Status {}).into()],
            "the generation probe must be a single Status round-trip"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A `Status` reply without the expected `WorkerInfos` content must be an
    /// explicit error, never an empty (= changed) generation.
    #[test]
    fn worker_pids_without_workers_content_is_an_error() {
        let path = temp_socket_path("worker-pids-empty");
        let server = spawn_scripted_fake_sozu(&path, vec![ok_response()]);

        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent.worker_pids().unwrap_err();
        assert!(matches!(err, SozuError::UnexpectedResponse), "got {err:?}");
        drop(agent);

        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    fn worker_response() -> Response {
        Response {
            status: ResponseStatus::Ok as i32,
            message: String::new(),
            content: Some(ResponseContent {
                content_type: Some(ContentType::Workers(WorkerInfos {
                    vec: [7, 8]
                        .into_iter()
                        .enumerate()
                        .map(|(id, pid)| WorkerInfo {
                            id: id as u32,
                            pid,
                            run_state: RunState::Running as i32,
                        })
                        .collect(),
                })),
            }),
        }
    }

    /// Reply once, then close the established connection. The caller can keep
    /// the listener alive for a transient reconnect, or replace the socket.
    fn serve_one_status(listener: std::os::unix::net::UnixListener) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            stream.set_nonblocking(true).expect("set nonblocking");
            let mut channel: Channel<Response, Request> =
                Channel::new(mio::net::UnixStream::from_std(stream), 16_384, 65_536);
            channel.blocking().expect("set blocking");
            let request = channel
                .read_message_blocking_timeout(Some(Duration::from_secs(5)))
                .expect("read Status");
            assert_eq!(request, RequestType::Status(Status {}).into());
            channel
                .write_message(&worker_response())
                .expect("write workers");
        })
    }

    #[test]
    fn generation_changes_on_socket_replacement_with_identical_worker_pids() {
        let path = temp_socket_path("generation-replaced");
        let old_listener = std::os::unix::net::UnixListener::bind(&path).expect("bind old socket");
        let first = serve_one_status(old_listener.try_clone().expect("clone listener"));
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let before = agent.generation().expect("initial generation");
        first.join().expect("old peer disconnected");

        std::fs::remove_file(&path).expect("unlink old socket");
        let new_listener = std::os::unix::net::UnixListener::bind(&path).expect("bind new socket");
        let second = serve_one_status(new_listener);
        let after = agent.generation().expect("generation after reconnect");
        assert_eq!(before.worker_pids, after.worker_pids);
        assert_ne!(before.socket, after.socket);
        assert_eq!(agent.reconnects.load(Ordering::Relaxed), 1);

        drop(agent);
        drop(old_listener);
        second.join().expect("new peer");
        std::fs::remove_file(path).expect("remove socket");
    }

    #[test]
    fn generation_survives_reconnect_to_the_same_socket() {
        let path = temp_socket_path("generation-reconnect");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind socket");
        let first = serve_one_status(listener.try_clone().expect("clone listener"));
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let before = agent.generation().expect("initial generation");
        first.join().expect("first connection closed");

        let second = serve_one_status(listener);
        let after = agent.generation().expect("generation after reconnect");
        assert_eq!(before, after);
        assert_eq!(agent.reconnects.load(Ordering::Relaxed), 1);

        drop(agent);
        second.join().expect("second connection");
        std::fs::remove_file(path).expect("remove socket");
    }

    #[test]
    fn generation_belongs_to_the_answering_connection_not_the_current_path() {
        let path = temp_socket_path("generation-bound");
        let server = spawn_scripted_fake_sozu(&path, vec![worker_response(), worker_response()]);
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let before = agent.generation().expect("initial generation");

        // The old connection remains live after another listener takes its path.
        // A path stat after Status would wrongly label its reply as the new peer.
        std::fs::remove_file(&path).expect("unlink old socket");
        let replacement = std::os::unix::net::UnixListener::bind(&path).expect("replace socket");
        assert!(before
            .socket
            .verify(path.to_str().expect("utf-8 path"))
            .is_err());
        let after = agent.generation().expect("old peer still answers");
        assert_eq!(before, after);
        assert_eq!(agent.reconnects.load(Ordering::Relaxed), 0);

        drop(agent);
        assert_eq!(server.join().expect("old peer").len(), 2);
        drop(replacement);
        std::fs::remove_file(path).expect("remove socket");
    }

    #[test]
    fn generation_requires_a_socket_and_a_valid_status_reply() {
        let path = temp_socket_path("generation-missing");
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        assert!(matches!(agent.generation(), Err(SozuError::Channel(_))));
        assert!(agent.channel.is_none());

        let server = spawn_scripted_fake_sozu(&path, vec![ok_response()]);
        assert!(matches!(
            agent.generation(),
            Err(SozuError::UnexpectedResponse)
        ));
        drop(agent);
        server.join().expect("fake peer");
        std::fs::remove_file(path).expect("remove socket");
    }

    #[test]
    fn unrelated_add_failure_from_sozu_still_fails_the_batch() {
        let path = temp_socket_path("bad-add");
        let server = spawn_fake_sozu_failing_with(&path, "invalid frontend");

        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let add: Request = RequestType::AddHttpFrontend(RequestHttpFrontend::default()).into();
        let err = agent.apply(&[add]).unwrap_err();
        assert!(matches!(err, SozuError::Failure(_)), "got {err:?}");

        drop(agent);
        server.join().expect("fake sozu thread");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_to_missing_socket_is_channel_error() {
        let mut agent = SozuAgent::new("/nonexistent/sozu.sock");
        let err = agent.status().unwrap_err();
        assert!(matches!(err, SozuError::Channel(_)), "got {err:?}");
    }

    #[test]
    fn query_metrics_to_missing_socket_is_channel_error() {
        let mut agent = SozuAgent::new("/nonexistent/sozu.sock");
        let err = agent
            .query_metrics(QueryMetricsOptions::default())
            .unwrap_err();
        assert!(matches!(err, SozuError::Channel(_)), "got {err:?}");
    }

    /// A job whose caller already gave up (receiver dropped — what a
    /// timed-out `/metrics` scrape leaves behind) must be skipped without
    /// touching the socket, so abandoned work can neither stack ahead of
    /// routing applies nor burn socket round-trips nobody will read. The
    /// script holds exactly one response: had the abandoned job reached the
    /// socket, it would consume it and fail the live job.
    #[tokio::test]
    async fn abandoned_job_is_skipped_without_touching_the_socket() {
        let path = temp_socket_path("abandoned-job");
        let server = spawn_scripted_fake_sozu(&path, vec![ok_response()]);

        let handle = SozuAgentHandle::spawn(path.to_str().expect("utf-8 path")).expect("spawn");
        // Queue a job that is abandoned before the worker picks it up.
        let (reply_tx, reply_rx) = oneshot::channel();
        drop(reply_rx);
        handle
            .tx
            .send(Job::QueryMetrics(QueryMetricsOptions::default(), reply_tx))
            .expect("queue the abandoned job");
        // The live job queued behind it must be the only thing on the wire.
        let live: Request = RequestType::Status(Status {}).into();
        handle
            .apply(vec![live.clone()])
            .await
            .expect("the live job must succeed on the single scripted response");
        drop(handle);

        let received = server.join().expect("fake sozu thread");
        assert_eq!(
            received,
            vec![live],
            "only the live job may reach the socket"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn handle_reports_connection_error() {
        let handle = SozuAgentHandle::spawn("/nonexistent/sozu.sock").expect("spawn");
        let err = handle
            .apply(vec![RequestType::Status(Status {}).into()])
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                ApplyError {
                    applied: 0,
                    replayable: 0,
                    cause: SozuError::Channel(_)
                }
            ),
            "got {err:?}"
        );
    }

    fn http_front(cluster: &str, path: &str) -> RequestHttpFrontend {
        RequestHttpFrontend {
            cluster_id: Some(cluster.to_string()),
            address: SocketAddress::new_v4(0, 0, 0, 0, 80),
            hostname: "lb.example.com".to_string(),
            path: PathRule {
                kind: PathRuleKind::Prefix as i32,
                value: path.to_string(),
            },
            ..RequestHttpFrontend::default()
        }
    }

    fn frontend_list_response(listed: ListedFrontends) -> Response {
        Response {
            status: ResponseStatus::Ok as i32,
            message: String::new(),
            content: Some(ContentType::FrontendList(listed).into()),
        }
    }

    /// The stored entry is found by the whole key Sōzu files it under, on the
    /// request's own protocol: another path on the same host, the same route
    /// on the other protocol, or another SNI on the same TCP address is never
    /// what the refusal was about.
    #[test]
    fn the_stored_entry_is_matched_on_its_whole_key() {
        let stale = http_front("stale", "/api");
        let listed = ListedFrontends {
            http_frontends: vec![http_front("other", "/"), stale.clone()],
            https_frontends: vec![http_front("https-side", "/api")],
            ..ListedFrontends::default()
        };
        let add: Request = RequestType::AddHttpFrontend(http_front("mine", "/api")).into();
        assert_eq!(
            stored_removal(&add, listed.clone()),
            Some(RequestType::RemoveHttpFrontend(stale).into())
        );
        let https: Request = RequestType::AddHttpsFrontend(http_front("mine", "/api")).into();
        assert_eq!(
            stored_removal(&https, listed.clone()),
            Some(RequestType::RemoveHttpsFrontend(http_front("https-side", "/api")).into())
        );
        let elsewhere: Request = RequestType::AddHttpFrontend(http_front("mine", "/v2")).into();
        assert_eq!(
            stored_removal(&elsewhere, listed),
            None,
            "the entry vanished"
        );

        let tcp = |cluster: &str, sni: Option<&str>| RequestTcpFrontend {
            cluster_id: cluster.to_string(),
            address: socket_address(9000),
            sni: sni.map(str::to_string),
            ..RequestTcpFrontend::default()
        };
        let listed = ListedFrontends {
            tcp_frontends: vec![
                tcp("sni-routed", Some("db.example.com")),
                tcp("stale", None),
            ],
            udp_frontends: vec![RequestUdpFrontend {
                cluster_id: "stale-udp".to_string(),
                address: socket_address(9000),
                ..RequestUdpFrontend::default()
            }],
            ..ListedFrontends::default()
        };
        let add_tcp: Request = RequestType::AddTcpFrontend(tcp("mine", None)).into();
        assert_eq!(
            stored_removal(&add_tcp, listed.clone()),
            Some(RequestType::RemoveTcpFrontend(tcp("stale", None)).into())
        );
        let remove_udp: Request = RequestType::RemoveUdpFrontend(RequestUdpFrontend {
            cluster_id: "mine".to_string(),
            address: socket_address(9000),
            ..RequestUdpFrontend::default()
        })
        .into();
        assert_eq!(
            stored_removal(&remove_udp, listed),
            Some(
                RequestType::RemoveUdpFrontend(RequestUdpFrontend {
                    cluster_id: "stale-udp".to_string(),
                    address: socket_address(9000),
                    ..RequestUdpFrontend::default()
                })
                .into()
            )
        );
    }

    /// HTTP routes are listed by their hostname, layer-4 frontends all at once
    /// (the listing has no UDP flag and a `domain` filter drops them).
    #[test]
    fn the_owner_query_lists_the_route_hostname_or_every_l4_frontend() {
        let http: Request = RequestType::RemoveHttpFrontend(http_front("a", "/")).into();
        assert_eq!(
            owner_query(&http),
            Some(
                RequestType::ListFrontends(FrontendFilters {
                    http: true,
                    https: false,
                    tcp: false,
                    domain: Some("lb.example.com".to_string()),
                    cluster_id: None,
                })
                .into()
            )
        );
        let udp: Request = RequestType::AddUdpFrontend(RequestUdpFrontend::default()).into();
        assert_eq!(
            owner_query(&udp),
            Some(
                RequestType::ListFrontends(FrontendFilters {
                    http: false,
                    https: false,
                    tcp: true,
                    domain: None,
                    cluster_id: None,
                })
                .into()
            )
        );
        let listener: Request = RequestType::AddTcpListener(TcpListenerConfig::default()).into();
        assert_eq!(owner_query(&listener), None);
    }

    /// Since Sōzu 2.3.0 a UDP address held by another cluster is refused with
    /// its own wording rather than `Exists`; it is the same stale duplicate.
    #[test]
    fn a_udp_address_claimed_by_another_cluster_is_a_duplicate_add() {
        const TAKEN: &str = "could not apply request on the main process state: UDP frontend \
             address 0.0.0.0:5353 is already claimed by another frontend: cluster_id_bytes=4 \
             current_cluster_id_bytes=5 current_tag_count=0";
        let udp: Request = RequestType::AddUdpFrontend(RequestUdpFrontend::default()).into();
        assert!(is_duplicate_add(&udp, TAKEN));
        let tcp: Request = RequestType::AddTcpFrontend(RequestTcpFrontend::default()).into();
        assert!(!is_duplicate_add(&tcp, TAKEN), "only UDP uses that wording");
    }

    /// A layer-4 address held by another cluster — a stale entry the shadow
    /// never recorded — is evicted as stored, then our add is re-sent.
    #[test]
    fn a_duplicate_l4_add_evicts_the_frontend_another_cluster_holds() {
        let path = temp_socket_path("repair-l4");
        let ours = RequestTcpFrontend {
            cluster_id: "ours".to_string(),
            address: socket_address(9000),
            ..RequestTcpFrontend::default()
        };
        let stale = RequestTcpFrontend {
            cluster_id: "stale".to_string(),
            ..ours.clone()
        };
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response("TcpFrontend with id_bytes=12 already exists; remove it first"),
                frontend_list_response(ListedFrontends {
                    tcp_frontends: vec![stale.clone()],
                    ..ListedFrontends::default()
                }),
                ok_response(),
                ok_response(),
            ],
        );
        let add: Request = RequestType::AddTcpFrontend(ours).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent
            .apply(std::slice::from_ref(&add))
            .expect("the stale owner is evicted and the add lands");
        drop(agent);

        let received = server.join().expect("fake sozu thread");
        assert_eq!(received.len(), 4, "add, lookup, eviction, re-add");
        assert_eq!(received[2], RequestType::RemoveTcpFrontend(stale).into());
        assert_eq!(received[3], add);
        let _ = std::fs::remove_file(&path);
    }

    /// A removal Sōzu answers with "nothing to remove" while the key still
    /// routes for another cluster must not be tolerated as done: the shadow
    /// would advance over a live route. The stored route is removed instead.
    #[test]
    fn a_teardown_that_finds_the_key_under_another_cluster_removes_it_as_stored() {
        let path = temp_socket_path("orphan");
        let stale = http_front("stale", "/");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response(
                    "could not apply request on the main process state: Did not find \
                     HttpFrontend with address or id_bytes=36",
                ),
                frontend_list_response(ListedFrontends {
                    http_frontends: vec![stale.clone()],
                    ..ListedFrontends::default()
                }),
                ok_response(),
                // the second teardown finds nothing anywhere: tolerated
                failure_response("Did not find HttpFrontend with address or id_bytes=36"),
                frontend_list_response(ListedFrontends::default()),
            ],
        );
        let remove: Request = RequestType::RemoveHttpFrontend(http_front("shadow", "/")).into();
        let gone: Request = RequestType::RemoveHttpFrontend(http_front("shadow", "/v2")).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent
            .apply(&[remove, gone])
            .expect("both teardowns converge");
        drop(agent);

        let received = server.join().expect("fake sozu thread");
        assert_eq!(received.len(), 5);
        assert_eq!(received[2], RequestType::RemoveHttpFrontend(stale).into());
        let _ = std::fs::remove_file(&path);
    }

    /// A teardown whose lookup cannot be read must fail the batch: "could not
    /// tell" is not "nothing there", and tolerating it would advance the
    /// shadow over a route another cluster may still hold.
    #[test]
    fn a_teardown_whose_lookup_fails_fails_the_batch() {
        let path = temp_socket_path("orphan-unknown");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response("Did not find HttpFrontend with address or id_bytes=36"),
                failure_response("main process could not list frontends"),
            ],
        );
        let remove: Request = RequestType::RemoveHttpFrontend(http_front("shadow", "/")).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent.apply(&[remove]).unwrap_err();
        assert!(
            matches!(&err, SozuError::Failure(m) if m.contains("could not be listed")),
            "{err:?}"
        );
        drop(agent);
        assert_eq!(server.join().expect("fake sozu thread").len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    /// A removal Sōzu refuses although its listing holds exactly that entry is
    /// not a stale owner: evicting it would resend the same refused removal.
    #[test]
    fn a_teardown_whose_lookup_finds_the_same_entry_is_tolerated_once() {
        let path = temp_socket_path("orphan-same");
        let front = http_front("shadow", "/");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response("Did not find HttpFrontend with address or id_bytes=36"),
                frontend_list_response(ListedFrontends {
                    http_frontends: vec![front.clone()],
                    ..ListedFrontends::default()
                }),
            ],
        );
        let remove: Request = RequestType::RemoveHttpFrontend(front).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent.apply(&[remove]).expect("tolerated as before");
        drop(agent);
        assert_eq!(server.join().expect("fake sozu thread").len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    /// The lookup is a round-trip: a caller that gives up during it must not
    /// see a route evicted afterwards.
    #[test]
    fn an_apply_abandoned_during_the_lookup_evicts_nothing() {
        let path = temp_socket_path("repair-abandon");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response(EXISTS_MESSAGE_2_2_1),
                frontend_list_response(ListedFrontends {
                    http_frontends: vec![http_front("stale", "/")],
                    ..ListedFrontends::default()
                }),
            ],
        );
        let add: Request = RequestType::AddHttpFrontend(http_front("mine", "/")).into();
        let checks = std::cell::Cell::new(0);
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent
            .apply_unless(&[add], || {
                checks.set(checks.get() + 1);
                checks.get() > 1
            })
            .unwrap_err();
        assert!(
            matches!(
                err,
                ApplyError {
                    applied: 0,
                    replayable: 0,
                    cause: SozuError::Abandoned {
                        applied: 0,
                        total: 1
                    }
                }
            ),
            "{err:?}"
        );
        drop(agent);
        assert_eq!(
            server.join().expect("fake sozu thread").len(),
            2,
            "the add and the lookup, no eviction"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Sōzu files a TCP claim with its SNI lowercased and its ALPN sorted and
    /// deduplicated, so the stored entry matches any spelling of the claim.
    #[test]
    fn a_tcp_claim_matches_its_stored_spelling() {
        let stored = RequestTcpFrontend {
            cluster_id: "stale".to_string(),
            address: socket_address(9443),
            sni: Some("db.example.com".to_string()),
            alpn: vec!["h2".to_string(), "http/1.1".to_string()],
            ..RequestTcpFrontend::default()
        };
        let ours = RequestTcpFrontend {
            cluster_id: "mine".to_string(),
            sni: Some("DB.Example.com".to_string()),
            alpn: vec!["http/1.1".to_string(), "h2".to_string(), "h2".to_string()],
            ..stored.clone()
        };
        let listed = ListedFrontends {
            tcp_frontends: vec![stored.clone()],
            ..ListedFrontends::default()
        };
        let add: Request = RequestType::AddTcpFrontend(ours).into();
        assert_eq!(
            stored_removal(&add, listed),
            Some(RequestType::RemoveTcpFrontend(stored).into())
        );
    }

    /// Since Sōzu 2.3.0 a removal naming a cluster the main process does not
    /// hold is answered `Ok`, leaving another cluster's route on the key in
    /// place: that route is the same stale owner and is removed as stored.
    #[test]
    fn a_removal_answered_nothing_to_remove_still_evicts_another_clusters_route() {
        let path = temp_socket_path("orphan-ok");
        let stale = http_front("stale", "/");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                Response {
                    status: ResponseStatus::Ok as i32,
                    message: "nothing to remove: cluster ghost does not exist (removed, or never \
                              added); this key belongs to cluster stale, left in place"
                        .to_string(),
                    content: None,
                },
                frontend_list_response(ListedFrontends {
                    http_frontends: vec![stale.clone()],
                    ..ListedFrontends::default()
                }),
                ok_response(),
            ],
        );
        let remove: Request = RequestType::RemoveHttpFrontend(http_front("ghost", "/")).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent.apply(&[remove]).expect("converges");
        drop(agent);
        let received = server.join().expect("fake sozu thread");
        assert_eq!(received.len(), 3, "removal, lookup, eviction");
        assert_eq!(received[2], RequestType::RemoveHttpFrontend(stale).into());
        let _ = std::fs::remove_file(&path);
    }

    /// A route only the main process still holds — every worker refuses its
    /// removal — cannot be evicted, and must not fail the batch over and over.
    #[test]
    fn a_stale_route_no_worker_serves_does_not_fail_the_batch() {
        let path = temp_socket_path("orphan-ghost");
        let stale = http_front("stale", "/");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response("Did not find HttpFrontend with address or id_bytes=36"),
                frontend_list_response(ListedFrontends {
                    http_frontends: vec![stale],
                    ..ListedFrontends::default()
                }),
                failure_response(
                    "0: Could not execute order on config state: Did not find HttpFrontend \
                     with address or id_bytes=36, 1: Could not execute order on config state: \
                     Did not find HttpFrontend with address or id_bytes=36",
                ),
            ],
        );
        let remove: Request = RequestType::RemoveHttpFrontend(http_front("shadow", "/")).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent.apply(&[remove]).expect("tolerated");
        drop(agent);
        assert_eq!(server.join().expect("fake sozu thread").len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    /// Sōzu 2.3.2 refusing a replace on a listener that holds no certificate,
    /// verbatim from `docs/probes/replace-certificate-fallback_sozu-2.3.2_2026-10-19.txt`
    /// (2.2.1's `StateError::NotFound` Display is the same).
    const REPLACE_NOT_FOUND: &str = "could not apply request on the main process state: Did not \
         find Certificate with address or id_bytes=12";

    fn replace_certificate() -> ReplaceCertificate {
        ReplaceCertificate {
            address: socket_address(8443),
            new_certificate: CertificateAndKey {
                certificate: "new leaf".to_string(),
                certificate_chain: vec!["intermediate".to_string()],
                key: "new key".to_string(),
                versions: vec![TlsVersion::TlsV12 as i32, TlsVersion::TlsV13 as i32],
                names: vec!["app.example.com".to_string()],
            },
            old_fingerprint: "ab".repeat(32),
            new_expired_at: Some(1_900_000_000),
        }
    }

    /// The add a correct fallback sends for [`replace_certificate`], spelled
    /// out field by field rather than through [`addition_for`].
    fn its_addition() -> Request {
        RequestType::AddCertificate(AddCertificate {
            address: socket_address(8443),
            certificate: replace_certificate().new_certificate,
            expired_at: Some(1_900_000_000),
        })
        .into()
    }

    /// Only the main process's own "no certificate at this address" refusal
    /// of a replace qualifies: not another refusal of a replace, not a worker
    /// aggregate (the main process accepted that one), not another verb.
    #[test]
    fn a_replace_into_an_empty_listener_is_recognised_by_its_refusal_only() {
        let replace: Request = RequestType::ReplaceCertificate(replace_certificate()).into();
        assert!(is_replace_into_empty_listener(&replace, REPLACE_NOT_FOUND));
        assert!(
            is_replace_into_empty_listener(
                &replace,
                "Could Not Apply Request On The Main Process State: DID NOT FIND CERTIFICATE \
                 with address or id_bytes=12"
            ),
            "message matching must be case-insensitive"
        );
        assert!(
            !is_replace_into_empty_listener(
                &replace,
                "0: Did not find Certificate with address or id_bytes=12"
            ),
            "a single worker's aggregate is one segment too, but not the main process's refusal"
        );
        assert!(
            !is_replace_into_empty_listener(
                &replace,
                "could not apply request on the main process state: Could not replace \
                 certificate: error_bytes=31"
            ),
            "a replacement Sōzu could not parse is a real failure"
        );
        assert!(
            !is_replace_into_empty_listener(
                &replace,
                "0: Did not find Certificate with address or id_bytes=12, 1: OK"
            ),
            "a worker aggregate means the main process accepted the replace"
        );
        assert!(
            !is_replace_into_empty_listener(&its_addition(), REPLACE_NOT_FOUND),
            "only a replace has an old certificate to miss"
        );
        let remove: Request = RequestType::RemoveCertificate(RemoveCertificate {
            address: socket_address(8443),
            fingerprint: "ab".repeat(32),
        })
        .into();
        assert!(
            !is_replace_into_empty_listener(&remove, REPLACE_NOT_FOUND),
            "a removal stays on the teardown-tolerance path"
        );
    }

    /// A replace refused because the listener holds no certificate lands as an
    /// add of the replacement — every field of it — and the batch goes on.
    #[test]
    fn a_replace_into_an_empty_listener_adds_the_replacement_instead() {
        let path = temp_socket_path("replace-empty");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response(REPLACE_NOT_FOUND), // the replace
                ok_response(),                       // the fallback add
                ok_response(),                       // the next request
            ],
        );
        let replace: Request = RequestType::ReplaceCertificate(replace_certificate()).into();
        let next: Request = RequestType::AddHttpsFrontend(http_front("app", "/")).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        agent
            .apply(&[replace.clone(), next.clone()])
            .expect("the replacement lands as an add");
        drop(agent);

        let received = server.join().expect("fake sozu thread");
        assert_eq!(
            received,
            vec![replace, its_addition(), next],
            "the replace, the add of its replacement, then the rest of the batch"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The fallback is one attempt: a refused add fails the batch, carrying
    /// both answers, and nothing after it is sent.
    #[test]
    fn a_refused_fallback_add_fails_the_batch_with_both_answers() {
        let path = temp_socket_path("replace-add-fails");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![
                failure_response(REPLACE_NOT_FOUND),
                failure_response(
                    "could not apply request on the main process state: Could not add \
                     certificate: error_kind=InvalidCertificate",
                ),
            ],
        );
        let replace: Request = RequestType::ReplaceCertificate(replace_certificate()).into();
        let next: Request = RequestType::AddHttpsFrontend(http_front("app", "/")).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent.apply(&[replace, next]).unwrap_err();
        assert!(
            matches!(&err, SozuError::Failure(m)
                if m.contains("Did not find Certificate") && m.contains("Could not add certificate")),
            "{err:?}"
        );
        drop(agent);
        let received = server.join().expect("fake sozu thread");
        assert_eq!(received.len(), 2, "the replace and the add, nothing after");
        assert_eq!(received[1], its_addition());
        let _ = std::fs::remove_file(&path);
    }

    /// Any other refusal of a replace stays fatal, with no fallback traffic.
    #[test]
    fn a_replace_refused_for_another_reason_sends_no_fallback() {
        let path = temp_socket_path("replace-other");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![failure_response(
                "could not apply request on the main process state: Could not replace \
                 certificate: error_bytes=31",
            )],
        );
        let replace: Request = RequestType::ReplaceCertificate(replace_certificate()).into();
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent.apply(&[replace]).unwrap_err();
        assert!(
            matches!(&err, SozuError::Failure(m) if m.contains("Could not replace certificate")),
            "{err:?}"
        );
        drop(agent);
        assert_eq!(
            server.join().expect("fake sozu thread").len(),
            1,
            "the replace only"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The refused replace is a round-trip: a caller that gives up during it
    /// must not see the replacement land afterwards.
    #[test]
    fn an_apply_abandoned_during_the_refused_replace_adds_nothing() {
        let path = temp_socket_path("replace-abandon");
        let server = spawn_scripted_fake_sozu(
            &path,
            vec![failure_response(REPLACE_NOT_FOUND), ok_response()],
        );
        let replace: Request = RequestType::ReplaceCertificate(replace_certificate()).into();
        let checks = std::cell::Cell::new(0);
        let mut agent = SozuAgent::new(path.to_str().expect("utf-8 path"));
        let err = agent
            .apply_unless(&[replace], || {
                checks.set(checks.get() + 1);
                checks.get() > 1
            })
            .unwrap_err();
        assert!(
            matches!(
                err,
                ApplyError {
                    applied: 0,
                    replayable: 0,
                    cause: SozuError::Abandoned {
                        applied: 0,
                        total: 1
                    }
                }
            ),
            "{err:?}"
        );
        drop(agent);
        assert_eq!(
            server.join().expect("fake sozu thread").len(),
            1,
            "the replace only, no fallback add"
        );
        let _ = std::fs::remove_file(&path);
    }
}
