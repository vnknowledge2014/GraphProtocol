//! Kit profile: the connector-specific probe data (`conformance/ngc/<name>.kit.toml`).
//!
//! The kit never guesses which operations or keys to call from `Describe`; the connector's owner says so here. The
//! extension is `.toml` on purpose: the GAP fixture loader reads every `*.json` under `conformance/` and rejects unknown
//! keys, so a profile named `*.json` would break `--offline`/`--target` runs (`VER-010`).

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

/// Default absolute deadline used by the deadline check, in milliseconds from "now" (`NGC-025`).
pub const DEFAULT_DEADLINE_MS: u64 = 200;
/// Default time a connector gets to react to a deadline or a cancel before the kit fails it (`NGC-025`).
pub const DEFAULT_GRACE_MS: u64 = 2_000;
/// `Health` must answer within this budget (`NGC-070`).
pub const DEFAULT_HEALTH_BUDGET_MS: u64 = 500;
/// Deadline attached to ordinary calls so that a hung connector cannot hang the kit.
pub const DEFAULT_CALL_TIMEOUT_MS: u64 = 10_000;
/// Number of concurrent slow calls used by the capacity-recovery proof of the deadline and cancel checks.
pub const DEFAULT_SLOW_CALLS: u32 = 8;
/// Upper bound on slow calls (the proof must stay well inside any sane `max_in_flight`).
pub const MAX_SLOW_CALLS: u32 = 64;
/// Upper bound on pages walked by the pagination check (a cursor that never ends is a failure, not a hang).
pub const DEFAULT_MAX_PAGES: u32 = 200;

/// Errors from loading or validating a [`Profile`].
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// The file could not be read.
    #[error("cannot read profile `{path}`: {message}")]
    Io {
        /// Path that failed.
        path: String,
        /// OS error text.
        message: String,
    },
    /// The text is not valid TOML for the profile schema (unknown keys are rejected).
    #[error("profile is not valid: {0}")]
    Parse(String),
    /// The profile parsed but a field is out of range or inconsistent.
    #[error("profile field `{field}`: {reason}")]
    Invalid {
        /// Dotted field name.
        field: &'static str,
        /// What is wrong.
        reason: String,
    },
}

/// Timing knobs (all optional).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timing {
    /// Deadline of the deadline check, in ms from now.
    #[serde(default = "default_deadline_ms")]
    pub deadline_ms: u64,
    /// Time allowed to react to a deadline or a cancel.
    #[serde(default = "default_grace_ms")]
    pub grace_ms: u64,
    /// `Health` budget.
    #[serde(default = "default_health_budget_ms")]
    pub health_budget_ms: u64,
    /// Deadline of ordinary calls.
    #[serde(default = "default_call_timeout_ms")]
    pub call_timeout_ms: u64,
    /// Optional `Describe` latency budget (`NGC-010` SLO is p95 <= 100 ms; off unless the profile sets it).
    #[serde(default)]
    pub describe_budget_ms: Option<u64>,
}

fn default_deadline_ms() -> u64 {
    DEFAULT_DEADLINE_MS
}
fn default_grace_ms() -> u64 {
    DEFAULT_GRACE_MS
}
fn default_health_budget_ms() -> u64 {
    DEFAULT_HEALTH_BUDGET_MS
}
fn default_call_timeout_ms() -> u64 {
    DEFAULT_CALL_TIMEOUT_MS
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            deadline_ms: DEFAULT_DEADLINE_MS,
            grace_ms: DEFAULT_GRACE_MS,
            health_budget_ms: DEFAULT_HEALTH_BUDGET_MS,
            call_timeout_ms: DEFAULT_CALL_TIMEOUT_MS,
            describe_budget_ms: None,
        }
    }
}

/// A keyed read operation that supports `foreach` (`NGC-020`, `NGC-026`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadProbe {
    /// Operation name.
    pub operation: String,
    /// Static arguments sent with every call.
    #[serde(default)]
    pub arguments: toml::Table,
    /// Name of the variable carried in each `VariableSet.values`.
    pub key_var: String,
    /// At least two distinct keys that exist in the connector's data.
    pub keys: Vec<toml::Value>,
    /// Optional key that matches no row: its index must close empty and without error.
    #[serde(default)]
    pub absent_key: Option<toml::Value>,
    /// Optional key the connector rejects: its index must close with `error`, the others must be unaffected.
    #[serde(default)]
    pub failing_key: Option<toml::Value>,
    /// Optional row field that equals the key; lets the kit catch rows attributed to the wrong index.
    #[serde(default)]
    pub row_key: Option<String>,
    /// Upstream batch size when it differs from `limits.max_batch_size` (`NGC-026`); default: `limits.max_batch_size`.
    #[serde(default)]
    pub upstream_batch: Option<u32>,
}

