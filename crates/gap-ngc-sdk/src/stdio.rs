//! Newline-delimited JSON-RPC 2.0 server over `stdio` (`NGC-003`, `NGC-093`, `NGC-095`).

use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::ngc::v1::{
    CancelTaskRequest, DescribeRequest, GetTaskRequest, HealthRequest, MutateRequest, QueryRequest,
    SketchRequest, StartTaskRequest, SubscribeRequest, WatchTaskRequest,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::connector::Connector;
use crate::core::{CoreStream, ServerCore};
use crate::error::{ngc_error_to_proto, sanitize_error_info};
use crate::json::{ProtoMessage, from_json_value, to_json_value};

/// JSON-RPC 2.0 parse error (`NGC-095`).
pub const JSONRPC_PARSE_ERROR: i32 = -32700;
/// JSON-RPC 2.0 invalid request (`NGC-095`).
pub const JSONRPC_INVALID_REQUEST: i32 = -32600;
/// JSON-RPC 2.0 method not found (`NGC-095`).
pub const JSONRPC_METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC 2.0 invalid params (`NGC-095`).
pub const JSONRPC_INVALID_PARAMS: i32 = -32602;
/// Connector-level error returned in JSON-RPC `error.data` (`NGC-095`).
pub const JSONRPC_CONNECTOR_ERROR: i32 = -32001;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RpcIdKey {
    Str(String),
    Num(String),
}

impl RpcIdKey {
    fn from_value(v: &serde_json::Value) -> Option<Self> {
        match v {
            serde_json::Value::String(s) => Some(Self::Str(s.clone())),
            serde_json::Value::Number(n) => Some(Self::Num(n.to_string())),
            _ => None,
        }
    }
}

type InFlightMap = Arc<Mutex<HashMap<RpcIdKey, (u64, CancellationToken)>>>;

struct InFlightEntryGuard {
    map: InFlightMap,
    key: RpcIdKey,
    call_seq: u64,
}

impl Drop for InFlightEntryGuard {
    fn drop(&mut self) {
        let mut guard = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard
            .get(&self.key)
            .is_some_and(|(seq, _)| *seq == self.call_seq)
        {
            guard.remove(&self.key);
        }
    }
}

fn try_register_in_flight(
    map: &InFlightMap,
    session_cancel: &CancellationToken,
    key: RpcIdKey,
    call_seq: u64,
) -> Option<(CancellationToken, InFlightEntryGuard)> {
    let mut guard = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.contains_key(&key) {
        return None;
    }
    let tok = session_cancel.child_token();
    guard.insert(key.clone(), (call_seq, tok.clone()));
    drop(guard);
    Some((
        tok,
        InFlightEntryGuard {
            map: Arc::clone(map),
            key,
            call_seq,
        },
    ))
}

enum BoundedLine {
    Eof,
    Line(Vec<u8>),
    Oversized,
    TimedOut,
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
    line_timeout: Duration,
) -> std::io::Result<BoundedLine> {
    let mut buf = Vec::new();
    let mut partial_deadline: Option<Pin<Box<tokio::time::Sleep>>> = None;

    loop {
        let available = if let Some(ref mut sleep) = partial_deadline {
            tokio::select! {
                biased;
                () = sleep.as_mut() => return Ok(BoundedLine::TimedOut),
                res = reader.fill_buf() => res?,
            }
        } else {
            reader.fill_buf().await?
        };

        if available.is_empty() {
            if buf.is_empty() {
                return Ok(BoundedLine::Eof);
            }
            return Ok(BoundedLine::Line(buf));
        }

        if let Some(nl_pos) = available.iter().position(|&b| b == b'\n') {
            let line_part = &available[..nl_pos];
            let trimmed_part_len = if buf.is_empty() && line_part.ends_with(b"\r") {
                line_part.len().saturating_sub(1)
            } else {
                line_part.len()
            };
            if buf.len().saturating_add(trimmed_part_len) > max_bytes {
                reader.consume(nl_pos + 1);
                return Ok(BoundedLine::Oversized);
            }
            buf.extend_from_slice(line_part);
            reader.consume(nl_pos + 1);
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(BoundedLine::Line(buf));
        }

        let avail_len = available.len();
        if buf.len().saturating_add(avail_len) > max_bytes {
            reader.consume(avail_len);
            return Ok(BoundedLine::Oversized);
        }
        if buf.is_empty() {
            let dl = Instant::now()
                .checked_add(line_timeout)
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(60));
            partial_deadline = Some(Box::pin(sleep_until(dl)));
        }
        buf.extend_from_slice(available);
        reader.consume(avail_len);
    }
}

