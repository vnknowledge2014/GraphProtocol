//! Typed `GapError`, `ErrorCode` (`spec/02-gap-ql.md` §10), and `NgcErrorInfo` conversions (`L0`).
//!
//! Because `gap-core` sits at layer `L0` (`tools/layers.toml`) and cannot depend on `gap-proto`,
//! this module defines [`NgcErrorCode`] and [`NgcErrorInfo`] mirroring `proto/gap/ngc/v1/ngc.proto`
//! (`NgcError`) with `From`/`TryFrom` conversions to and from [`GapError`] (`NGC-060`). Conversion
//! to and from the prost-generated `NgcError` lives in `gap-proto` / `gap-ngc-sdk` (`WP-M1-03`).

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::ids::IdValidationError;

/// Maximum length in Unicode scalar values for any GAP or NGC error message (`GQL-072`, `NGC-061`).
pub const MAX_ERROR_MESSAGE_CHARS: usize = 300;

/// Truncates `message` to at most [`MAX_ERROR_MESSAGE_CHARS`] Unicode scalar values (`GQL-072`).
#[must_use]
pub fn clamp_error_message(message: impl Into<String>) -> String {
    let s = message.into();
    if s.chars().count() <= MAX_ERROR_MESSAGE_CHARS {
        return s;
    }
    let keep = MAX_ERROR_MESSAGE_CHARS.saturating_sub(3);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("...");
    out
}

fn deserialize_clamped_message<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(clamp_error_message(raw))
}

/// Logical error family in `spec/02-gap-ql.md` §10.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ErrorClass {
    /// Client document syntax, validation, or directive errors.
    Request,
    /// Query complexity, depth, size, or budget caps.
    Limits,
    /// Rate limiting and capacity shedding.
    Load,
    /// Request deadline expiration or caller cancellation.
    Time,
    /// Authentication, authorization, approval, or tenant isolation failures.
    Security,
    /// Progressive-disclosure retrieval outcomes (`__search`).
    Discovery,
    /// Downstream branch connectivity, timeout, fault, quarantine, or drift.
    Execution,
    /// Cross-branch join key, cardinality, batchability, or pushdown warnings/errors.
    Joins,
    /// Mutation idempotency, dry-run, or side-effect classification errors.
    Mutation,
    /// Long-running task state and peer-to-peer loop/hop guards.
    #[serde(rename = "tasks/peers")]
    TasksPeers,
    /// Supergraph composition errors.
    Composition,
    /// Unexpected internal gateway fault.
    Internal,
}

