//! Validated graph types, scopes, traversal inputs/outputs, and deterministic write validation
//! (`ING-060`..`ING-067`, `INV-09`, `INV-19`, `GQL-040`, `GQL-043`).

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use gap_core::clock::Timestamp;
use gap_core::ids::TaskId;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::error::StoreError;

/// Maximum length in bytes of a [`NodeId`] (`m2-01-design.md` §2).
pub const MAX_NODE_ID_BYTES: usize = 512;

/// Maximum length in bytes of an [`EdgeId`] (`m2-01-design.md` §2).
pub const MAX_EDGE_ID_BYTES: usize = 512;

/// Maximum length in bytes of a [`NodeType`] or [`EdgeType`] (`[A-Za-z][A-Za-z0-9_]{0,63}`).
pub const MAX_TYPE_NAME_BYTES: usize = 64;

/// Maximum length in bytes of [`Provenance::source`] (`m2-01-design.md` §2).
pub const MAX_PROVENANCE_SOURCE_BYTES: usize = 256;

/// Maximum length in bytes of [`Caller::principal`].
pub const MAX_PRINCIPAL_BYTES: usize = 512;

/// Maximum length in bytes of a node `label` (`m2-01-design.md` §2).
pub const MAX_LABEL_BYTES: usize = 512;

/// Maximum length in bytes of a node `summary` (`m2-01-design.md` §2).
pub const MAX_SUMMARY_BYTES: usize = 1_024;

/// Maximum length in bytes of a `Group` scope claim (`m2-01-design.md` §2).
pub const MAX_GROUP_CLAIM_BYTES: usize = 128;

/// Maximum serialized UTF-8 byte size of a `props` JSON object (`64 KiB`, `m2-01-design.md` §2).
pub const MAX_PROPS_BYTES: usize = 65_536;

/// Maximum container nesting depth of a `props` JSON object (`m2-01-design.md` §2).
///
/// Depth counts simultaneous open JSON containers (`{` or `[`), with the root `props` map at depth `1`.
pub const MAX_PROPS_DEPTH: usize = 16;

/// Maximum total items (`nodes.len() + edges.len()`) accepted by a single `put_batch` (`m2-01-design.md` §4).
pub const MAX_BATCH_ITEMS: usize = 10_000;

/// Default time-to-live in microseconds for `ScopeSpec::Ephemeral` when `expires_at` is `None` (`1 h`, `ING-066`).
pub const DEFAULT_EPHEMERAL_TTL_US: i64 = 3_600 * 1_000_000;

/// Maximum time-to-live in microseconds for `ScopeSpec::Ephemeral` (`24 h`, `ING-066`).
pub const MAX_EPHEMERAL_TTL_US: i64 = 24 * 3_600 * 1_000_000;

/// Default retention window before tombstoned `Task` nodes are hard-deleted (`7 d`, `ING-066`).
pub const DEFAULT_TASK_RETENTION_SECS: u64 = 7 * 86_400;

/// Minimum allowed `max_depth` for [`TraverseQuery`] (`GQL-040`, `ING-064`).
pub const MIN_TRAVERSE_DEPTH: u32 = 1;

/// Maximum allowed `max_depth` for [`TraverseQuery`] (`GQL-040`, `ING-064`).
pub const MAX_TRAVERSE_DEPTH: u32 = 16;

/// Minimum allowed `first` page size for [`TraverseQuery`] (`ING-064`).
pub const MIN_TRAVERSE_FIRST: u32 = 1;

/// Maximum allowed `first` page size for [`TraverseQuery`] (`ING-064`).
pub const MAX_TRAVERSE_FIRST: u32 = 1_000;

/// Default per-query visited node cap for [`TraverseQuery`] (`ING-064`).
pub const DEFAULT_VISITED_CAP: u32 = 100_000;

/// Maximum allowed per-query visited node cap for [`TraverseQuery`] (`m2-01-design.md` §6).
pub const MAX_VISITED_CAP: u32 = 1_000_000;

/// Minimum allowed `limit` for `search_nodes` (`m2-01-design.md` §7).
pub const MIN_SEARCH_LIMIT: u32 = 1;

/// Maximum allowed `limit` for `search_nodes` (`m2-01-design.md` §7).
pub const MAX_SEARCH_LIMIT: u32 = 100;

/// Default per-tenant active node ceiling in SQLite (`ING-064`).
pub const DEFAULT_MAX_NODES_PER_TENANT: u64 = 5_000_000;

/// Default per-tenant active edge ceiling in SQLite (`ING-064`).
pub const DEFAULT_MAX_EDGES_PER_TENANT: u64 = 20_000_000;

fn validate_non_control_id(
    value: &str,
    max_bytes: usize,
    field: &'static str,
) -> Result<(), StoreError> {
    if value.is_empty() {
        return Err(StoreError::Invalid {
            field,
            reason: "must not be empty",
        });
    }
    if value.len() > max_bytes {
        return Err(StoreError::Invalid {
            field,
            reason: "exceeds maximum byte length",
        });
    }
    if value.as_bytes().contains(&0) || value.chars().any(char::is_control) {
        return Err(StoreError::Invalid {
            field,
            reason: "must not contain NUL or control characters",
        });
    }
    Ok(())
}

fn validate_type_identifier(value: &str, field: &'static str) -> Result<(), StoreError> {
    let bytes = value.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return Err(StoreError::Invalid {
            field,
            reason: "must not be empty",
        });
    };
    if bytes.len() > MAX_TYPE_NAME_BYTES {
        return Err(StoreError::Invalid {
            field,
            reason: "exceeds 64 bytes",
        });
    }
    if !first.is_ascii_alphabetic() {
        return Err(StoreError::Invalid {
            field,
            reason: "must start with an ASCII letter",
        });
    }
    if !rest.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_') {
        return Err(StoreError::Invalid {
            field,
            reason: "must match `[A-Za-z][A-Za-z0-9_]{0,63}`",
        });
    }
    Ok(())
}

