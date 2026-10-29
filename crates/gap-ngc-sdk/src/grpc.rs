//! gRPC transport adapter and typed [`GrpcClient`] over `tonic` (`NGC-001`, `NGC-062`, `NGC-063`, `NGC-093`).

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::{Stream, StreamExt as _};
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::ngc::v1::{
    CancelTaskRequest, DescribeRequest, DescribeResponse, Event, ExplainResponse, GetTaskRequest,
    HealthRequest, HealthResponse, MutateRequest, MutateResponse, QueryRequest, QueryResponse,
    SketchRequest, SketchResponse, StartTaskRequest, SubscribeRequest, TaskStatus,
    WatchTaskRequest,
    connector_client::ConnectorClient as TonicConnectorClient,
    connector_server::{Connector as TonicConnector, ConnectorServer},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tonic::metadata::{Ascii, MetadataKey, MetadataValue};
use tonic::transport::{Channel, Server, server::Connected};

use crate::connector::{BoxStream, Connector, NgcResult};
use crate::core::{ServerCore, headers_to_metadata};
use crate::error::{ngc_error_to_status, status_to_ngc_error};

/// Error returned when starting or running a gRPC connector server (`NGC-001`, `NGC-093` (g)).
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// Refused to bind plain-text TCP on a non-loopback address without `allow_insecure_non_loopback` (`NGC-093` (g)).
    #[error(
        "refusing plain-text gRPC bind on non-loopback address `{0}` without allow_insecure_non_loopback (NGC-093g)"
    )]
    InsecureNonLoopbackAddress(SocketAddr),
    /// Socket bind or accept I/O failure.
    #[error("gRPC listener I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// `tonic` transport runtime failure.
    #[error("gRPC transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
}

/// Thin `tonic` service adapter delegating all `NGC-093` enforcement to [`ServerCore`].
pub struct GrpcService<C> {
    core: Arc<ServerCore<C>>,
}

impl<C> Clone for GrpcService<C> {
    fn clone(&self) -> Self {
        Self {
            core: Arc::clone(&self.core),
        }
    }
}

impl<C: Connector> GrpcService<C> {
    /// Creates a [`GrpcService`] backed by `core`.
    #[must_use]
    pub const fn new(core: Arc<ServerCore<C>>) -> Self {
        Self { core }
    }

    /// Converts `self` into a configured [`ConnectorServer`] enforcing `max_request_bytes` (`NGC-093` (f)).
    #[must_use]
    pub fn into_server(self) -> ConnectorServer<Self> {
        let max_req = self.core.config().max_request_bytes;
        ConnectorServer::new(self).max_decoding_message_size(max_req)
    }
}

fn unpack_request<T>(req: tonic::Request<T>) -> (BTreeMap<String, String>, T) {
    let meta = headers_to_metadata(req.metadata().as_ref());
    (meta, req.into_inner())
}