/// Stable GAP error code enumeration (`spec/02-gap-ql.md` §10, `ADR-023`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum ErrorCode {
    /// GraphQL syntax error (`GAP_PARSE_ERROR`).
    #[serde(rename = "GAP_PARSE_ERROR")]
    ParseError,
    /// GraphQL validation failure (`GAP_VALIDATION_ERROR`).
    #[serde(rename = "GAP_VALIDATION_ERROR")]
    ValidationError,
    /// Selection references an unknown or invisible field (`GAP_UNKNOWN_FIELD`).
    #[serde(rename = "GAP_UNKNOWN_FIELD")]
    UnknownField,
    /// Field call passes an unknown argument (`GAP_UNKNOWN_ARGUMENT`).
    #[serde(rename = "GAP_UNKNOWN_ARGUMENT")]
    UnknownArgument,
    /// Required argument omitted (`GAP_MISSING_ARGUMENT`).
    #[serde(rename = "GAP_MISSING_ARGUMENT")]
    MissingArgument,
    /// Argument or variable value has incompatible type (`GAP_TYPE_MISMATCH`).
    #[serde(rename = "GAP_TYPE_MISMATCH")]
    TypeMismatch,
    /// Request uses an unsupported protocol or GraphQL feature (`GAP_UNSUPPORTED_FEATURE`).
    #[serde(rename = "GAP_UNSUPPORTED_FEATURE")]
    UnsupportedFeature,
    /// Persisted operation hash not registered (`GAP_PERSISTED_OPERATION_NOT_FOUND`).
    #[serde(rename = "GAP_PERSISTED_OPERATION_NOT_FOUND")]
    PersistedOperationNotFound,
    /// Operation disallowed in current mode, e.g. persisted-only (`GAP_OPERATION_NOT_ALLOWED`).
    #[serde(rename = "GAP_OPERATION_NOT_ALLOWED")]
    OperationNotAllowed,
    /// Non-fatal warning that a composer-managed or optional directive was ignored (`GAP_DIRECTIVE_IGNORED`, `GQL-030`).
    #[serde(rename = "GAP_DIRECTIVE_IGNORED")]
    DirectiveIgnored,
    /// Planned cost exceeds `max_cost_units` (`GAP_COMPLEXITY_EXCEEDED`).
    #[serde(rename = "GAP_COMPLEXITY_EXCEEDED")]
    ComplexityExceeded,
    /// Traversal or query depth exceeds cap (`GAP_DEPTH_EXCEEDED`).
    #[serde(rename = "GAP_DEPTH_EXCEEDED")]
    DepthExceeded,
    /// Serialized response exceeds `max_response_bytes` (`GAP_RESPONSE_TOO_LARGE`).
    #[serde(rename = "GAP_RESPONSE_TOO_LARGE")]
    ResponseTooLarge,
    /// Request cost or call budget exhausted (`GAP_BUDGET_EXCEEDED`).
    #[serde(rename = "GAP_BUDGET_EXCEEDED")]
    BudgetExceeded,
    /// Tenant, principal, or branch rate limit exceeded (`GAP_RATE_LIMITED`).
    #[serde(rename = "GAP_RATE_LIMITED")]
    RateLimited,
    /// Gateway or pool shedding load (`GAP_OVERLOADED`).
    #[serde(rename = "GAP_OVERLOADED")]
    Overloaded,
    /// Overall request deadline elapsed (`GAP_DEADLINE_EXCEEDED`).
    #[serde(rename = "GAP_DEADLINE_EXCEEDED")]
    DeadlineExceeded,
    /// Request cancelled by caller (`GAP_CANCELLED`).
    #[serde(rename = "GAP_CANCELLED")]
    Cancelled,
    /// Missing or invalid caller credentials (`GAP_UNAUTHENTICATED`).
    #[serde(rename = "GAP_UNAUTHENTICATED")]
    Unauthenticated,
    /// Caller lacks permission; reason redacted per policy (`GAP_FORBIDDEN`).
    #[serde(rename = "GAP_FORBIDDEN")]
    Forbidden,
    /// Operation requires human/policy approval before execution (`GAP_APPROVAL_REQUIRED`).
    #[serde(rename = "GAP_APPROVAL_REQUIRED")]
    ApprovalRequired,
    /// Downstream resource requires interactive or delegated auth (`GAP_AUTH_REQUIRED`).
    #[serde(rename = "GAP_AUTH_REQUIRED")]
    AuthRequired,
    /// Cross-tenant access or mismatched tenant binding (`GAP_TENANT_MISMATCH`).
    #[serde(rename = "GAP_TENANT_MISMATCH")]
    TenantMismatch,
    /// Calibrated discovery abstained due to low confidence (`GAP_INSUFFICIENT_EVIDENCE`).
    #[serde(rename = "GAP_INSUFFICIENT_EVIDENCE")]
    InsufficientEvidence,
    /// Multiple candidate branches/operations remain ambiguous (`GAP_AMBIGUOUS`).
    #[serde(rename = "GAP_AMBIGUOUS")]
    Ambiguous,
    /// Warning that S1 was unavailable and baseline retrieval was used (`GAP_DISCOVERY_DEGRADED`).
    #[serde(rename = "GAP_DISCOVERY_DEGRADED")]
    DiscoveryDegraded,
    /// Target branch is unreachable or circuit-open (`GAP_BRANCH_UNAVAILABLE`).
    #[serde(rename = "GAP_BRANCH_UNAVAILABLE")]
    BranchUnavailable,
    /// Downstream branch exceeded its per-call deadline (`GAP_BRANCH_TIMEOUT`).
    #[serde(rename = "GAP_BRANCH_TIMEOUT")]
    BranchTimeout,
    /// Downstream branch returned an upstream or execution fault (`GAP_BRANCH_ERROR`).
    #[serde(rename = "GAP_BRANCH_ERROR")]
    BranchError,
    /// Target branch is quarantined (`GAP_BRANCH_QUARANTINED`).
    #[serde(rename = "GAP_BRANCH_QUARANTINED")]
    BranchQuarantined,
    /// Branch schema/descriptor drifted from pinned snapshot (`GAP_BRANCH_DRIFT`).
    #[serde(rename = "GAP_BRANCH_DRIFT")]
    BranchDrift,
    /// Required join key absent from parent row (`GAP_JOIN_KEY_MISSING`).
    #[serde(rename = "GAP_JOIN_KEY_MISSING")]
    JoinKeyMissing,
    /// Join expected unique target key but found duplicates (`GAP_JOIN_KEY_NOT_UNIQUE`).
    #[serde(rename = "GAP_JOIN_KEY_NOT_UNIQUE")]
    JoinKeyNotUnique,
    /// Join fan-out or key count exceeded limit (`GAP_JOIN_LIMIT_EXCEEDED`).
    #[serde(rename = "GAP_JOIN_LIMIT_EXCEEDED")]
    JoinLimitExceeded,
    /// Join target lacks batching and `allow_per_key_fallback` is false (`GAP_JOIN_NOT_BATCHABLE`).
    #[serde(rename = "GAP_JOIN_NOT_BATCHABLE")]
    JoinNotBatchable,
    /// Warning that predicate pushdown fell back to gateway filter (`GAP_PUSHDOWN_FALLBACK`).
    #[serde(rename = "GAP_PUSHDOWN_FALLBACK")]
    PushdownFallback,
    /// Warning that part of a response or stream is partial (`GAP_PARTIAL_RESULT`).
    #[serde(rename = "GAP_PARTIAL_RESULT")]
    PartialResult,
    /// Idempotency key reused with different arguments/operation (`GAP_IDEMPOTENCY_CONFLICT`).
    #[serde(rename = "GAP_IDEMPOTENCY_CONFLICT")]
    IdempotencyConflict,
    /// Mutation target does not support `dryRun` (`GAP_DRY_RUN_UNSUPPORTED`).
    #[serde(rename = "GAP_DRY_RUN_UNSUPPORTED")]
    DryRunUnsupported,
    /// Mutation lacks verified side-effect classification (`GAP_SIDE_EFFECT_UNCLASSIFIED`).
    #[serde(rename = "GAP_SIDE_EFFECT_UNCLASSIFIED")]
    SideEffectUnclassified,
    /// Referenced long-running task ID was not found (`GAP_TASK_NOT_FOUND`).
    #[serde(rename = "GAP_TASK_NOT_FOUND")]
    TaskNotFound,
    /// Task transition invalid from its current state (`GAP_TASK_INVALID_STATE`).
    #[serde(rename = "GAP_TASK_INVALID_STATE")]
    TaskInvalidState,
    /// Peer-to-peer trace path contains a cycle (`GAP_LOOP_DETECTED`).
    #[serde(rename = "GAP_LOOP_DETECTED")]
    LoopDetected,
    /// Peer-to-peer `hop_count` exceeded `max_hops` (`GAP_HOP_LIMIT`).
    #[serde(rename = "GAP_HOP_LIMIT")]
    HopLimit,
    /// Composition rejected an unbounded join cycle (`GAP_CYCLE_UNBOUNDED`).
    #[serde(rename = "GAP_CYCLE_UNBOUNDED")]
    CycleUnbounded,
    /// Branch attempted to register a GAP reserved name (`GAP_RESERVED_NAME`).
    #[serde(rename = "GAP_RESERVED_NAME")]
    ReservedName,
    /// Two branches claimed the same root field or produced a colliding type name (`GAP_AMBIGUOUS_FIELD`, `RUN-009`, `RUN-011`).
    #[serde(rename = "GAP_AMBIGUOUS_FIELD")]
    AmbiguousField,
    /// Unexpected internal error; details logged under `correlationId` (`GAP_INTERNAL`).
    #[serde(rename = "GAP_INTERNAL")]
    Internal,
}