macro_rules! define_validated_string {
    ($(#[$meta:meta])* $vis:vis struct $name:ident, $field:literal, $validator:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        $vis struct $name(String);

        impl $name {
            /// Validates `raw` and constructs a new instance.
            ///
            /// # Errors
            ///
            /// Returns [`StoreError::Invalid`] if `raw` violates the format or length rules.
            pub fn new(raw: impl Into<String>) -> Result<Self, StoreError> {
                let value = raw.into();
                ($validator)(&value, $field)?;
                Ok(Self(value))
            }

            /// Borrows the validated string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = StoreError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = StoreError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = StoreError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

define_validated_string!(
    /// Validated context node identifier (`1..=512` bytes, no NUL or control characters).
    pub struct NodeId,
    "node_id",
    |s: &str, f: &'static str| validate_non_control_id(s, MAX_NODE_ID_BYTES, f)
);

define_validated_string!(
    /// Validated context edge identifier (`1..=512` bytes, no NUL or control characters).
    pub struct EdgeId,
    "edge_id",
    |s: &str, f: &'static str| validate_non_control_id(s, MAX_EDGE_ID_BYTES, f)
);

define_validated_string!(
    /// Validated context node type (`[A-Za-z][A-Za-z0-9_]{0,63}`, `ING-060`).
    pub struct NodeType,
    "node_type",
    validate_type_identifier
);

define_validated_string!(
    /// Validated context edge type (`[A-Za-z][A-Za-z0-9_]{0,63}`, `ING-060`).
    pub struct EdgeType,
    "edge_type",
    validate_type_identifier
);

/// Reported context visibility scope (`ING-065`, `spec/02-gap-ql.md` `ContextScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Scope {
    /// Visible only when `owner_principal == caller.principal`.
    Private,
    /// Visible when `task_id` is in `caller.task_chain` (`ADR-016`).
    Task,
    /// Visible when `group` is in `caller.groups`.
    Group,
    /// Visible to every caller within the same tenant (`INV-09`).
    Global,
    /// Time-bounded scope visible like its [`BaseScope`] while `expires_at > now` (`ING-065`, `ING-066`).
    Ephemeral,
}

impl Scope {
    /// Canonical uppercase SQL/wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Private => "PRIVATE",
            Self::Task => "TASK",
            Self::Group => "GROUP",
            Self::Global => "GLOBAL",
            Self::Ephemeral => "EPHEMERAL",
        }
    }

    /// Parses a stored uppercase scope string.
    pub fn parse_db(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "PRIVATE" => Ok(Self::Private),
            "TASK" => Ok(Self::Task),
            "GROUP" => Ok(Self::Group),
            "GLOBAL" => Ok(Self::Global),
            "EPHEMERAL" => Ok(Self::Ephemeral),
            _ => Err(StoreError::Corrupt),
        }
    }
}

/// Underlying non-ephemeral visibility rule attached to [`ScopeSpec::Ephemeral`] (`ING-065`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BaseScope {
    /// Visible only to `owner_principal`.
    Private,
    /// Visible to `task_id` and its descendants in `caller.task_chain`.
    Task {
        /// Owning task identifier.
        task_id: TaskId,
    },
    /// Visible to principals carrying `group` in `caller.groups`.
    Group {
        /// Non-empty group claim (`1..=128` bytes).
        group: String,
    },
    /// Visible to all principals in the tenant.
    Global,
}

/// Scope specification supplied when writing a node (`ING-065`, `ING-066`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScopeSpec {
    /// Owner-only scope (`owner_principal` taken from writer).
    Private,
    /// Task-chain scope (`ADR-016`).
    Task {
        /// Owning task identifier.
        task_id: TaskId,
    },
    /// Group-claim scope (`1..=128` bytes).
    Group {
        /// Principal group claim string.
        group: String,
    },
    /// Tenant-wide scope; the only scope that accepts caller-chosen node IDs on creation.
    Global,
    /// Time-bounded scope (`expires_at <= now + 24h`; defaults to `now + 1h` when `None`, `ING-066`).
    Ephemeral {
        /// Base visibility rule while unexpired.
        base: BaseScope,
        /// Optional explicit expiration timestamp (`None` defaults to `now + 1h`).
        expires_at: Option<Timestamp>,
    },
}

impl ScopeSpec {
    /// Constructs an ephemeral scope with an explicit `expires_at` instant.
    #[must_use]
    pub const fn ephemeral(base: BaseScope, expires_at: Timestamp) -> Self {
        Self::Ephemeral {
            base,
            expires_at: Some(expires_at),
        }
    }

    /// Constructs an ephemeral scope using the default `now + 1 h` lifetime (`ING-066`).
    #[must_use]
    pub const fn ephemeral_default(base: BaseScope) -> Self {
        Self::Ephemeral {
            base,
            expires_at: None,
        }
    }
}

/// Authenticated caller identity and claims evaluated inside a [`gap_core::ids::TenantScope`] (`ING-065`, `ADR-016`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caller {
    /// Principal identifier (`1..=512` bytes, no control characters).
    pub principal: String,
    /// Principal group claims.
    pub groups: BTreeSet<String>,
    /// Current task followed by its ancestors in the A2A task chain (`ADR-016`).
    pub task_chain: Vec<TaskId>,
}

impl Caller {
    /// Creates a [`Caller`] with no groups and an empty task chain.
    #[must_use]
    pub fn new(principal: impl Into<String>) -> Self {
        Self {
            principal: principal.into(),
            groups: BTreeSet::new(),
            task_chain: Vec::new(),
        }
    }

    /// Attaches principal group claims.
    #[must_use]
    pub fn with_groups<I, S>(mut self, groups: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.groups = groups.into_iter().map(Into::into).collect();
        self
    }

    /// Attaches the caller's task chain (`current` followed by ancestors).
    #[must_use]
    pub fn with_task_chain(mut self, task_chain: Vec<TaskId>) -> Self {
        self.task_chain = task_chain;
        self
    }
}

/// Entity provenance metadata (`spec/02-gap-ql.md` `Provenance`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Non-empty origin identifier (`1..=256` bytes, no NUL/control characters).
    pub source: String,
    /// Optional content or descriptor hash.
    pub desc_hash: Option<String>,
    /// Optional extractor confidence in `[0.0, 1.0]`.
    pub confidence: Option<f32>,
    /// Optional extraction timestamp (stored at microsecond precision).
    pub extracted_at: Option<Timestamp>,
    /// Optional agent or extractor identifier.
    pub agent: Option<String>,
}

impl Provenance {
    /// Creates minimal provenance with `source` and all optional fields `None`.
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            desc_hash: None,
            confidence: None,
            extracted_at: None,
            agent: None,
        }
    }
}

/// Edge derivation precision (`ING-060`, `spec/02-gap-ql.md` `EdgeResolution`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeResolution {
    /// Exact structural or compiler-verified edge.
    Exact,
    /// Heuristic or tree-sitter-inferred edge.
    Heuristic,
}