/// A list operation used for ordering and pagination (`NGC-021`, `NGC-022`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageProbe {
    /// Operation name.
    pub operation: String,
    /// Arguments of the traversal.
    #[serde(default)]
    pub arguments: toml::Table,
    /// A second argument set; a cursor of the first must not work with it (`NGC-022`).
    #[serde(default)]
    pub other_arguments: Option<toml::Table>,
    /// Page size of the main traversal (>= 1). The data set must hold more rows than this.
    pub limit: u32,
    /// Row field to order by (needs `capabilities.order_by`).
    #[serde(default)]
    pub order_by: Option<String>,
    /// Upper bound on pages walked.
    #[serde(default = "default_max_pages")]
    pub max_pages: u32,
}

fn default_max_pages() -> u32 {
    DEFAULT_MAX_PAGES
}

/// An operation that never finishes by itself (`NGC-025`, `INV-12`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlowProbe {
    /// Operation name (a `Query` that stays open until the call is cancelled or its deadline passes).
    pub operation: String,
    /// Arguments.
    #[serde(default)]
    pub arguments: toml::Table,
    /// Concurrent calls of the capacity-recovery proof (default 8, at most 64, capped by `limits.max_in_flight`).
    #[serde(default = "default_slow_calls")]
    pub calls: u32,
}

fn default_slow_calls() -> u32 {
    DEFAULT_SLOW_CALLS
}

/// Optional read operation whose row count is the number of applied effects.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectProbe {
    /// `Query` operation name.
    pub operation: String,
    /// Arguments.
    #[serde(default)]
    pub arguments: toml::Table,
}

/// A mutation used for idempotency and dry-run (`NGC-030`, `NGC-031`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutateProbe {
    /// `Mutate` operation name.
    pub operation: String,
    /// Arguments.
    #[serde(default)]
    pub arguments: toml::Table,
    /// Row count of this query is the number of applied effects; without it "at most once" cannot be proven.
    #[serde(default)]
    pub probe: Option<EffectProbe>,
}

/// An operation whose full answer exceeds `limits.max_response_bytes` (`NGC-027`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeProbe {
    /// `Query` operation name.
    pub operation: String,
    /// Arguments.
    #[serde(default)]
    pub arguments: toml::Table,
    /// Rows of the complete (untruncated) answer.
    pub expected_total_rows: u64,
    /// Whether `next_cursor` lets the caller fetch the rest.
    #[serde(default)]
    pub resumable: bool,
}

/// Error-mapping probes (`NGC-023`, `NGC-060`, `NGC-061`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorProbe {
    /// A `Query` that must fail `INVALID_ARGUMENT` (for example a wrongly typed argument).
    #[serde(default)]
    pub invalid_query: Option<EffectProbe>,
}

/// The whole profile.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Profile name (shown in the report).
    pub name: String,
    /// Timing knobs.
    #[serde(default)]
    pub timing: Timing,
    /// Keyed read with `foreach`.
    #[serde(default)]
    pub read: Option<ReadProbe>,
    /// List operation for ordering and pagination.
    #[serde(default)]
    pub page: Option<PageProbe>,
    /// Never-ending operation.
    #[serde(default)]
    pub slow: Option<SlowProbe>,
    /// Mutation.
    #[serde(default)]
    pub mutate: Option<MutateProbe>,
    /// Oversized answer.
    #[serde(default)]
    pub size: Option<SizeProbe>,
    /// Error probes.
    #[serde(default)]
    pub errors: Option<ErrorProbe>,
}

fn invalid(field: &'static str, reason: impl Into<String>) -> ProfileError {
    ProfileError::Invalid {
        field,
        reason: reason.into(),
    }
}

fn require_text(field: &'static str, value: &str) -> Result<(), ProfileError> {
    if value.trim().is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    Ok(())
}