fn encode_jsonrpc_error(
    id: serde_json::Value,
    rpc_code: i32,
    err: &NgcErrorInfo,
    max_details_bytes: usize,
) -> String {
    let sanitized = sanitize_error_info(err.clone(), max_details_bytes);
    let proto = ngc_error_to_proto(&sanitized);
    let data = to_json_value(&proto).unwrap_or_else(|_| {
        serde_json::json!({
            "code": "INTERNAL",
            "message": "error serialization failed",
        })
    });
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": rpc_code,
            "message": sanitized.message(),
            "data": data,
        }
    });
    serde_json::to_string(&envelope).unwrap_or_else(|_| {
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"internal error\"}}"
            .to_owned()
    })
}

fn encode_jsonrpc_result(
    id: &serde_json::Value,
    result: serde_json::Value,
) -> Result<String, NgcErrorInfo> {
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    serde_json::to_string(&envelope).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::Internal,
            format!("failed to serialize JSON-RPC result envelope: {err}"),
        )
    })
}

fn encode_stream_item(
    id: &serde_json::Value,
    item: serde_json::Value,
) -> Result<String, NgcErrorInfo> {
    let envelope = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "gap.ngc.item",
        "params": {
            "id": id,
            "item": item,
        }
    });
    serde_json::to_string(&envelope).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::Internal,
            format!("failed to serialize JSON-RPC stream item envelope: {err}"),
        )
    })
}

async fn send_unary<Resp: ProtoMessage>(
    out_tx: &mpsc::Sender<String>,
    id: serde_json::Value,
    res: Result<Resp, NgcErrorInfo>,
    max_details_bytes: usize,
) {
    let line = match res.and_then(|resp| to_json_value(&resp)) {
        Ok(val) => match encode_jsonrpc_result(&id, val) {
            Ok(encoded) => encoded,
            Err(err) => encode_jsonrpc_error(id, JSONRPC_CONNECTOR_ERROR, &err, max_details_bytes),
        },
        Err(err) => encode_jsonrpc_error(id, JSONRPC_CONNECTOR_ERROR, &err, max_details_bytes),
    };
    let _ = out_tx.send(line).await;
}

async fn send_stream<Item: ProtoMessage>(
    out_tx: &mpsc::Sender<String>,
    id: serde_json::Value,
    res: Result<CoreStream<Item>, NgcErrorInfo>,
    max_details_bytes: usize,
) {
    let mut stream = match res {
        Ok(s) => s,
        Err(err) => {
            let line = encode_jsonrpc_error(id, JSONRPC_CONNECTOR_ERROR, &err, max_details_bytes);
            let _ = out_tx.send(line).await;
            return;
        }
    };

    while let Some(item_res) = stream.next().await {
        match item_res.and_then(|item| to_json_value(&item)) {
            Ok(val) => match encode_stream_item(&id, val) {
                Ok(item_line) => {
                    if out_tx.send(item_line).await.is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let line =
                        encode_jsonrpc_error(id, JSONRPC_CONNECTOR_ERROR, &err, max_details_bytes);
                    let _ = out_tx.send(line).await;
                    return;
                }
            },
            Err(err) => {
                let line =
                    encode_jsonrpc_error(id, JSONRPC_CONNECTOR_ERROR, &err, max_details_bytes);
                let _ = out_tx.send(line).await;
                return;
            }
        }
    }

    let end_line = match encode_jsonrpc_result(&id, serde_json::json!({ "end": true })) {
        Ok(line) => line,
        Err(err) => encode_jsonrpc_error(id, JSONRPC_CONNECTOR_ERROR, &err, max_details_bytes),
    };
    let _ = out_tx.send(end_line).await;
}

#[derive(Debug, Clone, Copy)]
enum StdioMethod {
    Describe,
    Query,
    Mutate,
    Subscribe,
    StartTask,
    GetTask,
    CancelTask,
    WatchTask,
    Sketch,
    Health,
    Explain,
}