impl ErrorCode {
    /// Exhaustive list of all 48 [`ErrorCode`] variants in `spec/02-gap-ql.md` §10.
    pub const ALL: &'static [Self] = &[
        Self::ParseError,
        Self::ValidationError,
        Self::UnknownField,
        Self::UnknownArgument,
        Self::MissingArgument,
        Self::TypeMismatch,
        Self::UnsupportedFeature,
        Self::PersistedOperationNotFound,
        Self::OperationNotAllowed,
        Self::DirectiveIgnored,
        Self::ComplexityExceeded,
        Self::DepthExceeded,
        Self::ResponseTooLarge,
        Self::BudgetExceeded,
        Self::RateLimited,
        Self::Overloaded,
        Self::DeadlineExceeded,
        Self::Cancelled,
        Self::Unauthenticated,
        Self::Forbidden,
        Self::ApprovalRequired,
        Self::AuthRequired,
        Self::TenantMismatch,
        Self::InsufficientEvidence,
        Self::Ambiguous,
        Self::DiscoveryDegraded,
        Self::BranchUnavailable,
        Self::BranchTimeout,
        Self::BranchError,
        Self::BranchQuarantined,
        Self::BranchDrift,
        Self::JoinKeyMissing,
        Self::JoinKeyNotUnique,
        Self::JoinLimitExceeded,
        Self::JoinNotBatchable,
        Self::PushdownFallback,
        Self::PartialResult,
        Self::IdempotencyConflict,
        Self::DryRunUnsupported,
        Self::SideEffectUnclassified,
        Self::TaskNotFound,
        Self::TaskInvalidState,
        Self::LoopDetected,
        Self::HopLimit,
        Self::CycleUnbounded,
        Self::ReservedName,
        Self::AmbiguousField,
        Self::Internal,
    ];

    /// Canonical wire string serialized in `extensions.code` (`spec/02-gap-ql.md` §10).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ParseError => "GAP_PARSE_ERROR",
            Self::ValidationError => "GAP_VALIDATION_ERROR",
            Self::UnknownField => "GAP_UNKNOWN_FIELD",
            Self::UnknownArgument => "GAP_UNKNOWN_ARGUMENT",
            Self::MissingArgument => "GAP_MISSING_ARGUMENT",
            Self::TypeMismatch => "GAP_TYPE_MISMATCH",
            Self::UnsupportedFeature => "GAP_UNSUPPORTED_FEATURE",
            Self::PersistedOperationNotFound => "GAP_PERSISTED_OPERATION_NOT_FOUND",
            Self::OperationNotAllowed => "GAP_OPERATION_NOT_ALLOWED",
            Self::DirectiveIgnored => "GAP_DIRECTIVE_IGNORED",
            Self::ComplexityExceeded => "GAP_COMPLEXITY_EXCEEDED",
            Self::DepthExceeded => "GAP_DEPTH_EXCEEDED",
            Self::ResponseTooLarge => "GAP_RESPONSE_TOO_LARGE",
            Self::BudgetExceeded => "GAP_BUDGET_EXCEEDED",
            Self::RateLimited => "GAP_RATE_LIMITED",
            Self::Overloaded => "GAP_OVERLOADED",
            Self::DeadlineExceeded => "GAP_DEADLINE_EXCEEDED",
            Self::Cancelled => "GAP_CANCELLED",
            Self::Unauthenticated => "GAP_UNAUTHENTICATED",
            Self::Forbidden => "GAP_FORBIDDEN",
            Self::ApprovalRequired => "GAP_APPROVAL_REQUIRED",
            Self::AuthRequired => "GAP_AUTH_REQUIRED",
            Self::TenantMismatch => "GAP_TENANT_MISMATCH",
            Self::InsufficientEvidence => "GAP_INSUFFICIENT_EVIDENCE",
            Self::Ambiguous => "GAP_AMBIGUOUS",
            Self::DiscoveryDegraded => "GAP_DISCOVERY_DEGRADED",
            Self::BranchUnavailable => "GAP_BRANCH_UNAVAILABLE",
            Self::BranchTimeout => "GAP_BRANCH_TIMEOUT",
            Self::BranchError => "GAP_BRANCH_ERROR",
            Self::BranchQuarantined => "GAP_BRANCH_QUARANTINED",
            Self::BranchDrift => "GAP_BRANCH_DRIFT",
            Self::JoinKeyMissing => "GAP_JOIN_KEY_MISSING",
            Self::JoinKeyNotUnique => "GAP_JOIN_KEY_NOT_UNIQUE",
            Self::JoinLimitExceeded => "GAP_JOIN_LIMIT_EXCEEDED",
            Self::JoinNotBatchable => "GAP_JOIN_NOT_BATCHABLE",
            Self::PushdownFallback => "GAP_PUSHDOWN_FALLBACK",
            Self::PartialResult => "GAP_PARTIAL_RESULT",
            Self::IdempotencyConflict => "GAP_IDEMPOTENCY_CONFLICT",
            Self::DryRunUnsupported => "GAP_DRY_RUN_UNSUPPORTED",
            Self::SideEffectUnclassified => "GAP_SIDE_EFFECT_UNCLASSIFIED",
            Self::TaskNotFound => "GAP_TASK_NOT_FOUND",
            Self::TaskInvalidState => "GAP_TASK_INVALID_STATE",
            Self::LoopDetected => "GAP_LOOP_DETECTED",
            Self::HopLimit => "GAP_HOP_LIMIT",
            Self::CycleUnbounded => "GAP_CYCLE_UNBOUNDED",
            Self::ReservedName => "GAP_RESERVED_NAME",
            Self::AmbiguousField => "GAP_AMBIGUOUS_FIELD",
            Self::Internal => "GAP_INTERNAL",
        }
    }

    /// Logical class from the `spec/02-gap-ql.md` §10 table.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::ParseError
            | Self::ValidationError
            | Self::UnknownField
            | Self::UnknownArgument
            | Self::MissingArgument
            | Self::TypeMismatch
            | Self::UnsupportedFeature
            | Self::PersistedOperationNotFound
            | Self::OperationNotAllowed => ErrorClass::Request,
            Self::ComplexityExceeded
            | Self::DepthExceeded
            | Self::ResponseTooLarge
            | Self::BudgetExceeded => ErrorClass::Limits,
            Self::RateLimited | Self::Overloaded => ErrorClass::Load,
            Self::DeadlineExceeded | Self::Cancelled => ErrorClass::Time,
            Self::Unauthenticated
            | Self::Forbidden
            | Self::ApprovalRequired
            | Self::AuthRequired
            | Self::TenantMismatch => ErrorClass::Security,
            Self::InsufficientEvidence | Self::Ambiguous | Self::DiscoveryDegraded => {
                ErrorClass::Discovery
            }
            Self::BranchUnavailable
            | Self::BranchTimeout
            | Self::BranchError
            | Self::BranchQuarantined
            | Self::BranchDrift => ErrorClass::Execution,
            Self::JoinKeyMissing
            | Self::JoinKeyNotUnique
            | Self::JoinLimitExceeded
            | Self::JoinNotBatchable
            | Self::PushdownFallback
            | Self::PartialResult => ErrorClass::Joins,
            Self::IdempotencyConflict | Self::DryRunUnsupported | Self::SideEffectUnclassified => {
                ErrorClass::Mutation
            }
            Self::TaskNotFound | Self::TaskInvalidState | Self::LoopDetected | Self::HopLimit => {
                ErrorClass::TasksPeers
            }
            Self::CycleUnbounded
            | Self::ReservedName
            | Self::AmbiguousField
            | Self::DirectiveIgnored => ErrorClass::Composition,
            Self::Internal => ErrorClass::Internal,
        }
    }

    /// Default retryability when no cause-specific flag overrides it (`spec/02-gap-ql.md` §10).
    #[must_use]
    pub const fn default_retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::Overloaded)
    }

    /// Transport HTTP status when a request-level failure prevents producing a GraphQL response
    /// document (`ADR-023`, `spec/02-gap-ql.md` §10). Operation-level errors always use HTTP 200.
    #[must_use]
    pub const fn request_level_http_status(self) -> u16 {
        match self {
            Self::Unauthenticated => 401,
            Self::Forbidden | Self::TenantMismatch => 403,
            Self::RateLimited => 429,
            Self::Overloaded => 503,
            Self::DeadlineExceeded => 504,
            Self::Internal => 500,
            Self::ParseError
            | Self::ValidationError
            | Self::UnknownField
            | Self::UnknownArgument
            | Self::MissingArgument
            | Self::TypeMismatch
            | Self::UnsupportedFeature
            | Self::PersistedOperationNotFound
            | Self::OperationNotAllowed => 400,
            _ => 200,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown `GAP_*` string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown GAP error code `{0}`")]
pub struct UnknownErrorCode(pub String);

impl FromStr for ErrorCode {
    type Err = UnknownErrorCode;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for candidate in Self::ALL {
            if candidate.as_str() == s {
                return Ok(*candidate);
            }
        }
        Err(UnknownErrorCode(s.to_owned()))
    }
}

/// Structured retry guidance attached at `extensions.retry` (`spec/02-gap-ql.md` §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetryInfo {
    /// Whether the caller may safely retry the failed request or operation.
    pub retryable: bool,
    /// Optional backoff hint in milliseconds (`afterMs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_ms: Option<u32>,
}

