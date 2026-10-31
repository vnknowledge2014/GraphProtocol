//! Newline-delimited JSON-RPC over a duplex byte stream (`CMP-010`, `CMP-012`).
//!
//! One reader task matches replies to requests by id; one writer task owns the output. A reply is matched by id, never by arrival order.
//! EOF or a fatal frame fails every pending request. Server-initiated requests are refused (`-32601`), except the legacy `ping`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use super::{CallOptions, HeaderHints, McpTransport, Reply};
use crate::outbound::error::McpError;
use crate::outbound::jsonrpc::{
    Incoming, decode_frame, encode_empty_reply, encode_error_reply, encode_notification,
    encode_request, numeric_id,
};

type Pending = HashMap<u64, oneshot::Sender<Result<Value, McpError>>>;

struct Inner {
    next_id: AtomicU64,
    pending: Mutex<Pending>,
    out: mpsc::Sender<String>,
    closed: AtomicBool,
    refused_requests: AtomicU64,
    ignored_frames: AtomicU64,
}

impl Inner {
    fn lock_pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fail_all(&self, error: &McpError) {
        self.closed.store(true, Ordering::Release);
        let drained: Vec<_> = self.lock_pending().drain().collect();
        for (_, tx) in drained {
            let _ = tx.send(Err(error.clone()));
        }
    }
}

/// Removes the pending entry of a request and tells the server it was abandoned, unless the reply arrived.
struct PendingGuard {
    inner: Arc<Inner>,
    id: u64,
    armed: bool,
}

impl PendingGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let existed = self.inner.lock_pending().remove(&self.id).is_some();
        if existed && !self.inner.closed.load(Ordering::Acquire) {
            let line = encode_notification(
                "notifications/cancelled",
                &json!({"requestId": self.id, "reason": "client cancelled"}),
            );
            let _ = self.inner.out.try_send(line);
        }
    }
}

/// Stdio transport over any reader/writer pair (a child's pipes or `tokio::io::duplex`).
pub struct StdioTransport {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for StdioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioTransport")
            .field("closed", &self.inner.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl StdioTransport {
    /// Starts the reader and writer tasks (needs a Tokio runtime). `max_frame_bytes` bounds one line.
    pub fn new<R, W>(reader: R, writer: W, max_frame_bytes: usize) -> Self
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<String>(64);
        let inner = Arc::new(Inner {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            out: tx,
            closed: AtomicBool::new(false),
            refused_requests: AtomicU64::new(0),
            ignored_frames: AtomicU64::new(0),
        });
        tokio::spawn(writer_task(Arc::clone(&inner), writer, rx));
        tokio::spawn(reader_task(Arc::clone(&inner), reader, max_frame_bytes));
        Self { inner }
    }

    /// Server-initiated requests that were refused (sampling, roots, ...).
    #[must_use]
    pub fn refused_requests(&self) -> u64 {
        self.inner.refused_requests.load(Ordering::Relaxed)
    }

    /// Frames that could not be decoded or matched and were ignored.
    #[must_use]
    pub fn ignored_frames(&self) -> u64 {
        self.inner.ignored_frames.load(Ordering::Relaxed)
    }

    /// Number of requests still waiting for a reply.
    #[must_use]
    pub fn pending_requests(&self) -> usize {
        self.inner.lock_pending().len()
    }

    /// `true` once the peer closed the stream or a fatal error occurred.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }
}

async fn writer_task<W: AsyncWrite + Unpin>(
    inner: Arc<Inner>,
    mut writer: W,
    mut rx: mpsc::Receiver<String>,
) {
    while let Some(line) = rx.recv().await {
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
            inner.fail_all(&McpError::Closed);
            return;
        }
    }
}

/// Reads one line of at most `max` bytes; `Ok(None)` at EOF; an over-long line is fatal.
async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> Result<Option<()>, McpError> {
    buf.clear();
    loop {
        let available = reader.fill_buf().await.map_err(|_| McpError::Closed)?;
        if available.is_empty() {
            return Ok(if buf.is_empty() { None } else { Some(()) });
        }
        if let Some(pos) = available.iter().position(|b| *b == b'\n') {
            if buf.len() + pos > max {
                return Err(McpError::FrameTooLarge);
            }
            buf.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            return Ok(Some(()));
        }
        if buf.len() + available.len() > max {
            return Err(McpError::FrameTooLarge);
        }
        let n = available.len();
        buf.extend_from_slice(available);
        reader.consume(n);
    }
}

