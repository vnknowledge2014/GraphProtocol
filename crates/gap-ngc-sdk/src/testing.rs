//! Socket-free in-memory `tokio::io::duplex` harnesses for gRPC and stdio (`NGC-001`, `NGC-003`, `NGC-090`).

use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::{Stream, StreamExt as _};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::ngc::v1::{
    CancelTaskRequest, DescribeRequest, DescribeResponse, Event, ExplainResponse, GetTaskRequest,
    HealthRequest, HealthResponse, MutateRequest, MutateResponse, NgcError, QueryRequest,
    QueryResponse, SketchRequest, SketchResponse, StartTaskRequest, SubscribeRequest, TaskStatus,
    WatchTaskRequest,
};
use http::Uri;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, duplex};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::codegen::Service;
use tonic::transport::Endpoint;

use crate::connector::{BoxStream, Connector, NgcResult};
use crate::core::ServerCore;
use crate::error::ngc_error_from_proto;
use crate::grpc::{GrpcClient, ServeError, serve_incoming_with_shutdown};
use crate::json::{ProtoMessage, from_json_value, to_json_value};
use crate::stdio::serve_stdio;

const DUPLEX_BUF_BYTES: usize = 64 * 1024;

#[derive(Clone)]
struct DuplexConnector {
    tx: mpsc::Sender<DuplexStream>,
}

impl Service<Uri> for DuplexConnector {
    type Response = TokioIo<DuplexStream>;
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Uri) -> Self::Future {
        let tx = self.tx.clone();
        Box::pin(async move {
            let (client_io, server_io) = duplex(DUPLEX_BUF_BYTES);
            tx.send(server_io).await.map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "in-memory gRPC server closed",
                )
            })?;
            Ok(TokioIo::new(client_io))
        })
    }
}

/// RAII handle owning the background in-memory gRPC server task.
///
/// Dropping [`InMemoryGrpcHandle`] signals shutdown and aborts the background server task.
pub struct InMemoryGrpcHandle {
    shutdown: CancellationToken,
    task: Option<JoinHandle<Result<(), ServeError>>>,
}

impl InMemoryGrpcHandle {
    /// Gracefully shuts down the in-memory gRPC server and waits for its task to exit.
    ///
    /// # Errors
    ///
    /// Returns [`ServeError`] if the underlying server exited with an error.
    pub async fn shutdown(mut self) -> Result<(), ServeError> {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|e| ServeError::Io(std::io::Error::other(e)))??;
        }
        Ok(())
    }
}

impl Drop for InMemoryGrpcHandle {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Spawns `core` on an in-memory `tokio::io::duplex` transport without opening any OS socket.
#[must_use]
pub fn in_memory_grpc<C: Connector>(core: Arc<ServerCore<C>>) -> (GrpcClient, InMemoryGrpcHandle) {
    let (tx, rx) = mpsc::channel::<DuplexStream>(16);
    let shutdown = CancellationToken::new();
    let shutdown_sig = shutdown.clone();
    let incoming = ReceiverStream::new(rx).map(Ok::<_, std::io::Error>);
    let task = tokio::spawn(async move {
        serve_incoming_with_shutdown(core, incoming, shutdown_sig.cancelled_owned()).await
    });

    let channel = Endpoint::from_static("http://[::]:50051")
        .connect_with_connector_lazy(DuplexConnector { tx });
    let client = GrpcClient::from_channel(channel);
    (
        client,
        InMemoryGrpcHandle {
            shutdown,
            task: Some(task),
        },
    )
}

/// Spawns `serve_stdio` over raw `tokio::io::duplex` streams, returning `(stdin_writer, stdout_reader, join_handle)`.
#[must_use]
pub fn in_memory_raw_stdio<C: Connector>(
    core: Arc<ServerCore<C>>,
) -> (DuplexStream, DuplexStream, JoinHandle<std::io::Result<()>>) {
    let (client_stdin_tx, server_stdin_rx) = duplex(DUPLEX_BUF_BYTES);
    let (server_stdout_tx, client_stdout_rx) = duplex(DUPLEX_BUF_BYTES);
    let join = tokio::spawn(async move {
        serve_stdio(core, BufReader::new(server_stdin_rx), server_stdout_tx).await
    });
    (client_stdin_tx, client_stdout_rx, join)
}

type UnaryWaiter = oneshot::Sender<NgcResult<serde_json::Value>>;
type StreamItemSender = mpsc::Sender<NgcResult<serde_json::Value>>;

#[derive(Default)]
struct StdioDemux {
    unary: HashMap<u64, UnaryWaiter>,
    streams: HashMap<u64, StreamItemSender>,
}

fn decode_jsonrpc_error_obj(err_obj: &serde_json::Value) -> NgcErrorInfo {
    if let Some(data) = err_obj.get("data")
        && let Ok(proto) = from_json_value::<NgcError>(data.clone())
    {
        return ngc_error_from_proto(&proto);
    }
    let code_num = err_obj
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-32603);
    let msg = err_obj
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("stdio JSON-RPC error");
    let ngc_code = match code_num {
        -32700 | -32600 | -32602 => NgcErrorCode::InvalidArgument,
        -32601 => NgcErrorCode::Unimplemented,
        _ => NgcErrorCode::Internal,
    };
    NgcErrorInfo::new(ngc_code, msg)
}

