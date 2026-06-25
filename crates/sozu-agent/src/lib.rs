//! sozu-agent: thin, typed wrapper around `sozu-command-lib`'s command socket.
//!
//! Owns all socket I/O: connect, send a batch idempotently (each request is
//! acked through Sōzu's `Processing → Ok/Failure` reply sequence), bounded reads
//! (no permanent hang), and reconnect-and-retry on a broken channel.
//!
//! Two layers:
//!  - [`SozuAgent`] — the synchronous core (the command socket is a blocking,
//!    single-stream protocol; this type owns it).
//!  - [`SozuAgentHandle`] — an async, cloneable handle. It runs the blocking
//!    core on a dedicated thread and serialises all access through an mpsc
//!    queue, so concurrent async callers never share the socket unsafely.
#![forbid(unsafe_code)]

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sozu_command_lib::channel::Channel;
use sozu_command_lib::proto::command::{
    request::RequestType, response_content::ContentType, Request, Response, ResponseContent,
    ResponseStatus, Status,
};
use thiserror::Error;
use tokio::sync::oneshot;
use tracing::{debug, warn};

// Re-exported so callers (the controller) can drive `query_metrics` without
// taking a direct dependency on `sozu-command-lib`.
pub use sozu_command_lib::proto::command::{AggregatedMetrics, QueryMetricsOptions};

/// Client-side socket buffer sizes (server bounds come from `config.toml`).
const DEFAULT_BUFFER_SIZE: u64 = 1024 * 1024;
const DEFAULT_MAX_BUFFER_SIZE: u64 = 16 * 1024 * 1024;
/// Upper bound on a whole request's ack sequence, so a wedged Sōzu can't hang
/// us forever (applies across the Processing→Ok replies, not per read).
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Small backoff before a reconnect-and-retry, so an unhealthy Sōzu is not
/// hammered with reconnect storms across reconcile cycles.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(200);

/// Whether a request is a `Remove*` verb. Sōzu rejects removing an object it no
/// longer holds, so a failed remove is treated as an already-done no-op rather
/// than wedging reconciliation (see [`Agent::apply`]).
fn is_remove(request: &Request) -> bool {
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
        )
    )
}

#[derive(Debug, Error)]
pub enum SozuError {
    #[error("sozu command channel error: {0}")]
    Channel(String),
    #[error("sozu rejected the request: {0}")]
    Failure(String),
    #[error("sozu returned an unexpected response (no metrics content)")]
    UnexpectedResponse,
    #[error("sozu-agent worker thread is gone")]
    WorkerGone,
}

/// Synchronous client for the Sōzu command socket. Reconnects lazily.
pub struct SozuAgent {
    path: String,
    buffer_size: u64,
    max_buffer_size: u64,
    read_timeout: Duration,
    channel: Option<Channel<Request, Response>>,
}