#[tonic::async_trait]
impl<C: Connector> TonicConnector for GrpcService<C> {
    async fn describe(
        &self,
        request: tonic::Request<DescribeRequest>,
    ) -> Result<tonic::Response<DescribeResponse>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        self.core
            .describe(inner, meta, CancellationToken::new())
            .await
            .map(tonic::Response::new)
            .map_err(|e| ngc_error_to_status(&e))
    }

    type QueryStream =
        Pin<Box<dyn Stream<Item = Result<QueryResponse, tonic::Status>> + Send + 'static>>;

    async fn query(
        &self,
        request: tonic::Request<QueryRequest>,
    ) -> Result<tonic::Response<Self::QueryStream>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        let stream = self
            .core
            .query(inner, meta, CancellationToken::new())
            .await
            .map_err(|e| ngc_error_to_status(&e))?;
        let mapped = stream.map(|item| item.map_err(|e| ngc_error_to_status(&e)));
        Ok(tonic::Response::new(Box::pin(mapped)))
    }

    async fn mutate(
        &self,
        request: tonic::Request<MutateRequest>,
    ) -> Result<tonic::Response<MutateResponse>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        self.core
            .mutate(inner, meta, CancellationToken::new())
            .await
            .map(tonic::Response::new)
            .map_err(|e| ngc_error_to_status(&e))
    }

    type SubscribeStream =
        Pin<Box<dyn Stream<Item = Result<Event, tonic::Status>> + Send + 'static>>;

    async fn subscribe(
        &self,
        request: tonic::Request<SubscribeRequest>,
    ) -> Result<tonic::Response<Self::SubscribeStream>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        let stream = self
            .core
            .subscribe(inner, meta, CancellationToken::new())
            .await
            .map_err(|e| ngc_error_to_status(&e))?;
        let mapped = stream.map(|item| item.map_err(|e| ngc_error_to_status(&e)));
        Ok(tonic::Response::new(Box::pin(mapped)))
    }

    async fn start_task(
        &self,
        request: tonic::Request<StartTaskRequest>,
    ) -> Result<tonic::Response<TaskStatus>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        self.core
            .start_task(inner, meta, CancellationToken::new())
            .await
            .map(tonic::Response::new)
            .map_err(|e| ngc_error_to_status(&e))
    }

    async fn get_task(
        &self,
        request: tonic::Request<GetTaskRequest>,
    ) -> Result<tonic::Response<TaskStatus>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        self.core
            .get_task(inner, meta, CancellationToken::new())
            .await
            .map(tonic::Response::new)
            .map_err(|e| ngc_error_to_status(&e))
    }

    async fn cancel_task(
        &self,
        request: tonic::Request<CancelTaskRequest>,
    ) -> Result<tonic::Response<TaskStatus>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        self.core
            .cancel_task(inner, meta, CancellationToken::new())
            .await
            .map(tonic::Response::new)
            .map_err(|e| ngc_error_to_status(&e))
    }

    type WatchTaskStream =
        Pin<Box<dyn Stream<Item = Result<TaskStatus, tonic::Status>> + Send + 'static>>;

    async fn watch_task(
        &self,
        request: tonic::Request<WatchTaskRequest>,
    ) -> Result<tonic::Response<Self::WatchTaskStream>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        let stream = self
            .core
            .watch_task(inner, meta, CancellationToken::new())
            .await
            .map_err(|e| ngc_error_to_status(&e))?;
        let mapped = stream.map(|item| item.map_err(|e| ngc_error_to_status(&e)));
        Ok(tonic::Response::new(Box::pin(mapped)))
    }

    async fn sketch(
        &self,
        request: tonic::Request<SketchRequest>,
    ) -> Result<tonic::Response<SketchResponse>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        self.core
            .sketch(inner, meta, CancellationToken::new())
            .await
            .map(tonic::Response::new)
            .map_err(|e| ngc_error_to_status(&e))
    }

    async fn health(
        &self,
        request: tonic::Request<HealthRequest>,
    ) -> Result<tonic::Response<HealthResponse>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        self.core
            .health(inner, meta, CancellationToken::new())
            .await
            .map(tonic::Response::new)
            .map_err(|e| ngc_error_to_status(&e))
    }

    async fn explain(
        &self,
        request: tonic::Request<QueryRequest>,
    ) -> Result<tonic::Response<ExplainResponse>, tonic::Status> {
        let (meta, inner) = unpack_request(request);
        self.core
            .explain(inner, meta, CancellationToken::new())
            .await
            .map(tonic::Response::new)
            .map_err(|e| ngc_error_to_status(&e))
    }
}

/// Serves `core` over a generic stream of accepted I/O connections (`NGC-001`).
///
/// # Errors
///
/// Returns [`ServeError::Transport`] if the underlying `tonic` server encounters a fatal error.
pub async fn serve_incoming<C, I, IO, IE>(
    core: Arc<ServerCore<C>>,
    incoming: I,
) -> Result<(), ServeError>
where
    C: Connector,
    I: Stream<Item = Result<IO, IE>>,
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    IE: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let svc = GrpcService::new(core).into_server();
    Server::builder()
        .add_service(svc)
        .serve_with_incoming(incoming)
        .await?;
    Ok(())
}

