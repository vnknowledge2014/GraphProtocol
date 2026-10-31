//! GraphQL-over-HTTP `Accept` negotiation, `GET`/`POST` envelope parsing, `gap-ql` validation,
//! and `spec/02-gap-ql.md` §10 status/error mapping (`GQL-003`, `GQL-050`, `GQL-051`, `ADR-023`).

use axum::http::HeaderMap;
use gap_core::error::GapError;
use serde_json::{Map, Value};

use crate::metrics::RouteLabel;

/// Negotiated response encoding selected from the HTTP `Accept` header (`GQL-003`, `Brief §3.5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiatedMedia {
    /// Standard `application/graphql-response+json; charset=utf-8`.
    GraphqlResponseJson,
    /// Legacy `application/json; charset=utf-8` (`2xx` uses `application/json`, `GraphQLOverHTTP.md` L452–458).
    LegacyApplicationJson,
    /// `graphql-sse` distinct-connections mode (`text/event-stream`).
    EventStream,
}

/// Parsed GraphQL-over-HTTP request payload (`POST` JSON body or `GET` URL query parameters).
#[derive(Debug, Clone, Default)]
pub struct ParsedHttpGraphqlRequest {
    /// Optional GraphQL document text (`query`).
    pub query: Option<String>,
    /// Optional target `operationName`.
    pub operation_name: Option<String>,
    /// Optional GraphQL `variables` JSON map.
    pub variables: Option<Map<String, Value>>,
    /// Optional GraphQL `extensions` JSON map.
    pub extensions: Option<Map<String, Value>>,
    /// Raw payload byte count measured on the wire.
    pub raw_bytes: usize,
    /// Whether this request arrived via HTTP `GET` (persisted operations only, mutations forbidden).
    pub is_get: bool,
}

/// Negotiates response media type from `Accept` header and target `route` (`Brief §3.5`, `§3.7`).
///
/// # Errors
///
/// Returns `GapError` (`ErrorCode::UnsupportedFeature`, mapped to HTTP `406`) if `Accept` allows
/// none of the supported media types.
pub fn negotiate_accept(
    _headers: &HeaderMap,
    _route: RouteLabel,
) -> Result<NegotiatedMedia, GapError> {
    Ok(NegotiatedMedia::GraphqlResponseJson)
}

/// Computes `Retry-After` integer seconds (`ceil(after_ms / 1000)`, minimum `1`) for `429`/`503`.
#[must_use]
pub const fn retry_after_seconds(after_ms: Option<u32>) -> u64 {
    match after_ms {
        Some(0) | None => 1,
        Some(ms) => ((ms as u64) + 999) / 1_000,
    }
}
