//! GAP-QL strict GraphQL profile parser, validator, directives, and static complexity analyzer (`L1`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod cost;
pub mod directives;
pub mod limits;
pub mod persisted;
pub mod schema;
pub mod suggest;
pub mod validate;
pub mod visibility;

pub use apollo_compiler::Schema;
pub use apollo_compiler::validation::Valid;

pub use crate::cost::{
    CostEstimate, CostModelConfig, DEFAULT_TRAVERSE_BRANCHING_CAP, compute_operation_cost,
    enforce_operation_cost, traverse_visited_bound, validate_join_cycles,
};
pub use crate::directives::{
    BranchCompositionInput, ComposedSupergraph, ExecutableDirectives, JoinDirectiveStatus,
    OperationBudgetDirective, OutputFormat, PlannableJoin, SanitizedBranchAst,
    check_executable_incremental_directives, compose_branch_inputs, effective_field_side_effect,
    extract_executable_directives, plannable_joins, sanitize_branch_sdl,
};
pub use crate::limits::{
    DocumentMetrics, HARD_MAX_TRAVERSE_DEPTH, HARD_MAX_TRAVERSE_FIRST,
    HTTP_PAYLOAD_TOO_LARGE_STATUS, TraverseBounds, check_ast_pre_validation_limits,
    check_request_body_size, check_variable_map_size, enforce_operation_limits,
};
pub use crate::persisted::{
    EmptyPersistedStore, InMemoryPersistedStore, PersistedOperationId, PersistedOperationStore,
    ResolvedDocument, enforce_operation_mode, resolve_persisted_operation, sha256_bytes,
    sha256_hex,
};
pub use crate::schema::{
    BUILTIN_SDL, EXECUTABLE_DIRECTIVES_SDL, TYPE_SYSTEM_DIRECTIVES_SDL, builtin_schema,
    compose_supergraph_sdl, validate_branch_ast_reserved_names, validate_branch_sdl_reserved_names,
    with_gap_builtins,
};
pub use crate::suggest::{
    attach_validation_suggestion, levenshtein_distance, minimal_selection_template,
    rank_did_you_mean,
};
pub use crate::validate::{
    ValidatedOperation, classify_validation_message, parse_executable_ast, validate_document,
    validate_document_with_visibility, validate_executable_reserved_names, validate_query,
    validate_query_with_visibility, validate_unique_input_object_fields,
};
pub use crate::visibility::{
    AllowAllVisibility, FnVisibility, HiddenSetVisibility, Visibility,
    execute_visible_introspection, project_introspection_schema, project_visible_schema,
};

use gap_core::config::{LimitsConfig, OperationMode};
use gap_core::error::{ErrorCode, GapError, GapResult};

/// Unified front-end configuration governing limits, operation mode, cost model, and optional
/// GraphQL extensions (`spec/02-gap-ql.md` §§1, 7, 8, 12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontendConfig {
    /// Request size, depth, alias, variable, root-field, and cost caps (`spec/02-gap-ql.md` §8).
    pub limits: LimitsConfig,
    /// Document allow-listing mode (`open | hybrid | safelist`, `GQL-060`).
    pub operation_mode: OperationMode,
    /// Static cost-model parameters (`GQL-041`).
    pub cost_model: CostModelConfig,
    /// Allow `@defer` / `@stream` incremental delivery (`RUN-062`; default `false`, `GQL-001`).
    pub allow_incremental: bool,
    /// Allow `@format(as: COLUMNAR)` extension X-1 (`M7`; default `false`, `spec/02-gap-ql.md` §12).
    pub allow_columnar_format: bool,
}

impl Default for FrontendConfig {
    fn default() -> Self {
        Self {
            limits: LimitsConfig::default(),
            operation_mode: OperationMode::Open,
            cost_model: CostModelConfig::default(),
            allow_incremental: false,
            allow_columnar_format: false,
        }
    }
}