impl StdioMethod {
    fn parse(method: &str) -> Option<Self> {
        match method {
            "gap.ngc.Describe" => Some(Self::Describe),
            "gap.ngc.Query" => Some(Self::Query),
            "gap.ngc.Mutate" => Some(Self::Mutate),
            "gap.ngc.Subscribe" => Some(Self::Subscribe),
            "gap.ngc.StartTask" => Some(Self::StartTask),
            "gap.ngc.GetTask" => Some(Self::GetTask),
            "gap.ngc.CancelTask" => Some(Self::CancelTask),
            "gap.ngc.WatchTask" => Some(Self::WatchTask),
            "gap.ngc.Sketch" => Some(Self::Sketch),
            "gap.ngc.Health" => Some(Self::Health),
            "gap.ngc.Explain" => Some(Self::Explain),
            _ => None,
        }
    }
}

/// Serves newline-delimited JSON-RPC 2.0 over `reader` and `writer` (`NGC-003`, `NGC-095`).
///
/// Concurrent requests execute in parallel bounded by [`ServerCore`]'s `max_in_flight`.
/// When `reader` reaches EOF, all in-flight calls are cancelled and `serve_stdio` returns cleanly.
///
/// # Errors
///
/// Returns an [`std::io::Error`] if reading from `reader` or writing protocol lines to `writer` fails.
pub async fn serve_stdio<C, R, W>(
    core: Arc<ServerCore<C>>,
    mut reader: R,
    mut writer: W,
) -> std::io::Result<()>
where
    C: Connector,
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let max_bytes = core.config().max_request_bytes;
    let max_details = core.config().max_error_details_bytes;
    let line_timeout = core.config().default_timeout;
    let chan_cap = core.config().stream_buffer.max(64);
    let (out_tx, mut out_rx) = mpsc::channel::<String>(chan_cap);

    let writer_handle = tokio::spawn(async move {
        while let Some(mut line) = out_rx.recv().await {
            line.push('\n');
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await?;
        }
        writer.flush().await?;
        Ok::<(), std::io::Error>(())
    });

    let session_cancel = CancellationToken::new();
    let in_flight_map: InFlightMap = Arc::new(Mutex::new(HashMap::new()));
    let mut next_call_seq: u64 = 1;
    let mut tasks: JoinSet<()> = JoinSet::new();

    loop {
        let next_line = tokio::select! {
            biased;
            Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
            res = read_bounded_line(&mut reader, max_bytes, line_timeout) => res?,
        };

        match next_line {
            BoundedLine::Eof => {
                session_cancel.cancel();
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                drop(out_tx);
                return match writer_handle.await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                    Ok(Err(e)) => Err(e),
                    Err(join_err) => Err(std::io::Error::other(join_err)),
                };
            }
            BoundedLine::Oversized => {
                let err = NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "stdio JSON-RPC line exceeds max_request_bytes",
                );
                let fatal = encode_jsonrpc_error(
                    serde_json::Value::Null,
                    JSONRPC_INVALID_REQUEST,
                    &err,
                    max_details,
                );
                let _ = out_tx.send(fatal).await;
                session_cancel.cancel();
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                drop(out_tx);
                return match writer_handle.await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                    Ok(Err(e)) => Err(e),
                    Err(join_err) => Err(std::io::Error::other(join_err)),
                };
            }
            BoundedLine::TimedOut => {
                let err = NgcErrorInfo::new(
                    NgcErrorCode::DeadlineExceeded,
                    "timed out waiting for newline on partial stdio JSON-RPC line",
                );
                let fatal = encode_jsonrpc_error(
                    serde_json::Value::Null,
                    JSONRPC_INVALID_REQUEST,
                    &err,
                    max_details,
                );
                let _ = out_tx.send(fatal).await;
                session_cancel.cancel();
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                drop(out_tx);
                return match writer_handle.await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                    Ok(Err(e)) => Err(e),
                    Err(join_err) => Err(std::io::Error::other(join_err)),
                };
            }
            BoundedLine::Line(bytes) => {
                let parsed: serde_json::Value = match serde_json::from_slice(&bytes) {
                    Ok(v) => v,
                    Err(parse_err) => {
                        let err = NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            format!("JSON parse error: {parse_err}"),
                        );
                        let _ = out_tx
                            .send(encode_jsonrpc_error(
                                serde_json::Value::Null,
                                JSONRPC_PARSE_ERROR,
                                &err,
                                max_details,
                            ))
                            .await;
                        continue;
                    }
                };

                let serde_json::Value::Object(mut obj) = parsed else {
                    let err = NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        "JSON-RPC request must be a single JSON object (batch arrays are not supported)",
                    );
                    let _ = out_tx
                        .send(encode_jsonrpc_error(
                            serde_json::Value::Null,
                            JSONRPC_INVALID_REQUEST,
                            &err,
                            max_details,
                        ))
                        .await;
                    continue;
                };

                let raw_id = obj.remove("id");
                let err_id = raw_id
                    .as_ref()
                    .filter(|v| v.is_string() || v.is_number() || v.is_null())
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let is_v2 = obj
                    .get("jsonrpc")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|v| v == "2.0");
                if !is_v2 {
                    let err = NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        "jsonrpc field must equal \"2.0\"",
                    );
                    let _ = out_tx
                        .send(encode_jsonrpc_error(
                            err_id,
                            JSONRPC_INVALID_REQUEST,
                            &err,
                            max_details,
                        ))
                        .await;
                    continue;
                }

                let Some(method_str) = obj.remove("method").and_then(|v| match v {
                    serde_json::Value::String(s) => Some(s),
                    _ => None,
                }) else {
                    let err = NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        "method field must be a string",
                    );
                    let _ = out_tx
                        .send(encode_jsonrpc_error(
                            err_id,
                            JSONRPC_INVALID_REQUEST,
                            &err,
                            max_details,
                        ))
                        .await;
                    continue;
                };

                if method_str == "gap.ngc.cancel" {
                    if let Some(cancel_id) = obj
                        .get("params")
                        .and_then(|p| p.get("id"))
                        .and_then(RpcIdKey::from_value)
                    {
                        let maybe_tok = in_flight_map
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get(&cancel_id)
                            .map(|(_, tok)| tok.clone());
                        if let Some(tok) = maybe_tok {
                            tok.cancel();
                        }
                    }
                    continue;
                }

                let Some(id_val) = raw_id else {
                    let err =
                        NgcErrorInfo::new(NgcErrorCode::InvalidArgument, "request id is required");
                    let _ = out_tx
                        .send(encode_jsonrpc_error(
                            serde_json::Value::Null,
                            JSONRPC_INVALID_REQUEST,
                            &err,
                            max_details,
                        ))
                        .await;
                    continue;
                };

                let Some(id_key) = RpcIdKey::from_value(&id_val) else {
                    let err = NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        "request id must be a string or number",
                    );
                    let _ = out_tx
                        .send(encode_jsonrpc_error(
                            serde_json::Value::Null,
                            JSONRPC_INVALID_REQUEST,
                            &err,
                            max_details,
                        ))
                        .await;
                    continue;
                };

                let call_seq = next_call_seq;
                next_call_seq = next_call_seq.wrapping_add(1);
                let Some((call_tok, entry_guard)) =
                    try_register_in_flight(&in_flight_map, &session_cancel, id_key, call_seq)
                else {
                    let err = NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        "request id is already in flight",
                    );
                    let _ = out_tx
                        .send(encode_jsonrpc_error(
                            id_val,
                            JSONRPC_INVALID_REQUEST,
                            &err,
                            max_details,
                        ))
                        .await;
                    continue;
                };

                let Some(method) = StdioMethod::parse(&method_str) else {
                    let err = NgcErrorInfo::new(
                        NgcErrorCode::Unimplemented,
                        format!("unknown method `{method_str}`"),
                    );
                    let _ = out_tx
                        .send(encode_jsonrpc_error(
                            id_val,
                            JSONRPC_METHOD_NOT_FOUND,
                            &err,
                            max_details,
                        ))
                        .await;
                    continue;
                };

                let params_val = match obj.remove("params") {
                    None => serde_json::Value::Object(serde_json::Map::new()),
                    Some(v @ serde_json::Value::Object(_)) => v,
                    Some(_) => {
                        let err = NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            "params must be a JSON object",
                        );
                        let _ = out_tx
                            .send(encode_jsonrpc_error(
                                id_val,
                                JSONRPC_INVALID_PARAMS,
                                &err,
                                max_details,
                            ))
                            .await;
                        continue;
                    }
                };

                dispatch_rpc(
                    method,
                    params_val,
                    id_val,
                    call_tok,
                    entry_guard,
                    &core,
                    &out_tx,
                    &mut tasks,
                )
                .await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_rpc<C: Connector>(
    method: StdioMethod,
    params_val: serde_json::Value,
    id_val: serde_json::Value,
    tok: CancellationToken,
    entry_guard: InFlightEntryGuard,
    core: &Arc<ServerCore<C>>,
    out_tx: &mpsc::Sender<String>,
    tasks: &mut JoinSet<()>,
) {
    let max_details = core.config().max_error_details_bytes;
    macro_rules! parse_or_invalid_params {
        ($ty:ty) => {
            match from_json_value::<$ty>(params_val) {
                Ok(req) => req,
                Err(err) => {
                    let _ = out_tx
                        .send(encode_jsonrpc_error(
                            id_val,
                            JSONRPC_INVALID_PARAMS,
                            &err,
                            max_details,
                        ))
                        .await;
                    return;
                }
            }
        };
    }

    match method {
        StdioMethod::Describe => {
            let req = parse_or_invalid_params!(DescribeRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.describe(req, BTreeMap::new(), tok).await;
                send_unary(&tx, id_val, res, max_details).await;
            });
        }
        StdioMethod::Query => {
            let req = parse_or_invalid_params!(QueryRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.query(req, BTreeMap::new(), tok).await;
                send_stream(&tx, id_val, res, max_details).await;
            });
        }
        StdioMethod::Mutate => {
            let req = parse_or_invalid_params!(MutateRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.mutate(req, BTreeMap::new(), tok).await;
                send_unary(&tx, id_val, res, max_details).await;
            });
        }
        StdioMethod::Subscribe => {
            let req = parse_or_invalid_params!(SubscribeRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.subscribe(req, BTreeMap::new(), tok).await;
                send_stream(&tx, id_val, res, max_details).await;
            });
        }
        StdioMethod::StartTask => {
            let req = parse_or_invalid_params!(StartTaskRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.start_task(req, BTreeMap::new(), tok).await;
                send_unary(&tx, id_val, res, max_details).await;
            });
        }
        StdioMethod::GetTask => {
            let req = parse_or_invalid_params!(GetTaskRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.get_task(req, BTreeMap::new(), tok).await;
                send_unary(&tx, id_val, res, max_details).await;
            });
        }
        StdioMethod::CancelTask => {
            let req = parse_or_invalid_params!(CancelTaskRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.cancel_task(req, BTreeMap::new(), tok).await;
                send_unary(&tx, id_val, res, max_details).await;
            });
        }
        StdioMethod::WatchTask => {
            let req = parse_or_invalid_params!(WatchTaskRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.watch_task(req, BTreeMap::new(), tok).await;
                send_stream(&tx, id_val, res, max_details).await;
            });
        }
        StdioMethod::Sketch => {
            let req = parse_or_invalid_params!(SketchRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.sketch(req, BTreeMap::new(), tok).await;
                send_unary(&tx, id_val, res, max_details).await;
            });
        }
        StdioMethod::Health => {
            let req = parse_or_invalid_params!(HealthRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.health(req, BTreeMap::new(), tok).await;
                send_unary(&tx, id_val, res, max_details).await;
            });
        }
        StdioMethod::Explain => {
            let req = parse_or_invalid_params!(QueryRequest);
            let core = Arc::clone(core);
            let tx = out_tx.clone();
            tasks.spawn(async move {
                let _entry = entry_guard;
                let res = core.explain(req, BTreeMap::new(), tok).await;
                send_unary(&tx, id_val, res, max_details).await;
            });
        }
    }
}

/// Serves `core` over the current process's standard input and standard output (`NGC-003`, `NGC-095`).
///
/// # Errors
///
/// Returns an [`std::io::Error`] if standard I/O fails.
pub async fn serve_process_stdio<C: Connector>(core: Arc<ServerCore<C>>) -> std::io::Result<()> {
    let reader = BufReader::new(tokio::io::stdin());
    let writer = tokio::io::stdout();
    serve_stdio(core, reader, writer).await
}