impl EdgeResolution {
    /// Canonical uppercase SQL/wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "EXACT",
            Self::Heuristic => "HEURISTIC",
        }
    }

    /// Parses a stored uppercase resolution string.
    pub fn parse_db(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "EXACT" => Ok(Self::Exact),
            "HEURISTIC" => Ok(Self::Heuristic),
            _ => Err(StoreError::Corrupt),
        }
    }
}

/// Stored context graph node (`spec/02-gap-ql.md` `GapNode`, `m2-01-design.md` §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Node identifier.
    pub id: NodeId,
    /// Node category/type.
    pub node_type: NodeType,
    /// Human/agent-readable label (`<= 512` bytes).
    pub label: String,
    /// Optional short summary (`<= 1,024` bytes, `ING-063`).
    pub summary: Option<String>,
    /// Structured JSON properties (`<= 64 KiB`, depth `<= 16`).
    pub props: JsonMap<String, JsonValue>,
    /// Reported visibility scope.
    pub scope: Scope,
    /// Principal that wrote the node.
    pub owner_principal: String,
    /// Owning task when created under `Task` or `Ephemeral(Task)`.
    pub task_id: Option<TaskId>,
    /// Group claim when created under `Group` or `Ephemeral(Group)`.
    pub group: Option<String>,
    /// Expiration timestamp when `scope == Scope::Ephemeral`.
    pub expires_at: Option<Timestamp>,
    /// Provenance metadata.
    pub provenance: Provenance,
    /// Start of temporal validity (`inclusive`, UTC microseconds).
    pub valid_from: Timestamp,
    /// End of temporal validity (`exclusive`, `None` when current).
    pub valid_to: Option<Timestamp>,
}

/// Write payload for `put_node` and `put_batch` (`m2-01-design.md` §§2, 4, 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewNode {
    /// Target node ID (`Some` creates or updates for `Global`, and updates only for non-`Global`;
    /// `None` mints a store-generated UUIDv7).
    pub id: Option<NodeId>,
    /// Node category/type.
    pub node_type: NodeType,
    /// Node label (`<= 512` bytes).
    pub label: String,
    /// Optional summary (`<= 1,024` bytes).
    pub summary: Option<String>,
    /// Structured JSON properties (`<= 64 KiB`, depth `<= 16`).
    pub props: JsonMap<String, JsonValue>,
    /// Visibility scope specification.
    pub scope: ScopeSpec,
    /// Provenance metadata.
    pub provenance: Provenance,
}

/// Stored directed context graph edge (`spec/02-gap-ql.md` `GapEdge`, `m2-01-design.md` §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    /// Edge identifier.
    pub id: EdgeId,
    /// Edge relationship type.
    pub edge_type: EdgeType,
    /// Source node identifier.
    pub from: NodeId,
    /// Target node identifier.
    pub to: NodeId,
    /// Structured JSON properties (`<= 64 KiB`, depth `<= 16`).
    pub props: JsonMap<String, JsonValue>,
    /// Optional confidence in `[0.0, 1.0]`.
    pub confidence: Option<f32>,
    /// Exact vs heuristic resolution.
    pub resolution: EdgeResolution,
    /// Provenance metadata.
    pub provenance: Provenance,
    /// Start of temporal validity (`inclusive`, UTC microseconds).
    pub valid_from: Timestamp,
    /// End of temporal validity (`exclusive`, `None` when current).
    pub valid_to: Option<Timestamp>,
}

/// Write payload for `put_edge` and `put_batch` (`m2-01-design.md` §§2, 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEdge {
    /// Optional edge ID (`Some` allowed on `Global`-to-`Global` creation or matching an existing
    /// edge; `None` mints a UUIDv7 or reuses the current `(edge_type, from, to)` ID).
    pub id: Option<EdgeId>,
    /// Edge relationship type.
    pub edge_type: EdgeType,
    /// Source node identifier.
    pub from: NodeId,
    /// Target node identifier.
    pub to: NodeId,
    /// Structured JSON properties (`<= 64 KiB`, depth `<= 16`).
    pub props: JsonMap<String, JsonValue>,
    /// Optional confidence in `[0.0, 1.0]`.
    pub confidence: Option<f32>,
    /// Exact vs heuristic resolution.
    pub resolution: EdgeResolution,
    /// Provenance metadata.
    pub provenance: Provenance,
}

/// Identifiers returned by an atomic `put_batch` call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResult {
    /// Resulting node IDs in input slice order.
    pub node_ids: Vec<NodeId>,
    /// Resulting edge IDs in input slice order.
    pub edge_ids: Vec<EdgeId>,
}

/// Visible node and edge counts at an `as_of` instant (`ING-065`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphCounts {
    /// Number of visible nodes valid at `as_of`.
    pub nodes: u64,
    /// Number of edges valid at `as_of` whose both endpoints are visible at `as_of`.
    pub edges: u64,
}

/// Edge traversal direction (`spec/02-gap-ql.md` `Direction`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Direction {
    /// Follow edges from `from` to `to`.
    #[default]
    Outbound,
    /// Follow edges in reverse from `to` to `from`.
    Inbound,
    /// Follow edges in either direction.
    Both,
}

/// Bounded graph walk strategy (`spec/02-gap-ql.md` `TraverseStrategy`, `ING-067`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TraverseStrategy {
    /// Breadth-first search ordered by `(depth, node_id)`.
    #[default]
    Bfs,
    /// Preorder depth-first search with neighbours ordered by `(neighbour_id, edge_id)`.
    Dfs,
    /// BFS shortest-path tree (one edge per reached node from smallest-id predecessor at minimal depth).
    Shortest,
    /// Hop-bounded Dijkstra ordered by `(cost, node_id)` over `props["weight"]` (`ING-067`).
    Weighted,
}

/// Stateless pagination token storing the order key of the last returned node (`m2-01-design.md` §6).
///
/// Cursor signing and wire encoding belong to `WP-M2-03`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum TraverseResume {
    /// Order key `(depth, node_id)` for [`TraverseStrategy::Bfs`].
    Bfs {
        /// Minimal hop depth of the last returned node.
        depth: u32,
        /// Identifier of the last returned node.
        node_id: NodeId,
    },
    /// Preorder position `(ordinal, node_id)` for [`TraverseStrategy::Dfs`].
    Dfs {
        /// Zero-based preorder index of the last returned node.
        ordinal: u32,
        /// Identifier of the last returned node.
        node_id: NodeId,
    },
    /// Order key `(depth, node_id)` for [`TraverseStrategy::Shortest`].
    Shortest {
        /// Minimal hop depth of the last returned node.
        depth: u32,
        /// Identifier of the last returned node.
        node_id: NodeId,
    },
    /// Order key `(cost_bits, node_id)` for [`TraverseStrategy::Weighted`].
    Weighted {
        /// IEEE-754 bits of the non-negative finite path cost (`+0.0` normalized).
        cost_bits: u64,
        /// Identifier of the last returned node.
        node_id: NodeId,
    },
}

