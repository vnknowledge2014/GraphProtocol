//! Incremental GraphQL response assembly (`RUN-060`, `GQL-051`) and `RUN-061` policy/PII redaction seam.

use std::fmt;

use gap_core::error::PathSegment;
use gap_core::snapshot::Snapshot;

/// Documented `RUN-061` seam for applying AuthzLayer field/value redaction (`@pii` or Cedar policy
/// masks) during response assembly before bytes leave the process (`spec/06-runtime.md` §6).
///
/// Full policy-plan wiring lands with `AuthzLayer` (`M6`); `WP-M1-07` invokes this seam on every
/// projected leaf and object field so downstream redaction plugs in without altering assembly flow.
pub trait ResponseRedactor: Send + Sync + fmt::Debug {
    /// Inspects or masks `value` projected for `(parent_type, field_name)` at `path` under `snapshot`.
    ///
    /// Returning `None` omits/removes the field when policy dictates removal; returning `Some(v)`
    /// emits `v` (for example `"***"` when masked).
    fn redact_field(
        &self,
        snapshot: &Snapshot,
        parent_type: &str,
        field_name: &str,
        path: &[PathSegment],
        value: serde_json::Value,
    ) -> Option<serde_json::Value>;
}

/// Default pass-through [`ResponseRedactor`] used in `WP-M1-07` (`RUN-061` seam).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopResponseRedactor;

impl ResponseRedactor for NoopResponseRedactor {
    fn redact_field(
        &self,
        _snapshot: &Snapshot,
        _parent_type: &str,
        _field_name: &str,
        _path: &[PathSegment],
        value: serde_json::Value,
    ) -> Option<serde_json::Value> {
        Some(value)
    }
}
