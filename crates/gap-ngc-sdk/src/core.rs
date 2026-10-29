//! Transport-neutral server enforcement core implementing `NGC-093` (`INV-12`, `INV-15`, `NGC-025`, `NGC-061`, `NGC-070`, `NGC-071`, `NGC-080`).

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{FutureExt as _, Stream};
use gap_core::clock::{Clock, SystemClock};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::ngc::v1::{
    CallContext, CancelTaskRequest, DescribeRequest, DescribeResponse, Event, ExplainResponse,
    GetTaskRequest, HealthRequest, HealthResponse, MutateRequest, MutateResponse, QueryRequest,
    QueryResponse, SketchRequest, SketchResponse, StartTaskRequest, SubscribeRequest, TaskStatus,
    WatchTaskRequest, query_response,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Sleep, sleep_until};
use tokio_util::sync::{CancellationToken, DropGuard, WaitForCancellationFutureOwned};

use crate::connector::{BoxStream, CallScope, Connector, NgcResult};
use crate::error::{
    CONNECTOR_PANIC_MESSAGE, DEFAULT_MAX_ERROR_DETAILS_BYTES, sanitize_error_info,
    sanitize_proto_error,
};

/// Default timeout (`60 s`) applied when `CallContext.deadline_unix_ms == 0` (`NGC-093` (a)).
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Mandatory maximum timeout (`500 ms`) applied to `Health` (`NGC-070`, `NGC-093` (h)).
pub const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_millis(500);

/// Default operator `max_in_flight` cap (`256`) enforced by [`ServerCore`] (`NGC-071`, `NGC-093` (c)).
pub const DEFAULT_MAX_IN_FLIGHT: usize = 256;

/// Default maximum request byte size (`4 MiB`) enforced before buffering excess (`NGC-093` (f)).
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// Default bounded channel capacity (`64`) for streaming transport adapters (`ARC-011`).
pub const DEFAULT_STREAM_BUFFER: usize = 64;

/// Default `retry_after_ms` (`100 ms`) advertised on `max_in_flight` saturation (`NGC-071`, `NGC-093` (c)).
pub const DEFAULT_SATURATED_RETRY_AFTER_MS: u32 = 100;

/// Operator configuration for [`ServerCore`] and transport adapters (`NGC-093`).
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Time source used to evaluate `CallContext.deadline_unix_ms` (default: [`SystemClock`]).
    pub clock: Arc<dyn Clock>,
    /// Timeout assigned when `ctx.deadline_unix_ms == 0` or on `Describe` (`60 s`).
    pub default_timeout: Duration,
    /// Timeout assigned to `Health` (`500 ms`, `NGC-070`, `NGC-093` (h)).
    pub health_timeout: Duration,
    /// Operator-configured concurrent in-flight RPC limit (`256`; `Describe` limits are not consulted).
    pub max_in_flight: usize,
    /// Maximum incoming request payload/line byte size (`4 MiB`, `NGC-093` (f)).
    pub max_request_bytes: usize,
    /// Maximum serialized JSON byte size for `NgcError.details` before dropping (`4 KiB`, `NGC-093` (e)).
    pub max_error_details_bytes: usize,
    /// Bounded buffer capacity used by streaming transport adapters (`64`).
    pub stream_buffer: usize,
    /// Explicit insecure override allowing plain-text gRPC TCP binds on non-loopback addresses (`NGC-093` (g)).
    pub allow_insecure_non_loopback: bool,
    /// `retry_after_ms` returned with `RESOURCE_EXHAUSTED` when `max_in_flight` is saturated.
    pub saturated_retry_after_ms: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            clock: Arc::new(SystemClock),
            default_timeout: DEFAULT_CALL_TIMEOUT,
            health_timeout: DEFAULT_HEALTH_TIMEOUT,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_error_details_bytes: DEFAULT_MAX_ERROR_DETAILS_BYTES,
            stream_buffer: DEFAULT_STREAM_BUFFER,
            allow_insecure_non_loopback: false,
            saturated_retry_after_ms: DEFAULT_SATURATED_RETRY_AFTER_MS,
        }
    }
}

impl ServerConfig {
    /// Replaces the [`Clock`] used for `deadline_unix_ms` comparisons.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Sets `default_timeout` for calls with `deadline_unix_ms == 0`.
    #[must_use]
    pub const fn with_default_timeout(mut self, default_timeout: Duration) -> Self {
        self.default_timeout = default_timeout;
        self
    }

    /// Sets `health_timeout` for `Health` calls.
    #[must_use]
    pub const fn with_health_timeout(mut self, health_timeout: Duration) -> Self {
        self.health_timeout = health_timeout;
        self
    }