impl RetryInfo {
    /// Non-retryable guidance (`retryable: false`).
    #[must_use]
    pub const fn no() -> Self {
        Self {
            retryable: false,
            after_ms: None,
        }
    }

    /// Retryable guidance with optional `after_ms` backoff.
    #[must_use]
    pub const fn yes(after_ms: Option<u32>) -> Self {
        Self {
            retryable: true,
            after_ms,
        }
    }
}

/// Recommended caller action on `GAP_INSUFFICIENT_EVIDENCE` (`GQL-071`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NextStep {
    /// Propose registering a new source connector (`propose_source`).
    ProposeSource,
    /// Rephrase the discovery intent (`rephrase`).
    Rephrase,
    /// Escalate to the end user for clarification (`ask_user`).
    AskUser,
}

/// Self-repair hints for LLM/agent callers serialized at `extensions.suggestion` (`GQL-070`, `GQL-071`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Suggestion {
    /// Visible symbol candidates within edit distance <= 2 or top `__search` hits (`GQL-070`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub did_you_mean: Option<Vec<String>>,
    /// Minimal valid selection snippet for the parent type (`GQL-070`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Required argument names missing from the field call (`GQL-070`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_arguments: Option<Vec<String>>,
    /// Recommended next action when discovery abstains (`GQL-071`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<NextStep>,
    /// Positive query examples (`GQL-071`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub examples: Option<Vec<String>>,
    /// Suggested alternative field or operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alternative: Option<String>,
}

/// One element of a GraphQL response `path` (`spec/02-gap-ql.md` §10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PathSegment {
    /// Object field name.
    Field(String),
    /// Zero-based list index.
    Index(usize),
}

/// 1-based line and column in a GraphQL query document (`spec/02-gap-ql.md` §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column number.
    pub column: u32,
}

