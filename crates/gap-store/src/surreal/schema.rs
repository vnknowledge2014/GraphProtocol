//! SurrealDB 3.x SurrealQL schema definitions and query normalization (`ADR-020`, `INV-09`).
//!
//! Every table and composite index in `SURREAL_SCHEMA_SURREALQL` leads with `tenant` so tenant
//! isolation (`INV-09`) holds at the schema and key-prefix layers.

/// SurrealDB 3.x `SCHEMAFULL` SurrealQL DDL for GAP `ContextStore` and `TaskStore` tables (`ADR-020`, `INV-09`).
///
/// Operators deploying an external SurrealDB 3.x instance can apply this schema idempotently via
/// `SurrealContextStore::ensure_remote_schema` or the SurrealDB CLI.
pub const SURREAL_SCHEMA_SURREALQL: &str = "\
DEFINE TABLE OVERWRITE nodes SCHEMAFULL;
DEFINE FIELD OVERWRITE tenant ON TABLE nodes TYPE string;
DEFINE FIELD OVERWRITE id ON TABLE nodes TYPE string;
DEFINE FIELD OVERWRITE node_type ON TABLE nodes TYPE string;
DEFINE FIELD OVERWRITE label ON TABLE nodes TYPE string;
DEFINE FIELD OVERWRITE summary ON TABLE nodes TYPE option<string>;
DEFINE FIELD OVERWRITE props_json ON TABLE nodes TYPE string;
DEFINE FIELD OVERWRITE scope ON TABLE nodes TYPE string;
DEFINE FIELD OVERWRITE base_scope ON TABLE nodes TYPE string;
DEFINE FIELD OVERWRITE owner_principal ON TABLE nodes TYPE string;
DEFINE FIELD OVERWRITE task_id ON TABLE nodes TYPE option<string>;
DEFINE FIELD OVERWRITE group_name ON TABLE nodes TYPE option<string>;
DEFINE FIELD OVERWRITE expires_at_us ON TABLE nodes TYPE option<int>;
DEFINE FIELD OVERWRITE prov_source ON TABLE nodes TYPE string;
DEFINE FIELD OVERWRITE prov_desc_hash ON TABLE nodes TYPE option<string>;
DEFINE FIELD OVERWRITE prov_confidence ON TABLE nodes TYPE option<float>;
DEFINE FIELD OVERWRITE prov_extracted_at_us ON TABLE nodes TYPE option<int>;
DEFINE FIELD OVERWRITE prov_agent ON TABLE nodes TYPE option<string>;
DEFINE FIELD OVERWRITE valid_from_us ON TABLE nodes TYPE int;
DEFINE FIELD OVERWRITE valid_to_us ON TABLE nodes TYPE option<int>;
DEFINE FIELD OVERWRITE content_hash ON TABLE nodes TYPE string;
DEFINE INDEX OVERWRITE idx_nodes_tenant_temporal ON TABLE nodes FIELDS tenant, id, valid_from_us;
DEFINE INDEX OVERWRITE idx_nodes_tenant_base_task ON TABLE nodes FIELDS tenant, base_scope, task_id, valid_to_us;
DEFINE INDEX OVERWRITE idx_nodes_tenant_ephemeral ON TABLE nodes FIELDS tenant, scope, expires_at_us;

DEFINE TABLE OVERWRITE edges SCHEMAFULL;
DEFINE FIELD OVERWRITE tenant ON TABLE edges TYPE string;
DEFINE FIELD OVERWRITE id ON TABLE edges TYPE string;
DEFINE FIELD OVERWRITE edge_type ON TABLE edges TYPE string;
DEFINE FIELD OVERWRITE from_id ON TABLE edges TYPE string;
DEFINE FIELD OVERWRITE to_id ON TABLE edges TYPE string;
DEFINE FIELD OVERWRITE props_json ON TABLE edges TYPE string;
DEFINE FIELD OVERWRITE confidence ON TABLE edges TYPE option<float>;
DEFINE FIELD OVERWRITE resolution ON TABLE edges TYPE string;
DEFINE FIELD OVERWRITE prov_source ON TABLE edges TYPE string;
DEFINE FIELD OVERWRITE prov_desc_hash ON TABLE edges TYPE option<string>;
DEFINE FIELD OVERWRITE prov_confidence ON TABLE edges TYPE option<float>;
DEFINE FIELD OVERWRITE prov_extracted_at_us ON TABLE edges TYPE option<int>;
DEFINE FIELD OVERWRITE prov_agent ON TABLE edges TYPE option<string>;
DEFINE FIELD OVERWRITE valid_from_us ON TABLE edges TYPE int;
DEFINE FIELD OVERWRITE valid_to_us ON TABLE edges TYPE option<int>;
DEFINE FIELD OVERWRITE content_hash ON TABLE edges TYPE string;
DEFINE INDEX OVERWRITE idx_edges_tenant_id ON TABLE edges FIELDS tenant, id, valid_from_us;
DEFINE INDEX OVERWRITE idx_edges_tenant_from ON TABLE edges FIELDS tenant, from_id, valid_to_us;
DEFINE INDEX OVERWRITE idx_edges_tenant_to ON TABLE edges FIELDS tenant, to_id, valid_to_us;

