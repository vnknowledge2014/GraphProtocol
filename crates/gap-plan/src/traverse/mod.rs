//! Graph traversal and point-lookup query planning (`WP-M2-03`, `GQL-010`, `GQL-040`, `GQL-041`, `GQL-043`).
//!
//! Validates `gap.traverse(input: TraverseInput!)` and `gap.node(ref: NodeRef!)` arguments,
//! enforces `@oneOf` semantics on `NodeRef` and `Predicate`, rejects `maxDepth > 16`
//! (`GAP_DEPTH_EXCEEDED`), `first > 1000` or cost overruns (`GAP_COMPLEXITY_EXCEEDED`), and
//! empty `scopes` (`GAP_VALIDATION_ERROR`, `GQL-043`).

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use gap_core::clock::Timestamp;
use gap_core::error::{ErrorCode, GapError, GapResult};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::ir::{ArgValue, SelectionField, TraverseNode};

/// Hard ceiling for `TraverseInput.maxDepth` (`GQL-040`, `ING-064`).
pub const MAX_TRAVERSE_DEPTH: u32 = 16;

/// Default page size when `TraverseInput.first` is omitted (`spec/02-gap-ql.md`).
pub const DEFAULT_TRAVERSE_FIRST: u32 = 100;

/// Maximum page size allowed on `TraverseInput.first` (`GQL-040`, `ING-064`).
pub const MAX_TRAVERSE_FIRST: u32 = 1_000;

/// Default planner branching factor used in `visited_bound` cost estimation (`GQL-041`).
pub const DEFAULT_BRANCHING_CAP: u64 = 8;

/// Default `visited_cap` applied at execution time (`ING-064`).
pub const DEFAULT_VISITED_CAP: u32 = 100_000;

/// Hard maximum `visited_cap` accepted by the store (`ING-087`).
pub const MAX_VISITED_CAP: u32 = 1_000_000;

/// Maximum recursion depth allowed for nested [`PlannedPredicate`] expressions.
pub const MAX_PREDICATE_DEPTH: usize = 32;

/// Maximum length allowed for [`ArgValue`] recursive resolution.
const MAX_ARG_DEPTH: usize = 32;

/// Maximum number of `returnFields` entries allowed on a single traversal (`ING-063`).
pub const MAX_RETURN_FIELDS: usize = 256;

/// Maximum number of clauses allowed in `and` / `or` / `in` lists.
const MAX_PREDICATE_LIST_LEN: usize = 256;

/// Planner limits applied to traversal cost estimation (`GQL-040`, `GQL-041`, `ING-064`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraversePlanLimits {
    /// Planner branching cap (`GQL-041`, default `8`).
    pub branching_cap: u64,
    /// Per-query visited node cap passed to execution (`ING-064`, default `100_000`).
    pub visited_cap: u32,
    /// Maximum tolerated traversal cost under `GQL-041` (`None` disables cost ceiling check).
    pub max_cost: Option<u64>,
    /// Multiplier applied for child fields under `Subgraph` (`GQL-041`, default `1`).
    pub selection_weight: u64,
}

impl Default for TraversePlanLimits {
    fn default() -> Self {
        Self {
            branching_cap: DEFAULT_BRANCHING_CAP,
            visited_cap: DEFAULT_VISITED_CAP,
            max_cost: None,
            selection_weight: 1,
        }
    }
}

/// Validated `@oneOf` target reference (`spec/02-gap-ql.md` `NodeRef`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannedNodeRef {
    /// Direct lookup by `GapNode.id`.
    Id(String),
    /// Lookup by `(type, key)` pair (`spec/02-gap-ql.md` `NodeKey`).
    Key {
        /// Expected `GapNode.type` (validated type identifier).
        node_type: String,
        /// Structured JSON lookup key.
        value: JsonValue,
    },
}

/// Traversal direction (`spec/02-gap-ql.md` `Direction`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlannedDirection {
    /// Follow edges from `from` to `to`.
    #[default]
    Outbound,
    /// Follow edges in reverse from `to` to `from`.
    Inbound,
    /// Follow edges in either direction.
    Both,
}

/// Bounded graph walk strategy (`spec/02-gap-ql.md` `TraverseStrategy`, `ING-067`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlannedTraverseStrategy {
    /// Breadth-first search ordered by `(depth, id)`.
    #[default]
    Bfs,
    /// Preorder depth-first search ordered by `(id, edge_id)`.
    Dfs,
    /// BFS shortest-path tree (`ING-084`).
    Shortest,
    /// Hop-bounded Dijkstra over `props["weight"]` (`ING-067`).
    Weighted,
}

/// Context visibility scope enum (`spec/02-gap-ql.md` `ContextScope`, `ING-065`, `GQL-043`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlannedContextScope {
    /// Visible only to `owner_principal`.
    Private,
    /// Visible along the A2A `task_chain`.
    Task,
    /// Visible to members of `group`.
    Group,
    /// Visible to all principals in the tenant.
    Global,
    /// Time-bounded scope (`ING-066`).
    Ephemeral,
}

/// Single scalar comparison operand (`spec/02-gap-ql.md` `Compare`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedCompare {
    /// Dotted property path rooted at `props` (or top-level scalar field when explicitly supported).
    pub path: String,
    /// Reference comparison value.
    pub value: JsonValue,
}

