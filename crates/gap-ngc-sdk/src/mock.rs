//! Public scripted [`MockConnector`] with call counters, concurrency high-water mark, delays, and RAII drop tracking (`NGC-090`, `ARC-012`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Stream, stream};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    CancelTaskRequest, DescribeRequest, DescribeResponse, Event, ExplainResponse, GetTaskRequest,
    HealthRequest, HealthResponse, MutateRequest, MutateResponse, QueryRequest, QueryResponse,
    ServingStatus, SketchRequest, SketchResponse, StartTaskRequest, SubscribeRequest, TaskStatus,
    WatchTaskRequest,
};
use tokio::sync::Notify;

use crate::connector::{BoxStream, CallScope, Connector, NgcResult};

/// Enumeration of all 11 `gap.ngc.v1.Connector` RPC methods for [`MockConnector`] counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RpcMethod {
    /// `Describe`.
    Describe = 0,
    /// `Query`.
    Query = 1,
    /// `Mutate`.
    Mutate = 2,
    /// `Subscribe`.
    Subscribe = 3,
    /// `StartTask`.
    StartTask = 4,
    /// `GetTask`.
    GetTask = 5,
    /// `CancelTask`.
    CancelTask = 6,
    /// `WatchTask`.
    WatchTask = 7,
    /// `Sketch`.
    Sketch = 8,
    /// `Health`.
    Health = 9,
    /// `Explain`.
    Explain = 10,
}

type BoxFut<T> = Pin<Box<dyn Future<Output = NgcResult<T>> + Send>>;
type UnaryHandler<Req, Resp> = Arc<dyn Fn(Req, CallScope) -> BoxFut<Resp> + Send + Sync>;
type StreamHandler<Req, Item> =
    Arc<dyn Fn(Req, CallScope) -> BoxFut<BoxStream<Item>> + Send + Sync>;

#[derive(Default)]
struct Metrics {
    per_method: [AtomicU64; 11],
    total_calls: AtomicU64,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    dropped_count: AtomicU64,
    drop_notify: Notify,
}

impl Metrics {
    fn record_start(self: &Arc<Self>, method: RpcMethod) -> ActiveCallGuard {
        self.per_method[method as usize].fetch_add(1, Ordering::AcqRel);
        self.total_calls.fetch_add(1, Ordering::AcqRel);
        let cur = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_in_flight.fetch_max(cur, Ordering::AcqRel);
        ActiveCallGuard {
            metrics: Arc::clone(self),
            completed: false,
        }
    }
}

/// RAII guard tracking an active mock unary future or open stream (`ARC-012`).
///
/// If dropped before [`ActiveCallGuard::mark_completed`] is called (for example when cancelled by
/// `ServerCore`), increments `dropped_count` and notifies waiters on `drop_notify`.
struct ActiveCallGuard {
    metrics: Arc<Metrics>,
    completed: bool,
}

impl ActiveCallGuard {
    fn mark_completed(&mut self) {
        self.completed = true;
    }
}

impl Drop for ActiveCallGuard {
    fn drop(&mut self) {
        self.metrics.in_flight.fetch_sub(1, Ordering::AcqRel);
        if !self.completed {
            self.metrics.dropped_count.fetch_add(1, Ordering::AcqRel);
            self.metrics.drop_notify.notify_waiters();
        }
    }
}

struct GuardedStream<T> {
    inner: BoxStream<T>,
    guard: ActiveCallGuard,
}

