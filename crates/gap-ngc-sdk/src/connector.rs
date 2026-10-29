//! Async [`Connector`] trait, [`CallScope`] metadata with secret redaction, and stream aliases (`NGC-031`, `NGC-080`, `NGC-090`, `INV-15`).

use std::collections::BTreeMap;
use std::fmt;
use std::future::{Future, ready};
use std::pin::Pin;

use futures_util::Stream;
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    CancelTaskRequest, DescribeRequest, DescribeResponse, Event, ExplainResponse, GetTaskRequest,
    HealthRequest, HealthResponse, MutateRequest, MutateResponse, QueryRequest, QueryResponse,
    ServingStatus, SketchRequest, SketchResponse, StartTaskRequest, SubscribeRequest, TaskStatus,
    WatchTaskRequest,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Connector result alias returning [`NgcErrorInfo`] on failure (`NGC-060`, `NGC-090`).
pub type NgcResult<T> = Result<T, NgcErrorInfo>;

/// Heap-pinned [`Send`] stream of connector items (`NGC-090`).
pub type BoxStream<T> = Pin<Box<dyn Stream<Item = NgcResult<T>> + Send>>;

/// Returns `true` if `key` carries credentials or sensitive binary/token material that MUST be
/// redacted from `Debug` output and logs (`INV-15`, `NGC-080`).
#[must_use]
pub fn is_sensitive_metadata_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower == "authorization"
        || lower == "proxy-authorization"
        || lower == "cookie"
        || lower == "set-cookie"
        || lower.ends_with("-bin")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("key")
}

struct RedactedMetadata<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for RedactedMetadata<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (k, v) in self.0 {
            if is_sensitive_metadata_key(k) {
                map.entry(k, &"[REDACTED]");
            } else {
                map.entry(k, v);
            }
        }
        map.finish()
    }
}

/// Per-RPC execution scope supplied by `ServerCore` (`NGC-025`, `NGC-080`, `NGC-090`).
///
/// `Debug` redacts `authorization` and every `*-bin`, `*token*`, `*secret*`, and `*key*` metadata
/// entry to prevent credential leakage (`INV-15`).
#[derive(Clone)]
pub struct CallScope {
    /// Fires when the caller cancels, the connection closes, or the call deadline expires.
    pub cancel: CancellationToken,
    /// Monotonic deadline instant computed by `ServerCore`.
    pub deadline: Instant,
    /// Correlation `request_id` extracted from `CallContext` (empty when absent).
    pub request_id: String,
    /// Caller `tenant_id` extracted from `CallContext` (empty when absent).
    pub tenant_id: String,
    /// W3C `traceparent` extracted from `CallContext` or transport headers.
    pub traceparent: String,
    /// Lower-case transport metadata (gRPC metadata / HTTP headers / empty on stdio).
    pub metadata: BTreeMap<String, String>,
}

impl CallScope {
    /// Constructs a minimal [`CallScope`] anchored at `deadline` with empty metadata (convenient in tests).
    #[must_use]
    pub fn for_test(deadline: Instant) -> Self {
        Self {
            cancel: CancellationToken::new(),
            deadline,
            request_id: String::new(),
            tenant_id: String::new(),
            traceparent: String::new(),
            metadata: BTreeMap::new(),
        }
    }
}

impl fmt::Debug for CallScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallScope")
            .field("cancelled", &self.cancel.is_cancelled())
            .field("deadline", &self.deadline)
            .field("request_id", &self.request_id)
            .field("tenant_id", &self.tenant_id)
            .field("traceparent", &self.traceparent)
            .field("metadata", &RedactedMetadata(&self.metadata))
            .finish()
    }
}

fn unimplemented_err(method: &str) -> NgcErrorInfo {
    NgcErrorInfo::new(
        NgcErrorCode::Unimplemented,
        format!("{method} is not implemented by this connector"),
    )
}

/// Native Graph Connector (`NGC`) async server trait (`NGC-090`).
///
/// Used statically through generics (`C: Connector`), never via `dyn Connector`.
///
/// # Cancellation safety (`NGC-025`, `NGC-090`, `INV-12`)
///
/// Implementations **MUST** be cancellation-safe: transport adapters and
/// `ServerCore` drop the returned future or [`BoxStream`] at any `.await` point immediately when
/// [`CallScope::cancel`] triggers or [`CallScope::deadline`] expires. Hold per-call resources via
/// RAII drop guards rather than cleanup code after `.await`.
pub trait Connector: Send + Sync + 'static {
    /// Returns the connector's capabilities, limits, GraphQL SDL, and operation descriptors (`NGC-010`).
    fn describe(
        &self,
        request: DescribeRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<DescribeResponse>> + Send;

    /// Executes a read operation (including `foreach` batches) as a stream of [`QueryResponse`] chunks (`NGC-020`).
    fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send;

    /// Executes a side-effecting or `dry_run` mutation (`NGC-030`, `NGC-031`).
    ///
    /// Defaults to `UNIMPLEMENTED` (including when `request.dry_run == true`, per `NGC-031`).
    fn mutate(
        &self,
        _request: MutateRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<MutateResponse>> + Send {
        ready(Err(unimplemented_err("Mutate")))
    }

    /// Streams connector events (`NGC-040`). Defaults to `UNIMPLEMENTED`.
    fn subscribe(
        &self,
        _request: SubscribeRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<BoxStream<Event>>> + Send {
        ready(Err(unimplemented_err("Subscribe")))
    }

    /// Starts an asynchronous task (`NGC-041`). Defaults to `UNIMPLEMENTED`.
    fn start_task(
        &self,
        _request: StartTaskRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<TaskStatus>> + Send {
        ready(Err(unimplemented_err("StartTask")))
    }

    /// Retrieves current status of a task (`NGC-041`). Defaults to `UNIMPLEMENTED`.
    fn get_task(
        &self,
        _request: GetTaskRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<TaskStatus>> + Send {
        ready(Err(unimplemented_err("GetTask")))
    }

    /// Requests task cancellation (`NGC-041`). Defaults to `UNIMPLEMENTED`.
    fn cancel_task(
        &self,
        _request: CancelTaskRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<TaskStatus>> + Send {
        ready(Err(unimplemented_err("CancelTask")))
    }

    /// Replays and follows task status updates (`NGC-041`). Defaults to `UNIMPLEMENTED`.
    fn watch_task(
        &self,
        _request: WatchTaskRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<BoxStream<TaskStatus>>> + Send {
        ready(Err(unimplemented_err("WatchTask")))
    }

    /// Computes bottom-k column sketch hashes (`NGC-050`). Defaults to `UNIMPLEMENTED`.
    fn sketch(
        &self,
        _request: SketchRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<SketchResponse>> + Send {
        ready(Err(unimplemented_err("Sketch")))
    }

    /// Reports connector serving readiness (`NGC-070`). Defaults to `SERVING`.
    fn health(
        &self,
        _request: HealthRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<HealthResponse>> + Send {
        ready(Ok(HealthResponse {
            status: ServingStatus::Serving as i32,
            version: NGC_WIRE_VERSION.to_owned(),
            details: None,
        }))
    }

    /// Returns the native query plan without executing (`NGC-001`). Defaults to `UNIMPLEMENTED`.
    fn explain(
        &self,
        _request: QueryRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<ExplainResponse>> + Send {
        ready(Err(unimplemented_err("Explain")))
    }
}