/// Membership comparison operand (`spec/02-gap-ql.md` `CompareList`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedCompareList {
    /// Dotted property path.
    pub path: String,
    /// Candidate values matched by `IN`.
    pub values: Vec<JsonValue>,
}

/// Validated `@oneOf` recursive boolean predicate (`spec/02-gap-ql.md` `Predicate`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlannedPredicate {
    /// Logical conjunction (`and`).
    And(Vec<PlannedPredicate>),
    /// Logical disjunction (`or`).
    Or(Vec<PlannedPredicate>),
    /// Logical negation (`not`).
    Not(Box<PlannedPredicate>),
    /// Equality (`eq`).
    Eq(PlannedCompare),
    /// Inequality (`ne`).
    Ne(PlannedCompare),
    /// Strictly greater than (`gt`).
    Gt(PlannedCompare),
    /// Greater than or equal (`gte`).
    Gte(PlannedCompare),
    /// Strictly less than (`lt`).
    Lt(PlannedCompare),
    /// Less than or equal (`lte`).
    Lte(PlannedCompare),
    /// Set membership (`in`).
    In(PlannedCompareList),
    /// Substring / array containment (`contains`).
    Contains(PlannedCompare),
    /// String prefix match (`startsWith`).
    StartsWith(PlannedCompare),
    /// Path existence check (`exists`).
    Exists(String),
}

/// Validated edge filter (`spec/02-gap-ql.md` `EdgeFilter`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedEdgeFilter {
    /// Optional non-empty allow-list of edge types (`None` means all types).
    pub types: Option<Vec<String>>,
    /// Optional predicate evaluated against each candidate edge.
    pub where_edge: Option<PlannedPredicate>,
}

/// Planner cost breakdown for a validated `traverse` operation (`GQL-041`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraverseCostEstimate {
    /// Uncapped geometric bound `Σ_{d=1..maxDepth} min(first, branching_cap)^d` (`GQL-041`).
    pub visited_bound: u64,
    /// Cost contribution truncated by `first` (`min(visited_bound, first)` or page-bounded cost).
    pub page_bounded_visited: u64,
    /// Combined GraphQL field cost estimate `w + m * sum(child)` (`GQL-041`).
    pub total_cost: u64,
}

/// Fully validated, variable-resolved traversal plan (`WP-M2-03`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTraverse {
    /// Target response key on parent object.
    pub response_key: String,
    /// Start node reference (`NodeRef`).
    pub from: PlannedNodeRef,
    /// Optional edge type & edge predicate filter (`through`).
    pub through: Option<PlannedEdgeFilter>,
    /// Traversal direction.
    pub direction: PlannedDirection,
    /// Validated depth bound (`1..=16`, `GQL-040`).
    pub max_depth: u32,
    /// Traversal ordering strategy.
    pub strategy: PlannedTraverseStrategy,
    /// Optional caller-supplied `asOf` snapshot instant (may also come from verified cursor).
    pub as_of: Option<Timestamp>,
    /// Optional predicate evaluated against target `GapNode`.
    pub where_node: Option<PlannedPredicate>,
    /// Optional scope narrowing filter (`Some([])` rejected per `GQL-043`).
    pub scopes: Option<Vec<PlannedContextScope>>,
    /// Validated page limit (`1..=1000`, `GQL-040`).
    pub first: u32,
    /// Optional opaque signed cursor (`RUN-025`, `ING-085`).
    pub after: Option<String>,
    /// Optional `returnFields` prop slice projection (`ING-063`).
    pub return_fields: Option<Vec<String>>,
    /// Execution visited node cap (`1..=MAX_VISITED_CAP`).
    pub visited_cap: u32,
    /// Deterministic `GQL-041` cost estimate.
    pub cost: TraverseCostEstimate,
    /// Canonical `blake3-256` digest of normalized traversal parameters (for cursor binding).
    pub schema_hash: String,
}

/// Validated `node(ref: NodeRef!)` lookup plan (`spec/02-gap-ql.md`, `GQL-010`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedNodeLookup {
    /// Target `NodeRef`.
    pub reference: PlannedNodeRef,
    /// Optional `asOf` point-in-time snapshot timestamp.
    pub as_of: Option<Timestamp>,
    /// Optional prop slice projection.
    pub return_fields: Option<Vec<String>>,
}

/// Recursively resolves an [`ArgValue`] against caller-supplied variables (`depth <= 32`).
pub fn resolve_arg_value(
    value: &ArgValue,
    variables: &BTreeMap<String, JsonValue>,
) -> GapResult<JsonValue> {
    resolve_arg_value_inner(value, variables, 0)
}

fn resolve_arg_value_inner(
    value: &ArgValue,
    variables: &BTreeMap<String, JsonValue>,
    depth: usize,
) -> GapResult<JsonValue> {
    if depth > MAX_ARG_DEPTH {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "argument nesting depth exceeded maximum",
        ));
    }
    match value {
        ArgValue::Literal(v) => Ok(v.clone()),
        ArgValue::Variable(name) => Ok(variables.get(name).cloned().unwrap_or(JsonValue::Null)),
        ArgValue::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(resolve_arg_value_inner(item, variables, depth + 1)?);
            }
            Ok(JsonValue::Array(out))
        }
        ArgValue::Object(map) => {
            let mut out = JsonMap::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), resolve_arg_value_inner(v, variables, depth + 1)?);
            }
            Ok(JsonValue::Object(out))
        }
    }
}