/// Bounded traversal request (`spec/02-gap-ql.md` `TraverseInput`, `m2-01-design.md` §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseQuery {
    /// Start node identifier.
    pub from: NodeId,
    /// Optional edge-type allow-list (`Some([])` is invalid).
    pub edge_types: Option<Vec<EdgeType>>,
    /// Traversal direction.
    pub direction: Direction,
    /// Maximum hop depth (`1..=16`, `GQL-040`).
    pub max_depth: u32,
    /// Traversal strategy.
    pub strategy: TraverseStrategy,
    /// Optional point-in-time snapshot instant (`None` uses `clock.now_timestamp()`).
    pub as_of: Option<Timestamp>,
    /// Optional scope narrowing filter (`Some([])` is invalid, `GQL-043`).
    pub scopes: Option<Vec<Scope>>,
    /// Page size (`1..=1,000`, `ING-064`).
    pub first: u32,
    /// Optional per-query visited node cap (`1..=1,000,000`; defaults to `100,000`, `ING-064`).
    pub visited_cap: Option<u32>,
    /// Optional stateless resume token from a prior page.
    pub after: Option<TraverseResume>,
}

impl TraverseQuery {
    /// Creates a [`TraverseQuery`] with spec defaults (`Outbound`, `max_depth = 1`, `Bfs`, `first = 100`).
    #[must_use]
    pub fn new(from: NodeId) -> Self {
        Self {
            from,
            edge_types: None,
            direction: Direction::Outbound,
            max_depth: 1,
            strategy: TraverseStrategy::Bfs,
            as_of: None,
            scopes: None,
            first: 100,
            visited_cap: None,
            after: None,
        }
    }
}

/// Visible-only traversal counters (`ING-065`, `spec/02-gap-ql.md` `TraverseStats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseStats {
    /// Distinct visible nodes visited within `visited_cap` (including nodes skipped by `after`).
    pub visited: u32,
    /// Visible nodes returned in this page.
    pub returned: u32,
    /// Maximum hop depth reached among visited visible nodes.
    pub depth_reached: u32,
}

/// Non-fatal traversal warning (`ING-067`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraverseWarning {
    /// Count of candidate edges skipped in `WEIGHTED` mode due to negative, `NaN`, infinite, or
    /// non-numeric `props["weight"]` (`ING-067`).
    SkippedInvalidWeights {
        /// Number of distinct invalid-weight edges skipped.
        count: u32,
    },
}

/// Output of `ContextStore::traverse` (`m2-01-design.md` §6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraverseResult {
    /// Page of visible nodes in strategy order.
    pub nodes: Vec<Node>,
    /// Traversed visible edges whose both endpoints belong to `nodes`, ordered by `(source_depth, edge_id)`.
    pub edges: Vec<Edge>,
    /// Stateless resume key when additional visited nodes follow this page.
    pub resume: Option<TraverseResume>,
    /// `true` iff `visited_cap` stopped exploration while unvisited reachable visible nodes remained.
    pub truncated: bool,
    /// Visible-only traversal statistics.
    pub stats: TraverseStats,
    /// Traversal warnings (e.g. [`TraverseWarning::SkippedInvalidWeights`]).
    pub warnings: Vec<TraverseWarning>,
    /// Resolved `as_of` timestamp (microsecond precision) for cursor pinning (`WP-M2-03`).
    pub as_of: Timestamp,
}

/// Retention policy passed to [`crate::ContextMaintenance::purge_expired`] (`ING-066`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgePolicy {
    /// Minimum age since `valid_to` before tombstoned `Task` nodes are hard-deleted (default `7 d`).
    pub task_retention: Duration,
}

impl Default for PurgePolicy {
    fn default() -> Self {
        Self {
            task_retention: Duration::from_secs(DEFAULT_TASK_RETENTION_SECS),
        }
    }
}

/// Counts-only summary returned by [`crate::ContextMaintenance::purge_expired`] (`ING-066`, `INV-09`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeReport {
    /// Number of expired `Ephemeral` node rows hard-deleted across all tenants.
    pub purged_ephemeral_nodes: u64,
    /// Number of tombstoned `Task` node rows hard-deleted past retention across all tenants.
    pub purged_task_nodes: u64,
    /// Number of incident edge rows hard-deleted alongside purged nodes.
    pub purged_edges: u64,
}

/// Converts a [`Timestamp`] to UTC microseconds (`m2-01-design.md` §4).
#[must_use]
pub const fn timestamp_to_micros(ts: Timestamp) -> i64 {
    ts.unix_micros()
}

/// Converts UTC microseconds back into a [`Timestamp`].
pub fn micros_to_timestamp(micros: i64) -> Result<Timestamp, StoreError> {
    Timestamp::from_unix_nanos(i128::from(micros) * 1_000).map_err(|_| StoreError::Invalid {
        field: "timestamp",
        reason: "timestamp outside representable RFC 3339 range",
    })
}

/// Normalizes a [`Timestamp`] to stored microsecond precision.
pub fn normalize_timestamp(ts: Timestamp) -> Result<Timestamp, StoreError> {
    micros_to_timestamp(timestamp_to_micros(ts))
}

/// Validates a [`Caller`] before store access.
pub fn validate_caller(caller: &Caller) -> Result<(), StoreError> {
    validate_non_control_id(&caller.principal, MAX_PRINCIPAL_BYTES, "caller.principal")
}

/// Validates [`Provenance`] fields (`source` `1..=256` bytes, finite `confidence` in `0.0..=1.0`).
pub fn validate_provenance(prov: &Provenance) -> Result<(), StoreError> {
    validate_non_control_id(&prov.source, MAX_PROVENANCE_SOURCE_BYTES, "provenance.source")?;
    if let Some(conf) = prov.confidence
        && (!conf.is_finite() || !(0.0..=1.0).contains(&conf))
    {
        return Err(StoreError::Invalid {
            field: "provenance.confidence",
            reason: "must be finite and in 0.0..=1.0",
        });
    }
    Ok(())
}

