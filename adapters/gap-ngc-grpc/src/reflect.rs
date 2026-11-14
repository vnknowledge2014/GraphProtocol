//! Descriptor loading, `protox` compilation, and dynamic gRPC Server Reflection (`v1` / `v1alpha`) (`spec/09-compat.md` §5.3).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt as _;
use gap_core::error::{NgcErrorCode, NgcErrorInfo, clamp_error_message};
use gap_ngc_sdk::connector::{BoxStream, NgcResult};
use http::uri::PathAndQuery;
use prost::Message as _;
use prost_reflect::DescriptorPool;
use prost_types::{FileDescriptorProto, FileDescriptorSet};
use protox::file::{ChainFileResolver, File, FileResolver, GoogleFileResolver};
use tokio::time::timeout;
use tokio_stream::iter;
use tonic::Code;
use tonic::client::Grpc;
use tonic::metadata::MetadataMap;
use tonic::transport::{Channel, Endpoint};

use crate::codec::RawBytesCodec;
use crate::config::DescriptorSource;

const REFLECTION_V1_PATH: &str = "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo";
const REFLECTION_V1ALPHA_PATH: &str =
    "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo";

/// Wire-compatible `grpc.reflection.v1.ServerReflectionRequest` (`v1` & `v1alpha` share field tags).
#[derive(Clone, PartialEq, prost::Message)]
pub struct ReflectionRequest {
    #[prost(string, tag = "1")]
    pub host: String,
    #[prost(oneof = "reflection_request::MessageRequest", tags = "3, 4, 7")]
    pub message_request: Option<reflection_request::MessageRequest>,
}

/// Inner oneof variants for [`ReflectionRequest`].
pub mod reflection_request {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum MessageRequest {
        #[prost(string, tag = "3")]
        FileByFilename(String),
        #[prost(string, tag = "4")]
        FileContainingSymbol(String),
        #[prost(string, tag = "7")]
        ListServices(String),
    }
}

/// Wire-compatible `grpc.reflection.v1.ServerReflectionResponse`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ReflectionResponse {
    #[prost(string, tag = "1")]
    pub valid_host: String,
    #[prost(oneof = "reflection_response::MessageResponse", tags = "4, 6, 7")]
    pub message_response: Option<reflection_response::MessageResponse>,
}

/// Inner oneof variants for [`ReflectionResponse`].
pub mod reflection_response {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum MessageResponse {
        #[prost(message, tag = "4")]
        FileDescriptorResponse(super::FileDescriptorResponse),
        #[prost(message, tag = "6")]
        ListServicesResponse(super::ListServiceResponse),
        #[prost(message, tag = "7")]
        ErrorResponse(super::ErrorResponse),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FileDescriptorResponse {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub file_descriptor_proto: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ListServiceResponse {
    #[prost(message, repeated, tag = "1")]
    pub service: Vec<ServiceResponse>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ServiceResponse {
    #[prost(string, tag = "1")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ErrorResponse {
    #[prost(int32, tag = "1")]
    pub error_code: i32,
    #[prost(string, tag = "2")]
    pub error_message: String,
}

/// Abstraction over gRPC HTTP/2 RPC transport supporting unary, server-streaming, and bidi reflection (`spec/09-compat.md` §5.3).
pub trait GrpcTransport: Send + Sync + 'static {
    /// Invokes a unary RPC at `/<package.Service>/<Method>`.
    fn unary(
        &self,
        path: PathAndQuery,
        metadata: MetadataMap,
        request_bytes: Bytes,
        timeout_dur: Duration,
    ) -> impl Future<Output = NgcResult<Bytes>> + Send;

    /// Invokes a server-streaming RPC at `/<package.Service>/<Method>`.
    fn server_streaming(
        &self,
        path: PathAndQuery,
        metadata: MetadataMap,
        request_bytes: Bytes,
        timeout_dur: Duration,
    ) -> impl Future<Output = NgcResult<BoxStream<Bytes>>> + Send;

    /// Sends a finite batch of frames on a bidirectional-streaming RPC and collects responses (used for Server Reflection).
    fn bidi_streaming(
        &self,
        path: PathAndQuery,
        metadata: MetadataMap,
        requests: Vec<Bytes>,
        timeout_dur: Duration,
    ) -> impl Future<Output = NgcResult<Vec<Bytes>>> + Send;
}

/// Real `tonic::transport::Channel` implementation of [`GrpcTransport`].
#[derive(Debug, Clone)]
pub struct TonicChannelTransport {
    channel: Channel,
    max_decoding_bytes: usize,
}

impl TonicChannelTransport {
    /// Wraps an already-established `tonic::transport::Channel`.
    #[must_use]
    pub const fn new(channel: Channel, max_decoding_bytes: usize) -> Self {
        Self {
            channel,
            max_decoding_bytes,
        }
    }

    /// Creates a lazy HTTP/2 `Channel` targeting `endpoint`.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGUMENT` if `endpoint` cannot be parsed as a valid tonic `Endpoint`.
    pub fn connect_lazy(endpoint: &str, max_decoding_bytes: usize) -> NgcResult<Self> {
        let ep = Endpoint::from_shared(endpoint.to_owned()).map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("invalid gRPC endpoint URI: {err}"),
            )
        })?;
        Ok(Self::new(ep.connect_lazy(), max_decoding_bytes))
    }