struct StdioClientStream<T> {
    id: u64,
    rx: mpsc::Receiver<NgcResult<serde_json::Value>>,
    write_tx: mpsc::Sender<String>,
    demux: Arc<Mutex<StdioDemux>>,
    finished: bool,
    _pd: PhantomData<fn() -> T>,
}

impl<T: ProtoMessage + Unpin> Stream for StdioClientStream<T> {
    type Item = NgcResult<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        match this.rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(err))) => {
                this.finished = true;
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(Some(Ok(val))) => match from_json_value::<T>(val) {
                Ok(msg) => Poll::Ready(Some(Ok(msg))),
                Err(err) => {
                    this.finished = true;
                    Poll::Ready(Some(Err(err)))
                }
            },
        }
    }
}

impl<T> Drop for StdioClientStream<T> {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.demux.lock() {
            guard.streams.remove(&self.id);
        }
        if !self.finished {
            let cancel_msg = format!(
                "{{\"jsonrpc\":\"2.0\",\"method\":\"gap.ngc.cancel\",\"params\":{{\"id\":{}}}}}",
                self.id
            );
            let _ = self.write_tx.try_send(cancel_msg);
        }
    }
}

/// RAII handle owning the background in-memory stdio server and client I/O tasks.
pub struct InMemoryStdioHandle {
    server_task: JoinHandle<std::io::Result<()>>,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

impl Drop for InMemoryStdioHandle {
    fn drop(&mut self) {
        self.writer_task.abort();
        self.reader_task.abort();
        self.server_task.abort();
    }
}

/// Multiplexed JSON-RPC 2.0 test client driving [`serve_stdio`] over `tokio::io::duplex`.
#[derive(Clone)]
pub struct StdioTestClient {
    next_id: Arc<AtomicU64>,
    write_tx: mpsc::Sender<String>,
    demux: Arc<Mutex<StdioDemux>>,
}

impl StdioTestClient {
    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn call_unary<Req: ProtoMessage, Resp: ProtoMessage>(
        &self,
        method: &str,
        req: Req,
    ) -> NgcResult<Resp> {
        let params = to_json_value(&req)?;
        let id = self.alloc_id();
        let (tx, rx) = oneshot::channel();
        if let Ok(mut guard) = self.demux.lock() {
            guard.unary.insert(id, tx);
        }
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&envelope).map_err(|e| {
            NgcErrorInfo::new(
                NgcErrorCode::Internal,
                format!("request JSON encode failed: {e}"),
            )
        })?;
        self.write_tx.send(line).await.map_err(|_| {
            NgcErrorInfo::new(NgcErrorCode::Unavailable, "stdio session writer closed")
        })?;
        let val = rx.await.map_err(|_| {
            NgcErrorInfo::new(
                NgcErrorCode::Unavailable,
                "stdio session closed before response",
            )
        })??;
        from_json_value(val)
    }