/// Computes `GQL-041` `visited_bound = Σ_{d=1..max_depth} min(first, branching_cap)^d`.
#[must_use]
pub fn compute_traverse_visited_bound(max_depth: u32, first: u32, branching_cap: u64) -> u64 {
    if max_depth == 0 || first == 0 || branching_cap == 0 {
        return 0;
    }
    let step = u64::from(first).min(branching_cap);
    let mut sum: u64 = 0;
    let mut power: u64 = 1;
    for _ in 0..max_depth {
        power = power.saturating_mul(step);
        sum = sum.saturating_add(power);
    }
    sum
}

/// Validates and plans a [`TraverseNode`] IR node with variable substitution (`GQL-040`, `GQL-041`, `GQL-043`).
pub fn validate_and_plan_traverse_node(
    node: &TraverseNode,
    variables: &BTreeMap<String, JsonValue>,
    limits: TraversePlanLimits,
) -> GapResult<PlannedTraverse> {
    validate_and_plan_traverse(
        &node.response_key,
        &node.arguments,
        &node.selection,
        variables,
        limits,
    )
}

/// Validates and plans `gap.traverse(input: TraverseInput!)` (also accepting flattened `TraverseInput`
/// arguments for internal plan callers).
pub fn validate_and_plan_traverse(
    response_key: &str,
    arguments: &BTreeMap<String, ArgValue>,
    selection: &[SelectionField],
    variables: &BTreeMap<String, JsonValue>,
    limits: TraversePlanLimits,
) -> GapResult<PlannedTraverse> {
    let input_obj = extract_traverse_input_object(arguments, variables)?;

    for key in input_obj.keys() {
        match key.as_str() {
            "from" | "through" | "direction" | "maxDepth" | "strategy" | "asOf" | "where"
            | "scopes" | "first" | "after" | "returnFields" => {}
            other => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unknown TraverseInput field `{other}`"),
                ));
            }
        }
    }

    let from_val = input_obj.get("from").ok_or_else(|| {
        GapError::new(
            ErrorCode::ValidationError,
            "missing required `TraverseInput.from` argument",
        )
    })?;
    let from = parse_node_ref(from_val, "from")?;

    let through = match input_obj.get("through") {
        None | Some(JsonValue::Null) => None,
        Some(val) => Some(parse_edge_filter(val)?),
    };

    let direction = match input_obj.get("direction") {
        None | Some(JsonValue::Null) => PlannedDirection::Outbound,
        Some(val) => parse_direction(val)?,
    };

    let max_depth_val = input_obj.get("maxDepth").ok_or_else(|| {
        GapError::new(
            ErrorCode::ValidationError,
            "missing required `TraverseInput.maxDepth` argument",
        )
    })?;
    let max_depth = parse_max_depth(max_depth_val)?;

    let strategy = match input_obj.get("strategy") {
        None | Some(JsonValue::Null) => PlannedTraverseStrategy::Bfs,
        Some(val) => parse_strategy(val)?,
    };

    let as_of = match input_obj.get("asOf") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::String(s)) => {
            let ts = Timestamp::from_str(s).map_err(|_| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "invalid `TraverseInput.asOf` RFC3339 UTC timestamp",
                )
            })?;
            Some(ts)
        }
        Some(_) => {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "`TraverseInput.asOf` must be an RFC3339 DateTime string",
            ));
        }
    };

    let where_node = match input_obj.get("where") {
        None | Some(JsonValue::Null) => None,
        Some(val) => Some(parse_predicate(val, 0, "where")?),
    };

    let scopes = match input_obj.get("scopes") {
        None | Some(JsonValue::Null) => None,
        Some(val) => Some(parse_scopes(val)?),
    };

    let first = match input_obj.get("first") {
        None | Some(JsonValue::Null) => DEFAULT_TRAVERSE_FIRST,
        Some(val) => parse_first(val)?,
    };

    let after = match input_obj.get("after") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::String(s)) => {
            if s.is_empty() {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "`TraverseInput.after` must not be empty",
                ));
            }
            Some(s.clone())
        }
        Some(_) => {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "`TraverseInput.after` must be a Cursor string",
            ));
        }
    };

    let return_fields = match input_obj.get("returnFields") {
        None | Some(JsonValue::Null) => None,
        Some(val) => Some(parse_return_fields(val, "returnFields")?),
    };

    if limits.visited_cap == 0 || limits.visited_cap > MAX_VISITED_CAP {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("visited_cap must be within 1..={MAX_VISITED_CAP}"),
        ));
    }

    let visited_bound = compute_traverse_visited_bound(max_depth, first, limits.branching_cap);
    let page_bounded_visited = visited_bound.min(u64::from(first));
    let child_count = count_selection_cost(selection);
    let child_total = u64::from(first).saturating_mul(child_count);
    let total_cost = limits
        .selection_weight
        .max(1)
        .saturating_add(visited_bound)
        .saturating_add(child_total);

    if let Some(max_cost) = limits.max_cost
        && total_cost > max_cost
    {
        return Err(GapError::new(
            ErrorCode::ComplexityExceeded,
            format!("traverse cost {total_cost} exceeds max cost budget {max_cost}"),
        ));
    }

    let cost = TraverseCostEstimate {
        visited_bound,
        page_bounded_visited,
        total_cost,
    };

    let schema_hash = compute_traverse_schema_hash(
        strategy,
        direction,
        max_depth,
        through.as_ref().and_then(|t| t.types.as_deref()),
        scopes.as_deref(),
        return_fields.as_deref(),
    );

    Ok(PlannedTraverse {
        response_key: response_key.to_owned(),
        from,
        through,
        direction,
        max_depth,
        strategy,
        as_of,
        where_node,
        scopes,
        first,
        after,
        return_fields,
        visited_cap: limits.visited_cap,
        cost,
        schema_hash,
    })
}

