//! Connector and per-operation configuration (`CMP-050`, `CMP-051`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Supported SQL dialects (`CMP-052`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    /// SQLite 3.x; `?` / `?N` positional parameters; `IN (?,?,?)` foreach batches.
    Sqlite,
    /// PostgreSQL 12+; `$N` positional parameters; `= ANY($1)` foreach batches.
    /// Requires a user-supplied [`crate::driver::Driver`] backed by e.g. `tokio-postgres`.
    Postgres,
}

/// SQL type annotation for schema introspection (`CMP-050`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlType {
    Text,
    Integer,
    Float,
    Boolean,
    Uuid,
    Timestamp,
    Json,
    Bytes,
}

impl SqlType {
    /// Returns the GraphQL scalar name for this SQL type.
    #[must_use]
    pub fn graphql_scalar(&self) -> &'static str {
        match self {
            SqlType::Text | SqlType::Timestamp | SqlType::Json | SqlType::Bytes => "String",
            SqlType::Integer => "Int",
            SqlType::Float => "Float",
            SqlType::Boolean => "Boolean",
            SqlType::Uuid => "ID",
        }
    }

    /// Returns the JSON Schema `"type"` keyword for this SQL type (`NGC-013`).
    #[must_use]
    pub fn json_schema_type(&self) -> &'static str {
        match self {
            SqlType::Text | SqlType::Timestamp | SqlType::Uuid | SqlType::Bytes => "string",
            SqlType::Integer => "integer",
            SqlType::Float => "number",
            SqlType::Boolean => "boolean",
            SqlType::Json => "object",
        }
    }
}

/// Definition of one column in an operation's table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnDef {
    /// Column name as it appears in the database (validated against allow-list; `CMP-050`).
    pub name: String,
    /// SQL type for value conversion.
    pub sql_type: SqlType,
    /// Whether this column may be null.
    pub nullable: bool,
}

/// Read operation shape exposed by an SQL table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// Lookup by the configured key columns; returns at most one row.
    GetByKey,
    /// Filtered, ordered, paginated table scan.
    #[default]
    List,
}

/// Per-operation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationConfig {
    /// Operation name as exposed in the NGC descriptor (`[A-Za-z_][A-Za-z0-9_]*`).
    pub name: String,
    /// Query shape; defaults to `list` in deserialized configuration.
    #[serde(default)]
    pub kind: OperationKind,
    /// Database table name (validated as an SQL identifier; `CMP-050`).
    pub table: String,
    /// Column definitions, in order. Identifiers are validated before SQL generation.
    pub columns: Vec<ColumnDef>,
    /// Column(s) used as the primary key for `get_by_key` and foreach batches.
    pub key_columns: Vec<String>,
    /// Maximum rows returned per call (server-side cap; `CMP-050`).
    pub row_cap: u32,
    /// Statement timeout in milliseconds (enforced before execution; `CMP-050`).
    pub statement_timeout_ms: u64,
    /// Optional asynchronous simulated latency before query execution (used for test/fixture slow probes).
    #[serde(default)]
    pub simulated_latency_ms: u64,
}

impl OperationConfig {
    /// Returns the computed set of valid column identifiers for SQL generation.
    #[must_use]
    pub fn column_allow_list(&self) -> BTreeSet<&str> {
        self.columns.iter().map(|c| c.name.as_str()).collect()
    }

    /// Looks up a column definition by name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name == name)
    }
}

/// Top-level connector configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlConfig {
    /// Connector identifier (embedded in `DescribeResponse.connector_id`).
    pub connector_id: String,
    /// Connector version string.
    pub connector_version: String,
    /// SQL dialect.
    pub dialect: Dialect,
    /// Database connection string or file path.
    pub dsn: String,
    /// Maximum concurrent in-flight queries.
    pub max_in_flight: u32,
    /// Maximum variable-set batch size for `foreach` (1–10000; `CMP-050`).
    pub max_batch_size: u32,
    /// Maximum rows per page (default 1000).
    pub max_page_size: u32,
    /// Maximum serialized row bytes returned per call (`NGC-027`).
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: u64,
    /// Operations exposed by this connector.
    pub operations: Vec<OperationConfig>,
}