    async fn call_stream<Req: ProtoMessage, Item: ProtoMessage + Unpin + Send + 'static>(
        &self,
        method: &str,
        req: Req,
    ) -> NgcResult<BoxStream<Item>> {
        let params = to_json_value(&req)?;
        let id = self.alloc_id();
        let (tx, rx) = mpsc::channel(64);
        if let Ok(mut guard) = self.demux.lock() {
            guard.streams.insert(id, tx);
        }
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&envelope).map_err(|e| {
            NgcErrorInfo::new(
                NgcErrorCode::Internal,
                format!("request JSON encode failed: {e}"),
            )
        })?;
        self.write_tx.send(line).await.map_err(|_| {
            NgcErrorInfo::new(NgcErrorCode::Unavailable, "stdio session writer closed")
        })?;
        Ok(Box::pin(StdioClientStream {
            id,
            rx,
            write_tx: self.write_tx.clone(),
            demux: Arc::clone(&self.demux),
            finished: false,
            _pd: PhantomData,
        }))
    }

    /// Invokes `gap.ngc.Describe`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on JSON-RPC or connector failure.
    pub async fn describe(&self, request: DescribeRequest) -> NgcResult<DescribeResponse> {
        self.call_unary("gap.ngc.Describe", request).await
    }

    /// Invokes `gap.ngc.Query`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] if sending the request fails.
    pub async fn query(&self, request: QueryRequest) -> NgcResult<BoxStream<QueryResponse>> {
        self.call_stream("gap.ngc.Query", request).await
    }

    /// Invokes `gap.ngc.Mutate`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on JSON-RPC or connector failure.
    pub async fn mutate(&self, request: MutateRequest) -> NgcResult<MutateResponse> {
        self.call_unary("gap.ngc.Mutate", request).await
    }

    /// Invokes `gap.ngc.Subscribe`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] if sending the request fails.
    pub async fn subscribe(&self, request: SubscribeRequest) -> NgcResult<BoxStream<Event>> {
        self.call_stream("gap.ngc.Subscribe", request).await
    }

    /// Invokes `gap.ngc.StartTask`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on JSON-RPC or connector failure.
    pub async fn start_task(&self, request: StartTaskRequest) -> NgcResult<TaskStatus> {
        self.call_unary("gap.ngc.StartTask", request).await
    }

    /// Invokes `gap.ngc.GetTask`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on JSON-RPC or connector failure.
    pub async fn get_task(&self, request: GetTaskRequest) -> NgcResult<TaskStatus> {
        self.call_unary("gap.ngc.GetTask", request).await
    }

    /// Invokes `gap.ngc.CancelTask`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on JSON-RPC or connector failure.
    pub async fn cancel_task(&self, request: CancelTaskRequest) -> NgcResult<TaskStatus> {
        self.call_unary("gap.ngc.CancelTask", request).await
    }

    /// Invokes `gap.ngc.WatchTask`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] if sending the request fails.
    pub async fn watch_task(&self, request: WatchTaskRequest) -> NgcResult<BoxStream<TaskStatus>> {
        self.call_stream("gap.ngc.WatchTask", request).await
    }

    /// Invokes `gap.ngc.Sketch`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on JSON-RPC or connector failure.
    pub async fn sketch(&self, request: SketchRequest) -> NgcResult<SketchResponse> {
        self.call_unary("gap.ngc.Sketch", request).await
    }

    /// Invokes `gap.ngc.Health`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on JSON-RPC or connector failure.
    pub async fn health(&self, request: HealthRequest) -> NgcResult<HealthResponse> {
        self.call_unary("gap.ngc.Health", request).await
    }

    /// Invokes `gap.ngc.Explain`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on JSON-RPC or connector failure.
    pub async fn explain(&self, request: QueryRequest) -> NgcResult<ExplainResponse> {
        self.call_unary("gap.ngc.Explain", request).await
    }
}