/// Serves `core` over `incoming` until `shutdown` resolves (`NGC-001`).
///
/// # Errors
///
/// Returns [`ServeError::Transport`] if the underlying `tonic` server encounters a fatal error.
pub async fn serve_incoming_with_shutdown<C, I, IO, IE, F>(
    core: Arc<ServerCore<C>>,
    incoming: I,
    shutdown: F,
) -> Result<(), ServeError>
where
    C: Connector,
    I: Stream<Item = Result<IO, IE>>,
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    IE: Into<Box<dyn std::error::Error + Send + Sync>>,
    F: Future<Output = ()>,
{
    let svc = GrpcService::new(core).into_server();
    Server::builder()
        .add_service(svc)
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await?;
    Ok(())
}

/// Serves `core` over plain-text TCP at `addr`, failing closed on non-loopback addresses unless
/// `allow_insecure_non_loopback` is explicitly enabled (`NGC-001`, `NGC-093` (g)).
///
/// # Errors
///
/// Returns [`ServeError::InsecureNonLoopbackAddress`] before touching any socket if `addr` is not
/// loopback and `allow_insecure_non_loopback` is `false`.
pub async fn serve_tcp<C: Connector>(
    core: Arc<ServerCore<C>>,
    addr: SocketAddr,
) -> Result<(), ServeError> {
    if !addr.ip().is_loopback() && !core.config().allow_insecure_non_loopback {
        return Err(ServeError::InsecureNonLoopbackAddress(addr));
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    serve_incoming(core, incoming).await
}

/// Serves `core` over a Unix-domain socket at `path` (`NGC-001`, `NGC-093` (g)).
///
/// # Errors
///
/// Returns [`ServeError::Io`] on socket bind failure or [`ServeError::Transport`] on transport failure.
#[cfg(unix)]
pub async fn serve_uds<C: Connector>(
    core: Arc<ServerCore<C>>,
    path: impl AsRef<Path>,
) -> Result<(), ServeError> {
    let listener = tokio::net::UnixListener::bind(path.as_ref())?;
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
    serve_incoming(core, incoming).await
}

fn build_tonic_request<T>(
    payload: T,
    metadata: &BTreeMap<String, String>,
) -> NgcResult<tonic::Request<T>> {
    let mut req = tonic::Request::new(payload);
    for (k, v) in metadata {
        let key: MetadataKey<Ascii> = k.parse().map_err(|_| {
            NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("invalid gRPC metadata key `{k}`"),
            )
        })?;
        let val: MetadataValue<Ascii> = v.parse().map_err(|_| {
            NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("invalid gRPC metadata value for key `{k}`"),
            )
        })?;
        req.metadata_mut().insert(key, val);
    }
    Ok(req)
}

/// Typed gRPC client mapping `tonic::Status` back to [`NgcErrorInfo`] via `ngc-error-bin` with
/// code-table fallback (`NGC-062`, `NGC-063`).
#[derive(Debug, Clone)]
pub struct GrpcClient {
    inner: TonicConnectorClient<Channel>,
}

impl GrpcClient {
    /// Wraps an existing `tonic` [`Channel`].
    #[must_use]
    pub fn from_channel(channel: Channel) -> Self {
        Self {
            inner: TonicConnectorClient::new(channel).max_decoding_message_size(usize::MAX),
        }
    }

    /// Invokes `Describe`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] decoded from `tonic::Status`.
    pub async fn describe(&self, request: DescribeRequest) -> NgcResult<DescribeResponse> {
        self.describe_with_metadata(request, &BTreeMap::new()).await
    }