    /// Sets the operator `max_in_flight` cap.
    #[must_use]
    pub const fn with_max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = max_in_flight;
        self
    }

    /// Sets `max_request_bytes`.
    #[must_use]
    pub const fn with_max_request_bytes(mut self, max_request_bytes: usize) -> Self {
        self.max_request_bytes = max_request_bytes;
        self
    }

    /// Sets `max_error_details_bytes`.
    #[must_use]
    pub const fn with_max_error_details_bytes(mut self, max_error_details_bytes: usize) -> Self {
        self.max_error_details_bytes = max_error_details_bytes;
        self
    }

    /// Sets `stream_buffer`.
    #[must_use]
    pub const fn with_stream_buffer(mut self, stream_buffer: usize) -> Self {
        self.stream_buffer = stream_buffer;
        self
    }

    /// Sets `allow_insecure_non_loopback`.
    #[must_use]
    pub const fn with_allow_insecure_non_loopback(mut self, allow: bool) -> Self {
        self.allow_insecure_non_loopback = allow;
        self
    }

    /// Sets `saturated_retry_after_ms`.
    #[must_use]
    pub const fn with_saturated_retry_after_ms(mut self, retry_after_ms: u32) -> Self {
        self.saturated_retry_after_ms = retry_after_ms;
        self
    }
}

/// Converts an [`http::HeaderMap`] into lower-case call metadata (`NGC-080`, `NGC-094`).
#[must_use]
pub fn headers_to_metadata(headers: &http::HeaderMap) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, value) in headers {
        if let Ok(v) = value.to_str() {
            out.insert(name.as_str().to_ascii_lowercase(), v.to_owned());
        }
    }
    out
}

struct PreparedCall {
    permit: OwnedSemaphorePermit,
    scope: CallScope,
    call_cancel: CancellationToken,
    cancel_guard: DropGuard,
    deadline_instant: Instant,
    abs_deadline_ms: Option<i64>,
}

/// Streaming response guarded by [`ServerCore`] (`NGC-093`).
///
/// Holds the in-flight semaphore permit via RAII until stream completion, error, cancellation,
/// deadline expiration, or caller drop.
pub struct CoreStream<T> {
    inner: Option<BoxStream<T>>,
    _permit: Option<OwnedSemaphorePermit>,
    call_cancel: CancellationToken,
    _cancel_guard: DropGuard,
    cancel_fut: Pin<Box<WaitForCancellationFutureOwned>>,
    sleep: Pin<Box<Sleep>>,
    abs_deadline_ms: Option<i64>,
    clock: Arc<dyn Clock>,
    max_error_details_bytes: usize,
    sanitize_item: fn(&mut T, usize),
}

impl<T> CoreStream<T> {
    fn terminate(&mut self) {
        self.call_cancel.cancel();
        self.inner = None;
        self._permit = None;
    }
}

impl<T> Stream for CoreStream<T> {
    type Item = NgcResult<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.inner.is_none() {
            return Poll::Ready(None);
        }

        if this.cancel_fut.as_mut().poll(cx).is_ready() {
            this.terminate();
            return Poll::Ready(Some(Err(sanitize_error_info(
                NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"),
                this.max_error_details_bytes,
            ))));
        }

        let wall_expired = this
            .abs_deadline_ms
            .is_some_and(|abs_ms| this.clock.unix_millis() >= abs_ms);
        if wall_expired || this.sleep.as_mut().poll(cx).is_ready() {
            this.terminate();
            return Poll::Ready(Some(Err(sanitize_error_info(
                NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, "call deadline exceeded"),
                this.max_error_details_bytes,
            ))));
        }

        let poll_res = {
            let Some(ref mut stream) = this.inner else {
                return Poll::Ready(None);
            };
            catch_unwind(AssertUnwindSafe(|| stream.as_mut().poll_next(cx)))
        };

        match poll_res {
            Err(_panic) => {
                this.terminate();
                Poll::Ready(Some(Err(sanitize_error_info(
                    NgcErrorInfo::new(NgcErrorCode::Internal, CONNECTOR_PANIC_MESSAGE),
                    this.max_error_details_bytes,
                ))))
            }
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(None)) => {
                this.terminate();
                Poll::Ready(None)
            }
            Ok(Poll::Ready(Some(Ok(mut item)))) => {
                (this.sanitize_item)(&mut item, this.max_error_details_bytes);
                Poll::Ready(Some(Ok(item)))
            }
            Ok(Poll::Ready(Some(Err(err)))) => {
                this.terminate();
                Poll::Ready(Some(Err(sanitize_error_info(
                    err,
                    this.max_error_details_bytes,
                ))))
            }
        }
    }
}