/// Validates and plans `gap.node(ref: NodeRef!, ...)` lookup arguments (`GQL-010`).
pub fn validate_and_plan_node_lookup(
    arguments: &BTreeMap<String, ArgValue>,
    variables: &BTreeMap<String, JsonValue>,
) -> GapResult<PlannedNodeLookup> {
    for key in arguments.keys() {
        match key.as_str() {
            "ref" | "asOf" | "returnFields" => {}
            other => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unknown node argument `{other}`"),
                ));
            }
        }
    }
    let ref_arg = arguments.get("ref").ok_or_else(|| {
        GapError::new(
            ErrorCode::ValidationError,
            "missing required `node.ref` argument",
        )
    })?;
    let ref_json = resolve_arg_value(ref_arg, variables)?;
    let reference = parse_node_ref(&ref_json, "ref")?;

    let as_of = match arguments.get("asOf") {
        None => None,
        Some(arg) => match resolve_arg_value(arg, variables)? {
            JsonValue::Null => None,
            JsonValue::String(s) => Some(Timestamp::from_str(&s).map_err(|_| {
                GapError::new(
                    ErrorCode::ValidationError,
                    "invalid `node.asOf` RFC3339 UTC timestamp",
                )
            })?),
            _ => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "`node.asOf` must be an RFC3339 DateTime string",
                ));
            }
        },
    };

    let return_fields = match arguments.get("returnFields") {
        None => None,
        Some(arg) => match resolve_arg_value(arg, variables)? {
            JsonValue::Null => None,
            val => Some(parse_return_fields(&val, "node.returnFields")?),
        },
    };

    Ok(PlannedNodeLookup {
        reference,
        as_of,
        return_fields,
    })
}

fn extract_traverse_input_object(
    arguments: &BTreeMap<String, ArgValue>,
    variables: &BTreeMap<String, JsonValue>,
) -> GapResult<JsonMap<String, JsonValue>> {
    if let Some(input_arg) = arguments.get("input") {
        if arguments.len() > 1 {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "traverse accepts only `input` alongside no unexpected sibling arguments",
            ));
        }
        let resolved = resolve_arg_value(input_arg, variables)?;
        let JsonValue::Object(obj) = resolved else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "`traverse.input` must be a TraverseInput object",
            ));
        };
        return Ok(obj);
    }

    let mut flat = JsonMap::with_capacity(arguments.len());
    for (k, v) in arguments {
        flat.insert(k.clone(), resolve_arg_value(v, variables)?);
    }
    Ok(flat)
}

fn parse_node_ref(val: &JsonValue, field_path: &str) -> GapResult<PlannedNodeRef> {
    let JsonValue::Object(map) = val else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{field_path}` must be a @oneOf NodeRef object"),
        ));
    };

    // `@oneOf` rule: exactly one field must be present and non-null.
    if map.len() != 1 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{field_path}` @oneOf input must set exactly one of `id` or `key`"),
        ));
    }

    if let Some(id_val) = map.get("id") {
        let JsonValue::String(id) = id_val else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("`{field_path}.id` must be a non-null ID string"),
            ));
        };
        if id.is_empty() || id.len() > 512 || id.bytes().any(|b| b == 0 || b.is_ascii_control()) {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("`{field_path}.id` is empty or contains invalid characters"),
            ));
        }
        return Ok(PlannedNodeRef::Id(id.clone()));
    }

    if let Some(key_val) = map.get("key") {
        let JsonValue::Object(key_map) = key_val else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("`{field_path}.key` must be a non-null NodeKey object"),
            ));
        };
        for k in key_map.keys() {
            if k != "type" && k != "value" {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unknown NodeKey field `{k}`"),
                ));
            }
        }
        let Some(JsonValue::String(node_type)) = key_map.get("type") else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("`{field_path}.key.type` must be a non-null String"),
            ));
        };
        validate_type_identifier(node_type, "key.type")?;
        let Some(value) = key_map.get("value") else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("missing required `{field_path}.key.value`"),
            ));
        };
        if value.is_null() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("`{field_path}.key.value` must not be null"),
            ));
        }
        return Ok(PlannedNodeRef::Key {
            node_type: node_type.clone(),
            value: value.clone(),
        });
    }

    Err(GapError::new(
        ErrorCode::ValidationError,
        format!("`{field_path}` @oneOf input requires `id` or `key`"),
    ))
}