/// Incoming GraphQL request envelope supplied to [`prepare_request`] (`spec/02-gap-ql.md` §6).
#[derive(Debug, Clone, Default)]
pub struct GraphQlRequest<'a> {
    /// Raw transport payload byte count if already measured on the wire (`HTTP 413` pre-check).
    pub raw_body_bytes: Option<usize>,
    /// GraphQL `query` text (`None` for Apollo APQ hash-only requests).
    pub query: Option<&'a str>,
    /// Optional target `operationName`.
    pub operation_name: Option<&'a str>,
    /// Optional JSON `variables` object.
    pub variables: Option<&'a serde_json::Map<String, serde_json::Value>>,
    /// Optional JSON `extensions` object (`persistedQuery`, `gap`).
    pub extensions: Option<&'a serde_json::Map<String, serde_json::Value>>,
    /// Optional caller-supplied `costUnits` budget override from `RequestContext`.
    pub request_budget_cost_units: Option<u32>,
}

/// Fully validated, mode-checked, limit-checked, and cost-analyzed GAP-QL operation ready for the
/// query planner (`WP-M1-06`).
#[derive(Debug, Clone)]
pub struct PreparedOperation {
    /// Canonical SHA-256 persisted operation identifier (`GQL-061`).
    pub operation_id: PersistedOperationId,
    /// Whether this document was registered in the [`PersistedOperationStore`].
    pub is_registered: bool,
    /// Validated `apollo-compiler` executable document, selected operation, and coerced variables.
    pub validated: ValidatedOperation,
    /// Structural metrics computed before resolution (`spec/02-gap-ql.md` §8).
    pub metrics: DocumentMetrics,
    /// Static `GQL-041` cost estimate and budget check result.
    pub cost: CostEstimate,
    /// Extracted `@budget`, `@dryRun`, and `@format` executable directives.
    pub directives: ExecutableDirectives,
}

fn extract_gap_extension_cost_budget(
    extensions: Option<&serde_json::Map<String, serde_json::Value>>,
) -> GapResult<Option<u32>> {
    let Some(ext_map) = extensions else {
        return Ok(None);
    };
    let Some(gap_obj) = ext_map.get("gap").and_then(serde_json::Value::as_object) else {
        return Ok(None);
    };
    let Some(budget_obj) = gap_obj.get("budget").and_then(serde_json::Value::as_object) else {
        return Ok(None);
    };
    let Some(cost_val) = budget_obj.get("costUnits") else {
        return Ok(None);
    };
    let Some(raw) = cost_val.as_i64() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "`extensions.gap.budget.costUnits` must be an integer",
        ));
    };
    let Ok(cost_u32) = u32::try_from(raw) else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            format!(
                "`extensions.gap.budget.costUnits` {raw} must be in 0..={}",
                u32::MAX
            ),
        ));
    };
    Ok(Some(cost_u32))
}