fn noop_sanitize<T>(_item: &mut T, _max_bytes: usize) {}

fn sanitize_mutate_response(resp: &mut MutateResponse, max_bytes: usize) {
    if let Some(ref mut err) = resp.error {
        sanitize_proto_error(err, max_bytes);
    }
}

fn sanitize_task_status(status: &mut TaskStatus, max_bytes: usize) {
    if let Some(ref mut err) = status.error {
        sanitize_proto_error(err, max_bytes);
    }
}

fn sanitize_query_response(resp: &mut QueryResponse, max_bytes: usize) {
    if let Some(query_response::Chunk::Rows(ref mut rows_chunk)) = resp.chunk
        && let Some(ref mut err) = rows_chunk.error
    {
        sanitize_proto_error(err, max_bytes);
    }
}

/// Single enforcement point for `NGC-093` shared identically by gRPC, HTTP, and stdio.
pub struct ServerCore<C> {
    connector: C,
    config: ServerConfig,
    in_flight: Arc<Semaphore>,
}

impl<C: Connector> ServerCore<C> {
    /// Wraps `connector` with operator `config` and initializes the non-queueing `max_in_flight` semaphore.
    #[must_use]
    pub fn new(connector: C, config: ServerConfig) -> Self {
        let in_flight = Arc::new(Semaphore::new(config.max_in_flight));
        Self {
            connector,
            config,
            in_flight,
        }
    }

    /// Returns a reference to the [`ServerConfig`].
    #[must_use]
    pub const fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Returns a reference to the wrapped [`Connector`].
    #[must_use]
    pub const fn connector(&self) -> &C {
        &self.connector
    }

    fn sanitize_err(&self, err: NgcErrorInfo) -> NgcErrorInfo {
        sanitize_error_info(err, self.config.max_error_details_bytes)
    }

    fn deadline_exceeded_err(&self) -> NgcErrorInfo {
        self.sanitize_err(NgcErrorInfo::new(
            NgcErrorCode::DeadlineExceeded,
            "call deadline exceeded",
        ))
    }

    fn cancelled_err(&self) -> NgcErrorInfo {
        self.sanitize_err(NgcErrorInfo::new(NgcErrorCode::Cancelled, "call cancelled"))
    }

    fn panic_err(&self) -> NgcErrorInfo {
        self.sanitize_err(NgcErrorInfo::new(
            NgcErrorCode::Internal,
            CONNECTOR_PANIC_MESSAGE,
        ))
    }

    fn prepare_call(
        &self,
        ctx: Option<&CallContext>,
        is_health: bool,
        metadata: BTreeMap<String, String>,
        transport_cancel: &CancellationToken,
    ) -> NgcResult<PreparedCall> {
        let fallback_timeout = if is_health {
            self.config.health_timeout
        } else {
            self.config.default_timeout
        };

        let (timeout_dur, abs_deadline_ms) = match ctx {
            Some(c) if c.deadline_unix_ms != 0 => {
                let now_ms = self.config.clock.unix_millis();
                if c.deadline_unix_ms <= now_ms {
                    return Err(self.deadline_exceeded_err());
                }
                let remaining_ms =
                    u64::try_from(c.deadline_unix_ms.saturating_sub(now_ms)).unwrap_or(u64::MAX);
                let dur = Duration::from_millis(remaining_ms);
                let effective = if is_health {
                    dur.min(self.config.health_timeout)
                } else {
                    dur
                };
                (effective, Some(c.deadline_unix_ms))
            }
            _ => (fallback_timeout, None),
        };

        if transport_cancel.is_cancelled() {
            return Err(self.cancelled_err());
        }

        let permit = Arc::clone(&self.in_flight)
            .try_acquire_owned()
            .map_err(|_| {
                self.sanitize_err(
                    NgcErrorInfo::new(
                        NgcErrorCode::ResourceExhausted,
                        "server max_in_flight saturated",
                    )
                    .with_retry(true, self.config.saturated_retry_after_ms.max(1)),
                )
            })?;

        let now_instant = Instant::now();
        let deadline_instant = now_instant
            .checked_add(timeout_dur)
            .unwrap_or_else(|| now_instant + fallback_timeout);

        let call_cancel = CancellationToken::new();
        let cancel_guard = call_cancel.clone().drop_guard();

        let request_id = ctx.map(|c| c.request_id.clone()).unwrap_or_default();
        let tenant_id = ctx.map(|c| c.tenant_id.clone()).unwrap_or_default();
        let traceparent = match ctx {
            Some(c) if !c.traceparent.is_empty() => c.traceparent.clone(),
            _ => metadata.get("traceparent").cloned().unwrap_or_default(),
        };

        let scope = CallScope {
            cancel: call_cancel.clone(),
            deadline: deadline_instant,
            request_id,
            tenant_id,
            traceparent,
            metadata,
        };

        Ok(PreparedCall {
            permit,
            scope,
            call_cancel,
            cancel_guard,
            deadline_instant,
            abs_deadline_ms,
        })
    }

