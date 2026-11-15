//! Deterministic execution plane engine (`L3`): `ExecLayer` over `Fetch` / `Parallel` / `Sequence`
//! plans, `ConnectorLayer` v0 with per-branch multiplexed clients and non-queueing bulkheads,
//! deadline propagation (`RUN-030`), cancellation (`RUN-031`, `INV-12`), incremental response-size
//! enforcement (`RUN-060`), GraphQL null/error propagation (`GQL-050`, `GQL-051`), and untrusted
//! connector error sanitisation (`NGC-060`, `NGC-061`, `NGC-063`, `INV-15`).
//!
//! # Pooling & Lock-Free Read Path (`INV-02`)
//!
//! [`ConnectorRegistry`] is an immutable
//! `BTreeMap<BranchUid, RegisteredBranch>` constructed before serving. In `gap-exec` v0,
//! connection pooling means **one multiplexed [`BranchClient`]
//! instance per branch** (e.g. HTTP/2 multiplexed `tonic` [`GrpcClient`](gap_ngc_sdk::grpc::GrpcClient)
//! or [`InProcess`]) guarded by a per-branch
//! `tokio::sync::Semaphore` sized to `BranchDescriptor.limits.max_in_flight` and acquired solely
//! via `try_acquire_owned` (failing fast with `GAP_OVERLOADED`, never queueing). Zero `Mutex` or
//! `RwLock` primitives exist on the request execution path.
//!
//! # `RUN-061` Authorization & Redaction Seam
//!
//! Field-level `@pii` and policy-driven response redaction (`RUN-061`) is isolated behind the
//! [`ResponseRedactor`] trait injected into [`ExecLayer`]
//! (defaulting to [`NoopResponseRedactor`] in `WP-M1-07`).
//!
//! # NGC to GAP Error Mapping (`NGC-060`, `NGC-061`, `NGC-063`, `INV-15`)
//!
//! Every `gap.ngc.v1.ErrorCode` (`NgcErrorCode`) maps deterministically to a stable GAP
//! [`ErrorCode`](gap_core::error::ErrorCode). Connector `details` payloads are dropped, untrusted
//! connector `message` text is scrubbed of control characters, stack traces, and credential
//! patterns (`INV-15`), and clamped to `<= 300` Unicode scalar values (`GQL-072`, `NGC-061`):
//!
//! | `NgcErrorCode` (wire) | GAP `ErrorCode` | Retryable rule (`NGC-063`) |
//! |---|---|---|
//! | `ERROR_CODE_UNSPECIFIED` (`0`) | `GAP_BRANCH_ERROR` | Preserves connector `retryable` flag |
//! | `INVALID_ARGUMENT` (`1`) | `GAP_VALIDATION_ERROR` | Preserves connector `retryable` flag |
//! | `NOT_FOUND` (`2`) | `GAP_BRANCH_ERROR` | Preserves connector `retryable` flag |
//! | `PERMISSION_DENIED` (`3`) | `GAP_FORBIDDEN` | Never retryable (`false`) |
//! | `UNAUTHENTICATED` (`4`) | `GAP_UNAUTHENTICATED` | Preserves connector `retryable` flag |
//! | `RESOURCE_EXHAUSTED` (`5`) | `GAP_RATE_LIMITED` | Always retryable (`true`) |
//! | `FAILED_PRECONDITION` (`6`) | `GAP_BRANCH_ERROR` | Preserves connector `retryable` flag |
//! | `DEADLINE_EXCEEDED` (`7`) | `GAP_BRANCH_TIMEOUT` | Preserves connector `retryable` flag |
//! | `CANCELLED` (`8`) | `GAP_CANCELLED` | Preserves connector `retryable` flag |
//! | `UNAVAILABLE` (`9`) | `GAP_BRANCH_ERROR` | Preserves connector `retryable` flag |
//! | `UNIMPLEMENTED` (`10`) | `GAP_UNSUPPORTED_FEATURE` | Preserves connector `retryable` flag |
//! | `INTERNAL` (`11`) | `GAP_BRANCH_ERROR` | Preserves connector `retryable` flag |
//! | `CONFLICT` (`12`) | `GAP_IDEMPOTENCY_CONFLICT` | Preserves connector `retryable` flag |
//! | `UPSTREAM_ERROR` (`13`) | `GAP_BRANCH_ERROR` | Preserves connector `retryable` flag |
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod bulkhead;
pub mod columnar;
pub mod connector_layer;
pub mod exec;
pub mod join;
pub(crate) mod lockfree;
pub mod mutate;
pub mod saga;
pub mod shadow;
pub mod tasks;
pub mod traverse;

pub use connector_layer::{
    BranchClient, ConnectorRegistry, ConnectorRegistryBuilder, InProcess, RegisteredBranch,
};
pub use exec::{
    CostSummary, DEFAULT_DEADLINE_MARGIN_FLOOR_MS, DEFAULT_DEADLINE_MARGIN_FRACTION,
    DEFAULT_MAX_PARALLEL_FETCHES, DEFAULT_MAX_RESPONSE_BYTES, ExecLayer, ExecLimits, ExecRequest,
    ExecResponse, GapExtensions, GraphqlError, GraphqlErrorExtensions, NoopResponseRedactor,
    ResponseRedactor, map_ngc_error, sanitize_connector_message,
};
