//! Zero-ETL OpenAPI `3.0.x` / `3.1.x` read connector (`L4`, `CMP-030..033`, `NGC-091`).
//!
//! `gap-ngc-openapi` turns a static OpenAPI 3.x JSON document into a `gap_ngc_sdk::Connector`
//! exposing read (`GET`) operations over GAP's Native Graph Connector wire contract without
//! executing generated code or following untrusted `servers[]` or external `$ref` targets.
//!
//! # Path Parameter Safety (`CMP-030`)
//!
//! Path parameter arguments are percent-encoded outside RFC 3986 `unreserved` so `/`, `?`, `#`,
//! `\` and spaces stay confined to their single segment slot. Because WHATWG URL path
//! normalization collapses dot segments even in percent-encoded spellings (`%2e`, `%2E%2E`),
//! path parameter values equal to `"."` or `".."` (as well as empty `""`) are refused with
//! `INVALID_ARGUMENT` naming only the parameter (isolated per-index in `foreach`). Path templates
//! whose literal segments contain dot segments, `\`, `?`, `#`, `%`, spaces, or control characters
//! are skipped during extraction (`unsafe_path_literal`).
//!
//! # Native Batching (`CMP-032`, `NGC-020`, `NGC-026`)
//!
//! Single-keyed reads carrying `x-gap-in-param` batch `foreach` calls through the declared target
//! `GET` endpoint. `x-gap-in-param.key_field` MUST be a unique key (at most one row per key).
//! When the batch target declares an integer `limit` query parameter, effective `max_batch_size`
//! (and `Describe.limits.max_batch_size`) is capped at `limit.maximum` and each batch call sends
//! `limit = <distinct keys in call>` so server-side default page sizes cannot truncate trailing
//! keys into false absences. Non-key per-index variables shared with the batch target are grouped
//! deterministically; unsupported non-key variables fail only their own index with
//! `INVALID_ARGUMENT`.
//!
//! # Upstream Error Mapping (`NGC-060`, `NGC-061`, `CMP-033`)
//!
//! Upstream HTTP statuses and transport outcomes map deterministically to [`gap_core::error::NgcErrorCode`].
//! Upstream response bodies and target URLs are never copied into error messages (`INV-15`).
//!
//! | Upstream condition | NGC outcome / `NgcErrorCode` | `retryable` | Automatic retry (`GET`) | Notes |
//! |---|---|---|---|---|
//! | `200..=299` (`application/json` / `+json`) | Rows emitted | n/a | No | Non-JSON `Content-Type` fails `UPSTREAM_ERROR` unless `allow_binary = true` (`{"raw": "<base64>"}`) |
//! | `404` on a keyed read (has path parameters) | `0` rows (`last_for_index = true`) | n/a | No | Applies identically to solo reads and `foreach` indices |
//! | `404` on an unkeyed read | `UPSTREAM_ERROR` | `false` | No | Fixed diagnostic message |
//! | `400`, `422` | `INVALID_ARGUMENT` | `false` | No | Upstream rejected request inputs |
//! | `401`, `403` | `UPSTREAM_ERROR` | `false` | No | Fixed message (`"upstream rejected connector credentials"`); never echoes body or credential |
//! | `429`, `503` | `UNAVAILABLE` | `true` | Yes (`<= 2` retries) | Honours `Retry-After` (delta-seconds or `1 s` for HTTP-date) when delay fits before call deadline; else fails immediately with `retry_after_ms` |
//! | Other `5xx` (`500`, `502`, `504`, ...) | `UPSTREAM_ERROR` | `true` | No | Caller-visible retryable upstream failure |
//! | `3xx` redirect | `UPSTREAM_ERROR` | `false` | No | Redirects are never followed (`redirect::Policy::none()`) |
//! | Other `4xx` | `UPSTREAM_ERROR` | `false` | No | Non-retryable upstream failure |
//! | Connect failure | `UNAVAILABLE` | `true` | Yes (`<= 2` retries) | Bounded by `CallScope::deadline` |
//! | Over-cap upstream body (`Content-Length` or streaming chunks) | `UPSTREAM_ERROR` | `false` | No | Stream aborted as soon as cap is exceeded |
//! | Call deadline expired (before or mid-call) | `DEADLINE_EXCEEDED` | `false` | No | Upstream is never called if deadline has already passed |
//! | Call cancelled (`CallScope::cancel`) | `CANCELLED` | `false` | No | In-flight upstream future is dropped immediately |
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod auth;
pub mod connector;
pub mod document;
pub mod extract;
pub mod fixture;
pub mod request;
pub mod schema;
pub mod upstream;

pub use auth::{ApiKeyLocation, AuthConfig, AuthError, CredentialSource, StaticCredentialSource};
pub use connector::{ConnectorBuildError, OpenApiConfig, OpenApiConnector};
pub use document::{ExtractError, ExtractLimits, OpenApiDocument};
pub use extract::{Extraction, ExtractionReport, SkippedOperation};
pub use fixture::FixtureService;
pub use upstream::{HttpUpstream, Upstream, UpstreamError, UpstreamRequest, UpstreamResponse};
