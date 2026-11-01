//! Deterministic transport stack (`GraphQL-over-HTTP`, `graphql-sse`, gRPC, stdio), rate limiting, and load shedding (`L4`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod app;
pub mod config;
pub mod engine;
pub mod gateway;
pub mod health;
pub mod metrics;
pub mod modules;
pub mod operation;
pub mod pipeline;
pub mod ratelimit;
pub mod serve;
pub mod shed;
pub mod sse;

pub use app::{AppState, RequestCorrelation, router};
pub use config::{
    ApiKeyRecord, ApiKeyVerifier, Blake3ApiKeyVerifier, ConfigValidationError, DEFAULT_DEADLINE,
    DEFAULT_MAX_DEADLINE, DEFAULT_MAX_JSON_DEPTH, DEFAULT_SSE_CHANNEL_CAPACITY, Profile,
    TransportConfig, VerificationStats,
};
pub use engine::TransportEngine;
pub use gateway::{AdmissionPolicy, AllowAllAdmission};
pub use metrics::{DURATION_BUCKETS_SECS, InFlightGuard, RouteLabel, TransportMetrics};
pub use operation::{NegotiatedMedia, ParsedHttpGraphqlRequest, retry_after_seconds};
pub use pipeline::{
    AssembledPayload, AuthzLayer, AuthzRequest, AuthzService, ExecStageRequest, GatewayLayer,
    GatewayRequest, GatewayService, GraphqlPipelineService, OperationLayer, OperationRequest,
    OperationService, PipelineError, PipelineOutput, PlanLayer, PlanService, PlanStageRequest,
    TransportExecService, TransportMuxLayer, TransportMuxRequest, TransportMuxService,
    build_graphql_pipeline,
};
pub use serve::{ServeOptions, serve};
pub use sse::{SseBodyStream, SseStreamHandle, format_sse_complete, format_sse_next};