    async fn execute_unary<Resp, F, Fut>(
        &self,
        ctx: Option<CallContext>,
        is_health: bool,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
        invoke: F,
        sanitize_ok: fn(&mut Resp, usize),
    ) -> NgcResult<Resp>
    where
        F: FnOnce(CallScope) -> Fut,
        Fut: Future<Output = NgcResult<Resp>> + Send,
    {
        let prepared = self.prepare_call(ctx.as_ref(), is_health, metadata, &transport_cancel)?;
        let PreparedCall {
            permit: _permit,
            scope,
            call_cancel,
            cancel_guard: _cancel_guard,
            deadline_instant,
            abs_deadline_ms,
        } = prepared;

        let fut = match catch_unwind(AssertUnwindSafe(|| invoke(scope))) {
            Ok(fut) => fut,
            Err(_panic) => {
                call_cancel.cancel();
                return Err(self.panic_err());
            }
        };

        {
            let mut guarded_fut = std::pin::pin!(AssertUnwindSafe(fut).catch_unwind());
            tokio::select! {
                biased;
                () = transport_cancel.cancelled() => {
                    call_cancel.cancel();
                    Err(self.cancelled_err())
                }
                () = sleep_until(deadline_instant) => {
                    call_cancel.cancel();
                    Err(self.deadline_exceeded_err())
                }
                res = &mut guarded_fut => {
                    match res {
                        Ok(Ok(mut resp)) => {
                            if abs_deadline_ms.is_some_and(|abs_ms| self.config.clock.unix_millis() >= abs_ms) {
                                call_cancel.cancel();
                                Err(self.deadline_exceeded_err())
                            } else {
                                sanitize_ok(&mut resp, self.config.max_error_details_bytes);
                                Ok(resp)
                            }
                        }
                        Ok(Err(err)) => Err(self.sanitize_err(err)),
                        Err(_panic) => {
                            call_cancel.cancel();
                            Err(self.panic_err())
                        }
                    }
                }
            }
        }
    }

    async fn execute_stream<Item, F, Fut>(
        &self,
        ctx: Option<CallContext>,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
        invoke: F,
        sanitize_item: fn(&mut Item, usize),
    ) -> NgcResult<CoreStream<Item>>
    where
        F: FnOnce(CallScope) -> Fut,
        Fut: Future<Output = NgcResult<BoxStream<Item>>> + Send,
    {
        let prepared = self.prepare_call(ctx.as_ref(), false, metadata, &transport_cancel)?;
        let PreparedCall {
            permit,
            scope,
            call_cancel,
            cancel_guard,
            deadline_instant,
            abs_deadline_ms,
        } = prepared;

        let fut = match catch_unwind(AssertUnwindSafe(|| invoke(scope))) {
            Ok(fut) => fut,
            Err(_panic) => {
                call_cancel.cancel();
                return Err(self.panic_err());
            }
        };

        let open_res = {
            let mut guarded_fut = std::pin::pin!(AssertUnwindSafe(fut).catch_unwind());
            tokio::select! {
                biased;
                () = transport_cancel.cancelled() => {
                    call_cancel.cancel();
                    Err(self.cancelled_err())
                }
                () = sleep_until(deadline_instant) => {
                    call_cancel.cancel();
                    Err(self.deadline_exceeded_err())
                }
                res = &mut guarded_fut => {
                    match res {
                        Ok(Ok(stream)) => {
                            if abs_deadline_ms.is_some_and(|abs_ms| self.config.clock.unix_millis() >= abs_ms) {
                                call_cancel.cancel();
                                Err(self.deadline_exceeded_err())
                            } else {
                                Ok(stream)
                            }
                        }
                        Ok(Err(err)) => Err(self.sanitize_err(err)),
                        Err(_panic) => {
                            call_cancel.cancel();
                            Err(self.panic_err())
                        }
                    }
                }
            }
        };

        let stream = open_res?;
        Ok(CoreStream {
            inner: Some(stream),
            _permit: Some(permit),
            call_cancel,
            _cancel_guard: cancel_guard,
            cancel_fut: Box::pin(transport_cancel.cancelled_owned()),
            sleep: Box::pin(sleep_until(deadline_instant)),
            abs_deadline_ms,
            clock: Arc::clone(&self.config.clock),
            max_error_details_bytes: self.config.max_error_details_bytes,
            sanitize_item,
        })
    }