    async fn ready_grpc_client(
        &self,
        timeout_dur: Duration,
    ) -> NgcResult<(Grpc<Channel>, Duration)> {
        let started = tokio::time::Instant::now();
        let mut client =
            Grpc::new(self.channel.clone()).max_decoding_message_size(self.max_decoding_bytes);
        let ready_res = timeout(timeout_dur, client.ready()).await.map_err(|_| {
            NgcErrorInfo::new(
                NgcErrorCode::DeadlineExceeded,
                "gRPC channel readiness exceeded deadline",
            )
            .with_retry(true, 0)
        })?;
        ready_res.map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::Unavailable,
                format!("gRPC channel not ready: {err}"),
            )
            .with_retry(true, 0)
        })?;
        let remaining = timeout_dur
            .saturating_sub(started.elapsed())
            .max(Duration::from_millis(1));
        Ok((client, remaining))
    }
}

/// Converts a `tonic::Status` into a clamped, secret-free [`NgcErrorInfo`].
#[must_use]
pub fn map_tonic_status(status: &tonic::Status) -> NgcErrorInfo {
    let (code, retryable) = match status.code() {
        Code::InvalidArgument | Code::OutOfRange => (NgcErrorCode::InvalidArgument, false),
        Code::NotFound => (NgcErrorCode::NotFound, false),
        Code::AlreadyExists | Code::Aborted => (NgcErrorCode::Conflict, false),
        Code::PermissionDenied => (NgcErrorCode::PermissionDenied, false),
        Code::Unauthenticated => (NgcErrorCode::Unauthenticated, false),
        Code::ResourceExhausted => (NgcErrorCode::ResourceExhausted, true),
        Code::FailedPrecondition => (NgcErrorCode::FailedPrecondition, false),
        Code::DeadlineExceeded => (NgcErrorCode::DeadlineExceeded, true),
        Code::Cancelled
            if status
                .message()
                .to_ascii_lowercase()
                .contains("timeout expired") =>
        {
            (NgcErrorCode::DeadlineExceeded, true)
        }
        Code::Cancelled => (NgcErrorCode::Cancelled, false),
        Code::Unavailable => (NgcErrorCode::Unavailable, true),
        Code::Unimplemented => (NgcErrorCode::Unimplemented, false),
        Code::Internal | Code::DataLoss => (NgcErrorCode::Internal, false),
        Code::Ok | Code::Unknown => (NgcErrorCode::UpstreamError, false),
    };
    let msg = if status.message().is_empty() {
        format!("upstream gRPC error ({:?})", status.code())
    } else {
        clamp_error_message(status.message())
    };
    NgcErrorInfo::new(code, msg).with_retry(retryable, 0)
}

fn attach_metadata(target: &mut MetadataMap, source: &MetadataMap) {
    for kv in source.iter() {
        match kv {
            tonic::metadata::KeyAndValueRef::Ascii(k, v) => {
                target.insert(k.clone(), v.clone());
            }
            tonic::metadata::KeyAndValueRef::Binary(k, v) => {
                target.insert_bin(k.clone(), v.clone());
            }
        }
    }
}