/// Computes the maximum container nesting depth of a JSON value iteratively (root container = `1`).
#[must_use]
pub fn json_container_depth(root: &JsonValue) -> usize {
    match root {
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => 0,
        JsonValue::Array(_) | JsonValue::Object(_) => {
            let mut max_depth = 1usize;
            let mut stack: Vec<(&JsonValue, usize)> = vec![(root, 1)];
            while let Some((current, depth)) = stack.pop() {
                if depth > max_depth {
                    max_depth = depth;
                }
                match current {
                    JsonValue::Array(items) => {
                        for item in items {
                            if matches!(item, JsonValue::Array(_) | JsonValue::Object(_)) {
                                stack.push((item, depth + 1));
                            }
                        }
                    }
                    JsonValue::Object(map) => {
                        for val in map.values() {
                            if matches!(val, JsonValue::Array(_) | JsonValue::Object(_)) {
                                stack.push((val, depth + 1));
                            }
                        }
                    }
                    _ => {}
                }
            }
            max_depth
        }
    }
}

enum CanonWork<'a> {
    Value(&'a JsonValue),
    Raw(&'static [u8]),
    Key(&'a str),
}

/// Appends canonical sorted-key JSON bytes for `value` into `out` iteratively (immune to Cargo
/// `serde_json/preserve_order` unification and normalizing `-0.0` to `0.0`).
pub fn append_canonical_json(value: &JsonValue, out: &mut Vec<u8>) -> Result<(), StoreError> {
    let mut stack = vec![CanonWork::Value(value)];
    while let Some(task) = stack.pop() {
        match task {
            CanonWork::Raw(bytes) => out.extend_from_slice(bytes),
            CanonWork::Key(key) => {
                serde_json::to_writer(&mut *out, key).map_err(|_| StoreError::Backend)?;
            }
            CanonWork::Value(val) => match val {
                JsonValue::Null => out.extend_from_slice(b"null"),
                JsonValue::Bool(true) => out.extend_from_slice(b"true"),
                JsonValue::Bool(false) => out.extend_from_slice(b"false"),
                JsonValue::Number(num) => {
                    if !num.is_i64() && !num.is_u64() && num.as_f64() == Some(0.0) {
                        out.extend_from_slice(b"0.0");
                    } else {
                        serde_json::to_writer(&mut *out, num).map_err(|_| StoreError::Backend)?;
                    }
                }
                JsonValue::String(s) => {
                    serde_json::to_writer(&mut *out, s).map_err(|_| StoreError::Backend)?;
                }
                JsonValue::Array(items) => {
                    stack.push(CanonWork::Raw(b"]"));
                    for (idx, item) in items.iter().enumerate().rev() {
                        stack.push(CanonWork::Value(item));
                        if idx > 0 {
                            stack.push(CanonWork::Raw(b","));
                        }
                    }
                    stack.push(CanonWork::Raw(b"["));
                }
                JsonValue::Object(map) => {
                    let mut entries: Vec<(&str, &JsonValue)> =
                        map.iter().map(|(k, v)| (k.as_str(), v)).collect();
                    entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
                    stack.push(CanonWork::Raw(b"}"));
                    for (idx, (k, v)) in entries.into_iter().enumerate().rev() {
                        stack.push(CanonWork::Value(v));
                        stack.push(CanonWork::Raw(b":"));
                        stack.push(CanonWork::Key(k));
                        if idx > 0 {
                            stack.push(CanonWork::Raw(b","));
                        }
                    }
                    stack.push(CanonWork::Raw(b"{"));
                }
            },
        }
    }
    Ok(())
}

/// Validates `props` nesting depth (`<= 16`) and canonical UTF-8 byte length (`<= 65,536`),
/// returning the canonical serialized JSON string.
pub fn validate_and_serialize_props(props: &JsonMap<String, JsonValue>) -> Result<String, StoreError> {
    let wrapped = JsonValue::Object(props.clone());
    if json_container_depth(&wrapped) > MAX_PROPS_DEPTH {
        return Err(StoreError::Invalid {
            field: "props",
            reason: "JSON nesting depth exceeds 16",
        });
    }
    let mut bytes = Vec::with_capacity(64);
    append_canonical_json(&wrapped, &mut bytes)?;
    if bytes.len() > MAX_PROPS_BYTES {
        return Err(StoreError::Invalid {
            field: "props",
            reason: "serialized JSON exceeds 65536 bytes",
        });
    }
    String::from_utf8(bytes).map_err(|_| StoreError::Backend)
}

/// Normalized DB representation of a validated [`ScopeSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedScope {
    /// Reported top-level scope.
    pub scope: Scope,
    /// Underlying non-ephemeral scope stored in `nodes.base_scope`.
    pub base_scope: Scope,
    /// Task ID if `Task` or `Ephemeral(Task)`.
    pub task_id: Option<TaskId>,
    /// Group claim if `Group` or `Ephemeral(Group)`.
    pub group: Option<String>,
    /// Expiration microsecond timestamp if `Ephemeral`.
    pub expires_at_us: Option<i64>,
}

fn validate_group_claim(group: &str) -> Result<(), StoreError> {
    validate_non_control_id(group, MAX_GROUP_CLAIM_BYTES, "group")
}

/// Validates a [`ScopeSpec`] against `now_us` (`ING-065`, `ING-066`).
pub fn resolve_scope_spec(spec: &ScopeSpec, now_us: i64) -> Result<ResolvedScope, StoreError> {
    match spec {
        ScopeSpec::Private => Ok(ResolvedScope {
            scope: Scope::Private,
            base_scope: Scope::Private,
            task_id: None,
            group: None,
            expires_at_us: None,
        }),
        ScopeSpec::Task { task_id } => Ok(ResolvedScope {
            scope: Scope::Task,
            base_scope: Scope::Task,
            task_id: Some(*task_id),
            group: None,
            expires_at_us: None,
        }),
        ScopeSpec::Group { group } => {
            validate_group_claim(group)?;
            Ok(ResolvedScope {
                scope: Scope::Group,
                base_scope: Scope::Group,
                task_id: None,
                group: Some(group.clone()),
                expires_at_us: None,
            })
        }
        ScopeSpec::Global => Ok(ResolvedScope {
            scope: Scope::Global,
            base_scope: Scope::Global,
            task_id: None,
            group: None,
            expires_at_us: None,
        }),
        ScopeSpec::Ephemeral { base, expires_at } => {
            let expires_at_us = match expires_at {
                None => now_us
                    .checked_add(DEFAULT_EPHEMERAL_TTL_US)
                    .ok_or(StoreError::Invalid {
                        field: "expires_at",
                        reason: "timestamp overflow",
                    })?,
                Some(ts) => {
                    let exp_us = timestamp_to_micros(*ts);
                    let max_exp_us = now_us.saturating_add(MAX_EPHEMERAL_TTL_US);
                    if exp_us <= now_us {
                        return Err(StoreError::Invalid {
                            field: "expires_at",
                            reason: "ephemeral expires_at must be strictly after now",
                        });
                    }
                    if exp_us > max_exp_us {
                        return Err(StoreError::Invalid {
                            field: "expires_at",
                            reason: "ephemeral expires_at exceeds now + 24h",
                        });
                    }
                    exp_us
                }
            };
            let _ = micros_to_timestamp(expires_at_us)?;
            match base {
                BaseScope::Private => Ok(ResolvedScope {
                    scope: Scope::Ephemeral,
                    base_scope: Scope::Private,
                    task_id: None,
                    group: None,
                    expires_at_us: Some(expires_at_us),
                }),
                BaseScope::Task { task_id } => Ok(ResolvedScope {
                    scope: Scope::Ephemeral,
                    base_scope: Scope::Task,
                    task_id: Some(*task_id),
                    group: None,
                    expires_at_us: Some(expires_at_us),
                }),
                BaseScope::Group { group } => {
                    validate_group_claim(group)?;
                    Ok(ResolvedScope {
                        scope: Scope::Ephemeral,
                        base_scope: Scope::Group,
                        task_id: None,
                        group: Some(group.clone()),
                        expires_at_us: Some(expires_at_us),
                    })
                }
                BaseScope::Global => Ok(ResolvedScope {
                    scope: Scope::Ephemeral,
                    base_scope: Scope::Global,
                    task_id: None,
                    group: None,
                    expires_at_us: Some(expires_at_us),
                }),
            }
        }
    }
}

/// Validates a [`NewNode`] write payload and returns its canonical `props` JSON and [`ResolvedScope`].
pub fn validate_new_node(
    node: &NewNode,
    now_us: i64,
) -> Result<(String, ResolvedScope), StoreError> {
    if node.label.len() > MAX_LABEL_BYTES {
        return Err(StoreError::Invalid {
            field: "label",
            reason: "exceeds 512 bytes",
        });
    }
    if let Some(summary) = &node.summary
        && summary.len() > MAX_SUMMARY_BYTES
    {
        return Err(StoreError::Invalid {
            field: "summary",
            reason: "exceeds 1024 bytes",
        });
    }
    validate_provenance(&node.provenance)?;
    let props_json = validate_and_serialize_props(&node.props)?;
    let resolved_scope = resolve_scope_spec(&node.scope, now_us)?;
    Ok((props_json, resolved_scope))
}

/// Validates a [`NewEdge`] write payload and returns its canonical `props` JSON and normalized confidence.
pub fn validate_new_edge(edge: &NewEdge) -> Result<(String, Option<f32>), StoreError> {
    let normalized_conf = match edge.confidence {
        None => None,
        Some(c) => {
            if !c.is_finite() || !(0.0..=1.0).contains(&c) {
                return Err(StoreError::Invalid {
                    field: "confidence",
                    reason: "must be finite and in 0.0..=1.0",
                });
            }
            Some(if c == 0.0 { 0.0 } else { c })
        }
    };
    validate_provenance(&edge.provenance)?;
    let props_json = validate_and_serialize_props(&edge.props)?;
    Ok((props_json, normalized_conf))
}

/// Validates [`TraverseQuery`] limits and returns the resolved `visited_cap` (`ING-064`, `GQL-040`).
pub fn validate_traverse_query(query: &TraverseQuery) -> Result<u32, StoreError> {
    if !(MIN_TRAVERSE_DEPTH..=MAX_TRAVERSE_DEPTH).contains(&query.max_depth) {
        return Err(StoreError::Invalid {
            field: "max_depth",
            reason: "must be in 1..=16",
        });
    }
    if !(MIN_TRAVERSE_FIRST..=MAX_TRAVERSE_FIRST).contains(&query.first) {
        return Err(StoreError::Invalid {
            field: "first",
            reason: "must be in 1..=1000",
        });
    }
    let visited_cap = match query.visited_cap {
        None => DEFAULT_VISITED_CAP,
        Some(cap) if (1..=MAX_VISITED_CAP).contains(&cap) => cap,
        Some(_) => {
            return Err(StoreError::Invalid {
                field: "visited_cap",
                reason: "must be in 1..=1000000",
            });
        }
    };
    if let Some(edge_types) = &query.edge_types
        && edge_types.is_empty()
    {
        return Err(StoreError::Invalid {
            field: "edge_types",
            reason: "edge_types filter must not be empty when present",
        });
    }
    if let Some(scopes) = &query.scopes
        && scopes.is_empty()
    {
        return Err(StoreError::Invalid {
            field: "scopes",
            reason: "scopes filter must not be empty when present",
        });
    }
    if let Some(resume) = &query.after {
        let matches_strategy = matches!(
            (query.strategy, resume),
            (TraverseStrategy::Bfs, TraverseResume::Bfs { .. })
                | (TraverseStrategy::Dfs, TraverseResume::Dfs { .. })
                | (TraverseStrategy::Shortest, TraverseResume::Shortest { .. })
                | (TraverseStrategy::Weighted, TraverseResume::Weighted { .. })
        );
        if !matches_strategy {
            return Err(StoreError::Invalid {
                field: "after",
                reason: "resume token strategy does not match query strategy",
            });
        }
        if let TraverseResume::Weighted { cost_bits, .. } = resume {
            let cost = f64::from_bits(*cost_bits);
            if !cost.is_finite() || cost < 0.0 || cost.is_sign_negative() {
                return Err(StoreError::Invalid {
                    field: "after",
                    reason: "weighted resume cost_bits must represent a finite non-negative float",
                });
            }
        }
    }
    Ok(visited_cap)
}

/// Validates `search_nodes` `limit` (`1..=100`).
pub fn validate_search_limit(limit: u32) -> Result<(), StoreError> {
    if !(MIN_SEARCH_LIMIT..=MAX_SEARCH_LIMIT).contains(&limit) {
        return Err(StoreError::Invalid {
            field: "limit",
            reason: "must be in 1..=100",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use gap_core::clock::Timestamp;
    use gap_core::ids::TaskId;
    use serde_json::{Map as JsonMap, Value as JsonValue, json};

    use super::*;

    fn build_nested_props(total_depth: usize) -> JsonMap<String, JsonValue> {
        assert!(total_depth >= 1);
        let mut current = JsonValue::String("leaf".to_owned());
        for _ in 0..total_depth {
            let mut inner = JsonMap::new();
            inner.insert("n".to_owned(), current);
            current = JsonValue::Object(inner);
        }
        match current {
            JsonValue::Object(map) => map,
            _ => unreachable!("loop runs at least once"),
        }
    }

    // spec: ING-064, ING-065
    #[test]
    fn node_and_edge_id_boundaries_and_hostile_characters() {
        assert!(NodeId::new("").is_err());
        assert!(EdgeId::new("").is_err());

        let one = NodeId::new("x").expect("1-byte node id");
        assert_eq!(one.as_str(), "x");

        let id_512 = "n".repeat(512);
        assert_eq!(id_512.len(), 512);
        assert!(NodeId::new(id_512.clone()).is_ok());
        assert!(EdgeId::new(id_512).is_ok());

        let id_513 = "n".repeat(513);
        assert_eq!(id_513.len(), 513);
        assert!(NodeId::new(id_513.clone()).is_err());
        assert!(EdgeId::new(id_513).is_err());

        let sql_looking = "node'; DROP TABLE nodes;--";
        assert_eq!(
            NodeId::new(sql_looking).expect("SQL-looking id allowed").as_str(),
            sql_looking
        );

        for hostile in [
            "bad\0nul",
            "\0",
            "line\nbreak",
            "carriage\rreturn",
            "tab\there",
            "bell\x07char",
            "del\x7fchar",
            "c1\u{0085}control",
        ] {
            assert!(
                NodeId::new(hostile).is_err(),
                "expected NodeId `{hostile:?}` to be rejected"
            );
            assert!(
                EdgeId::new(hostile).is_err(),
                "expected EdgeId `{hostile:?}` to be rejected"
            );
        }
    }

    // spec: ING-060, ING-064
    #[test]
    fn node_and_edge_type_boundaries_and_lexical_rules() {
        assert!(NodeType::new("").is_err());
        assert!(EdgeType::new("").is_err());

        assert!(NodeType::new("R").is_ok());
        assert!(EdgeType::new("C").is_ok());

        let valid_64 = format!("A{}", "b9_".repeat(21));
        assert_eq!(valid_64.len(), 64);
        assert!(NodeType::new(valid_64.clone()).is_ok());
        assert!(EdgeType::new(valid_64).is_ok());

        let invalid_65 = format!("A{}", "b".repeat(64));
        assert_eq!(invalid_65.len(), 65);
        assert!(NodeType::new(invalid_65.clone()).is_err());
        assert!(EdgeType::new(invalid_65).is_err());

        for bad in [
            "_LeadingUnderscore",
            "0LeadingDigit",
            "has-hyphen",
            "has.dot",
            "has space",
            "Type'; DROP TABLE nodes;--",
            "Type\0Nul",
            "NonAsciiÄ",
        ] {
            assert!(NodeType::new(bad).is_err(), "expected `{bad}` to fail");
            assert!(EdgeType::new(bad).is_err(), "expected `{bad}` to fail");
        }
    }

    // spec: ING-064, ING-065, ING-066
    #[test]
    fn write_field_limits_exact_boundary_and_one_past() {
        let now_us: i64 = 1_700_000_000_000_000;
        let base_node = NewNode {
            id: Some(NodeId::new("file:lib.rs").expect("valid id")),
            node_type: NodeType::new("File").expect("valid type"),
            label: "a".repeat(512),
            summary: Some("s".repeat(1024)),
            props: JsonMap::new(),
            scope: ScopeSpec::Global,
            provenance: Provenance {
                source: "p".repeat(256),
                desc_hash: None,
                confidence: Some(0.0),
                extracted_at: None,
                agent: None,
            },
        };
        assert!(validate_new_node(&base_node, now_us).is_ok());

        let mut label_513 = base_node.clone();
        label_513.label = "a".repeat(513);
        assert!(matches!(
            validate_new_node(&label_513, now_us),
            Err(StoreError::Invalid { field: "label", .. })
        ));

        let mut summary_1025 = base_node.clone();
        summary_1025.summary = Some("s".repeat(1025));
        assert!(matches!(
            validate_new_node(&summary_1025, now_us),
            Err(StoreError::Invalid {
                field: "summary",
                ..
            })
        ));

        let mut prov_0 = base_node.clone();
        prov_0.provenance.source = String::new();
        assert!(matches!(
            validate_new_node(&prov_0, now_us),
            Err(StoreError::Invalid {
                field: "provenance.source",
                ..
            })
        ));

        let mut prov_257 = base_node.clone();
        prov_257.provenance.source = "p".repeat(257);
        assert!(matches!(
            validate_new_node(&prov_257, now_us),
            Err(StoreError::Invalid {
                field: "provenance.source",
                ..
            })
        ));

        for valid_conf in [0.0_f32, 0.5_f32, 1.0_f32] {
            let mut n = base_node.clone();
            n.provenance.confidence = Some(valid_conf);
            assert!(validate_new_node(&n, now_us).is_ok());
        }
        for bad_conf in [-0.0001_f32, 1.0001_f32, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut n = base_node.clone();
            n.provenance.confidence = Some(bad_conf);
            assert!(matches!(
                validate_new_node(&n, now_us),
                Err(StoreError::Invalid {
                    field: "provenance.confidence",
                    ..
                })
            ));
        }

        // Group claim: 0 invalid, 1 valid, 128 valid, 129 invalid.
        assert!(resolve_scope_spec(&ScopeSpec::Group { group: String::new() }, now_us).is_err());
        assert!(
            resolve_scope_spec(
                &ScopeSpec::Group {
                    group: "g".to_owned()
                },
                now_us
            )
            .is_ok()
        );
        assert!(
            resolve_scope_spec(
                &ScopeSpec::Group {
                    group: "g".repeat(128)
                },
                now_us
            )
            .is_ok()
        );
        assert!(
            resolve_scope_spec(
                &ScopeSpec::Group {
                    group: "g".repeat(129)
                },
                now_us
            )
            .is_err()
        );

        // Ephemeral TTL: None -> now + 1h; now -> invalid; now + 1us -> valid; now + 24h -> valid; now + 24h + 1us -> invalid.
        let default_eph = resolve_scope_spec(&ScopeSpec::ephemeral_default(BaseScope::Private), now_us)
            .expect("default ephemeral");
        assert_eq!(default_eph.expires_at_us, Some(now_us + 3_600_000_000));

        let at_now = micros_to_timestamp(now_us).expect("ts");
        assert!(resolve_scope_spec(&ScopeSpec::ephemeral(BaseScope::Global, at_now), now_us).is_err());

        let plus_1us = micros_to_timestamp(now_us + 1).expect("ts");
        assert!(
            resolve_scope_spec(&ScopeSpec::ephemeral(BaseScope::Global, plus_1us), now_us).is_ok()
        );

        let plus_24h = micros_to_timestamp(now_us + 86_400_000_000).expect("ts");
        assert!(
            resolve_scope_spec(&ScopeSpec::ephemeral(BaseScope::Global, plus_24h), now_us).is_ok()
        );

        let plus_24h_1us = micros_to_timestamp(now_us + 86_400_000_001).expect("ts");
        assert!(
            resolve_scope_spec(
                &ScopeSpec::ephemeral(BaseScope::Global, plus_24h_1us),
                now_us
            )
            .is_err()
        );
    }

    // spec: ING-064
    #[test]
    fn props_size_64kib_and_depth_16_boundaries() {
        let depth_16 = build_nested_props(16);
        assert_eq!(json_container_depth(&JsonValue::Object(depth_16.clone())), 16);
        assert!(validate_and_serialize_props(&depth_16).is_ok());

        let depth_17 = build_nested_props(17);
        assert_eq!(json_container_depth(&JsonValue::Object(depth_17.clone())), 17);
        assert!(matches!(
            validate_and_serialize_props(&depth_17),
            Err(StoreError::Invalid { field: "props", .. })
        ));

        // {"k":"..."} overhead is 8 bytes: `{"k":"` (6) + `"}` (2) = 8 bytes -> 65528 'x' chars == 65536 bytes.
        let mut exact_64k = JsonMap::new();
        exact_64k.insert("k".to_owned(), JsonValue::String("x".repeat(65_528)));
        let serialized = validate_and_serialize_props(&exact_64k).expect("exact 65536 bytes valid");
        assert_eq!(serialized.len(), 65_536);

        let mut over_64k = JsonMap::new();
        over_64k.insert("k".to_owned(), JsonValue::String("x".repeat(65_529)));
        assert!(matches!(
            validate_and_serialize_props(&over_64k),
            Err(StoreError::Invalid { field: "props", .. })
        ));

        // Canonical JSON key ordering and -0.0 normalization.
        let mut m1 = JsonMap::new();
        m1.insert("z".to_owned(), json!(-0.0));
        m1.insert("a".to_owned(), json!({"b": 2, "a": 1}));
        let mut m2 = JsonMap::new();
        m2.insert("a".to_owned(), json!({"a": 1, "b": 2}));
        m2.insert("z".to_owned(), json!(0.0));
        assert_eq!(
            validate_and_serialize_props(&m1).expect("m1"),
            validate_and_serialize_props(&m2).expect("m2")
        );
    }

    // spec: GQL-040, GQL-043, ING-064, ING-067
    #[test]
    fn traverse_and_search_query_limit_boundaries() {
        let start = NodeId::new("start").expect("id");
        let base = TraverseQuery::new(start.clone());
        assert_eq!(validate_traverse_query(&base).expect("default cap"), 100_000);

        let mut d0 = base.clone();
        d0.max_depth = 0;
        assert!(validate_traverse_query(&d0).is_err());

        let mut d1 = base.clone();
        d1.max_depth = 1;
        assert!(validate_traverse_query(&d1).is_ok());

        let mut d16 = base.clone();
        d16.max_depth = 16;
        assert!(validate_traverse_query(&d16).is_ok());

        let mut d17 = base.clone();
        d17.max_depth = 17;
        assert!(validate_traverse_query(&d17).is_err());

        let mut f0 = base.clone();
        f0.first = 0;
        assert!(validate_traverse_query(&f0).is_err());

        let mut f1 = base.clone();
        f1.first = 1;
        assert!(validate_traverse_query(&f1).is_ok());

        let mut f1000 = base.clone();
        f1000.first = 1_000;
        assert!(validate_traverse_query(&f1000).is_ok());

        let mut f1001 = base.clone();
        f1001.first = 1_001;
        assert!(validate_traverse_query(&f1001).is_err());

        let mut c0 = base.clone();
        c0.visited_cap = Some(0);
        assert!(validate_traverse_query(&c0).is_err());

        let mut c1 = base.clone();
        c1.visited_cap = Some(1);
        assert_eq!(validate_traverse_query(&c1).expect("cap 1"), 1);

        let mut c1m = base.clone();
        c1m.visited_cap = Some(1_000_000);
        assert_eq!(validate_traverse_query(&c1m).expect("cap 1M"), 1_000_000);

        let mut c1m_plus_1 = base.clone();
        c1m_plus_1.visited_cap = Some(1_000_001);
        assert!(validate_traverse_query(&c1m_plus_1).is_err());

        let mut empty_edges = base.clone();
        empty_edges.edge_types = Some(Vec::new());
        assert!(validate_traverse_query(&empty_edges).is_err());

        let mut empty_scopes = base.clone();
        empty_scopes.scopes = Some(Vec::new());
        assert!(validate_traverse_query(&empty_scopes).is_err());

        let mut wrong_resume = base.clone();
        wrong_resume.strategy = TraverseStrategy::Bfs;
        wrong_resume.after = Some(TraverseResume::Dfs {
            ordinal: 0,
            node_id: start.clone(),
        });
        assert!(validate_traverse_query(&wrong_resume).is_err());

        let mut nan_resume = base;
        nan_resume.strategy = TraverseStrategy::Weighted;
        nan_resume.after = Some(TraverseResume::Weighted {
            cost_bits: f64::NAN.to_bits(),
            node_id: start,
        });
        assert!(validate_traverse_query(&nan_resume).is_err());

        assert!(validate_search_limit(0).is_err());
        assert!(validate_search_limit(1).is_ok());
        assert!(validate_search_limit(100).is_ok());
        assert!(validate_search_limit(101).is_err());

        let edge_bad_conf = NewEdge {
            id: None,
            edge_type: EdgeType::new("CALLS").expect("type"),
            from: NodeId::new("a").expect("id"),
            to: NodeId::new("b").expect("id"),
            props: JsonMap::new(),
            confidence: Some(1.01),
            resolution: EdgeResolution::Heuristic,
            provenance: Provenance::new("test"),
        };
        assert!(validate_new_edge(&edge_bad_conf).is_err());
        let _ = TaskId::from_unix_ms(1_700_000_000_000, [1u8; 10]);
        let _ = Timestamp::UNIX_EPOCH;
    }
}