const fn default_max_response_bytes() -> u64 {
    4 * 1024 * 1024
}

impl Default for SqlConfig {
    fn default() -> Self {
        Self {
            connector_id: "gap-ngc-sql".to_owned(),
            connector_version: "0.1.0".to_owned(),
            dialect: Dialect::Sqlite,
            dsn: ":memory:".to_owned(),
            max_in_flight: 8,
            max_batch_size: 1000,
            max_page_size: 1000,
            max_response_bytes: default_max_response_bytes(),
            operations: Vec::new(),
        }
    }
}

impl SqlConfig {
    /// Returns `Err` with a description of the first invalid field, if any.
    pub fn validate(&self) -> Result<(), String> {
        if self.connector_id.trim().is_empty() {
            return Err("connector_id must not be empty".into());
        }
        if self.max_batch_size == 0 {
            return Err("max_batch_size must be >= 1".into());
        }
        if self.max_batch_size > 10_000 {
            return Err(format!(
                "max_batch_size {} exceeds limit 10,000",
                self.max_batch_size
            ));
        }
        if self.max_in_flight == 0 {
            return Err("max_in_flight must be >= 1".into());
        }
        if self.max_page_size == 0 {
            return Err("max_page_size must be >= 1".into());
        }
        if self.max_response_bytes == 0 {
            return Err("max_response_bytes must be >= 1".into());
        }
        if self.operations.is_empty() {
            return Err("operations must not be empty".into());
        }
        for op in &self.operations {
            validate_identifier(&op.name).map_err(|e| format!("operation.name: {e}"))?;
            validate_identifier(&op.table)
                .map_err(|e| format!("operation '{}' table: {e}", op.name))?;
            if op.columns.is_empty() {
                return Err(format!("operation '{}' has no columns", op.name));
            }
            if op.key_columns.is_empty() {
                return Err(format!("operation '{}' has no key_columns", op.name));
            }
            if op.row_cap == 0 {
                return Err(format!("operation '{}' row_cap must be >= 1", op.name));
            }
            if op.statement_timeout_ms == 0 {
                return Err(format!(
                    "operation '{}' statement_timeout_ms must be >= 1",
                    op.name
                ));
            }
            let mut seen_cols = BTreeSet::new();
            for col in &op.columns {
                validate_identifier(&col.name)
                    .map_err(|e| format!("operation '{}' column: {e}", op.name))?;
                if !seen_cols.insert(col.name.as_str()) {
                    return Err(format!(
                        "operation '{}' duplicate column '{}'",
                        op.name, col.name
                    ));
                }
            }
            let allow = op.column_allow_list();
            for k in &op.key_columns {
                if !allow.contains(k.as_str()) {
                    return Err(format!(
                        "operation '{}' key_column '{}' not in columns list",
                        op.name, k
                    ));
                }
            }
        }

        // Check for duplicate operation names.
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for (i, op) in self.operations.iter().enumerate() {
            if let Some(j) = seen.insert(op.name.as_str(), i) {
                return Err(format!(
                    "duplicate operation name '{}' at indices {} and {}",
                    op.name, j, i
                ));
            }
        }

        Ok(())
    }

    /// Looks up an operation by name.
    #[must_use]
    pub fn operation(&self, name: &str) -> Option<&OperationConfig> {
        self.operations.iter().find(|op| op.name == name)
    }
}

/// Returns `Ok(())` if `s` is a valid SQL identifier: `[A-Za-z_][A-Za-z0-9_]*`, max 63 chars.
///
/// This is the core SQL-injection guard: identifiers come only from the config allow-list,
/// never from request data (`CMP-050`).
pub fn validate_identifier(s: &str) -> Result<(), String> {
    if s.len() > 63 {
        return Err(format!("identifier '{s}' exceeds 63 characters"));
    }
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return Err("identifier must not be empty".into());
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(format!(
            "identifier '{s}' must start with a letter or underscore"
        ));
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' {
            return Err(format!(
                "identifier '{s}' contains disallowed character '{c}'"
            ));
        }
    }
    Ok(())
}