fn parse_edge_filter(val: &JsonValue) -> GapResult<PlannedEdgeFilter> {
    let JsonValue::Object(map) = val else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`TraverseInput.through` must be an EdgeFilter object",
        ));
    };
    for k in map.keys() {
        if k != "types" && k != "where" {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("unknown EdgeFilter field `{k}`"),
            ));
        }
    }

    let types = match map.get("types") {
        None | Some(JsonValue::Null) => None,
        Some(JsonValue::Array(arr)) => {
            if arr.is_empty() {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "`EdgeFilter.types` must not be an empty array",
                ));
            }
            let mut out = Vec::with_capacity(arr.len());
            let mut seen = BTreeSet::new();
            for item in arr {
                let JsonValue::String(s) = item else {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        "`EdgeFilter.types` elements must be strings",
                    ));
                };
                validate_type_identifier(s, "through.types")?;
                if seen.insert(s.clone()) {
                    out.push(s.clone());
                }
            }
            Some(out)
        }
        Some(_) => {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "`EdgeFilter.types` must be a list of strings",
            ));
        }
    };

    let where_edge = match map.get("where") {
        None | Some(JsonValue::Null) => None,
        Some(p) => Some(parse_predicate(p, 0, "through.where")?),
    };

    Ok(PlannedEdgeFilter { types, where_edge })
}

fn parse_direction(val: &JsonValue) -> GapResult<PlannedDirection> {
    let JsonValue::String(s) = val else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`TraverseInput.direction` must be OUTBOUND, INBOUND, or BOTH",
        ));
    };
    match s.as_str() {
        "OUTBOUND" => Ok(PlannedDirection::Outbound),
        "INBOUND" => Ok(PlannedDirection::Inbound),
        "BOTH" => Ok(PlannedDirection::Both),
        _ => Err(GapError::new(
            ErrorCode::ValidationError,
            format!("invalid `TraverseInput.direction` `{s}`"),
        )),
    }
}

fn parse_strategy(val: &JsonValue) -> GapResult<PlannedTraverseStrategy> {
    let JsonValue::String(s) = val else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`TraverseInput.strategy` must be BFS, DFS, SHORTEST, or WEIGHTED",
        ));
    };
    match s.as_str() {
        "BFS" => Ok(PlannedTraverseStrategy::Bfs),
        "DFS" => Ok(PlannedTraverseStrategy::Dfs),
        "SHORTEST" => Ok(PlannedTraverseStrategy::Shortest),
        "WEIGHTED" => Ok(PlannedTraverseStrategy::Weighted),
        _ => Err(GapError::new(
            ErrorCode::ValidationError,
            format!("invalid `TraverseInput.strategy` `{s}`"),
        )),
    }
}

fn parse_max_depth(val: &JsonValue) -> GapResult<u32> {
    let Some(raw) = val.as_i64() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`TraverseInput.maxDepth` must be an integer in 1..=16",
        ));
    };
    if raw <= 0 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`TraverseInput.maxDepth` must be at least 1",
        ));
    }
    if raw > i64::from(MAX_TRAVERSE_DEPTH) {
        return Err(GapError::new(
            ErrorCode::DepthExceeded,
            format!("traverse maxDepth {raw} exceeds maximum {MAX_TRAVERSE_DEPTH}"),
        ));
    }
    Ok(raw as u32)
}

fn parse_first(val: &JsonValue) -> GapResult<u32> {
    let Some(raw) = val.as_i64() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`TraverseInput.first` must be a positive integer",
        ));
    };
    if raw <= 0 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`TraverseInput.first` must be at least 1",
        ));
    }
    if raw > i64::from(MAX_TRAVERSE_FIRST) {
        return Err(GapError::new(
            ErrorCode::ComplexityExceeded,
            format!("traverse first {raw} exceeds maximum {MAX_TRAVERSE_FIRST}"),
        ));
    }
    Ok(raw as u32)
}

fn parse_scopes(val: &JsonValue) -> GapResult<Vec<PlannedContextScope>> {
    let JsonValue::Array(arr) = val else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`TraverseInput.scopes` must be a list of ContextScope enum values",
        ));
    };
    // Empty `scopes` array is explicitly forbidden by `GQL-043`.
    if arr.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`TraverseInput.scopes` must not be empty (omit field for all visible scopes)",
        ));
    }
    let mut seen = BTreeSet::new();
    for item in arr {
        let JsonValue::String(s) = item else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "`TraverseInput.scopes` entries must be ContextScope strings",
            ));
        };
        let scope = match s.as_str() {
            "PRIVATE" => PlannedContextScope::Private,
            "TASK" => PlannedContextScope::Task,
            "GROUP" => PlannedContextScope::Group,
            "GLOBAL" => PlannedContextScope::Global,
            "EPHEMERAL" => PlannedContextScope::Ephemeral,
            _ => {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unknown ContextScope `{s}`"),
                ));
            }
        };
        seen.insert(scope);
    }
    Ok(seen.into_iter().collect())
}

fn parse_return_fields(val: &JsonValue, field_path: &str) -> GapResult<Vec<String>> {
    let JsonValue::Array(arr) = val else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{field_path}` must be a list of property path strings"),
        ));
    };
    if arr.len() > MAX_RETURN_FIELDS {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{field_path}` exceeds maximum {MAX_RETURN_FIELDS} fields"),
        ));
    }
    let mut seen = BTreeSet::new();
    for item in arr {
        let JsonValue::String(s) = item else {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("`{field_path}` elements must be strings"),
            ));
        };
        validate_prop_path(s, field_path)?;
        seen.insert(s.clone());
    }
    Ok(seen.into_iter().collect())
}

