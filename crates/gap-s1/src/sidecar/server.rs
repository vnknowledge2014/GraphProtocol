//! Local HTTP/1.1 golden-oracle sidecar server (`POST /v1/systemone`, `POST /v1/systemone/batch`)
//! for deterministic integration tests and `omni decide --endpoint` emulation (`WP-M5-06`).

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_route::DecisionBackend;

use crate::candle::json::parse_json;
use crate::sidecar::protocol::{
    DEFAULT_MAX_RESPONSE_BYTES, MAX_HTTP_HEADER_BYTES, SystemOneBatchResponse, SystemOneRequest,
    SystemOneResponse,
};

/// Single-process loopback `/v1/systemone` HTTP server backed by any [`DecisionBackend`].
#[derive(Debug)]
pub struct LoopbackSidecarServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LoopbackSidecarServer {
    /// Binds to `127.0.0.1:0` and begins serving `/v1/systemone` and `/v1/systemone/batch` using `backend`.
    ///
    /// # Errors
    ///
    /// Returns [`GapError`] if binding the loopback socket or setting non-blocking mode fails.
    pub fn start(backend: Arc<dyn DecisionBackend>) -> GapResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("failed to bind loopback sidecar server: {err}"),
            )
        })?;
        listener.set_nonblocking(true).map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("failed to configure non-blocking loopback listener: {err}"),
            )
        })?;
        let addr = listener.local_addr().map_err(|err| {
            GapError::new(
                ErrorCode::DiscoveryDegraded,
                format!("failed to read loopback listener address: {err}"),
            )
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        let _ = handle_connection(stream, backend.as_ref());
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            addr,
            stop,
            handle: Some(handle),
        })
    }

    /// Returns the base HTTP URL (`http://127.0.0.1:<port>`).
    #[must_use]
    pub fn endpoint_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Returns the socket address bound by this oracle server.
    #[must_use]
    pub const fn socket_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for LoopbackSidecarServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(50));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(mut stream: TcpStream, backend: &dyn DecisionBackend) -> GapResult<()> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let Some((method, path, body)) = read_http_request(&mut stream)? else {
        return Ok(());
    };

    let (status, status_text, resp_body) = match (method.as_str(), path.as_str()) {
        ("GET", "/healthz") => (200u16, "OK", "{\"status\":\"ok\"}".to_owned()),
        ("POST", "/v1/systemone") => match handle_single(&body, backend) {
            Ok(json) => (200, "OK", json),
            Err(err) => (
                400,
                "Bad Request",
                format!(
                    "{{\"error\":\"{}\"}}",
                    crate::candle::json::escape_json_str(&err.to_string())
                ),
            ),
        },
        ("POST", "/v1/systemone/batch") => match handle_batch(&body, backend) {
            Ok(json) => (200, "OK", json),
            Err(err) => (
                400,
                "Bad Request",
                format!(
                    "{{\"error\":\"{}\"}}",
                    crate::candle::json::escape_json_str(&err.to_string())
                ),
            ),
        },
        _ => (404, "Not Found", "{\"error\":\"not_found\"}".to_owned()),
    };

    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
        resp_body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    Ok(())
}

fn handle_single(body: &str, backend: &dyn DecisionBackend) -> GapResult<String> {
    let parsed = parse_json(body)?;
    let req = SystemOneRequest::from_json_value(&parsed)?;
    let logits = block_on_backend(
        backend,
        &req.state_jcs,
        &req.question,
        &req.options,
        Duration::from_secs(2),
    )?;
    Ok(SystemOneResponse::to_json_string(
        backend.model_id(),
        &logits,
    ))
}

fn handle_batch(body: &str, backend: &dyn DecisionBackend) -> GapResult<String> {
    let parsed = parse_json(body)?;
    let req_arr = parsed
        .get("requests")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            GapError::new(
                ErrorCode::ValidationError,
                "missing `requests` array in /v1/systemone/batch",
            )
        })?;
    let mut logits_batch = Vec::with_capacity(req_arr.len());
    for entry in req_arr {
        let req = SystemOneRequest::from_json_value(entry)?;
        let logits = block_on_backend(
            backend,
            &req.state_jcs,
            &req.question,
            &req.options,
            Duration::from_secs(2),
        )?;
        logits_batch.push(logits);
    }
    Ok(SystemOneBatchResponse::to_json_string(
        backend.model_id(),
        &logits_batch,
    ))
}

fn block_on_backend(
    backend: &dyn DecisionBackend,
    state_jcs: &str,
    question: &str,
    options: &[gap_route::GroupOption],
    timeout: Duration,
) -> GapResult<Vec<f32>> {
    let mut fut = backend.score_choice_with_timeout(state_jcs, question, options, timeout);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let deadline = Instant::now() + timeout;

    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(res) => return res,
            Poll::Pending => {
                if Instant::now() >= deadline {
                    return Err(GapError::new(
                        ErrorCode::DiscoveryDegraded,
                        "oracle backend timed out polling future",
                    ));
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> GapResult<Option<(String, String, String)>> {
    let mut header_buf = Vec::with_capacity(512);
    let mut b = [0u8; 1];
    while header_buf.len() < MAX_HTTP_HEADER_BYTES {
        match stream.read(&mut b) {
            Ok(0) => {
                if header_buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Ok(_) => {
                header_buf.push(b[0]);
                if header_buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return Ok(None),
        }
    }
    if !header_buf.ends_with(b"\r\n\r\n") {
        return Ok(None);
    }
    let header_str = std::str::from_utf8(&header_buf).map_err(|_| {
        GapError::new(
            ErrorCode::ValidationError,
            "sidecar HTTP request headers not valid UTF-8",
        )
    })?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();

    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, val)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = val.trim().parse::<usize>().unwrap_or(0);
        }
    }
    if content_length > DEFAULT_MAX_RESPONSE_BYTES {
        return Err(GapError::new(
            ErrorCode::ResponseTooLarge,
            "sidecar request body exceeded cap",
        ));
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).map_err(|err| {
            GapError::new(
                ErrorCode::ValidationError,
                format!("failed to read HTTP request body: {err}"),
            )
        })?;
    }
    let body_str = String::from_utf8(body).map_err(|_| {
        GapError::new(
            ErrorCode::ValidationError,
            "sidecar HTTP request body not valid UTF-8",
        )
    })?;
    Ok(Some((method, path, body_str)))
}