impl<T> Stream for GuardedStream<T> {
    type Item = NgcResult<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(None) => {
                this.guard.mark_completed();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

fn unimplemented_err(method: &str) -> NgcErrorInfo {
    NgcErrorInfo::new(
        NgcErrorCode::Unimplemented,
        format!("{method} is not implemented by MockConnector"),
    )
}

/// Scriptable connector double for unit, transport-matrix, and conformance tests (`NGC-090`).
///
/// Public without any Cargo feature flag (`m1-03-brief.md` §3.9).
#[derive(Clone)]
pub struct MockConnector {
    metrics: Arc<Metrics>,
    delay: Duration,
    describe_fn: Option<UnaryHandler<DescribeRequest, DescribeResponse>>,
    query_fn: Option<StreamHandler<QueryRequest, QueryResponse>>,
    mutate_fn: Option<UnaryHandler<MutateRequest, MutateResponse>>,
    subscribe_fn: Option<StreamHandler<SubscribeRequest, Event>>,
    start_task_fn: Option<UnaryHandler<StartTaskRequest, TaskStatus>>,
    get_task_fn: Option<UnaryHandler<GetTaskRequest, TaskStatus>>,
    cancel_task_fn: Option<UnaryHandler<CancelTaskRequest, TaskStatus>>,
    watch_task_fn: Option<StreamHandler<WatchTaskRequest, TaskStatus>>,
    sketch_fn: Option<UnaryHandler<SketchRequest, SketchResponse>>,
    health_fn: Option<UnaryHandler<HealthRequest, HealthResponse>>,
    explain_fn: Option<UnaryHandler<QueryRequest, ExplainResponse>>,
}

impl Default for MockConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MockConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockConnector")
            .field("delay", &self.delay)
            .field("total_calls", &self.total_calls())
            .field("in_flight", &self.in_flight())
            .field("max_in_flight_observed", &self.max_in_flight_observed())
            .field("dropped_count", &self.dropped_count())
            .finish()
    }
}