fn parse_predicate(val: &JsonValue, depth: usize, ctx: &str) -> GapResult<PlannedPredicate> {
    if depth > MAX_PREDICATE_DEPTH {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{ctx}` predicate nesting depth exceeded {MAX_PREDICATE_DEPTH}"),
        ));
    }
    let JsonValue::Object(map) = val else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{ctx}` must be a @oneOf Predicate object"),
        ));
    };
    if map.len() != 1 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{ctx}` @oneOf Predicate must specify exactly one operator field"),
        ));
    }
    let Some((op, arg)) = map.iter().next() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{ctx}` @oneOf Predicate must specify one operator field"),
        ));
    };
    if arg.is_null() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{ctx}.{op}` @oneOf variant must not be null"),
        ));
    }

    match op.as_str() {
        "and" => Ok(PlannedPredicate::And(parse_predicate_list(
            arg,
            depth + 1,
            "and",
        )?)),
        "or" => Ok(PlannedPredicate::Or(parse_predicate_list(
            arg,
            depth + 1,
            "or",
        )?)),
        "not" => Ok(PlannedPredicate::Not(Box::new(parse_predicate(
            arg,
            depth + 1,
            "not",
        )?))),
        "eq" => Ok(PlannedPredicate::Eq(parse_compare(arg, "eq")?)),
        "ne" => Ok(PlannedPredicate::Ne(parse_compare(arg, "ne")?)),
        "gt" => Ok(PlannedPredicate::Gt(parse_compare(arg, "gt")?)),
        "gte" => Ok(PlannedPredicate::Gte(parse_compare(arg, "gte")?)),
        "lt" => Ok(PlannedPredicate::Lt(parse_compare(arg, "lt")?)),
        "lte" => Ok(PlannedPredicate::Lte(parse_compare(arg, "lte")?)),
        "in" => Ok(PlannedPredicate::In(parse_compare_list(arg)?)),
        "contains" => Ok(PlannedPredicate::Contains(parse_compare(arg, "contains")?)),
        "startsWith" => {
            let cmp = parse_compare(arg, "startsWith")?;
            if !cmp.value.is_string() {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "`Predicate.startsWith.value` must be a string",
                ));
            }
            Ok(PlannedPredicate::StartsWith(cmp))
        }
        "exists" => {
            let JsonValue::String(path) = arg else {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "`Predicate.exists` must be a non-empty path string",
                ));
            };
            validate_prop_path(path, "Predicate.exists")?;
            Ok(PlannedPredicate::Exists(path.clone()))
        }
        other => Err(GapError::new(
            ErrorCode::ValidationError,
            format!("unknown Predicate operator `{other}`"),
        )),
    }
}

fn parse_predicate_list(
    val: &JsonValue,
    depth: usize,
    op: &str,
) -> GapResult<Vec<PlannedPredicate>> {
    let JsonValue::Array(arr) = val else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`Predicate.{op}` must be a list of Predicate objects"),
        ));
    };
    if arr.len() > MAX_PREDICATE_LIST_LEN {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`Predicate.{op}` exceeds maximum {MAX_PREDICATE_LIST_LEN} clauses"),
        ));
    }
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        out.push(parse_predicate(item, depth, op)?);
    }
    Ok(out)
}

fn parse_compare(val: &JsonValue, op: &str) -> GapResult<PlannedCompare> {
    let JsonValue::Object(map) = val else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`Predicate.{op}` must be a Compare object"),
        ));
    };
    for k in map.keys() {
        if k != "path" && k != "value" {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("unknown Compare field `{k}`"),
            ));
        }
    }
    let Some(JsonValue::String(path)) = map.get("path") else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`Predicate.{op}.path` must be a non-null string"),
        ));
    };
    validate_prop_path(path, "Compare.path")?;
    let Some(value) = map.get("value") else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("missing required `Predicate.{op}.value`"),
        ));
    };
    if value.is_null() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`Predicate.{op}.value` must not be null"),
        ));
    }
    Ok(PlannedCompare {
        path: path.clone(),
        value: value.clone(),
    })
}

fn parse_compare_list(val: &JsonValue) -> GapResult<PlannedCompareList> {
    let JsonValue::Object(map) = val else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`Predicate.in` must be a CompareList object",
        ));
    };
    for k in map.keys() {
        if k != "path" && k != "values" {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("unknown CompareList field `{k}`"),
            ));
        }
    }
    let Some(JsonValue::String(path)) = map.get("path") else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`Predicate.in.path` must be a non-null string",
        ));
    };
    validate_prop_path(path, "CompareList.path")?;
    let Some(JsonValue::Array(arr)) = map.get("values") else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`Predicate.in.values` must be a list of JSON values",
        ));
    };
    if arr.len() > MAX_PREDICATE_LIST_LEN {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`Predicate.in.values` exceeds maximum {MAX_PREDICATE_LIST_LEN} entries"),
        ));
    }
    for v in arr {
        if v.is_null() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                "`Predicate.in.values` items must not be null",
            ));
        }
    }
    Ok(PlannedCompareList {
        path: path.clone(),
        values: arr.clone(),
    })
}

fn validate_type_identifier(s: &str, field: &str) -> GapResult<()> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{field}` must be 1..=64 characters"),
        ));
    }
    if !bytes[0].is_ascii_alphabetic()
        || bytes[1..]
            .iter()
            .any(|b| !b.is_ascii_alphanumeric() && *b != b'_')
    {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{field}` must match [A-Za-z][A-Za-z0-9_]{{0,63}}"),
        ));
    }
    Ok(())
}

fn validate_prop_path(s: &str, field: &str) -> GapResult<()> {
    if s.is_empty() || s.len() > 256 || s.bytes().any(|b| b == 0 || b.is_ascii_control()) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{field}` path must be 1..=256 non-control bytes"),
        ));
    }
    if s.split('.').any(str::is_empty) {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!("`{field}` path must not have empty segments"),
        ));
    }
    Ok(())
}