impl GrpcTransport for TonicChannelTransport {
    async fn unary(
        &self,
        path: PathAndQuery,
        metadata: MetadataMap,
        request_bytes: Bytes,
        timeout_dur: Duration,
    ) -> NgcResult<Bytes> {
        let (mut client, remaining) = self.ready_grpc_client(timeout_dur).await?;
        let mut req = tonic::Request::new(request_bytes);
        attach_metadata(req.metadata_mut(), &metadata);
        req.set_timeout(remaining);

        let fut = client.unary(req, path, RawBytesCodec);
        match timeout(remaining, fut).await {
            Ok(Ok(resp)) => Ok(resp.into_inner()),
            Ok(Err(status)) => Err(map_tonic_status(&status)),
            Err(_) => Err(NgcErrorInfo::new(
                NgcErrorCode::DeadlineExceeded,
                "gRPC unary call exceeded deadline",
            )
            .with_retry(true, 0)),
        }
    }

    async fn server_streaming(
        &self,
        path: PathAndQuery,
        metadata: MetadataMap,
        request_bytes: Bytes,
        timeout_dur: Duration,
    ) -> NgcResult<BoxStream<Bytes>> {
        let (mut client, remaining) = self.ready_grpc_client(timeout_dur).await?;
        let mut req = tonic::Request::new(request_bytes);
        attach_metadata(req.metadata_mut(), &metadata);
        req.set_timeout(remaining);

        let response = timeout(remaining, client.server_streaming(req, path, RawBytesCodec))
            .await
            .map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::DeadlineExceeded,
                    "gRPC stream open exceeded deadline",
                )
                .with_retry(true, 0)
            })?
            .map_err(|status| map_tonic_status(&status))?;

        let stream = response
            .into_inner()
            .map(|res| res.map_err(|status| map_tonic_status(&status)));
        Ok(Box::pin(stream))
    }

    async fn bidi_streaming(
        &self,
        path: PathAndQuery,
        metadata: MetadataMap,
        requests: Vec<Bytes>,
        timeout_dur: Duration,
    ) -> NgcResult<Vec<Bytes>> {
        let (mut client, remaining) = self.ready_grpc_client(timeout_dur).await?;
        let mut req = tonic::Request::new(iter(requests));
        attach_metadata(req.metadata_mut(), &metadata);
        req.set_timeout(remaining);

        let response = timeout(remaining, client.streaming(req, path, RawBytesCodec))
            .await
            .map_err(|_| {
                NgcErrorInfo::new(
                    NgcErrorCode::DeadlineExceeded,
                    "gRPC bidi stream exceeded reflection deadline",
                )
                .with_retry(true, 0)
            })?
            .map_err(|status| map_tonic_status(&status))?;

        let mut inbound = response.into_inner();
        let mut frames = Vec::new();
        while let Some(item) = timeout(remaining, inbound.next()).await.map_err(|_| {
            NgcErrorInfo::new(
                NgcErrorCode::DeadlineExceeded,
                "gRPC bidi reflection response exceeded deadline",
            )
            .with_retry(true, 0)
        })? {
            let bytes = item.map_err(|status| map_tonic_status(&status))?;
            frames.push(bytes);
        }
        Ok(frames)
    }
}

struct InlineProtoResolver {
    files: BTreeMap<String, String>,
}

impl FileResolver for InlineProtoResolver {
    fn open_file(&self, name: &str) -> Result<File, protox::Error> {
        let source = self
            .files
            .get(name)
            .ok_or_else(|| protox::Error::file_not_found(name))?;
        File::from_source(name, source)
    }
}

/// Loaded Protobuf [`DescriptorPool`] paired with its deterministic `blake3` source hash (`NGC-010`).
#[derive(Debug, Clone)]
pub struct LoadedDescriptors {
    pub pool: DescriptorPool,
    pub spec_hash: String,
}