impl MockConnector {
    /// Creates a [`MockConnector`] with default responses (`DescribeResponse::default()`, empty `Query` stream, `SERVING` `Health`, `UNIMPLEMENTED` optional RPCs) and zero delay.
    #[must_use]
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(Metrics::default()),
            delay: Duration::ZERO,
            describe_fn: None,
            query_fn: None,
            mutate_fn: None,
            subscribe_fn: None,
            start_task_fn: None,
            get_task_fn: None,
            cancel_task_fn: None,
            watch_task_fn: None,
            sketch_fn: None,
            health_fn: None,
            explain_fn: None,
        }
    }

    /// Configures a synthetic `tokio::time::sleep` delay applied before each RPC completes.
    #[must_use]
    pub const fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Scripts synchronous `Describe` responses.
    #[must_use]
    pub fn on_describe<F>(mut self, f: F) -> Self
    where
        F: Fn(DescribeRequest, CallScope) -> NgcResult<DescribeResponse> + Send + Sync + 'static,
    {
        self.describe_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Scripts asynchronous `Describe` responses.
    #[must_use]
    pub fn on_describe_async<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(DescribeRequest, CallScope) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = NgcResult<DescribeResponse>> + Send + 'static,
    {
        self.describe_fn = Some(Arc::new(move |req, scope| Box::pin(f(req, scope))));
        self
    }

    /// Scripts synchronous `Query` stream responses.
    #[must_use]
    pub fn on_query<F>(mut self, f: F) -> Self
    where
        F: Fn(QueryRequest, CallScope) -> NgcResult<BoxStream<QueryResponse>>
            + Send
            + Sync
            + 'static,
    {
        self.query_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Scripts asynchronous `Query` stream responses.
    #[must_use]
    pub fn on_query_async<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(QueryRequest, CallScope) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send + 'static,
    {
        self.query_fn = Some(Arc::new(move |req, scope| Box::pin(f(req, scope))));
        self
    }

    /// Scripts synchronous `Mutate` responses.
    #[must_use]
    pub fn on_mutate<F>(mut self, f: F) -> Self
    where
        F: Fn(MutateRequest, CallScope) -> NgcResult<MutateResponse> + Send + Sync + 'static,
    {
        self.mutate_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Scripts asynchronous `Mutate` responses.
    #[must_use]
    pub fn on_mutate_async<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(MutateRequest, CallScope) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = NgcResult<MutateResponse>> + Send + 'static,
    {
        self.mutate_fn = Some(Arc::new(move |req, scope| Box::pin(f(req, scope))));
        self
    }

    /// Scripts synchronous `Subscribe` stream responses.
    #[must_use]
    pub fn on_subscribe<F>(mut self, f: F) -> Self
    where
        F: Fn(SubscribeRequest, CallScope) -> NgcResult<BoxStream<Event>> + Send + Sync + 'static,
    {
        self.subscribe_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Scripts synchronous `StartTask` responses.
    #[must_use]
    pub fn on_start_task<F>(mut self, f: F) -> Self
    where
        F: Fn(StartTaskRequest, CallScope) -> NgcResult<TaskStatus> + Send + Sync + 'static,
    {
        self.start_task_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Scripts synchronous `GetTask` responses.
    #[must_use]
    pub fn on_get_task<F>(mut self, f: F) -> Self
    where
        F: Fn(GetTaskRequest, CallScope) -> NgcResult<TaskStatus> + Send + Sync + 'static,
    {
        self.get_task_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Scripts synchronous `CancelTask` responses.
    #[must_use]
    pub fn on_cancel_task<F>(mut self, f: F) -> Self
    where
        F: Fn(CancelTaskRequest, CallScope) -> NgcResult<TaskStatus> + Send + Sync + 'static,
    {
        self.cancel_task_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Scripts synchronous `WatchTask` stream responses.
    #[must_use]
    pub fn on_watch_task<F>(mut self, f: F) -> Self
    where
        F: Fn(WatchTaskRequest, CallScope) -> NgcResult<BoxStream<TaskStatus>>
            + Send
            + Sync
            + 'static,
    {
        self.watch_task_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Scripts synchronous `Sketch` responses.
    #[must_use]
    pub fn on_sketch<F>(mut self, f: F) -> Self
    where
        F: Fn(SketchRequest, CallScope) -> NgcResult<SketchResponse> + Send + Sync + 'static,
    {
        self.sketch_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Scripts synchronous `Health` responses.
    #[must_use]
    pub fn on_health<F>(mut self, f: F) -> Self
    where
        F: Fn(HealthRequest, CallScope) -> NgcResult<HealthResponse> + Send + Sync + 'static,
    {
        self.health_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Scripts asynchronous `Health` responses.
    #[must_use]
    pub fn on_health_async<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(HealthRequest, CallScope) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = NgcResult<HealthResponse>> + Send + 'static,
    {
        self.health_fn = Some(Arc::new(move |req, scope| Box::pin(f(req, scope))));
        self
    }

    /// Scripts synchronous `Explain` responses.
    #[must_use]
    pub fn on_explain<F>(mut self, f: F) -> Self
    where
        F: Fn(QueryRequest, CallScope) -> NgcResult<ExplainResponse> + Send + Sync + 'static,
    {
        self.explain_fn = Some(Arc::new(move |req, scope| {
            let res = f(req, scope);
            Box::pin(std::future::ready(res))
        }));
        self
    }

    /// Total RPC invocations started on this [`MockConnector`].
    #[must_use]
    pub fn total_calls(&self) -> u64 {
        self.metrics.total_calls.load(Ordering::Acquire)
    }

    /// Invocations started for a specific [`RpcMethod`].
    #[must_use]
    pub fn calls_for(&self, method: RpcMethod) -> u64 {
        self.metrics.per_method[method as usize].load(Ordering::Acquire)
    }

    /// Current number of in-flight futures and open streams.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.metrics.in_flight.load(Ordering::Acquire)
    }

    /// Peak concurrent in-flight futures/streams observed since construction.
    #[must_use]
    pub fn max_in_flight_observed(&self) -> usize {
        self.metrics.max_in_flight.load(Ordering::Acquire)
    }

    /// Number of futures or streams dropped before reaching natural completion (cancellation/deadline).
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.metrics.dropped_count.load(Ordering::Acquire)
    }

    /// Waits until `dropped_count()` reaches at least `target` without polling or wall-clock sleeps.
    pub async fn wait_for_drops(&self, target: u64) {
        loop {
            let notified = self.metrics.drop_notify.notified();
            if self.dropped_count() >= target {
                return;
            }
            notified.await;
        }
    }

    fn run_unary<Req, Resp, D>(
        &self,
        method: RpcMethod,
        req: Req,
        scope: CallScope,
        handler: Option<UnaryHandler<Req, Resp>>,
        default_fn: D,
    ) -> impl Future<Output = NgcResult<Resp>> + Send
    where
        Req: Send + 'static,
        Resp: Send + 'static,
        D: FnOnce(Req, CallScope) -> NgcResult<Resp> + Send + 'static,
    {
        let mut guard = self.metrics.record_start(method);
        let delay = self.delay;
        async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let out = match handler {
                Some(h) => h(req, scope).await,
                None => default_fn(req, scope),
            };
            guard.mark_completed();
            out
        }
    }

    fn run_stream<Req, Item, D>(
        &self,
        method: RpcMethod,
        req: Req,
        scope: CallScope,
        handler: Option<StreamHandler<Req, Item>>,
        default_fn: D,
    ) -> impl Future<Output = NgcResult<BoxStream<Item>>> + Send
    where
        Req: Send + 'static,
        Item: Send + 'static,
        D: FnOnce(Req, CallScope) -> NgcResult<BoxStream<Item>> + Send + 'static,
    {
        let mut guard = self.metrics.record_start(method);
        let delay = self.delay;
        async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let res = match handler {
                Some(h) => h(req, scope).await,
                None => default_fn(req, scope),
            };
            match res {
                Ok(inner) => {
                    let wrapped: BoxStream<Item> = Box::pin(GuardedStream { inner, guard });
                    Ok(wrapped)
                }
                Err(err) => {
                    guard.mark_completed();
                    Err(err)
                }
            }
        }
    }
}

impl Connector for MockConnector {
    fn describe(
        &self,
        request: DescribeRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<DescribeResponse>> + Send {
        self.run_unary(
            RpcMethod::Describe,
            request,
            scope,
            self.describe_fn.clone(),
            |_req, _scope| Ok(DescribeResponse::default()),
        )
    }

    fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send {
        self.run_stream(
            RpcMethod::Query,
            request,
            scope,
            self.query_fn.clone(),
            |_req, _scope| {
                let s: BoxStream<QueryResponse> = Box::pin(stream::empty());
                Ok(s)
            },
        )
    }

    fn mutate(
        &self,
        request: MutateRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<MutateResponse>> + Send {
        self.run_unary(
            RpcMethod::Mutate,
            request,
            scope,
            self.mutate_fn.clone(),
            |_req, _scope| Err(unimplemented_err("Mutate")),
        )
    }

    fn subscribe(
        &self,
        request: SubscribeRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<BoxStream<Event>>> + Send {
        self.run_stream(
            RpcMethod::Subscribe,
            request,
            scope,
            self.subscribe_fn.clone(),
            |_req, _scope| Err(unimplemented_err("Subscribe")),
        )
    }

    fn start_task(
        &self,
        request: StartTaskRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<TaskStatus>> + Send {
        self.run_unary(
            RpcMethod::StartTask,
            request,
            scope,
            self.start_task_fn.clone(),
            |_req, _scope| Err(unimplemented_err("StartTask")),
        )
    }

    fn get_task(
        &self,
        request: GetTaskRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<TaskStatus>> + Send {
        self.run_unary(
            RpcMethod::GetTask,
            request,
            scope,
            self.get_task_fn.clone(),
            |_req, _scope| Err(unimplemented_err("GetTask")),
        )
    }

    fn cancel_task(
        &self,
        request: CancelTaskRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<TaskStatus>> + Send {
        self.run_unary(
            RpcMethod::CancelTask,
            request,
            scope,
            self.cancel_task_fn.clone(),
            |_req, _scope| Err(unimplemented_err("CancelTask")),
        )
    }

    fn watch_task(
        &self,
        request: WatchTaskRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<BoxStream<TaskStatus>>> + Send {
        self.run_stream(
            RpcMethod::WatchTask,
            request,
            scope,
            self.watch_task_fn.clone(),
            |_req, _scope| Err(unimplemented_err("WatchTask")),
        )
    }

    fn sketch(
        &self,
        request: SketchRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<SketchResponse>> + Send {
        self.run_unary(
            RpcMethod::Sketch,
            request,
            scope,
            self.sketch_fn.clone(),
            |_req, _scope| Err(unimplemented_err("Sketch")),
        )
    }

    fn health(
        &self,
        request: HealthRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<HealthResponse>> + Send {
        self.run_unary(
            RpcMethod::Health,
            request,
            scope,
            self.health_fn.clone(),
            |_req, _scope| {
                Ok(HealthResponse {
                    status: ServingStatus::Serving as i32,
                    version: NGC_WIRE_VERSION.to_owned(),
                    details: None,
                })
            },
        )
    }

    fn explain(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<ExplainResponse>> + Send {
        self.run_unary(
            RpcMethod::Explain,
            request,
            scope,
            self.explain_fn.clone(),
            |_req, _scope| Err(unimplemented_err("Explain")),
        )
    }
}