/// Spawns `core` over an in-memory `tokio::io::duplex` stdio session and returns a [`StdioTestClient`].
#[must_use]
pub fn in_memory_stdio<C: Connector>(
    core: Arc<ServerCore<C>>,
) -> (StdioTestClient, InMemoryStdioHandle) {
    let (mut stdin_tx, stdout_rx, server_task) = in_memory_raw_stdio(core);
    let (write_tx, mut write_rx) = mpsc::channel::<String>(64);
    let demux = Arc::new(Mutex::new(StdioDemux::default()));
    let demux_reader = Arc::clone(&demux);

    let writer_task = tokio::spawn(async move {
        while let Some(mut line) = write_rx.recv().await {
            line.push('\n');
            if stdin_tx.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdin_tx.flush().await.is_err() {
                break;
            }
        }
    });

    let reader_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout_rx).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if val.get("method").and_then(serde_json::Value::as_str) == Some("gap.ngc.item") {
                let id = val
                    .get("params")
                    .and_then(|p| p.get("id"))
                    .and_then(serde_json::Value::as_u64);
                let item = val.get("params").and_then(|p| p.get("item")).cloned();
                if let (Some(id), Some(item)) = (id, item) {
                    let tx = demux_reader
                        .lock()
                        .ok()
                        .and_then(|g| g.streams.get(&id).cloned());
                    if let Some(tx) = tx {
                        let _ = tx.send(Ok(item)).await;
                    }
                }
                continue;
            }

            if let Some(res_val) = val.get("result") {
                if let Some(id) = val.get("id").and_then(serde_json::Value::as_u64) {
                    let unary_tx = demux_reader
                        .lock()
                        .ok()
                        .and_then(|mut g| g.unary.remove(&id));
                    if let Some(tx) = unary_tx {
                        let _ = tx.send(Ok(res_val.clone()));
                    } else if let Ok(mut g) = demux_reader.lock() {
                        g.streams.remove(&id);
                    }
                }
                continue;
            }

            if let Some(err_obj) = val.get("error") {
                let err = decode_jsonrpc_error_obj(err_obj);
                if let Some(id) = val.get("id").and_then(serde_json::Value::as_u64) {
                    let (unary_tx, stream_tx) = match demux_reader.lock() {
                        Ok(mut g) => (g.unary.remove(&id), g.streams.remove(&id)),
                        Err(_) => (None, None),
                    };
                    if let Some(tx) = unary_tx {
                        let _ = tx.send(Err(err));
                    } else if let Some(tx) = stream_tx {
                        let _ = tx.send(Err(err)).await;
                    }
                } else {
                    // Session-level error (`id: null`, e.g. fatal `-32600` oversized line): fail all waiters!
                    let (unaries, streams) = match demux_reader.lock() {
                        Ok(mut g) => (std::mem::take(&mut g.unary), std::mem::take(&mut g.streams)),
                        Err(_) => (HashMap::new(), HashMap::new()),
                    };
                    for (_, tx) in unaries {
                        let _ = tx.send(Err(err.clone()));
                    }
                    for (_, tx) in streams {
                        let _ = tx.send(Err(err.clone())).await;
                    }
                }
            }
        }

        // EOF on stdout: fail any remaining waiters cleanly.
        let eof_err = NgcErrorInfo::new(NgcErrorCode::Unavailable, "stdio session ended");
        let (unaries, streams) = match demux_reader.lock() {
            Ok(mut g) => (std::mem::take(&mut g.unary), std::mem::take(&mut g.streams)),
            Err(_) => (HashMap::new(), HashMap::new()),
        };
        for (_, tx) in unaries {
            let _ = tx.send(Err(eof_err.clone()));
        }
        for (_, tx) in streams {
            let _ = tx.send(Err(eof_err.clone())).await;
        }
    });

    let client = StdioTestClient {
        next_id: Arc::new(AtomicU64::new(1)),
        write_tx,
        demux,
    };
    let handle = InMemoryStdioHandle {
        server_task,
        reader_task,
        writer_task,
    };
    (client, handle)
}