/// Loads descriptors from `source` (`FileDescriptorSet`, `.proto` files, inline `.proto`, or Server Reflection).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` on malformed descriptors/proto syntax or `UNAVAILABLE`/`UPSTREAM_ERROR` on reflection failure.
pub async fn load_descriptors<T: GrpcTransport>(
    source: &DescriptorSource,
    transport: Option<&T>,
    reflection_metadata: &MetadataMap,
    timeout_dur: Duration,
) -> NgcResult<LoadedDescriptors> {
    match source {
        DescriptorSource::FileDescriptorSet(bytes) => {
            let pool = DescriptorPool::decode(bytes.as_slice()).map_err(|err| {
                NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    format!("invalid protobuf FileDescriptorSet: {err}"),
                )
            })?;
            let spec_hash = blake3::hash(bytes).to_hex().to_string();
            Ok(LoadedDescriptors { pool, spec_hash })
        }
        DescriptorSource::ProtoFiles {
            files,
            include_dirs,
        } => {
            let mut compiler = CompilerBuilder::from_includes(include_dirs)?;
            compiler.include_imports(true);
            for f in files {
                compiler.open_file(f).map_err(|err| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("failed to compile proto file `{}`: {err}", f.display()),
                    )
                })?;
            }
            let encoded = compiler.encode_file_descriptor_set();
            let spec_hash = blake3::hash(&encoded).to_hex().to_string();
            let pool = compiler.descriptor_pool();
            Ok(LoadedDescriptors { pool, spec_hash })
        }
        DescriptorSource::InlineProtos(sources) => {
            if sources.is_empty() {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "InlineProtos source must contain at least one .proto file",
                ));
            }
            let mut map = BTreeMap::new();
            let mut names = Vec::with_capacity(sources.len());
            for (name, src) in sources {
                map.insert(name.clone(), src.clone());
                names.push(name.clone());
            }
            let mut resolver = ChainFileResolver::new();
            resolver.add(InlineProtoResolver { files: map });
            resolver.add(GoogleFileResolver::new());
            let mut compiler = protox::Compiler::with_file_resolver(resolver);
            compiler.include_imports(true);
            for name in &names {
                compiler.open_file(name).map_err(|err| {
                    NgcErrorInfo::new(
                        NgcErrorCode::InvalidArgument,
                        format!("failed to compile inline proto `{name}`: {err}"),
                    )
                })?;
            }
            let encoded = compiler.encode_file_descriptor_set();
            let spec_hash = blake3::hash(&encoded).to_hex().to_string();
            let pool = compiler.descriptor_pool();
            Ok(LoadedDescriptors { pool, spec_hash })
        }
        DescriptorSource::ServerReflection { services } => {
            let tr = transport.ok_or_else(|| {
                NgcErrorInfo::new(
                    NgcErrorCode::FailedPrecondition,
                    "ServerReflection descriptor source requires a gRPC transport",
                )
            })?;
            load_via_server_reflection(tr, reflection_metadata, services, timeout_dur).await
        }
    }
}

struct CompilerBuilder;

impl CompilerBuilder {
    fn from_includes(include_dirs: &[std::path::PathBuf]) -> NgcResult<protox::Compiler> {
        protox::Compiler::new(include_dirs).map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("invalid proto include directory: {err}"),
            )
        })
    }
}

async fn reflect_roundtrip<T: GrpcTransport>(
    transport: &T,
    path: &str,
    metadata: &MetadataMap,
    req: ReflectionRequest,
    timeout_dur: Duration,
) -> NgcResult<Vec<ReflectionResponse>> {
    let parsed_path = path.parse::<PathAndQuery>().map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::Internal,
            format!("bad reflection path: {err}"),
        )
    })?;
    let bytes = Bytes::from(req.encode_to_vec());
    let raw_resps = transport
        .bidi_streaming(parsed_path, metadata.clone(), vec![bytes], timeout_dur)
        .await?;

    let mut out = Vec::with_capacity(raw_resps.len());
    for raw in raw_resps {
        let decoded = ReflectionResponse::decode(raw.as_ref()).map_err(|err| {
            NgcErrorInfo::new(
                NgcErrorCode::UpstreamError,
                format!("malformed gRPC ServerReflectionResponse: {err}"),
            )
        })?;
        out.push(decoded);
    }
    Ok(out)
}