/// `extensions` payload inside a serialized [`GapError`] (`spec/02-gap-ql.md` §10, `ADR-023`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ErrorExtensions {
    /// Stable GAP error code (`GAP_*`).
    pub code: ErrorCode,
    /// Structured retry metadata.
    pub retry: RetryInfo,
    /// Optional LLM self-repair suggestion (`GQL-070`, `GQL-071`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<Box<Suggestion>>,
    /// Request correlation identifier (`correlationId`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

/// Canonical GAP error value (`spec/02-gap-ql.md` §10, `GQL-070`..`GQL-072`, `ADR-023`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Error)]
#[serde(deny_unknown_fields)]
#[error("{}: {message}", self.extensions.code)]
pub struct GapError {
    #[serde(deserialize_with = "deserialize_clamped_message")]
    message: String,
    /// Optional GraphQL document locations associated with the error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<SourceLocation>>,
    /// Optional GraphQL response path where the error occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<Vec<PathSegment>>,
    /// Structured GAP error extensions (boxed to keep `GapResult<T>` stack-compact).
    pub extensions: Box<ErrorExtensions>,
}

/// Convenience alias for `Result<T, GapError>`.
pub type GapResult<T> = Result<T, GapError>;

impl GapError {
    /// Constructs a [`GapError`] whose message is clamped to [`MAX_ERROR_MESSAGE_CHARS`] (`GQL-072`)
    /// and whose `retryable` flag defaults to [`ErrorCode::default_retryable`].
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            message: clamp_error_message(message),
            locations: None,
            path: None,
            extensions: Box::new(ErrorExtensions {
                code,
                retry: RetryInfo {
                    retryable: code.default_retryable(),
                    after_ms: None,
                },
                suggestion: None,
                correlation_id: None,
            }),
        }
    }

    /// Constructs an opaque `GAP_INTERNAL` error carrying `correlation_id` (`spec/02-gap-ql.md` §10).
    #[must_use]
    pub fn internal(correlation_id: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, "internal error").with_correlation_id(correlation_id)
    }

    /// Clamped human/agent-readable error message (`<= 300` Unicode scalar values).
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Stable [`ErrorCode`] in `extensions.code`.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        self.extensions.code
    }

    /// Retry guidance in `extensions.retry`.
    #[must_use]
    pub const fn retry(&self) -> RetryInfo {
        self.extensions.retry
    }

    /// Optional self-repair suggestion in `extensions.suggestion`.
    #[must_use]
    pub fn suggestion(&self) -> Option<&Suggestion> {
        self.extensions.suggestion.as_deref()
    }

    /// Optional request correlation identifier in `extensions.correlationId`.
    #[must_use]
    pub fn correlation_id(&self) -> Option<&str> {
        self.extensions.correlation_id.as_deref()
    }

    /// Overrides `extensions.retry`.
    #[must_use]
    pub const fn with_retry(mut self, retryable: bool, after_ms: Option<u32>) -> Self {
        self.extensions.retry = RetryInfo {
            retryable,
            after_ms,
        };
        self
    }

    /// Attaches `extensions.suggestion`.
    #[must_use]
    pub fn with_suggestion(mut self, suggestion: Suggestion) -> Self {
        self.extensions.suggestion = Some(Box::new(suggestion));
        self
    }

    /// Attaches `extensions.correlationId`.
    #[must_use]
    pub fn with_correlation_id(mut self, correlation_id: impl Into<String>) -> Self {
        self.extensions.correlation_id = Some(correlation_id.into());
        self
    }

    /// Attaches GraphQL response `path`.
    #[must_use]
    pub fn with_path(mut self, path: Vec<PathSegment>) -> Self {
        self.path = Some(path);
        self
    }

    /// Attaches GraphQL source `locations`.
    #[must_use]
    pub fn with_locations(mut self, locations: Vec<SourceLocation>) -> Self {
        self.locations = Some(locations);
        self
    }
}

impl From<IdValidationError> for GapError {
    fn from(err: IdValidationError) -> Self {
        let code = match &err {
            IdValidationError::InvalidOperationId { reason, .. }
                if reason.contains("reserved") || reason.contains("__") =>
            {
                ErrorCode::ReservedName
            }
            _ => ErrorCode::ValidationError,
        };
        Self::new(code, err.to_string())
    }
}

/// Wire error codes mirroring `gap.ngc.v1.ErrorCode` in `proto/gap/ngc/v1/ngc.proto`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[repr(i32)]
pub enum NgcErrorCode {
    /// `ERROR_CODE_UNSPECIFIED = 0`.
    #[serde(rename = "ERROR_CODE_UNSPECIFIED")]
    Unspecified = 0,
    /// `INVALID_ARGUMENT = 1`.
    InvalidArgument = 1,
    /// `NOT_FOUND = 2`.
    NotFound = 2,
    /// `PERMISSION_DENIED = 3`.
    PermissionDenied = 3,
    /// `UNAUTHENTICATED = 4`.
    Unauthenticated = 4,
    /// `RESOURCE_EXHAUSTED = 5`.
    ResourceExhausted = 5,
    /// `FAILED_PRECONDITION = 6`.
    FailedPrecondition = 6,
    /// `DEADLINE_EXCEEDED = 7`.
    DeadlineExceeded = 7,
    /// `CANCELLED = 8`.
    Cancelled = 8,
    /// `UNAVAILABLE = 9`.
    Unavailable = 9,
    /// `UNIMPLEMENTED = 10`.
    Unimplemented = 10,
    /// `INTERNAL = 11`.
    Internal = 11,
    /// `CONFLICT = 12`.
    Conflict = 12,
    /// `UPSTREAM_ERROR = 13`.
    UpstreamError = 13,
}

impl From<NgcErrorCode> for i32 {
    fn from(code: NgcErrorCode) -> Self {
        code as Self
    }
}

/// Error returned when decoding an unrecognized `gap.ngc.v1.ErrorCode` integer tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("unknown NGC error code tag `{0}`")]
pub struct UnknownNgcErrorCode(pub i32);