async fn reader_task<R: AsyncBufRead + Unpin>(inner: Arc<Inner>, mut reader: R, max: usize) {
    let mut buf = Vec::new();
    loop {
        match read_bounded_line(&mut reader, &mut buf, max).await {
            Ok(Some(())) => {}
            Ok(None) => {
                inner.fail_all(&McpError::Closed);
                return;
            }
            Err(error) => {
                inner.fail_all(&error);
                return;
            }
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match decode_frame(&buf, max) {
            Ok(Incoming::Response { id, outcome }) => {
                let tx = numeric_id(&id).and_then(|n| inner.lock_pending().remove(&n));
                match tx {
                    Some(tx) => {
                        let _ = tx.send(outcome);
                    }
                    None => {
                        inner.ignored_frames.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Ok(Incoming::Request { id, method }) => {
                let line = if method == "ping" {
                    encode_empty_reply(&id)
                } else {
                    inner.refused_requests.fetch_add(1, Ordering::Relaxed);
                    encode_error_reply(&id, -32601, "method not supported by this client")
                };
                let _ = inner.out.try_send(line);
            }
            Ok(Incoming::Notification { .. }) | Err(_) => {
                inner.ignored_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl McpTransport for StdioTransport {
    async fn request(
        &self,
        method: &str,
        params: Value,
        _hints: HeaderHints,
        opts: CallOptions,
    ) -> Result<Reply, McpError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(McpError::Closed);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::AcqRel);
        let (tx, rx) = oneshot::channel();
        self.inner.lock_pending().insert(id, tx);
        let mut guard = PendingGuard {
            inner: Arc::clone(&self.inner),
            id,
            armed: true,
        };
        let line = encode_request(id, method, &params);
        let out = self.inner.out.clone();
        let work = async move {
            out.send(line).await.map_err(|_| McpError::Closed)?;
            rx.await.map_err(|_| McpError::Closed)?
        };
        let result = tokio::select! {
            biased;
            () = opts.cancel.cancelled() => Err(McpError::Cancelled),
            () = tokio::time::sleep(opts.timeout) => Err(McpError::Timeout),
            res = work => res,
        };
        if result.is_ok() || matches!(result, Err(McpError::Rpc { .. } | McpError::Closed)) {
            guard.disarm();
        }
        result.map(|value| Reply {
            value,
            session_id: None,
        })
    }

    async fn notify(
        &self,
        method: &str,
        params: Value,
        _hints: HeaderHints,
    ) -> Result<(), McpError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(McpError::Closed);
        }
        self.inner
            .out
            .send(encode_notification(method, &params))
            .await
            .map_err(|_| McpError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{BufReader, duplex};
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn opts(ms: u64) -> CallOptions {
        CallOptions {
            timeout: Duration::from_millis(ms),
            cancel: CancellationToken::new(),
        }
    }

    async fn pair() -> (
        StdioTransport,
        BufReader<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        // client side: (reader of server->client, writer of client->server)
        let (client_w, server_r) = duplex(64 * 1024);
        let (server_w, client_r) = duplex(64 * 1024);
        let transport = StdioTransport::new(BufReader::new(client_r), client_w, 1 << 20);
        (transport, BufReader::new(server_r), server_w)
    }

    async fn read_line(r: &mut BufReader<tokio::io::DuplexStream>) -> Value {
        let mut line = String::new();
        r.read_line(&mut line).await.expect("read");
        serde_json::from_str(&line).expect("json line")
    }

    // spec: CMP-010, CMP-012
    #[tokio::test(start_paused = true)]
    async fn outbound_stdio_replies_are_matched_by_id_not_by_arrival_order() {
        let (t, mut srv_r, mut srv_w) = pair().await;
        let t = Arc::new(t);
        let a = tokio::spawn({
            let t = Arc::clone(&t);
            async move {
                t.request("a", json!({}), HeaderHints::default(), opts(5_000))
                    .await
            }
        });
        let b = tokio::spawn({
            let t = Arc::clone(&t);
            async move {
                t.request("b", json!({}), HeaderHints::default(), opts(5_000))
                    .await
            }
        });
        let first = read_line(&mut srv_r).await;
        let second = read_line(&mut srv_r).await;
        let id_of = |v: &Value, m: &str| {
            if v["method"] == json!(m) {
                Some(v["id"].clone())
            } else {
                None
            }
        };
        let (id_a, id_b) = match (
            id_of(&first, "a"),
            id_of(&second, "b"),
            id_of(&first, "b"),
            id_of(&second, "a"),
        ) {
            (Some(a), Some(b), _, _) | (_, _, Some(b), Some(a)) => (a, b),
            _ => panic!("unexpected requests {first} {second}"),
        };
        // answer b first, then a
        for (id, text) in [(id_b, "for-b"), (id_a, "for-a")] {
            srv_w
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":id,"result":{"who":text}})
                    )
                    .as_bytes(),
                )
                .await
                .expect("write");
        }
        assert_eq!(
            a.await.expect("join").expect("a ok").value["who"],
            json!("for-a")
        );
        assert_eq!(
            b.await.expect("join").expect("b ok").value["who"],
            json!("for-b")
        );
        assert_eq!(t.pending_requests(), 0);
    }

    // spec: CMP-012
    #[tokio::test(start_paused = true)]
    async fn outbound_stdio_timeout_and_cancel_send_notifications_cancelled_with_the_request_id() {
        let (t, mut srv_r, _srv_w) = pair().await;
        let err = t
            .request("slow", json!({}), HeaderHints::default(), opts(50))
            .await
            .expect_err("timeout");
        assert_eq!(err, McpError::Timeout);
        let req = read_line(&mut srv_r).await;
        let note = read_line(&mut srv_r).await;
        assert_eq!(note["method"], json!("notifications/cancelled"));
        assert_eq!(note["params"]["requestId"], req["id"]);
        assert_eq!(t.pending_requests(), 0);

        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        let o = CallOptions {
            timeout: Duration::from_secs(60),
            cancel,
        };
        let h = tokio::spawn(async move {
            t.request("slow2", json!({}), HeaderHints::default(), o)
                .await
        });
        let req2 = read_line(&mut srv_r).await;
        c2.cancel();
        assert_eq!(
            h.await.expect("join").expect_err("cancel"),
            McpError::Cancelled
        );
        let note2 = read_line(&mut srv_r).await;
        assert_eq!(note2["params"]["requestId"], req2["id"]);
    }

    // spec: CMP-012
    #[tokio::test(start_paused = true)]
    async fn outbound_stdio_dropping_the_future_cancels_and_forgets_the_request() {
        let (t, mut srv_r, _srv_w) = pair().await;
        {
            let fut = t.request("dropme", json!({}), HeaderHints::default(), opts(60_000));
            tokio::pin!(fut);
            tokio::select! { _ = &mut fut => panic!("no reply expected"), () = tokio::time::sleep(Duration::from_millis(10)) => {} }
        }
        let req = read_line(&mut srv_r).await;
        let note = read_line(&mut srv_r).await;
        assert_eq!(note["params"]["requestId"], req["id"]);
        assert_eq!(t.pending_requests(), 0);
    }

    // spec: CMP-010
    #[tokio::test(start_paused = true)]
    async fn outbound_stdio_eof_fails_pending_calls_and_later_calls() {
        let (t, mut srv_r, srv_w) = pair().await;
        let t = Arc::new(t);
        let h = tokio::spawn({
            let t = Arc::clone(&t);
            async move {
                t.request("x", json!({}), HeaderHints::default(), opts(60_000))
                    .await
            }
        });
        let _ = read_line(&mut srv_r).await;
        drop(srv_w); // server closes stdout
        assert_eq!(
            h.await.expect("join").expect_err("closed"),
            McpError::Closed
        );
        assert!(t.is_closed());
        assert_eq!(
            t.request("y", json!({}), HeaderHints::default(), opts(100))
                .await
                .expect_err("closed"),
            McpError::Closed
        );
    }

    proptest::proptest! {
        // spec: CMP-010 (a reply is matched by id whatever the order in which the server answers)
        #[test]
        fn outbound_stdio_id_matching_survives_any_reply_permutation(order in proptest::collection::vec(0_u32..1000, 2..8)) {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            runtime.block_on(async {
                let (t, mut srv_r, mut srv_w) = pair().await;
                let t = Arc::new(t);
                let n = order.len();
                let mut calls = Vec::new();
                for i in 0..n {
                    let t = Arc::clone(&t);
                    calls.push(tokio::spawn(async move { t.request("m", json!({"i": i}), HeaderHints::default(), opts(60_000)).await }));
                }
                let mut seen: Vec<(Value, u64)> = Vec::new();
                for _ in 0..n {
                    let req = read_line(&mut srv_r).await;
                    seen.push((req["id"].clone(), req["params"]["i"].as_u64().expect("i")));
                }
                // answer in the order given by sorting the random keys
                let mut idx: Vec<usize> = (0..n).collect();
                idx.sort_by_key(|k| (order[*k], *k));
                for k in idx {
                    let (id, i) = &seen[k];
                    srv_w.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":id,"result":{"i":i}})).as_bytes()).await.expect("write");
                }
                for (i, call) in calls.into_iter().enumerate() {
                    let reply = call.await.expect("join").expect("reply");
                    proptest::prop_assert_eq!(reply.value["i"].as_u64(), Some(i as u64));
                }
                Ok(())
            })?;
        }
    }

    // spec: CMP-010, CMP-013
    #[tokio::test(start_paused = true)]
    async fn outbound_stdio_server_requests_are_refused_except_ping_and_oversized_frames_are_fatal()
    {
        let (t, mut srv_r, mut srv_w) = pair().await;
        srv_w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"sampling/createMessage\",\"params\":{}}\n").await.expect("w");
        srv_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"ping\"}\n")
            .await
            .expect("w");
        let refusal = read_line(&mut srv_r).await;
        assert_eq!(refusal["id"], json!(9));
        assert_eq!(refusal["error"]["code"], json!(-32601));
        let pong = read_line(&mut srv_r).await;
        assert_eq!(pong["id"], json!(10));
        assert!(pong.get("result").is_some());
        assert_eq!(t.refused_requests(), 1);

        let h = tokio::spawn({
            let t = Arc::new(t);
            async move {
                t.request("z", json!({}), HeaderHints::default(), opts(60_000))
                    .await
            }
        });
        let _ = read_line(&mut srv_r).await;
        let huge = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"{}\"}}\n",
            "x".repeat(2 << 20)
        );
        // the reader gives up after the cap, so the write may fail with a broken pipe
        let _ = srv_w.write_all(huge.as_bytes()).await;
        assert_eq!(
            h.await.expect("join").expect_err("fatal"),
            McpError::FrameTooLarge
        );
    }
}