async fn load_via_server_reflection<T: GrpcTransport>(
    transport: &T,
    metadata: &MetadataMap,
    filter_services: &[String],
    timeout_dur: Duration,
) -> NgcResult<LoadedDescriptors> {
    let list_req = ReflectionRequest {
        host: String::new(),
        message_request: Some(reflection_request::MessageRequest::ListServices(
            "*".to_owned(),
        )),
    };

    let (active_path, list_resps) = match reflect_roundtrip(
        transport,
        REFLECTION_V1_PATH,
        metadata,
        list_req.clone(),
        timeout_dur,
    )
    .await
    {
        Ok(resps) => (REFLECTION_V1_PATH, resps),
        Err(err) if err.code == NgcErrorCode::Unimplemented => {
            let resps = reflect_roundtrip(
                transport,
                REFLECTION_V1ALPHA_PATH,
                metadata,
                list_req,
                timeout_dur,
            )
            .await?;
            (REFLECTION_V1ALPHA_PATH, resps)
        }
        Err(err) => return Err(err),
    };

    let mut services = BTreeSet::new();
    for resp in list_resps {
        match resp.message_response {
            Some(reflection_response::MessageResponse::ListServicesResponse(list)) => {
                for svc in list.service {
                    if !svc.name.starts_with("grpc.reflection.") {
                        services.insert(svc.name);
                    }
                }
            }
            Some(reflection_response::MessageResponse::ErrorResponse(err)) => {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::UpstreamError,
                    format!(
                        "gRPC reflection list_services failed ({}): {}",
                        err.error_code, err.error_message
                    ),
                ));
            }
            _ => {}
        }
    }

    let targets: Vec<String> = if filter_services.is_empty() {
        services.into_iter().collect()
    } else {
        filter_services
            .iter()
            .filter(|s| services.contains(s.as_str()))
            .cloned()
            .collect()
    };

    let mut files_by_name: BTreeMap<String, FileDescriptorProto> = BTreeMap::new();
    let mut pending_files: VecDeque<String> = VecDeque::new();

    for svc in targets {
        let req = ReflectionRequest {
            host: String::new(),
            message_request: Some(reflection_request::MessageRequest::FileContainingSymbol(
                svc,
            )),
        };
        let resps = reflect_roundtrip(transport, active_path, metadata, req, timeout_dur).await?;
        collect_descriptor_responses(resps, &mut files_by_name, &mut pending_files)?;
    }

    while let Some(dep_name) = pending_files.pop_front() {
        if files_by_name.contains_key(&dep_name) {
            continue;
        }
        let req = ReflectionRequest {
            host: String::new(),
            message_request: Some(reflection_request::MessageRequest::FileByFilename(dep_name)),
        };
        let resps = reflect_roundtrip(transport, active_path, metadata, req, timeout_dur).await?;
        collect_descriptor_responses(resps, &mut files_by_name, &mut pending_files)?;
    }

    let sorted_files = topo_sort_files(&files_by_name);
    let fds = FileDescriptorSet { file: sorted_files };
    let encoded = fds.encode_to_vec();
    let spec_hash = blake3::hash(&encoded).to_hex().to_string();
    let pool = DescriptorPool::from_file_descriptor_set(fds).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::UpstreamError,
            format!("reflected descriptors failed validation: {err}"),
        )
    })?;

    Ok(LoadedDescriptors { pool, spec_hash })
}

fn collect_descriptor_responses(
    resps: Vec<ReflectionResponse>,
    files_by_name: &mut BTreeMap<String, FileDescriptorProto>,
    pending_files: &mut VecDeque<String>,
) -> NgcResult<()> {
    for resp in resps {
        match resp.message_response {
            Some(reflection_response::MessageResponse::FileDescriptorResponse(fdr)) => {
                for raw_fdp in fdr.file_descriptor_proto {
                    let fdp = FileDescriptorProto::decode(raw_fdp.as_slice()).map_err(|err| {
                        NgcErrorInfo::new(
                            NgcErrorCode::UpstreamError,
                            format!("malformed reflected FileDescriptorProto: {err}"),
                        )
                    })?;
                    let name = fdp.name().to_owned();
                    for dep in &fdp.dependency {
                        if !files_by_name.contains_key(dep) {
                            pending_files.push_back(dep.clone());
                        }
                    }
                    files_by_name.entry(name).or_insert(fdp);
                }
            }
            Some(reflection_response::MessageResponse::ErrorResponse(err)) => {
                return Err(NgcErrorInfo::new(
                    NgcErrorCode::UpstreamError,
                    format!(
                        "gRPC reflection descriptor lookup failed ({}): {}",
                        err.error_code, err.error_message
                    ),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn topo_sort_files(
    files_by_name: &BTreeMap<String, FileDescriptorProto>,
) -> Vec<FileDescriptorProto> {
    let mut visited = BTreeSet::new();
    let mut out = Vec::with_capacity(files_by_name.len());

    fn visit(
        name: &str,
        files_by_name: &BTreeMap<String, FileDescriptorProto>,
        visited: &mut BTreeSet<String>,
        out: &mut Vec<FileDescriptorProto>,
    ) {
        if !visited.insert(name.to_owned()) {
            return;
        }
        if let Some(fdp) = files_by_name.get(name) {
            let mut deps = fdp.dependency.clone();
            deps.sort();
            for dep in deps {
                visit(&dep, files_by_name, visited, out);
            }
            out.push(fdp.clone());
        }
    }

    for name in files_by_name.keys() {
        visit(name, files_by_name, &mut visited, &mut out);
    }
    out
}