impl Profile {
    /// Parses and validates a profile.
    ///
    /// # Errors
    ///
    /// [`ProfileError::Parse`] on malformed TOML or an unknown key, [`ProfileError::Invalid`] on a bad value.
    pub fn from_toml_str(text: &str) -> Result<Self, ProfileError> {
        let profile: Self = toml::from_str(text).map_err(|e| ProfileError::Parse(e.to_string()))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Reads, parses and validates a profile file.
    ///
    /// # Errors
    ///
    /// [`ProfileError::Io`] if the file cannot be read, otherwise as [`Profile::from_toml_str`].
    pub fn from_path(path: &Path) -> Result<Self, ProfileError> {
        let text = std::fs::read_to_string(path).map_err(|e| ProfileError::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        Self::from_toml_str(&text)
    }

    /// Checks ranges and cross-field consistency.
    ///
    /// # Errors
    ///
    /// [`ProfileError::Invalid`] naming the first offending field.
    pub fn validate(&self) -> Result<(), ProfileError> {
        require_text("name", &self.name)?;
        let t = &self.timing;
        if t.deadline_ms == 0 {
            return Err(invalid("timing.deadline_ms", "must be >= 1"));
        }
        if t.grace_ms < t.deadline_ms {
            return Err(invalid("timing.grace_ms", "must be >= timing.deadline_ms"));
        }
        if t.health_budget_ms == 0 || t.call_timeout_ms < 1_000 {
            return Err(invalid(
                "timing",
                "health_budget_ms must be >= 1 and call_timeout_ms >= 1000",
            ));
        }
        if let Some(read) = &self.read {
            require_text("read.operation", &read.operation)?;
            require_text("read.key_var", &read.key_var)?;
            let mut seen = BTreeSet::new();
            for key in &read.keys {
                if !seen.insert(key.to_string()) {
                    return Err(invalid("read.keys", format!("duplicate key {key}")));
                }
            }
            if read.keys.len() < 2 {
                return Err(invalid("read.keys", "needs at least two distinct keys"));
            }
            for (field, key) in [
                ("read.absent_key", &read.absent_key),
                ("read.failing_key", &read.failing_key),
            ] {
                if let Some(key) = key
                    && seen.contains(&key.to_string())
                {
                    return Err(invalid(field, "must differ from every entry of read.keys"));
                }
            }
            if read.upstream_batch == Some(0) {
                return Err(invalid("read.upstream_batch", "must be >= 1"));
            }
        }
        if let Some(page) = &self.page {
            require_text("page.operation", &page.operation)?;
            if page.limit == 0 {
                return Err(invalid("page.limit", "must be >= 1"));
            }
            if page.max_pages < 2 {
                return Err(invalid("page.max_pages", "must be >= 2"));
            }
        }
        if let Some(slow) = &self.slow {
            require_text("slow.operation", &slow.operation)?;
            if slow.calls == 0 || slow.calls > MAX_SLOW_CALLS {
                return Err(invalid(
                    "slow.calls",
                    format!("must be in 1..={MAX_SLOW_CALLS}"),
                ));
            }
        }
        if let Some(mutate) = &self.mutate {
            require_text("mutate.operation", &mutate.operation)?;
            if let Some(probe) = &mutate.probe {
                require_text("mutate.probe.operation", &probe.operation)?;
            }
        }
        if let Some(size) = &self.size {
            require_text("size.operation", &size.operation)?;
            if size.expected_total_rows == 0 {
                return Err(invalid("size.expected_total_rows", "must be >= 1"));
            }
        }
        if let Some(errors) = &self.errors
            && let Some(q) = &errors.invalid_query
        {
            require_text("errors.invalid_query.operation", &q.operation)?;
        }
        Ok(())
    }
}

/// Converts a TOML value into the `google.protobuf.Value` the wire carries.
#[must_use]
pub fn toml_to_prost_value(value: &toml::Value) -> prost_types::Value {
    gap_ngc_sdk::error::json_to_prost_value(&toml_to_json(value))
}

/// Converts a TOML table into a `google.protobuf.Struct`.
#[must_use]
pub fn table_to_struct(table: &toml::Table) -> prost_types::Struct {
    let mut map = serde_json::Map::with_capacity(table.len());
    for (k, v) in table {
        map.insert(k.clone(), toml_to_json(v));
    }
    gap_ngc_sdk::error::json_map_to_prost_struct(&map)
}

fn toml_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(toml_to_json).collect())
        }
        toml::Value::Table(t) => {
            let mut map = serde_json::Map::with_capacity(t.len());
            for (k, v) in t {
                map.insert(k.clone(), toml_to_json(v));
            }
            serde_json::Value::Object(map)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
name = "demo"
[timing]
deadline_ms = 100
grace_ms = 1000
[read]
operation = "customers"
key_var = "id"
keys = [1, 2, 3]
absent_key = 99
failing_key = "bad"
row_key = "id"
[page]
operation = "customers_list"
limit = 3
order_by = "id"
other_arguments = { region = "us" }
[slow]
operation = "slow"
[mutate]
operation = "create_order"
arguments = { item = "x" }
[mutate.probe]
operation = "orders"
[size]
operation = "blobs"
expected_total_rows = 40
resumable = true
[errors.invalid_query]
operation = "customers"
arguments = { id = "oops" }
"#;

    // spec: NGC-091
    #[test]
    fn ngc_kit_profile_parses_a_full_profile_and_applies_defaults() {
        let p = Profile::from_toml_str(FULL).expect("full profile");
        assert_eq!(p.name, "demo");
        assert_eq!(p.timing.deadline_ms, 100);
        assert_eq!(p.timing.health_budget_ms, DEFAULT_HEALTH_BUDGET_MS);
        assert_eq!(p.slow.as_ref().map(|s| s.calls), Some(DEFAULT_SLOW_CALLS));
        assert_eq!(
            p.page.as_ref().map(|s| s.max_pages),
            Some(DEFAULT_MAX_PAGES)
        );
        assert!(p.mutate.as_ref().and_then(|m| m.probe.as_ref()).is_some());
    }

    // spec: NGC-091
    #[test]
    fn ngc_kit_profile_rejects_unknown_keys_and_bad_toml() {
        let err = Profile::from_toml_str("name = \"x\"\nbogus = 1\n").expect_err("unknown key");
        assert!(matches!(err, ProfileError::Parse(_)), "{err}");
        let err = Profile::from_toml_str("name = ").expect_err("bad toml");
        assert!(matches!(err, ProfileError::Parse(_)), "{err}");
        let err = Profile::from_toml_str(
            "name = \"x\"\n[read]\noperation = \"o\"\nkey_var = \"k\"\nkeys = [1, 2]\nextra = 1\n",
        )
        .expect_err("unknown nested key");
        assert!(matches!(err, ProfileError::Parse(_)), "{err}");
    }

    // spec: NGC-091
    #[test]
    fn ngc_kit_profile_validation_names_the_offending_field() {
        let cases: [(&str, &str); 8] = [
            ("name = \" \"\n", "name"),
            (
                "name = \"x\"\n[timing]\ndeadline_ms = 0\n",
                "timing.deadline_ms",
            ),
            (
                "name = \"x\"\n[timing]\ndeadline_ms = 500\ngrace_ms = 100\n",
                "timing.grace_ms",
            ),
            (
                "name = \"x\"\n[read]\noperation = \"o\"\nkey_var = \"k\"\nkeys = [1]\n",
                "read.keys",
            ),
            (
                "name = \"x\"\n[read]\noperation = \"o\"\nkey_var = \"k\"\nkeys = [1, 1]\n",
                "read.keys",
            ),
            (
                "name = \"x\"\n[read]\noperation = \"o\"\nkey_var = \"k\"\nkeys = [1, 2]\nabsent_key = 2\n",
                "read.absent_key",
            ),
            (
                "name = \"x\"\n[page]\noperation = \"o\"\nlimit = 0\n",
                "page.limit",
            ),
            (
                "name = \"x\"\n[slow]\noperation = \"o\"\ncalls = 65\n",
                "slow.calls",
            ),
        ];
        for (text, field) in cases {
            match Profile::from_toml_str(text) {
                Err(ProfileError::Invalid { field: got, .. }) => assert_eq!(got, field, "{text}"),
                other => panic!("expected Invalid({field}) for {text:?}, got {other:?}"),
            }
        }
    }

    // spec: NGC-091
    #[test]
    fn ngc_kit_profile_converts_toml_values_to_protobuf_values() {
        let table: toml::Table =
            toml::from_str("a = 1\nb = \"s\"\nc = [true, 2.5]\n[d]\ne = 3\n").expect("table");
        let st = table_to_struct(&table);
        let json = gap_ngc_sdk::error::prost_struct_to_json_map(&st);
        assert_eq!(
            serde_json::Value::Object(json),
            serde_json::json!({"a": 1.0, "b": "s", "c": [true, 2.5], "d": {"e": 3.0}})
        );
    }
}