    /// Invokes `Describe` with ASCII gRPC call metadata.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on invalid metadata or RPC failure.
    pub async fn describe_with_metadata(
        &self,
        request: DescribeRequest,
        metadata: &BTreeMap<String, String>,
    ) -> NgcResult<DescribeResponse> {
        let req = build_tonic_request(request, metadata)?;
        self.inner
            .clone()
            .describe(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))
    }

    /// Invokes `Query` and returns a stream of [`QueryResponse`] items.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] if opening the stream fails.
    pub async fn query(&self, request: QueryRequest) -> NgcResult<BoxStream<QueryResponse>> {
        self.query_with_metadata(request, &BTreeMap::new()).await
    }

    /// Invokes `Query` with ASCII gRPC call metadata.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on invalid metadata or RPC failure.
    pub async fn query_with_metadata(
        &self,
        request: QueryRequest,
        metadata: &BTreeMap<String, String>,
    ) -> NgcResult<BoxStream<QueryResponse>> {
        let req = build_tonic_request(request, metadata)?;
        let streaming = self
            .inner
            .clone()
            .query(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))?;
        Ok(Box::pin(
            streaming.map(|res| res.map_err(|s| status_to_ngc_error(&s))),
        ))
    }

    /// Invokes `Mutate`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] decoded from `tonic::Status`.
    pub async fn mutate(&self, request: MutateRequest) -> NgcResult<MutateResponse> {
        self.mutate_with_metadata(request, &BTreeMap::new()).await
    }

    /// Invokes `Mutate` with ASCII gRPC call metadata.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on invalid metadata or RPC failure.
    pub async fn mutate_with_metadata(
        &self,
        request: MutateRequest,
        metadata: &BTreeMap<String, String>,
    ) -> NgcResult<MutateResponse> {
        let req = build_tonic_request(request, metadata)?;
        self.inner
            .clone()
            .mutate(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))
    }

    /// Invokes `Subscribe` and returns a stream of [`Event`] items.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] if opening the stream fails.
    pub async fn subscribe(&self, request: SubscribeRequest) -> NgcResult<BoxStream<Event>> {
        self.subscribe_with_metadata(request, &BTreeMap::new())
            .await
    }

    /// Invokes `Subscribe` with ASCII gRPC call metadata.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] on invalid metadata or RPC failure.
    pub async fn subscribe_with_metadata(
        &self,
        request: SubscribeRequest,
        metadata: &BTreeMap<String, String>,
    ) -> NgcResult<BoxStream<Event>> {
        let req = build_tonic_request(request, metadata)?;
        let streaming = self
            .inner
            .clone()
            .subscribe(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))?;
        Ok(Box::pin(
            streaming.map(|res| res.map_err(|s| status_to_ngc_error(&s))),
        ))
    }

    /// Invokes `StartTask`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] decoded from `tonic::Status`.
    pub async fn start_task(&self, request: StartTaskRequest) -> NgcResult<TaskStatus> {
        let req = build_tonic_request(request, &BTreeMap::new())?;
        self.inner
            .clone()
            .start_task(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))
    }

    /// Invokes `GetTask`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] decoded from `tonic::Status`.
    pub async fn get_task(&self, request: GetTaskRequest) -> NgcResult<TaskStatus> {
        let req = build_tonic_request(request, &BTreeMap::new())?;
        self.inner
            .clone()
            .get_task(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))
    }

    /// Invokes `CancelTask`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] decoded from `tonic::Status`.
    pub async fn cancel_task(&self, request: CancelTaskRequest) -> NgcResult<TaskStatus> {
        let req = build_tonic_request(request, &BTreeMap::new())?;
        self.inner
            .clone()
            .cancel_task(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))
    }

    /// Invokes `WatchTask` and returns a stream of [`TaskStatus`] items.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] if opening the stream fails.
    pub async fn watch_task(&self, request: WatchTaskRequest) -> NgcResult<BoxStream<TaskStatus>> {
        let req = build_tonic_request(request, &BTreeMap::new())?;
        let streaming = self
            .inner
            .clone()
            .watch_task(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))?;
        Ok(Box::pin(
            streaming.map(|res| res.map_err(|s| status_to_ngc_error(&s))),
        ))
    }

    /// Invokes `Sketch`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] decoded from `tonic::Status`.
    pub async fn sketch(&self, request: SketchRequest) -> NgcResult<SketchResponse> {
        let req = build_tonic_request(request, &BTreeMap::new())?;
        self.inner
            .clone()
            .sketch(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))
    }

    /// Invokes `Health`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] decoded from `tonic::Status`.
    pub async fn health(&self, request: HealthRequest) -> NgcResult<HealthResponse> {
        let req = build_tonic_request(request, &BTreeMap::new())?;
        self.inner
            .clone()
            .health(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))
    }

    /// Invokes `Explain`.
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] decoded from `tonic::Status`.
    pub async fn explain(&self, request: QueryRequest) -> NgcResult<ExplainResponse> {
        let req = build_tonic_request(request, &BTreeMap::new())?;
        self.inner
            .clone()
            .explain(req)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| status_to_ngc_error(&s))
    }
}