impl TryFrom<i32> for NgcErrorCode {
    type Error = UnknownNgcErrorCode;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unspecified),
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::NotFound),
            3 => Ok(Self::PermissionDenied),
            4 => Ok(Self::Unauthenticated),
            5 => Ok(Self::ResourceExhausted),
            6 => Ok(Self::FailedPrecondition),
            7 => Ok(Self::DeadlineExceeded),
            8 => Ok(Self::Cancelled),
            9 => Ok(Self::Unavailable),
            10 => Ok(Self::Unimplemented),
            11 => Ok(Self::Internal),
            12 => Ok(Self::Conflict),
            13 => Ok(Self::UpstreamError),
            other => Err(UnknownNgcErrorCode(other)),
        }
    }
}

/// Layer-`L0` representation of `gap.ngc.v1.NgcError` (`proto/gap/ngc/v1/ngc.proto`).
///
/// Mirrors fields `1..=5` (`code`, `message`, `retryable`, `retry_after_ms`, `details`) without
/// pulling `prost` into `gap-core`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NgcErrorInfo {
    /// Connector error classification (`field 1`).
    pub code: NgcErrorCode,
    /// Sanitized connector message, clamped to `<= 300` chars (`field 2`, `NGC-061`, `INV-15`).
    #[serde(deserialize_with = "deserialize_clamped_message")]
    message: String,
    /// Whether the connector marks the failure retryable (`field 3`).
    pub retryable: bool,
    /// Connector-suggested retry delay in milliseconds (`field 4`; `0` means unspecified).
    pub retry_after_ms: u32,
    /// Optional structured metadata mirroring `google.protobuf.Struct` (`field 5`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Map<String, serde_json::Value>>,
}

impl NgcErrorInfo {
    /// Constructs a new [`NgcErrorInfo`] with its message clamped to [`MAX_ERROR_MESSAGE_CHARS`].
    #[must_use]
    pub fn new(code: NgcErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: clamp_error_message(message),
            retryable: false,
            retry_after_ms: 0,
            details: None,
        }
    }

    /// Fallibly constructs an [`NgcErrorInfo`] from raw wire fields (e.g. a `prost` `i32` enum tag).
    ///
    /// # Errors
    ///
    /// Returns [`UnknownNgcErrorCode`] if `code` is outside `0..=13`.
    pub fn try_from_wire(
        code: i32,
        message: impl Into<String>,
        retryable: bool,
        retry_after_ms: u32,
        details: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<Self, UnknownNgcErrorCode> {
        Ok(Self {
            code: NgcErrorCode::try_from(code)?,
            message: clamp_error_message(message),
            retryable,
            retry_after_ms,
            details,
        })
    }

    /// Clamped connector error message (`<= 300` Unicode scalar values).
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Sets `retryable` and `retry_after_ms`.
    #[must_use]
    pub const fn with_retry(mut self, retryable: bool, retry_after_ms: u32) -> Self {
        self.retryable = retryable;
        self.retry_after_ms = retry_after_ms;
        self
    }

    /// Attaches structured `details`.
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Map<String, serde_json::Value>) -> Self {
        self.details = Some(details);
        self
    }

    /// Translates an NGC `Mutate(dry_run = true)` error into a [`GapError`], mapping
    /// `UNIMPLEMENTED` to `GAP_DRY_RUN_UNSUPPORTED` (`NGC-031`).
    #[must_use]
    pub fn into_dry_run_gap_error(self) -> GapError {
        if self.code == NgcErrorCode::Unimplemented {
            return GapError::new(ErrorCode::DryRunUnsupported, self.message)
                .with_retry(false, None);
        }
        GapError::from(self)
    }
}

impl From<NgcErrorInfo> for GapError {
    fn from(ngc: NgcErrorInfo) -> Self {
        let after_ms = (ngc.retry_after_ms > 0).then_some(ngc.retry_after_ms);
        let (code, retryable) = match ngc.code {
            // NGC-060 normative mappings:
            NgcErrorCode::Unavailable | NgcErrorCode::UpstreamError => {
                (ErrorCode::BranchError, ngc.retryable)
            }
            NgcErrorCode::DeadlineExceeded => (ErrorCode::BranchTimeout, ngc.retryable),
            NgcErrorCode::PermissionDenied => (ErrorCode::Forbidden, false),
            NgcErrorCode::ResourceExhausted => (ErrorCode::RateLimited, true),
            // Remaining NGC codes (recorded under Deviations as UNSPECIFIED defaults):
            NgcErrorCode::Unauthenticated => (ErrorCode::Unauthenticated, false),
            NgcErrorCode::InvalidArgument => (ErrorCode::ValidationError, false),
            NgcErrorCode::Unimplemented => (ErrorCode::UnsupportedFeature, false),
            NgcErrorCode::Cancelled => (ErrorCode::Cancelled, ngc.retryable),
            NgcErrorCode::Conflict => (ErrorCode::IdempotencyConflict, false),
            NgcErrorCode::NotFound
            | NgcErrorCode::FailedPrecondition
            | NgcErrorCode::Internal
            | NgcErrorCode::Unspecified => (ErrorCode::BranchError, ngc.retryable),
        };
        Self::new(code, ngc.message).with_retry(retryable, retryable.then_some(after_ms).flatten())
    }
}