/// Runs the complete pre-execution GAP-QL front-end pipeline in spec order:
/// 1. Raw body byte limit (`max_request_body_bytes`) and JSON variable-map limit (`max_variables`).
/// 2. Persisted operation (`extensions.persistedQuery.sha256Hash`) resolution (`GQL-061`).
/// 3. Bounded GraphQL syntax parsing (`GQL-001`, `GQL-002`).
/// 4. `@defer` / `@stream` feature gate (`GQL-001`), AST pre-validation limits, and `GQL-004`
///    reserved-name enforcement.
/// 5. `OperationMode` (`open | hybrid | safelist`) enforcement (`GQL-060`).
/// 6. Caller-visibility schema projection (`GQL-062`, `GQL-031`), `apollo-compiler` validation,
///    variable coercion, `@oneOf` + `GQL-043` checks, and `didYouMean` self-repair hints (`GQL-070`).
/// 7. Structural limits (`max_root_fields`, `max_aliases`, `max_selection_depth`, `GQL-040`).
/// 8. Static `GQL-041` cost model (`max_cost_units` and caller `@budget` / `extensions.gap.budget`).
/// 9. Executable directive extraction (`@budget`, `@dryRun`, `@format`).
pub fn prepare_request(
    schema: &Valid<Schema>,
    request: &GraphQlRequest<'_>,
    config: &FrontendConfig,
    visibility: &dyn Visibility,
    persisted_store: &dyn PersistedOperationStore,
) -> Result<PreparedOperation, Vec<GapError>> {
    let empty_vars = serde_json::Map::new();
    let vars = request.variables.unwrap_or(&empty_vars);

    if let Some(raw_len) = request.raw_body_bytes
        && raw_len as u64 > config.limits.max_request_body_bytes
    {
        return Err(vec![GapError::new(
            ErrorCode::ComplexityExceeded,
            format!(
                "request body size {raw_len} bytes exceeds maximum {} bytes (HTTP 413)",
                config.limits.max_request_body_bytes
            ),
        )]);
    }

    check_variable_map_size(vars.len(), &config.limits).map_err(|e| vec![e])?;

    let resolved = resolve_persisted_operation(request.query, request.extensions, persisted_store)
        .map_err(|e| vec![e])?;

    let effective_body_bytes = request.raw_body_bytes.unwrap_or(resolved.query.len());
    if effective_body_bytes as u64 > config.limits.max_request_body_bytes {
        return Err(vec![GapError::new(
            ErrorCode::ComplexityExceeded,
            format!(
                "request body size {effective_body_bytes} bytes exceeds maximum {} bytes (HTTP 413)",
                config.limits.max_request_body_bytes
            ),
        )]);
    }

    let ast_doc = parse_executable_ast(&resolved.query, "request.graphql")?;
    check_executable_incremental_directives(&ast_doc, config.allow_incremental)
        .map_err(|e| vec![e])?;
    check_ast_pre_validation_limits(&ast_doc, vars.len(), &config.limits).map_err(|e| vec![e])?;
    validate_executable_reserved_names(&ast_doc)?;
    enforce_operation_mode(config.operation_mode, resolved.is_registered, &ast_doc)
        .map_err(|e| vec![e])?;

    let projected = project_visible_schema(schema, visibility);
    let validated = crate::validate::validate_parsed_ast_against_projected_schema(
        &projected,
        &ast_doc,
        request.operation_name,
        vars,
        visibility,
    )?;

    let metrics =
        enforce_operation_limits(&validated, effective_body_bytes, vars.len(), &config.limits)
            .map_err(|e| vec![e])?;

    let ext_budget = extract_gap_extension_cost_budget(request.extensions).map_err(|e| vec![e])?;
    let merged_req_budget = match (request.request_budget_cost_units, ext_budget) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let cost = enforce_operation_cost(
        &validated,
        &config.limits,
        merged_req_budget,
        config.cost_model,
    )
    .map_err(|e| vec![e])?;

    let directives = extract_executable_directives(&validated).map_err(|e| vec![e])?;
    directives
        .ensure_columnar_allowed(config.allow_columnar_format)
        .map_err(|e| vec![e])?;

    Ok(PreparedOperation {
        operation_id: resolved.operation_id,
        is_registered: resolved.is_registered,
        validated,
        metrics,
        cost,
        directives,
    })
}

