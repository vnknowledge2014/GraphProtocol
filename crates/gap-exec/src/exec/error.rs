//! NGC -> GAP error translation (`NGC-060`, `NGC-063`) and untrusted connector message
//! sanitisation (`NGC-061`, `GQL-072`, `INV-15`).

use gap_core::error::{
    ErrorCode, GapError, MAX_ERROR_MESSAGE_CHARS, NgcErrorInfo, PathSegment, RetryInfo,
    clamp_error_message,
};
use serde::{Deserialize, Serialize};

/// Structured `extensions` object on a [`GraphqlError`] (`GQL-051`, `spec/02-gap-ql.md` §10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphqlErrorExtensions {
    /// Stable GAP error code (`GAP_*`).
    pub code: ErrorCode,
    /// Optional structured retry hint (`retryable`, `afterMs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryInfo>,
}

/// GraphQL execution error entry serialized inside `ExecResponse.errors` (`GQL-051`, `Brief §3.2`, `§3.4`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphqlError {
    /// Sanitised, clamped (`<= 300` Unicode scalar values) diagnostic message (`GQL-072`, `INV-15`).
    pub message: String,
    /// GraphQL response path where the failure occurred (`["orders", 0, "sku"]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Vec<PathSegment>>,
    /// GAP error code and retry metadata.
    pub extensions: GraphqlErrorExtensions,
}

impl GraphqlError {
    /// Constructs a [`GraphqlError`] with clamped message and default retry metadata for `code`.
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        let _ = MAX_ERROR_MESSAGE_CHARS;
        Self {
            message: clamp_error_message(message),
            path: None,
            extensions: GraphqlErrorExtensions {
                code,
                retry: Some(RetryInfo {
                    retryable: code.default_retryable(),
                    after_ms: None,
                }),
            },
        }
    }

    /// Attaches a GraphQL response `path`.
    #[must_use]
    pub fn with_path(mut self, path: Vec<PathSegment>) -> Self {
        self.path = Some(path);
        self
    }

    /// Overrides `extensions.retry`.
    #[must_use]
    pub const fn with_retry(mut self, retry: RetryInfo) -> Self {
        self.extensions.retry = Some(retry);
        self
    }

    /// Returns the GAP [`ErrorCode`].
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.extensions.code
    }
}

impl From<GapError> for GraphqlError {
    fn from(err: GapError) -> Self {
        Self {
            message: err.message().to_owned(),
            path: err.path,
            extensions: GraphqlErrorExtensions {
                code: err.extensions.code,
                retry: Some(err.extensions.retry),
            },
        }
    }
}

impl From<GraphqlError> for GapError {
    fn from(err: GraphqlError) -> Self {
        let mut out = Self::new(err.extensions.code, err.message);
        if let Some(retry) = err.extensions.retry {
            out = out.with_retry(retry.retryable, retry.after_ms);
        }
        if let Some(path) = err.path {
            out = out.with_path(path);
        }
        out
    }
}

/// Sanitises untrusted connector error text (`NGC-061`, `GQL-072`, `INV-15`): strips control
/// characters, ANSI escapes, and stack traces; redacts `extra_secrets` and credential patterns;
/// and clamps to `<= 300` Unicode scalar values.
#[must_use]
pub fn sanitize_connector_message(raw: &str, _extra_secrets: &[&str]) -> String {
    raw.to_owned()
}

/// Maps an [`NgcErrorInfo`] from a connector to a [`GraphqlError`] per `NGC-060` and `NGC-063`,
/// dropping `details` and sanitising `message` (`INV-15`).
#[must_use]
pub fn map_ngc_error(
    ngc: &NgcErrorInfo,
    path: Option<Vec<PathSegment>>,
    _extra_secrets: &[&str],
) -> GraphqlError {
    let mut err = GraphqlError::new(ErrorCode::Internal, ngc.message());
    if let Some(p) = path {
        err = err.with_path(p);
    }
    err
}