impl From<GapError> for NgcErrorInfo {
    fn from(err: GapError) -> Self {
        let code = match err.extensions.code {
            ErrorCode::ParseError
            | ErrorCode::ValidationError
            | ErrorCode::UnknownField
            | ErrorCode::UnknownArgument
            | ErrorCode::MissingArgument
            | ErrorCode::TypeMismatch
            | ErrorCode::ComplexityExceeded
            | ErrorCode::DepthExceeded
            | ErrorCode::CycleUnbounded
            | ErrorCode::ReservedName
            | ErrorCode::AmbiguousField
            | ErrorCode::DirectiveIgnored => NgcErrorCode::InvalidArgument,
            ErrorCode::PersistedOperationNotFound | ErrorCode::TaskNotFound => {
                NgcErrorCode::NotFound
            }
            ErrorCode::Forbidden
            | ErrorCode::OperationNotAllowed
            | ErrorCode::TenantMismatch
            | ErrorCode::ApprovalRequired => NgcErrorCode::PermissionDenied,
            ErrorCode::Unauthenticated | ErrorCode::AuthRequired => NgcErrorCode::Unauthenticated,
            ErrorCode::RateLimited
            | ErrorCode::Overloaded
            | ErrorCode::BudgetExceeded
            | ErrorCode::ResponseTooLarge
            | ErrorCode::JoinLimitExceeded => NgcErrorCode::ResourceExhausted,
            ErrorCode::TaskInvalidState
            | ErrorCode::InsufficientEvidence
            | ErrorCode::Ambiguous
            | ErrorCode::JoinKeyMissing
            | ErrorCode::JoinKeyNotUnique
            | ErrorCode::JoinNotBatchable
            | ErrorCode::SideEffectUnclassified
            | ErrorCode::LoopDetected
            | ErrorCode::HopLimit
            | ErrorCode::BranchDrift
            | ErrorCode::BranchQuarantined => NgcErrorCode::FailedPrecondition,
            ErrorCode::DeadlineExceeded | ErrorCode::BranchTimeout => {
                NgcErrorCode::DeadlineExceeded
            }
            ErrorCode::Cancelled => NgcErrorCode::Cancelled,
            ErrorCode::BranchUnavailable => NgcErrorCode::Unavailable,
            ErrorCode::UnsupportedFeature | ErrorCode::DryRunUnsupported => {
                NgcErrorCode::Unimplemented
            }
            ErrorCode::IdempotencyConflict => NgcErrorCode::Conflict,
            ErrorCode::BranchError => NgcErrorCode::UpstreamError,
            ErrorCode::Internal
            | ErrorCode::DiscoveryDegraded
            | ErrorCode::PushdownFallback
            | ErrorCode::PartialResult => NgcErrorCode::Internal,
        };
        let retryable = err.extensions.retry.retryable;
        let retry_after_ms = err.extensions.retry.after_ms.unwrap_or(0);
        Self {
            code,
            message: err.message,
            retryable,
            retry_after_ms,
            details: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use super::*;

    fn extract_section_10_codes_from_spec() -> BTreeSet<String> {
        let spec_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/02-gap-ql.md");
        let text = fs::read_to_string(&spec_path).expect("read spec/02-gap-ql.md");
        let mut in_sec_10 = false;
        let mut codes = BTreeSet::new();
        for line in text.lines() {
            if line.starts_with("## 10.") {
                in_sec_10 = true;
                continue;
            }
            if in_sec_10 && line.starts_with("## ") {
                break;
            }
            if in_sec_10 && line.trim_start().starts_with('|') {
                let first_col = line.split('|').nth(1).unwrap_or("");
                for part in first_col.split('`') {
                    if part.starts_with("GAP_")
                        && part
                            .bytes()
                            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                    {
                        codes.insert(part.to_owned());
                    }
                }
            }
        }
        codes
    }

    // spec: GQL-070, GQL-071, GQL-072 (ADR-023)
    #[test]
    fn error_code_enum_matches_spec_02_section_10_exactly_and_serializes_to_extensions_code() {
        let spec_codes = extract_section_10_codes_from_spec();
        assert_eq!(spec_codes.len(), 48);
        assert_eq!(ErrorCode::ALL.len(), 48);

        let enum_codes: BTreeSet<String> = ErrorCode::ALL
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect();
        assert_eq!(enum_codes, spec_codes);

        for &variant in ErrorCode::ALL {
            let expected = variant.as_str();
            assert_eq!(variant.to_string(), expected);
            assert_eq!(ErrorCode::from_str(expected).expect("parse code"), variant);

            let err = GapError::new(variant, "test failure").with_correlation_id("req-123");
            let json = serde_json::to_value(&err).expect("serialize GapError");
            assert_eq!(json["extensions"]["code"], serde_json::json!(expected));

            let decoded: GapError = serde_json::from_value(json).expect("deserialize GapError");
            assert_eq!(decoded, err);
        }

        assert!(ErrorCode::from_str("GAP_DOES_NOT_EXIST").is_err());
    }

    // spec: RUN-009, RUN-011, GQL-030 (ADR-023)
    #[test]
    fn composition_error_codes_ambiguous_field_and_directive_ignored_honor_spec_02() {
        assert_eq!(ErrorCode::AmbiguousField.as_str(), "GAP_AMBIGUOUS_FIELD");
        assert_eq!(ErrorCode::AmbiguousField.class(), ErrorClass::Composition);
        assert!(!ErrorCode::AmbiguousField.default_retryable());
        assert_eq!(ErrorCode::AmbiguousField.request_level_http_status(), 200);
        let ngc_amb = NgcErrorInfo::from(GapError::new(ErrorCode::AmbiguousField, "dup field"));
        assert_eq!(ngc_amb.code, NgcErrorCode::InvalidArgument);

        assert_eq!(
            ErrorCode::DirectiveIgnored.as_str(),
            "GAP_DIRECTIVE_IGNORED"
        );
        assert_eq!(ErrorCode::DirectiveIgnored.class(), ErrorClass::Composition);
        assert!(!ErrorCode::DirectiveIgnored.default_retryable());
        let ngc_dir = NgcErrorInfo::from(GapError::new(
            ErrorCode::DirectiveIgnored,
            "ignored @branch",
        ));
        assert_eq!(ngc_dir.code, NgcErrorCode::InvalidArgument);
    }

    // spec: GQL-070, GQL-071, GQL-072, INV-15 (ADR-023)
    #[test]
    fn gap_error_clamps_messages_to_300_chars_and_serializes_suggestions_and_retry() {
        let oversized = "x".repeat(450);
        let err = GapError::new(ErrorCode::UnknownField, oversized)
            .with_path(vec![
                PathSegment::Field("billing".to_owned()),
                PathSegment::Index(0),
                PathSegment::Field("invoce".to_owned()),
            ])
            .with_locations(vec![SourceLocation {
                line: 1,
                column: 12,
            }])
            .with_suggestion(Suggestion {
                did_you_mean: Some(vec!["invoice".to_owned()]),
                template: Some("invoice(id: \"...\") { total }".to_owned()),
                missing_arguments: None,
                next_step: None,
                examples: None,
                alternative: None,
            })
            .with_correlation_id("corr-42");

        assert_eq!(err.message().chars().count(), MAX_ERROR_MESSAGE_CHARS);
        assert!(err.message().ends_with("..."));
        assert_eq!(err.code().class(), ErrorClass::Request);
        assert!(!err.retry().retryable);
        assert_eq!(err.code().request_level_http_status(), 400);

        let val = serde_json::to_value(&err).expect("serialize");
        assert_eq!(val["extensions"]["code"], "GAP_UNKNOWN_FIELD");
        assert_eq!(
            val["extensions"]["suggestion"]["didYouMean"],
            serde_json::json!(["invoice"])
        );
        assert_eq!(val["extensions"]["correlationId"], "corr-42");
        assert_eq!(val["path"], serde_json::json!(["billing", 0, "invoce"]));

        // Discovery abstention carries GQL-071 nextStep + examples
        let abstain = GapError::new(ErrorCode::InsufficientEvidence, "low confidence")
            .with_suggestion(Suggestion {
                next_step: Some(NextStep::AskUser),
                examples: Some(vec!["find open invoices for customer 42".to_owned()]),
                ..Suggestion::default()
            });
        let abstain_val = serde_json::to_value(&abstain).expect("serialize abstain");
        assert_eq!(
            abstain_val["extensions"]["suggestion"]["nextStep"],
            "ask_user"
        );
        assert_eq!(abstain.code().request_level_http_status(), 200);
    }

    fn convert_via_try_from<U, T: TryFrom<U>>(value: U) -> Result<T, T::Error> {
        T::try_from(value)
    }

    // spec: NGC-060, NGC-031, GQL-072, INV-15
    #[test]
    fn ngc_error_info_conversions_honor_ngc_060_and_try_from_roundtrips() {
        for tag in 0..=13 {
            let code = NgcErrorCode::try_from(tag).expect("valid NGC tag");
            assert_eq!(i32::from(code), tag);
        }
        assert!(NgcErrorCode::try_from(99).is_err());
        assert!(NgcErrorInfo::try_from_wire(99, "bad tag", false, 0, None).is_err());

        // Unavailable / UpstreamError -> GAP_BRANCH_ERROR (retryable per flag)
        let unavail = NgcErrorInfo::try_from_wire(9, "upstream socket closed", true, 250, None)
            .expect("wire tag 9 is Unavailable");
        let gap_unavail: GapError =
            convert_via_try_from(unavail).expect("TryFrom<NgcErrorInfo> for GapError");
        assert_eq!(gap_unavail.code(), ErrorCode::BranchError);
        assert!(gap_unavail.retry().retryable);
        assert_eq!(gap_unavail.retry().after_ms, Some(250));

        let upstream_non_retry =
            NgcErrorInfo::new(NgcErrorCode::UpstreamError, "500 from upstream")
                .with_retry(false, 0);
        let gap_upstream = GapError::from(upstream_non_retry);
        assert_eq!(gap_upstream.code(), ErrorCode::BranchError);
        assert!(!gap_upstream.retry().retryable);
        assert_eq!(gap_upstream.retry().after_ms, None);

        // DeadlineExceeded -> GAP_BRANCH_TIMEOUT
        let timeout = GapError::from(NgcErrorInfo::new(
            NgcErrorCode::DeadlineExceeded,
            "timed out",
        ));
        assert_eq!(timeout.code(), ErrorCode::BranchTimeout);

        // PermissionDenied -> GAP_FORBIDDEN (never retryable)
        let forbidden = GapError::from(
            NgcErrorInfo::new(NgcErrorCode::PermissionDenied, "denied").with_retry(true, 100),
        );
        assert_eq!(forbidden.code(), ErrorCode::Forbidden);
        assert!(!forbidden.retry().retryable);

        // ResourceExhausted -> GAP_RATE_LIMITED (retryable with afterMs)
        let exhausted = GapError::from(
            NgcErrorInfo::new(NgcErrorCode::ResourceExhausted, "saturated").with_retry(true, 1_500),
        );
        assert_eq!(exhausted.code(), ErrorCode::RateLimited);
        assert!(exhausted.retry().retryable);
        assert_eq!(exhausted.retry().after_ms, Some(1_500));

        // NGC-031 dry-run UNIMPLEMENTED -> GAP_DRY_RUN_UNSUPPORTED
        let dry_run_err =
            NgcErrorInfo::new(NgcErrorCode::Unimplemented, "no dry run").into_dry_run_gap_error();
        assert_eq!(dry_run_err.code(), ErrorCode::DryRunUnsupported);

        // Reverse conversion GapError -> NgcErrorInfo via TryFrom/From
        let back: NgcErrorInfo =
            convert_via_try_from(exhausted).expect("TryFrom<GapError> for NgcErrorInfo");
        assert_eq!(back.code, NgcErrorCode::ResourceExhausted);
        assert!(back.retryable);
        assert_eq!(back.retry_after_ms, 1_500);
    }

    // spec: GQL-004
    #[test]
    fn id_validation_error_maps_reserved_names_to_gap_reserved_name() {
        let bad_op = "billing.__schema"
            .parse::<crate::ids::OperationId>()
            .expect_err("reserved __ prefix rejected");
        let gap_err = GapError::from(bad_op);
        assert_eq!(gap_err.code(), ErrorCode::ReservedName);
    }
}
