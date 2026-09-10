//! Fixed loopback responses for HTTPRoute backend errors.
//!
//! Sōzu forwards the response normally, including conversion for HTTP/2 clients.
//! A dedicated runtime keeps this backend responsive during CPU-bound builds.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderValue, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::health::READ_TIMEOUT;

const BODY: &[u8] = b"Internal Server Error\n";
const UNAVAILABLE_BODY: &[u8] = b"Service Unavailable\n";
// Limits apply separately to the 500 and 503 listeners.
const MAX_CONNECTIONS: usize = 256;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone, Copy)]
pub enum ResponseCode {
    InternalServerError,
    ServiceUnavailable,
}

impl ResponseCode {
    fn status(self) -> StatusCode {
        match self {
            Self::InternalServerError => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn body(self) -> &'static [u8] {
        match self {
            Self::InternalServerError => BODY,
            Self::ServiceUnavailable => UNAVAILABLE_BODY,
        }
    }
}

pub struct ErrorResponder {
    address: SocketAddr,
    finished: oneshot::Receiver<Result<()>>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ErrorResponder {
    /// Bind synchronously before the controller starts caches or readiness.
    pub fn start(address: SocketAddr, response: ResponseCode) -> Result<Self> {
        let listener = StdTcpListener::bind(address)
            .with_context(|| format!("binding HTTP error backend at {address}"))?;
        let address = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("creating HTTP error backend runtime")?;
        let (finished_tx, finished) = oneshot::channel();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("http-error-backend".into())
            .spawn(move || {
                let result = runtime.block_on(async {
                    let listener = TcpListener::from_std(listener)?;
                    tokio::select! {
                        result = serve(listener, response) => result,
                        _ = shutdown_rx => Ok(()),
                    }
                });
                let _ = finished_tx.send(result);
            })
            .context("starting HTTP error backend thread")?;
        Ok(Self {
            address,
            finished,
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// A required backend cannot disappear while the controller stays healthy.
    /// A panic also closes this channel, so it is observed by the supervisor.
    pub async fn wait(&mut self) -> Result<()> {
        (&mut self.finished)
            .await
            .context("HTTP error backend thread stopped without a result")?
            .context("HTTP error backend stopped")?;
        anyhow::bail!("HTTP error backend exited unexpectedly")
    }
}

impl Drop for ErrorResponder {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn serve(listener: TcpListener, response: ResponseCode) -> Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                match accepted {
                    Ok((stream, _)) => { connections.spawn(serve_one(stream, response)); }
                    Err(error) => {
                        // Resource exhaustion can be temporary and affects the
                        // entire process. Avoid turning a failed accept into a
                        // restart of otherwise healthy Kubernetes watches.
                        warn!(%error, "accepting HTTP error backend connection failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    warn!(%error, "HTTP error backend connection task failed");
                }
            }
        }
    }
}

async fn serve_one(stream: TcpStream, response: ResponseCode) {
    let mut builder = http1::Builder::new();
    builder
        .keep_alive(false)
        .max_buf_size(MAX_HEADER_BYTES)
        .timer(TokioTimer::new())
        .header_read_timeout(READ_TIMEOUT);
    // Bound the entire connection, including fragmented headers, streaming
    // bodies and a client that stops reading the response. The body budget is
    // separate from the short header timeout and remains below Sōzu's default
    // 30-second backend timeout.
    let connection = builder.serve_connection(
        TokioIo::new(stream),
        service_fn(move |request| respond(request, response)),
    );
    match tokio::time::timeout(CONNECTION_TIMEOUT, connection).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => debug!(%error, "HTTP error backend connection rejected"),
        Err(_) => debug!("HTTP error backend connection timed out"),
    }
}

async fn respond(
    mut request: Request<Incoming>,
    code: ResponseCode,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Sōzu streams request bodies. Closing before consuming them can reset its
    // backend connection before it reads the 500, or leave client bytes unread.
    // Drain frames without accumulating them; serve_one bounds the total time.
    while let Some(frame) = request.body_mut().frame().await {
        frame?;
    }
    let mut response = Response::new(Full::new(Bytes::from_static(code.body())));
    *response.status_mut() = code.status();
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(CONTENT_LENGTH, code.body().len().into());
    headers.insert(CONNECTION, HeaderValue::from_static("close"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn responder() -> ErrorResponder {
        ErrorResponder::start(
            "127.0.0.1:0".parse().unwrap(),
            ResponseCode::InternalServerError,
        )
        .unwrap()
    }

    async fn response(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut bytes))
            .await
            .expect("response deadline")
            .expect("read response");
        String::from_utf8(bytes).unwrap()
    }

    fn assert_500(response: &str, head: bool) {
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(headers.lines().next().unwrap().contains(" 500 "));
        let headers = headers.to_ascii_lowercase();
        assert!(headers.lines().any(|line| line == "content-length: 22"));
        assert!(headers.contains("connection: close"));
        assert_eq!(body.as_bytes(), if head { b"" } else { BODY });
    }

    #[test]
    fn responder_does_not_need_the_controller_runtime() {
        use std::io::{Read, Write};
        let server = responder();
        let mut stream = std::net::TcpStream::connect(server.address()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(b"GET /anything HTTP/1.1\r\nHost: example.org\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert_500(&response, false);
    }

    #[tokio::test]
    async fn every_method_and_path_gets_a_framed_500() {
        let server = responder();
        for (method, path, version) in [
            ("GET", "/arbitrary?q=1", "1.1"),
            ("HEAD", "/", "1.1"),
            ("DELETE", "/resource", "1.0"),
            ("OPTIONS", "*", "1.1"),
            ("PATCH", "/resource", "1.1"),
        ] {
            let mut stream = TcpStream::connect(server.address()).await.unwrap();
            let request = format!(
                "{method} {path} HTTP/{version}\r\nHost: example.org\r\nContent-Length: 0\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            assert_500(&response(&mut stream).await, method == "HEAD");
        }
    }

    #[tokio::test]
    async fn unavailable_backend_keeps_its_503_body_and_does_not_change_500s() {
        let invalid = responder();
        let unavailable = ErrorResponder::start(
            "127.0.0.1:0".parse().unwrap(),
            ResponseCode::ServiceUnavailable,
        )
        .unwrap();
        for method in ["GET", "HEAD", "POST"] {
            let mut stream = TcpStream::connect(unavailable.address()).await.unwrap();
            let request = format!("{method} /any/path HTTP/1.1\r\nHost: example.org\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nbody\r\n0\r\n\r\n");
            stream.write_all(request.as_bytes()).await.unwrap();
            let response = response(&mut stream).await;
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            assert!(headers.starts_with("HTTP/1.1 503 "));
            assert!(headers.to_ascii_lowercase().contains("content-length: 20"));
            assert_eq!(
                body.as_bytes(),
                if method == "HEAD" {
                    b""
                } else {
                    UNAVAILABLE_BODY
                }
            );
        }
        let mut stream = TcpStream::connect(invalid.address()).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: example.org\r\n\r\n")
            .await
            .unwrap();
        assert_500(&response(&mut stream).await, false);
    }

    #[tokio::test]
    async fn fragmented_headers_and_content_length_body_are_drained() {
        let server = responder();
        let mut stream = TcpStream::connect(server.address()).await.unwrap();
        stream.set_nodelay(true).unwrap();
        stream
            .write_all(b"POST /upload HTTP/1.1\r\nHo")
            .await
            .unwrap();
        tokio::task::yield_now().await;
        stream
            .write_all(b"st: example.org\r\nContent-Length: 12\r\n\r\nhello ")
            .await
            .unwrap();
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), stream.read(&mut byte))
                .await
                .is_err(),
            "no final response before consuming the complete request body"
        );
        stream.write_all(b"world!").await.unwrap();
        assert_500(&response(&mut stream).await, false);
    }

    #[tokio::test]
    async fn chunked_bodies_and_trailers_are_drained() {
        let server = responder();
        let mut stream = TcpStream::connect(server.address()).await.unwrap();
        stream.write_all(b"POST /upload HTTP/1.1\r\nHost: example.org\r\nTransfer-Encoding: chunked\r\nTrailer: X-Example\r\n\r\n").await.unwrap();
        for fragment in [
            b"3\r\none\r\n".as_slice(),
            b"3\r\ntwo\r\n",
            b"0\r\nX-Example: done\r\n\r\n",
        ] {
            stream.write_all(fragment).await.unwrap();
        }
        assert_500(&response(&mut stream).await, false);
    }

    #[tokio::test]
    async fn request_bodies_are_not_limited_to_the_header_buffer() {
        let server = responder();
        let mut stream = TcpStream::connect(server.address()).await.unwrap();
        let block = vec![b'x'; MAX_HEADER_BYTES];
        let request = format!(
            "POST /upload HTTP/1.1\r\nHost: example.org\r\nContent-Length: {}\r\n\r\n",
            block.len() * 16
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        for _ in 0..16 {
            stream.write_all(&block).await.unwrap();
        }
        assert_500(&response(&mut stream).await, false);
    }

    #[tokio::test]
    async fn expect_continue_is_answered_before_draining_the_body() {
        let server = responder();
        let mut stream = TcpStream::connect(server.address()).await.unwrap();
        stream.write_all(b"POST /upload HTTP/1.1\r\nHost: example.org\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n").await.unwrap();
        let mut interim = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !interim.ends_with(b"\r\n\r\n") {
                interim.push(stream.read_u8().await.unwrap());
            }
        })
        .await
        .unwrap();
        assert_eq!(interim, b"HTTP/1.1 100 Continue\r\n\r\n");
        stream.write_all(b"body").await.unwrap();
        assert_500(&response(&mut stream).await, false);
    }

    #[tokio::test]
    async fn body_drain_can_outlive_the_header_deadline() {
        let server = responder();
        let mut stream = TcpStream::connect(server.address()).await.unwrap();
        stream
            .write_all(b"POST / HTTP/1.1\r\nHost: example.org\r\nContent-Length: 4\r\n\r\nbo")
            .await
            .unwrap();
        tokio::time::sleep(READ_TIMEOUT + Duration::from_secs(1)).await;
        stream.write_all(b"dy").await.unwrap();
        assert_500(&response(&mut stream).await, false);
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_requests_cannot_hold_a_connection_forever() {
        for request in [
            b"".as_slice(),
            b"POST / HTTP/1.1\r\nHost: example.org\r\nContent-Length: 100\r\n\r\none",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let task = tokio::spawn(serve_one(stream, ResponseCode::InternalServerError));
            client.write_all(request).await.unwrap();
            tokio::task::yield_now().await;
            tokio::time::advance(CONNECTION_TIMEOUT).await;
            task.await.unwrap();
            let mut bytes = Vec::new();
            let _ = client.read_to_end(&mut bytes).await;
            assert!(!String::from_utf8_lossy(&bytes).contains(" 500 "));
        }
    }

    #[tokio::test]
    async fn malformed_requests_are_rejected_by_the_http_parser() {
        let server = responder();
        let mut stream = TcpStream::connect(server.address()).await.unwrap();
        stream.write_all(b"not a request\r\n\r\n").await.unwrap();
        assert!(response(&mut stream).await.starts_with("HTTP/1.1 400 "));
    }

    #[test]
    fn an_occupied_backend_port_is_a_startup_error() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        assert!(ErrorResponder::start(
            listener.local_addr().unwrap(),
            ResponseCode::InternalServerError
        )
        .is_err());
    }

    #[tokio::test]
    async fn backend_exit_is_fatal_to_its_supervisor() {
        let mut server = responder();
        server.shutdown.take().unwrap().send(()).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(2), server.wait())
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("exited unexpectedly"));
    }
}