fn count_selection_cost(fields: &[SelectionField]) -> u64 {
    let mut total: u64 = 0;
    for field in fields {
        total = total
            .saturating_add(1)
            .saturating_add(count_selection_cost(&field.children));
    }
    total
}

fn compute_traverse_schema_hash(
    strategy: PlannedTraverseStrategy,
    direction: PlannedDirection,
    max_depth: u32,
    edge_types: Option<&[String]>,
    scopes: Option<&[PlannedContextScope]>,
    return_fields: Option<&[String]>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gap-traverse-schema-v1\0");
    let strat_tag = match strategy {
        PlannedTraverseStrategy::Bfs => b"BFS" as &[u8],
        PlannedTraverseStrategy::Dfs => b"DFS",
        PlannedTraverseStrategy::Shortest => b"SHORTEST",
        PlannedTraverseStrategy::Weighted => b"WEIGHTED",
    };
    hasher.update(strat_tag);
    hasher.update(b"\0");
    let dir_tag = match direction {
        PlannedDirection::Outbound => b"OUTBOUND" as &[u8],
        PlannedDirection::Inbound => b"INBOUND",
        PlannedDirection::Both => b"BOTH",
    };
    hasher.update(dir_tag);
    hasher.update(b"\0");
    hasher.update(&max_depth.to_le_bytes());

    match edge_types {
        Some(ets) => {
            hasher.update(&[1]);
            for et in ets {
                hasher.update(&(et.len() as u32).to_le_bytes());
                hasher.update(et.as_bytes());
            }
        }
        None => {
            hasher.update(&[0]);
        }
    }

    match scopes {
        Some(sc) => {
            hasher.update(&[1]);
            for s in sc {
                let tag = match s {
                    PlannedContextScope::Private => 1_u8,
                    PlannedContextScope::Task => 2,
                    PlannedContextScope::Group => 3,
                    PlannedContextScope::Global => 4,
                    PlannedContextScope::Ephemeral => 5,
                };
                hasher.update(&[tag]);
            }
        }
        None => {
            hasher.update(&[0]);
        }
    }

    match return_fields {
        Some(rfs) => {
            hasher.update(&[1]);
            for rf in rfs {
                hasher.update(&(rf.len() as u32).to_le_bytes());
                hasher.update(rf.as_bytes());
            }
        }
        None => {
            hasher.update(&[0]);
        }
    }

    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn base_args(from_id: &str, max_depth: i64) -> BTreeMap<String, ArgValue> {
        let mut input = BTreeMap::new();
        let mut ref_obj = BTreeMap::new();
        ref_obj.insert(
            "id".to_owned(),
            ArgValue::Literal(JsonValue::String(from_id.to_owned())),
        );
        input.insert("from".to_owned(), ArgValue::Object(ref_obj));
        input.insert(
            "maxDepth".to_owned(),
            ArgValue::Literal(JsonValue::from(max_depth)),
        );

        let mut args = BTreeMap::new();
        args.insert("input".to_owned(), ArgValue::Object(input));
        args
    }

    // spec: GQL-040
    #[test]
    fn traverse_rejects_depth_above_16_and_first_above_1000() {
        let args = base_args("sym:root", 17);
        let err = validate_and_plan_traverse(
            "traverse",
            &args,
            &[SelectionField::leaf("nodes")],
            &BTreeMap::new(),
            TraversePlanLimits::default(),
        )
        .expect_err("depth 17 must fail");
        assert_eq!(err.code(), ErrorCode::DepthExceeded);

        // maxDepth = 0 -> ValidationError
        let err0 = validate_and_plan_traverse(
            "traverse",
            &base_args("sym:root", 0),
            &[],
            &BTreeMap::new(),
            TraversePlanLimits::default(),
        )
        .expect_err("depth 0 must fail");
        assert_eq!(err0.code(), ErrorCode::ValidationError);

        // first = 1001 -> ComplexityExceeded
        let mut args_over_first = base_args("sym:root", 2);
        if let Some(ArgValue::Object(input)) = args_over_first.get_mut("input") {
            input.insert("first".to_owned(), ArgValue::Literal(JsonValue::from(1001)));
        }
        let err_first = validate_and_plan_traverse(
            "traverse",
            &args_over_first,
            &[],
            &BTreeMap::new(),
            TraversePlanLimits::default(),
        )
        .expect_err("first > 1000 must fail");
        assert_eq!(err_first.code(), ErrorCode::ComplexityExceeded);
    }

    // spec: GQL-041
    #[test]
    fn traverse_computes_cost_formula_and_enforces_budget() {
        // sum_{d=1..3} min(100, 8)^d = 8 + 64 + 512 = 584
        assert_eq!(compute_traverse_visited_bound(3, 100, 8), 584);
        // min(5, 8) = 5 -> 5 + 25 = 30
        assert_eq!(compute_traverse_visited_bound(2, 5, 8), 30);

        let args = base_args("sym:root", 4);
        let tight_limits = TraversePlanLimits {
            branching_cap: 8,
            visited_cap: 100_000,
            max_cost: Some(1_000),
            selection_weight: 1,
        };
        // visited_bound for depth=4, cap=8 is 8+64+512+4096 = 4680 > 1000
        let err =
            validate_and_plan_traverse("traverse", &args, &[], &BTreeMap::new(), tight_limits)
                .expect_err("over cost budget");
        assert_eq!(err.code(), ErrorCode::ComplexityExceeded);
    }

    // spec: GQL-010, GQL-043
    #[test]
    fn traverse_rejects_empty_scopes_and_invalid_oneof_inputs() {
        // Empty scopes list forbidden (GQL-043)
        let mut args = base_args("sym:root", 2);
        if let Some(ArgValue::Object(input)) = args.get_mut("input") {
            input.insert(
                "scopes".to_owned(),
                ArgValue::Literal(JsonValue::Array(vec![])),
            );
        }
        let err = validate_and_plan_traverse(
            "traverse",
            &args,
            &[],
            &BTreeMap::new(),
            TraversePlanLimits::default(),
        )
        .expect_err("empty scopes must fail");
        assert_eq!(err.code(), ErrorCode::ValidationError);

        // NodeRef @oneOf with both id and key must fail
        let mut bad_ref = BTreeMap::new();
        bad_ref.insert(
            "id".to_owned(),
            ArgValue::Literal(JsonValue::String("a".to_owned())),
        );
        bad_ref.insert(
            "key".to_owned(),
            ArgValue::Literal(serde_json::json!({"type": "Symbol", "value": "k"})),
        );
        let mut lookup_args = BTreeMap::new();
        lookup_args.insert("ref".to_owned(), ArgValue::Object(bad_ref));
        assert_eq!(
            validate_and_plan_node_lookup(&lookup_args, &BTreeMap::new())
                .expect_err("dual oneOf must fail")
                .code(),
            ErrorCode::ValidationError
        );
    }

    // spec: GQL-010, ING-067
    #[test]
    fn traverse_plans_variables_predicates_weighted_strategy_and_return_fields() {
        let mut input = BTreeMap::new();
        input.insert(
            "from".to_owned(),
            ArgValue::Literal(serde_json::json!({"key": {"type": "Symbol", "value": "main"}})),
        );
        input.insert(
            "maxDepth".to_owned(),
            ArgValue::Variable("depth".to_owned()),
        );
        input.insert(
            "strategy".to_owned(),
            ArgValue::Literal(JsonValue::String("WEIGHTED".to_owned())),
        );
        input.insert(
            "asOf".to_owned(),
            ArgValue::Literal(JsonValue::String("2026-01-01T00:00:00Z".to_owned())),
        );
        input.insert(
            "through".to_owned(),
            ArgValue::Literal(serde_json::json!({
                "types": ["CALLS"],
                "where": { "gt": { "path": "weight", "value": 0 } }
            })),
        );
        input.insert(
            "where".to_owned(),
            ArgValue::Literal(serde_json::json!({
                "and": [
                    { "startsWith": { "path": "lang", "value": "rust" } },
                    { "exists": "signature" }
                ]
            })),
        );
        input.insert(
            "scopes".to_owned(),
            ArgValue::Literal(serde_json::json!(["GLOBAL", "GROUP"])),
        );
        input.insert(
            "returnFields".to_owned(),
            ArgValue::Literal(serde_json::json!(["signature", "lang"])),
        );
        input.insert("first".to_owned(), ArgValue::Literal(JsonValue::from(25)));

        let mut args = BTreeMap::new();
        args.insert("input".to_owned(), ArgValue::Object(input));
        let mut vars = BTreeMap::new();
        vars.insert("depth".to_owned(), JsonValue::from(3));

        let plan = validate_and_plan_traverse(
            "sub",
            &args,
            &[SelectionField::leaf("nodes")],
            &vars,
            TraversePlanLimits::default(),
        )
        .expect("valid plan");

        assert_eq!(plan.max_depth, 3);
        assert_eq!(plan.first, 25);
        assert_eq!(plan.strategy, PlannedTraverseStrategy::Weighted);
        assert!(plan.as_of.is_some());
        assert_eq!(
            plan.scopes,
            Some(vec![
                PlannedContextScope::Group,
                PlannedContextScope::Global
            ])
        );
        assert_eq!(
            plan.return_fields,
            Some(vec!["lang".to_owned(), "signature".to_owned()])
        );
        assert!(matches!(plan.where_node, Some(PlannedPredicate::And(ref c)) if c.len() == 2));
    }
}