    /// Executes `Describe` through [`ServerCore`].
    pub async fn describe(
        &self,
        request: DescribeRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<DescribeResponse> {
        self.execute_unary(
            None,
            false,
            metadata,
            transport_cancel,
            |scope| self.connector.describe(request, scope),
            noop_sanitize,
        )
        .await
    }

    /// Executes `Query` through [`ServerCore`].
    pub async fn query(
        &self,
        request: QueryRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<CoreStream<QueryResponse>> {
        let ctx = request.ctx.clone();
        self.execute_stream(
            ctx,
            metadata,
            transport_cancel,
            |scope| self.connector.query(request, scope),
            sanitize_query_response,
        )
        .await
    }

    /// Executes `Mutate` through [`ServerCore`].
    pub async fn mutate(
        &self,
        request: MutateRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<MutateResponse> {
        let ctx = request.ctx.clone();
        self.execute_unary(
            ctx,
            false,
            metadata,
            transport_cancel,
            |scope| self.connector.mutate(request, scope),
            sanitize_mutate_response,
        )
        .await
    }

    /// Executes `Subscribe` through [`ServerCore`].
    pub async fn subscribe(
        &self,
        request: SubscribeRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<CoreStream<Event>> {
        let ctx = request.ctx.clone();
        self.execute_stream(
            ctx,
            metadata,
            transport_cancel,
            |scope| self.connector.subscribe(request, scope),
            noop_sanitize,
        )
        .await
    }

    /// Executes `StartTask` through [`ServerCore`].
    pub async fn start_task(
        &self,
        request: StartTaskRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<TaskStatus> {
        let ctx = request.ctx.clone();
        self.execute_unary(
            ctx,
            false,
            metadata,
            transport_cancel,
            |scope| self.connector.start_task(request, scope),
            sanitize_task_status,
        )
        .await
    }

    /// Executes `GetTask` through [`ServerCore`].
    pub async fn get_task(
        &self,
        request: GetTaskRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<TaskStatus> {
        let ctx = request.ctx.clone();
        self.execute_unary(
            ctx,
            false,
            metadata,
            transport_cancel,
            |scope| self.connector.get_task(request, scope),
            sanitize_task_status,
        )
        .await
    }

    /// Executes `CancelTask` through [`ServerCore`].
    pub async fn cancel_task(
        &self,
        request: CancelTaskRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<TaskStatus> {
        let ctx = request.ctx.clone();
        self.execute_unary(
            ctx,
            false,
            metadata,
            transport_cancel,
            |scope| self.connector.cancel_task(request, scope),
            sanitize_task_status,
        )
        .await
    }

    /// Executes `WatchTask` through [`ServerCore`].
    pub async fn watch_task(
        &self,
        request: WatchTaskRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<CoreStream<TaskStatus>> {
        let ctx = request.ctx.clone();
        self.execute_stream(
            ctx,
            metadata,
            transport_cancel,
            |scope| self.connector.watch_task(request, scope),
            sanitize_task_status,
        )
        .await
    }

    /// Executes `Sketch` through [`ServerCore`].
    pub async fn sketch(
        &self,
        request: SketchRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<SketchResponse> {
        let ctx = request.ctx.clone();
        self.execute_unary(
            ctx,
            false,
            metadata,
            transport_cancel,
            |scope| self.connector.sketch(request, scope),
            noop_sanitize,
        )
        .await
    }

    /// Executes `Health` through [`ServerCore`] bounded by `health_timeout` (`500 ms`, `NGC-070`, `NGC-093` (h)).
    pub async fn health(
        &self,
        request: HealthRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<HealthResponse> {
        self.execute_unary(
            None,
            true,
            metadata,
            transport_cancel,
            |scope| self.connector.health(request, scope),
            noop_sanitize,
        )
        .await
    }

    /// Executes `Explain` through [`ServerCore`].
    pub async fn explain(
        &self,
        request: QueryRequest,
        metadata: BTreeMap<String, String>,
        transport_cancel: CancellationToken,
    ) -> NgcResult<ExplainResponse> {
        let ctx = request.ctx.clone();
        self.execute_unary(
            ctx,
            false,
            metadata,
            transport_cancel,
            |scope| self.connector.explain(request, scope),
            noop_sanitize,
        )
        .await
    }
}