DEFINE TABLE OVERWRITE tenant_stats SCHEMAFULL;
DEFINE FIELD OVERWRITE tenant ON TABLE tenant_stats TYPE string;
DEFINE FIELD OVERWRITE active_nodes ON TABLE tenant_stats TYPE int;
DEFINE FIELD OVERWRITE active_edges ON TABLE tenant_stats TYPE int;
DEFINE INDEX OVERWRITE idx_tenant_stats_tenant ON TABLE tenant_stats FIELDS tenant UNIQUE;

DEFINE TABLE OVERWRITE tasks SCHEMAFULL;
DEFINE FIELD OVERWRITE tenant ON TABLE tasks TYPE string;
DEFINE FIELD OVERWRITE task_id ON TABLE tasks TYPE string;
DEFINE FIELD OVERWRITE status ON TABLE tasks TYPE string;
DEFINE FIELD OVERWRITE payload_json ON TABLE tasks TYPE string;
DEFINE FIELD OVERWRITE lease_owner ON TABLE tasks TYPE option<string>;
DEFINE FIELD OVERWRITE lease_expires_at_rfc3339 ON TABLE tasks TYPE option<string>;
DEFINE FIELD OVERWRITE lease_expires_at_ms ON TABLE tasks TYPE option<int>;
DEFINE FIELD OVERWRITE updated_at_rfc3339 ON TABLE tasks TYPE string;
DEFINE FIELD OVERWRITE updated_at_ms ON TABLE tasks TYPE int;
DEFINE FIELD OVERWRITE last_seq ON TABLE tasks TYPE int;
DEFINE INDEX OVERWRITE idx_tasks_tenant_id ON TABLE tasks FIELDS tenant, task_id UNIQUE;
DEFINE INDEX OVERWRITE idx_tasks_tenant_status ON TABLE tasks FIELDS tenant, status, updated_at_ms;

DEFINE TABLE OVERWRITE task_events SCHEMAFULL;
DEFINE FIELD OVERWRITE tenant ON TABLE task_events TYPE string;
DEFINE FIELD OVERWRITE task_id ON TABLE task_events TYPE string;
DEFINE FIELD OVERWRITE seq ON TABLE task_events TYPE int;
DEFINE FIELD OVERWRITE status ON TABLE task_events TYPE string;
DEFINE FIELD OVERWRITE payload_json ON TABLE task_events TYPE string;
DEFINE FIELD OVERWRITE emitted_at_rfc3339 ON TABLE task_events TYPE string;
DEFINE INDEX OVERWRITE idx_task_events_tenant_task_seq ON TABLE task_events FIELDS tenant, task_id, seq UNIQUE;
";

/// Builds a tenant-prefixed SurrealDB record identifier (`<table>:<len-prefixed hex tenant>_<id>`)
/// so `tenant` always leads record keys (`INV-09`) and hostile characters never inject syntax.
#[must_use]
pub fn tenant_record_key(table: &str, tenant: &str, entity_id: &str) -> String {
    let t_hex = blake3_short_hex(tenant.as_bytes());
    let e_hex = blake3_short_hex(entity_id.as_bytes());
    format!("{table}:t{t_hex}_{e_hex}")
}

fn blake3_short_hex(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    let raw = hash.as_bytes();
    let mut out = String::with_capacity(32);
    for b in &raw[..16] {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Splits raw text or an untrusted search query into lowercase alphanumeric tokens, ensuring
/// hostile FTS punctuation (`"`, `*`, `NEAR(`, `OR`, `-`, `;`) is neutralized as literal tokens.
#[must_use]
pub fn tokenize_alphanumeric(raw: &str) -> Vec<String> {
    raw.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}