impl SozuAgent {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            buffer_size: DEFAULT_BUFFER_SIZE,
            max_buffer_size: DEFAULT_MAX_BUFFER_SIZE,
            read_timeout: DEFAULT_READ_TIMEOUT,
            channel: None,
        }
    }

    fn connect(&mut self) -> Result<(), SozuError> {
        debug!(path = %self.path, "connecting to sozu command socket");
        let mut channel: Channel<Request, Response> =
            Channel::from_path(&self.path, self.buffer_size, self.max_buffer_size)
                .map_err(|e| SozuError::Channel(format!("connect: {e:?}")))?;
        // Blocking mode is required: a non-blocking `write_message` only buffers,
        // it does not flush to the socket.
        channel
            .blocking()
            .map_err(|e| SozuError::Channel(format!("set blocking: {e:?}")))?;
        self.channel = Some(channel);
        Ok(())
    }

    fn channel_mut(&mut self) -> Result<&mut Channel<Request, Response>, SozuError> {
        if self.channel.is_none() {
            self.connect()?;
        }
        self.channel
            .as_mut()
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
                thread::sleep(RECONNECT_BACKOFF);
                let channel = self.channel_mut()?;
                Self::send_one(channel, read_timeout, request)
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
    /// failed *remove*, which is tolerated (see [`is_remove`]).
    pub fn apply(&mut self, requests: &[Request]) -> Result<(), SozuError> {
        for request in requests {
            match self.apply_one(request) {
                Ok(()) => {}
                // Sōzu's `Remove*` verbs are NOT idempotent: removing an object it
                // no longer holds (state diverged from our shadow — a partially
                // applied batch, or a worker-side drop) is a hard `Failure`. Such a
                // remove is effectively already done, so treat it as a no-op. This
                // keeps the invariant that re-diffing from the shadow converges:
                // without it, one un-removable object wedges *all* reconciliation
                // forever (the same failing remove re-emitted every cycle).
                Err(SozuError::Failure(msg)) if is_remove(request) => {
                    warn!(error = %msg, "sozu rejected a remove; treating it as already-gone so reconciliation converges");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Liveness check: `Status` round-trip.
    pub fn status(&mut self) -> Result<(), SozuError> {
        self.apply_one(&RequestType::Status(Status {}).into())
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
    Apply(Vec<Request>, oneshot::Sender<Result<(), SozuError>>),
    SaveState(String, oneshot::Sender<Result<(), SozuError>>),
    QueryMetrics(
        QueryMetricsOptions,
        oneshot::Sender<Result<AggregatedMetrics, SozuError>>,
    ),
}

/// Cloneable async handle to a single Sōzu command socket. All work runs on one
/// dedicated thread, so socket access is serialised across clones.
#[derive(Clone)]
pub struct SozuAgentHandle {
    tx: mpsc::Sender<Job>,
}

impl SozuAgentHandle {
    /// Spawn the worker thread for the socket at `path`. The connection is
    /// established lazily on first use (so this never fails on a not-yet-ready
    /// Sōzu).
    pub fn spawn(path: impl Into<String>) -> std::io::Result<Self> {
        let path = path.into();
        let (tx, rx) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("sozu-agent".to_string())
            .spawn(move || {
                let mut agent = SozuAgent::new(path);
                // Ends when every `SozuAgentHandle` (and thus every Sender) drops.
                for job in rx {
                    match job {
                        Job::Apply(requests, reply) => {
                            let _ = reply.send(agent.apply(&requests));
                        }
                        Job::SaveState(path, reply) => {
                            let _ = reply.send(agent.save_state(path));
                        }
                        Job::QueryMetrics(options, reply) => {
                            let _ = reply.send(agent.query_metrics(options));
                        }
                    }
                }
                debug!("sozu-agent worker thread exiting");
            })?;
        Ok(Self { tx })
    }

    /// Apply a batch of requests, awaiting Sōzu's acks.
    pub async fn apply(&self, requests: Vec<Request>) -> Result<(), SozuError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job::Apply(requests, reply_tx))
            .map_err(|_| SozuError::WorkerGone)?;
        reply_rx.await.map_err(|_| SozuError::WorkerGone)?
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_batch_does_not_connect() {
        // No socket exists; an empty batch must succeed without touching it.
        let mut agent = SozuAgent::new("/nonexistent/sozu.sock");
        assert!(agent.apply(&[]).is_ok());
        assert!(agent.channel.is_none());
    }

    #[test]
    fn removes_are_recognized_for_failure_tolerance() {
        let remove: Request = RequestType::RemoveCluster("c".to_string()).into();
        assert!(is_remove(&remove), "RemoveCluster must be a remove");
        let status: Request = RequestType::Status(Status {}).into();
        assert!(
            !is_remove(&status),
            "non-remove verbs must not be tolerated"
        );
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

    #[tokio::test]
    async fn handle_reports_connection_error() {
        let handle = SozuAgentHandle::spawn("/nonexistent/sozu.sock").expect("spawn");
        let err = handle
            .apply(vec![RequestType::Status(Status {}).into()])
            .await
            .unwrap_err();
        assert!(matches!(err, SozuError::Channel(_)), "got {err:?}");
    }
}