/// Convenience wrapper around [`prepare_request`] returning the primary [`GapError`] when
/// preparation fails.
pub fn prepare_request_single(
    schema: &Valid<Schema>,
    request: &GraphQlRequest<'_>,
    config: &FrontendConfig,
    visibility: &dyn Visibility,
    persisted_store: &dyn PersistedOperationStore,
) -> GapResult<PreparedOperation> {
    prepare_request(schema, request, config, visibility, persisted_store).map_err(|mut errs| {
        if errs.is_empty() {
            GapError::new(
                ErrorCode::ValidationError,
                "GraphQL request preparation failed",
            )
        } else {
            errs.swap_remove(0)
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;

    fn billing_supergraph() -> Valid<Schema> {
        with_gap_builtins(
            r#"
            type BillingQuery {
                invoice(id: ID!): Invoice @cost(weight: 3)
                secretLedger(id: ID!): Invoice @cost(weight: 5)
            }
            type Invoice @key(fields: "id") {
                id: ID!
                total: Float!
            }
            type Query {
                billing: BillingQuery!
            }
            type Mutation {
                voidInvoice(id: ID!): Invoice! @cost(weight: 10)
            }
            "#,
        )
        .expect("compose billing supergraph")
    }

    // spec: GQL-001, GQL-041, GQL-060, GQL-061, GQL-062, GQL-070
    #[test]
    fn prepare_request_runs_end_to_end_pipeline_across_apq_modes_visibility_and_budgets() {
        let schema = billing_supergraph();
        let vis = HiddenSetVisibility::new().hide_field("BillingQuery", "secretLedger");
        let mut store = InMemoryPersistedStore::new();
        let reg_mut_doc =
            r#"mutation VoidIt($id: ID!) @dryRun { voidInvoice(id: $id) { id total } }"#;
        let reg_mut_id = store.register(reg_mut_doc);

        let hybrid_cfg = FrontendConfig {
            operation_mode: OperationMode::Hybrid,
            ..FrontendConfig::default()
        };

        // 1. Registered mutation via hash-only APQ in Hybrid mode succeeds and extracts @dryRun
        let vars = json!({ "id": "inv-42" });
        let ext_apq = json!({
            "persistedQuery": { "version": 1, "sha256Hash": reg_mut_id.as_str() },
            "gap": { "budget": { "costUnits": 50 } }
        });
        let req_ok = GraphQlRequest {
            query: None,
            operation_name: Some("VoidIt"),
            variables: vars.as_object(),
            extensions: ext_apq.as_object(),
            ..GraphQlRequest::default()
        };
        let prepared = prepare_request_single(&schema, &req_ok, &hybrid_cfg, &vis, &store)
            .expect("registered APQ mutation succeeds in hybrid mode");
        assert!(prepared.is_registered);
        assert_eq!(prepared.operation_id, reg_mut_id);
        assert!(prepared.directives.dry_run);
        assert_eq!(prepared.cost.effective_budget, Some(50));
        // voidInvoice(weight 10) + id(1) + total(1) = 12
        assert_eq!(prepared.cost.planned_cost, 12);

        // 2. Unregistered mutation in Hybrid mode -> GAP_OPERATION_NOT_ALLOWED
        let req_unreg_mut = GraphQlRequest {
            query: Some(r#"mutation { voidInvoice(id: "1") { id } }"#),
            ..GraphQlRequest::default()
        };
        let mode_err = prepare_request_single(&schema, &req_unreg_mut, &hybrid_cfg, &vis, &store)
            .expect_err("unregistered mutation rejected in hybrid");
        assert_eq!(mode_err.code(), ErrorCode::OperationNotAllowed);

        // 3. Unregistered read query with typo gets visible didYouMean without leaking secretLedger
        let req_typo = GraphQlRequest {
            query: Some(r#"{ billing { invoce(id: "1") { total } } }"#),
            ..GraphQlRequest::default()
        };
        let typo_err = prepare_request_single(&schema, &req_typo, &hybrid_cfg, &vis, &store)
            .expect_err("typo field fails");
        assert_eq!(typo_err.code(), ErrorCode::UnknownField);
        let suggestion = typo_err.suggestion().expect("didYouMean attached");
        assert_eq!(suggestion.did_you_mean, Some(vec!["invoice".to_owned()]));

        // 4. extensions.gap.budget.costUnits exceeded -> GAP_BUDGET_EXCEEDED
        let tight_ext = json!({ "gap": { "budget": { "costUnits": 3 } } });
        let req_budget_fail = GraphQlRequest {
            query: Some(r#"{ billing { invoice(id: "1") { id total } } }"#),
            extensions: tight_ext.as_object(),
            ..GraphQlRequest::default()
        };
        let budget_err =
            prepare_request_single(&schema, &req_budget_fail, &hybrid_cfg, &vis, &store)
                .expect_err("cost 6 > budget 3");
        assert_eq!(budget_err.code(), ErrorCode::BudgetExceeded);
        assert!(budget_err.message().contains('6') && budget_err.message().contains('3'));
    }
}
