//! Parameterized SQL statement generation (`CMP-050`, `NGC-021`, `NGC-023`).
//!
//! Identifiers are accepted only from the operation's configured column allow-list. Request values
//! are always bound parameters and are never interpolated into SQL text.

use std::collections::{BTreeMap, BTreeSet};

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::ngc::v1::{Expr, PredicateOp, expr};

use crate::config::{Dialect, OperationConfig, SqlType};
use crate::driver::SqlParam;

fn invalid(msg: impl Into<String>) -> NgcErrorInfo {
    NgcErrorInfo::new(NgcErrorCode::InvalidArgument, msg)
}

fn placeholder(dialect: Dialect, n: usize) -> String {
    match dialect {
        Dialect::Sqlite => "?".to_owned(),
        Dialect::Postgres => format!("${n}"),
    }
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{identifier}\"")
}

/// Converts a protobuf scalar to a type-checked bound SQL parameter (`CMP-050`, `NGC-060`).
pub fn prost_to_typed_param(
    value: &prost_types::Value,
    sql_type: &SqlType,
) -> Result<SqlParam, NgcErrorInfo> {
    use prost_types::value::Kind;
    let Some(kind) = &value.kind else {
        return Ok(SqlParam::Null);
    };
    if matches!(kind, Kind::NullValue(_)) {
        return Ok(SqlParam::Null);
    }

    match sql_type {
        SqlType::Integer => match kind {
            Kind::NumberValue(n) => {
                if !n.is_finite()
                    || n.fract() != 0.0
                    || *n < (i64::MIN as f64)
                    || *n >= 9_223_372_036_854_775_808.0
                {
                    return Err(invalid("expected an exact integer argument"));
                }
                Ok(SqlParam::Integer(*n as i64))
            }
            _ => Err(invalid("expected integer argument")),
        },
        SqlType::Float => match kind {
            Kind::NumberValue(n) => {
                if !n.is_finite() {
                    return Err(invalid("numeric argument must be finite"));
                }
                Ok(SqlParam::Real(if *n == 0.0 { 0.0 } else { *n }))
            }
            _ => Err(invalid("expected numeric argument")),
        },
        SqlType::Text | SqlType::Uuid => match kind {
            Kind::StringValue(s) => Ok(SqlParam::Text(s.clone())),
            _ => Err(invalid("expected string argument")),
        },
        SqlType::Timestamp => match kind {
            Kind::StringValue(s) => Ok(SqlParam::Text(s.clone())),
            Kind::NumberValue(n) => {
                if !n.is_finite() || n.fract() != 0.0 {
                    return Err(invalid("timestamp numeric value must be finite integer"));
                }
                Ok(SqlParam::Integer(*n as i64))
            }
            _ => Err(invalid("expected timestamp string or integer argument")),
        },
        SqlType::Boolean => match kind {
            Kind::BoolValue(b) => Ok(SqlParam::Bool(*b)),
            _ => Err(invalid("expected boolean argument")),
        },
        SqlType::Bytes => match kind {
            Kind::StringValue(s) => Ok(SqlParam::Blob(s.as_bytes().to_vec())),
            _ => Err(invalid("expected string bytes argument")),
        },
        SqlType::Json => {
            let json_val = gap_ngc_sdk::error::prost_value_to_json(value);
            serde_json::to_string(&json_val)
                .map(SqlParam::Text)
                .map_err(|_| invalid("invalid json argument"))
        }
    }
}

/// Converts an untyped protobuf scalar to a bound SQL parameter.
pub fn prost_to_param(value: &prost_types::Value) -> Result<SqlParam, NgcErrorInfo> {
    use prost_types::value::Kind;
    Ok(match &value.kind {
        None | Some(Kind::NullValue(_)) => SqlParam::Null,
        Some(Kind::BoolValue(value)) => SqlParam::Bool(*value),
        Some(Kind::NumberValue(value)) => {
            if !value.is_finite() {
                return Err(invalid("numeric argument must be finite"));
            }
            if value.fract() == 0.0
                && *value >= (i64::MIN as f64)
                && *value < 9_223_372_036_854_775_808.0
            {
                SqlParam::Integer(*value as i64)
            } else {
                SqlParam::Real(if *value == 0.0 { 0.0 } else { *value })
            }
        }
        Some(Kind::StringValue(value)) => SqlParam::Text(value.clone()),
        Some(Kind::ListValue(_)) | Some(Kind::StructValue(_)) => {
            return Err(invalid(
                "argument must be a scalar value, not a list or object",
            ));
        }
    })
}

/// Generated parameterized SQL query alongside bound arguments (`CMP-050`).
#[derive(Debug)]
pub struct BoundStatement {
    pub sql: String,
    pub params: Vec<SqlParam>,
}

fn require_col_def<'a>(
    op: &'a OperationConfig,
    name: &str,
) -> Result<&'a crate::config::ColumnDef, NgcErrorInfo> {
    op.column(name)
        .ok_or_else(|| invalid(format!("unknown column '{name}'")))
}

fn selected_columns<'a>(
    op: &'a OperationConfig,
    selected: &[&str],
) -> Result<Vec<&'a str>, NgcErrorInfo> {
    if selected.is_empty() {
        return Ok(op
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect());
    }
    let allow = op.column_allow_list();
    let mut seen = BTreeSet::new();
    for column in selected {
        if !allow.contains(column) {
            return Err(invalid(format!("unknown selected column '{column}'")));
        }
        if !seen.insert(*column) {
            return Err(invalid(format!("duplicate selected column '{column}'")));
        }
    }
    Ok(op
        .columns
        .iter()
        .filter(|column| seen.contains(column.name.as_str()))
        .map(|column| column.name.as_str())
        .collect())
}

/// Validates and extracts a single `foreach` item's key parameter against the configured key column.
pub fn validate_foreach_key(
    op: &OperationConfig,
    static_arguments: &prost_types::Struct,
    vars: &prost_types::Struct,
) -> Result<SqlParam, NgcErrorInfo> {
    if op.key_columns.len() != 1 {
        return Err(invalid("foreach IN batches require exactly one key column"));
    }
    let key = &op.key_columns[0];
    let key_def = require_col_def(op, key)?;

    let mut merged: BTreeMap<&str, &prost_types::Value> = BTreeMap::new();
    for (k, v) in &static_arguments.fields {
        merged.insert(k.as_str(), v);
    }
    for (k, v) in &vars.fields {
        merged.insert(k.as_str(), v);
    }

    for k in merged.keys() {
        if *k != key.as_str() {
            return Err(invalid(format!("unknown foreach argument '{k}'")));
        }
    }

    let raw_val = merged
        .get(key.as_str())
        .ok_or_else(|| invalid(format!("missing key argument '{key}'")))?;
    let param = prost_to_typed_param(raw_val, &key_def.sql_type)?;
    if param == SqlParam::Null {
        return Err(invalid(format!("key argument '{key}' must not be null")));
    }
    Ok(param)
}

/// Builds a single-row lookup using every configured key column (`CMP-050`).
pub fn build_get_by_key(
    op: &OperationConfig,
    dialect: Dialect,
    arguments: &prost_types::Struct,
    selected_cols: &[&str],
) -> Result<BoundStatement, NgcErrorInfo> {
    for name in arguments.fields.keys() {
        if !op.key_columns.contains(name) {
            return Err(invalid(format!("unknown get_by_key argument '{name}'")));
        }
    }
    let columns = selected_columns(op, selected_cols)?;
    let column_list = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    let table = quote_ident(&op.table);
    let mut params = Vec::new();
    let mut predicates = Vec::new();
    for key in &op.key_columns {
        let key_def = require_col_def(op, key)?;
        let value = arguments
            .fields
            .get(key)
            .ok_or_else(|| invalid(format!("missing key argument '{key}'")))?;
        let param = prost_to_typed_param(value, &key_def.sql_type)?;
        if param == SqlParam::Null {
            return Err(invalid(format!("key argument '{key}' must not be null")));
        }
        params.push(param);
        predicates.push(format!(
            "{} = {}",
            quote_ident(key),
            placeholder(dialect, params.len())
        ));
    }
    Ok(BoundStatement {
        sql: format!(
            "SELECT {column_list} FROM {table} WHERE {} LIMIT 1",
            predicates.join(" AND ")
        ),
        params,
    })
}

/// Builds one batched key lookup from pre-validated [`SqlParam`] values (`CMP-052`).
pub fn build_foreach_params(
    op: &OperationConfig,
    dialect: Dialect,
    values: Vec<SqlParam>,
    selected_cols: &[&str],
) -> Result<BoundStatement, NgcErrorInfo> {
    if values.is_empty() {
        return Err(invalid("foreach batch must have at least one key value"));
    }
    if op.key_columns.len() != 1 {
        return Err(invalid("foreach IN batches require exactly one key column"));
    }
    if values.contains(&SqlParam::Null) {
        return Err(invalid("foreach key values must not be null"));
    }
    let key = &op.key_columns[0];
    let columns = selected_columns(op, selected_cols)?;
    let column_list = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    let table = quote_ident(&op.table);

    let (sql, params) = match dialect {
        Dialect::Sqlite => {
            let placeholders = (1..=values.len())
                .map(|index| placeholder(dialect, index))
                .collect::<Vec<_>>()
                .join(", ");
            (
                format!(
                    "SELECT {column_list} FROM {table} WHERE {} IN ({placeholders})",
                    quote_ident(key)
                ),
                values,
            )
        }
        Dialect::Postgres => (
            format!(
                "SELECT {column_list} FROM {table} WHERE {} = ANY($1)",
                quote_ident(key)
            ),
            vec![SqlParam::Array(values)],
        ),
    };
    Ok(BoundStatement { sql, params })
}

/// Builds one batched key lookup. PostgreSQL receives one typed array parameter (`CMP-052`).
pub fn build_foreach_in(
    op: &OperationConfig,
    dialect: Dialect,
    key_values: &[&prost_types::Value],
    selected_cols: &[&str],
) -> Result<BoundStatement, NgcErrorInfo> {
    if op.key_columns.len() != 1 {
        return Err(invalid("foreach IN batches require exactly one key column"));
    }
    let key_def = require_col_def(op, &op.key_columns[0])?;
    let values = key_values
        .iter()
        .map(|value| prost_to_typed_param(value, &key_def.sql_type))
        .collect::<Result<Vec<_>, _>>()?;
    build_foreach_params(op, dialect, values, selected_cols)
}

fn render_equality_arguments(
    op: &OperationConfig,
    dialect: Dialect,
    arguments: &prost_types::Struct,
    param_index: &mut usize,
    params: &mut Vec<SqlParam>,
) -> Result<Vec<String>, NgcErrorInfo> {
    let mut clauses = Vec::with_capacity(arguments.fields.len());
    for (name, val) in &arguments.fields {
        let col_def = op
            .column(name)
            .ok_or_else(|| invalid(format!("unknown argument column '{name}'")))?;
        let param = prost_to_typed_param(val, &col_def.sql_type)?;
        let quoted = quote_ident(name);
        if param == SqlParam::Null {
            clauses.push(format!("{quoted} IS NULL"));
        } else {
            params.push(param);
            *param_index += 1;
            clauses.push(format!("{quoted} = {}", placeholder(dialect, *param_index)));
        }
    }
    Ok(clauses)
}

/// Builds a deterministic, bounded list query (`CMP-050`, `NGC-021`).
#[allow(clippy::too_many_arguments)]
pub fn build_list_with_args(
    op: &OperationConfig,
    dialect: Dialect,
    selected_cols: &[&str],
    arguments: Option<&prost_types::Struct>,
    predicate: Option<&Expr>,
    order_by: &[(String, bool)],
    limit: u32,
    offset: u64,
) -> Result<BoundStatement, NgcErrorInfo> {
    if limit == 0 {
        return Err(invalid("query limit must be greater than zero"));
    }
    let offset = i64::try_from(offset).map_err(|_| invalid("page offset exceeds SQL range"))?;
    let columns = selected_columns(op, selected_cols)?;
    let column_list = columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ");
    let table = quote_ident(&op.table);

    let mut params = Vec::new();
    let mut param_index = 0_usize;
    let mut where_parts = Vec::new();

    if let Some(args) = arguments {
        where_parts.extend(render_equality_arguments(
            op,
            dialect,
            args,
            &mut param_index,
            &mut params,
        )?);
    }
    if let Some(predicate) = predicate {
        let pred_sql = render_expr(op, dialect, predicate, &mut param_index, &mut params)?;
        where_parts.push(format!("({pred_sql})"));
    }

    let where_sql = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };

    let allow = op.column_allow_list();
    let mut ordering = Vec::new();
    let mut seen = BTreeSet::new();
    for (column, descending) in order_by {
        if !allow.contains(column.as_str()) {
            return Err(invalid(format!("unknown order_by column '{column}'")));
        }
        if !seen.insert(column.as_str()) {
            return Err(invalid(format!("duplicate order_by column '{column}'")));
        }
        ordering.push(format!(
            "{}{}",
            quote_ident(column),
            if *descending { " DESC" } else { " ASC" }
        ));
    }
    for key in &op.key_columns {
        if seen.insert(key.as_str()) {
            ordering.push(format!("{} ASC", quote_ident(key)));
        }
    }

    params.push(SqlParam::Integer(i64::from(limit)));
    params.push(SqlParam::Integer(offset));
    let limit_placeholder = placeholder(dialect, params.len() - 1);
    let offset_placeholder = placeholder(dialect, params.len());
    Ok(BoundStatement {
        sql: format!(
            "SELECT {column_list} FROM {table}{where_sql} ORDER BY {} LIMIT {limit_placeholder} OFFSET {offset_placeholder}",
            ordering.join(", ")
        ),
        params,
    })
}

/// Builds a deterministic, bounded list query without top-level arguments (`CMP-050`, `NGC-021`).
pub fn build_list(
    op: &OperationConfig,
    dialect: Dialect,
    selected_cols: &[&str],
    predicate: Option<&Expr>,
    order_by: &[(String, bool)],
    limit: u32,
    offset: u64,
) -> Result<BoundStatement, NgcErrorInfo> {
    build_list_with_args(
        op,
        dialect,
        selected_cols,
        None,
        predicate,
        order_by,
        limit,
        offset,
    )
}

/// Builds a parameterized single-column sampling statement for `Sketch` (`NGC-050`, `CMP-050`).
pub fn build_sketch_sample(
    op: &OperationConfig,
    dialect: Dialect,
    field: &str,
    arguments: Option<&prost_types::Struct>,
    limit: u32,
) -> Result<BoundStatement, NgcErrorInfo> {
    if limit == 0 {
        return Err(invalid("sketch sample limit must be greater than zero"));
    }
    let _ = require_col_def(op, field)?;
    let table = quote_ident(&op.table);
    let col = quote_ident(field);
    let mut params = Vec::new();
    let mut param_index = 0_usize;
    let where_sql = match arguments {
        Some(args) if !args.fields.is_empty() => {
            let clauses =
                render_equality_arguments(op, dialect, args, &mut param_index, &mut params)?;
            format!(" WHERE {}", clauses.join(" AND "))
        }
        _ => String::new(),
    };
    let ordering: Vec<String> = op
        .key_columns
        .iter()
        .map(|k| format!("{} ASC", quote_ident(k)))
        .collect();
    params.push(SqlParam::Integer(i64::from(limit)));
    let limit_placeholder = placeholder(dialect, params.len());
    Ok(BoundStatement {
        sql: format!(
            "SELECT {col} FROM {table}{where_sql} ORDER BY {} LIMIT {limit_placeholder}",
            ordering.join(", ")
        ),
        params,
    })
}

/// Renders an expression recursively. Unsupported or unadvertised operators are rejected
/// (`NGC-023`); no predicate is ever silently discarded.
pub fn render_simple_predicate(
    op: &OperationConfig,
    dialect: Dialect,
    predicate: &Expr,
    param_base: usize,
) -> Result<(String, Vec<SqlParam>), NgcErrorInfo> {
    let mut params = Vec::new();
    let mut index = param_base;
    let sql = render_expr(op, dialect, predicate, &mut index, &mut params)?;
    Ok((sql, params))
}

fn render_expr(
    op: &OperationConfig,
    dialect: Dialect,
    expression: &Expr,
    param_index: &mut usize,
    params: &mut Vec<SqlParam>,
) -> Result<String, NgcErrorInfo> {
    match &expression.node {
        Some(expr::Node::And(list)) => {
            render_junction(op, dialect, list, "AND", param_index, params)
        }
        Some(expr::Node::Or(list)) => render_junction(op, dialect, list, "OR", param_index, params),
        Some(expr::Node::Not(child)) => Ok(format!(
            "NOT ({})",
            render_expr(op, dialect, child, param_index, params)?
        )),
        Some(expr::Node::Exists(path)) => {
            let _ = require_predicate_column(op, path)?;
            Ok(format!("{} IS NOT NULL", quote_ident(path)))
        }
        Some(expr::Node::Cmp(cmp)) => {
            let col_def = require_predicate_column(op, &cmp.path)?;
            let column = quote_ident(&cmp.path);
            let operator = PredicateOp::try_from(cmp.op)
                .map_err(|_| invalid(format!("unknown predicate operator {}", cmp.op)))?;
            let value = cmp.value.as_ref();
            match operator {
                PredicateOp::PEq | PredicateOp::PNe if value.is_none() => {
                    Err(invalid("comparison predicate requires a value"))
                }
                PredicateOp::PEq | PredicateOp::PNe
                    if matches!(
                        value.and_then(|value| value.kind.as_ref()),
                        None | Some(prost_types::value::Kind::NullValue(_))
                    ) =>
                {
                    Ok(format!(
                        "{column} {}",
                        if operator == PredicateOp::PEq {
                            "IS NULL"
                        } else {
                            "IS NOT NULL"
                        }
                    ))
                }
                PredicateOp::PEq
                | PredicateOp::PNe
                | PredicateOp::PLt
                | PredicateOp::PLte
                | PredicateOp::PGt
                | PredicateOp::PGte => {
                    let value = value.ok_or_else(|| invalid("comparison requires a value"))?;
                    params.push(prost_to_typed_param(value, &col_def.sql_type)?);
                    *param_index += 1;
                    let sql_operator = match operator {
                        PredicateOp::PEq => "=",
                        PredicateOp::PNe => "!=",
                        PredicateOp::PLt => "<",
                        PredicateOp::PLte => "<=",
                        PredicateOp::PGt => ">",
                        PredicateOp::PGte => ">=",
                        _ => return Err(invalid("invalid comparison operator")),
                    };
                    Ok(format!(
                        "{column} {sql_operator} {}",
                        placeholder(dialect, *param_index)
                    ))
                }
                PredicateOp::PIn => {
                    let Some(prost_types::Value {
                        kind: Some(prost_types::value::Kind::ListValue(list)),
                    }) = value
                    else {
                        return Err(invalid("P_IN requires a list value"));
                    };
                    let values = list
                        .values
                        .iter()
                        .map(|item| prost_to_typed_param(item, &col_def.sql_type))
                        .collect::<Result<Vec<_>, _>>()?;
                    if dialect == Dialect::Postgres {
                        params.push(SqlParam::Array(values));
                        *param_index += 1;
                        Ok(format!(
                            "{column} = ANY({})",
                            placeholder(dialect, *param_index)
                        ))
                    } else if values.is_empty() {
                        Ok("1 = 0".to_owned())
                    } else {
                        let placeholders = values
                            .iter()
                            .map(|_| {
                                *param_index += 1;
                                placeholder(dialect, *param_index)
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        params.extend(values);
                        Ok(format!("{column} IN ({placeholders})"))
                    }
                }
                PredicateOp::PExists => {
                    let exists = match value.and_then(|value| value.kind.as_ref()) {
                        None | Some(prost_types::value::Kind::NullValue(_)) => true,
                        Some(prost_types::value::Kind::BoolValue(exists)) => *exists,
                        _ => return Err(invalid("P_EXISTS value must be boolean")),
                    };
                    Ok(format!(
                        "{column} {}",
                        if exists { "IS NOT NULL" } else { "IS NULL" }
                    ))
                }
                _ => Err(invalid(format!(
                    "predicate operator {operator:?} is not advertised by this connector"
                ))),
            }
        }
        None => Err(invalid("predicate expression must not be empty")),
    }
}

fn require_predicate_column<'a>(
    op: &'a OperationConfig,
    path: &str,
) -> Result<&'a crate::config::ColumnDef, NgcErrorInfo> {
    op.column(path)
        .ok_or_else(|| invalid(format!("unknown predicate column '{path}'")))
}

fn render_junction(
    op: &OperationConfig,
    dialect: Dialect,
    list: &gap_proto::ngc::v1::ExprList,
    junction: &str,
    param_index: &mut usize,
    params: &mut Vec<SqlParam>,
) -> Result<String, NgcErrorInfo> {
    if list.exprs.is_empty() {
        return Err(invalid(format!("empty {junction} predicate")));
    }
    let children = list
        .exprs
        .iter()
        .map(|child| render_expr(op, dialect, child, param_index, params))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!("({})", children.join(&format!(" {junction} "))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColumnDef, OperationKind, SqlType};
    use gap_proto::ngc::v1::{Cmp, ExprList};

    fn make_op() -> OperationConfig {
        OperationConfig {
            name: "users".to_owned(),
            kind: OperationKind::List,
            table: "users".to_owned(),
            columns: vec![
                ColumnDef {
                    name: "id".to_owned(),
                    sql_type: SqlType::Integer,
                    nullable: false,
                },
                ColumnDef {
                    name: "name".to_owned(),
                    sql_type: SqlType::Text,
                    nullable: true,
                },
            ],
            key_columns: vec!["id".to_owned()],
            row_cap: 1000,
            statement_timeout_ms: 5000,
            simulated_latency_ms: 0,
        }
    }

    fn integer(value: i64) -> prost_types::Value {
        prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(value as f64)),
        }
    }

    fn text(value: &str) -> prost_types::Value {
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(value.to_owned())),
        }
    }

    fn comparison(path: &str, operator: PredicateOp, value: prost_types::Value) -> Expr {
        Expr {
            node: Some(expr::Node::Cmp(Cmp {
                path: path.to_owned(),
                op: operator as i32,
                value: Some(value),
            })),
        }
    }

    // spec: CMP-050
    #[test]
    fn get_by_key_binds_values_and_rejects_injected_identifiers() -> Result<(), NgcErrorInfo> {
        let op = make_op();
        let arguments = prost_types::Struct {
            fields: [("id".to_owned(), integer(42))].into_iter().collect(),
        };
        let statement = build_get_by_key(&op, Dialect::Sqlite, &arguments, &[])?;
        assert!(statement.sql.contains("WHERE \"id\" = ?"));
        assert_eq!(statement.params, vec![SqlParam::Integer(42)]);
        assert!(
            build_get_by_key(&op, Dialect::Sqlite, &arguments, &["id; DROP TABLE users"]).is_err()
        );

        let wrong_type = prost_types::Struct {
            fields: [("id".to_owned(), text("not-an-int"))]
                .into_iter()
                .collect(),
        };
        assert!(build_get_by_key(&op, Dialect::Sqlite, &wrong_type, &[]).is_err());
        Ok(())
    }

    // spec: CMP-052
    #[test]
    fn postgres_foreach_uses_one_array_parameter() -> Result<(), NgcErrorInfo> {
        let statement = build_foreach_in(
            &make_op(),
            Dialect::Postgres,
            &[&integer(1), &integer(2)],
            &[],
        )?;
        assert!(statement.sql.contains("= ANY($1)"));
        assert_eq!(
            statement.params,
            vec![SqlParam::Array(vec![
                SqlParam::Integer(1),
                SqlParam::Integer(2)
            ])]
        );
        Ok(())
    }

    // spec: NGC-021
    #[test]
    fn list_adds_deterministic_key_tiebreak() -> Result<(), NgcErrorInfo> {
        let statement = build_list(
            &make_op(),
            Dialect::Sqlite,
            &[],
            None,
            &[("name".to_owned(), true)],
            10,
            0,
        )?;
        assert!(statement.sql.contains("ORDER BY \"name\" DESC, \"id\" ASC"));
        Ok(())
    }

    // spec: NGC-023
    #[test]
    fn predicate_rejects_unknown_columns_and_unadvertised_operators() {
        let unknown = comparison("name; DROP TABLE users", PredicateOp::PEq, text("x"));
        assert!(render_simple_predicate(&make_op(), Dialect::Sqlite, &unknown, 0).is_err());
        let unsupported = comparison("name", PredicateOp::PContains, text("x"));
        assert!(render_simple_predicate(&make_op(), Dialect::Sqlite, &unsupported, 0).is_err());
    }

    // spec: CMP-050
    #[test]
    fn injection_string_remains_a_bound_parameter() -> Result<(), NgcErrorInfo> {
        let predicate = comparison("name", PredicateOp::PEq, text("'; DROP TABLE users; --"));
        let (sql, params) = render_simple_predicate(&make_op(), Dialect::Sqlite, &predicate, 0)?;
        assert_eq!(sql, "\"name\" = ?");
        assert!(!sql.contains("DROP TABLE"));
        assert_eq!(params.len(), 1);
        Ok(())
    }

    // spec: NGC-023
    #[test]
    fn boolean_expression_preserves_grouping_and_parameter_order() -> Result<(), NgcErrorInfo> {
        let predicate = Expr {
            node: Some(expr::Node::Or(ExprList {
                exprs: vec![
                    comparison("id", PredicateOp::PGt, integer(10)),
                    comparison("name", PredicateOp::PEq, text("Ada")),
                ],
            })),
        };
        let (sql, params) = render_simple_predicate(&make_op(), Dialect::Postgres, &predicate, 0)?;
        assert_eq!(sql, "(\"id\" > $1 OR \"name\" = $2)");
        assert_eq!(
            params,
            vec![SqlParam::Integer(10), SqlParam::Text("Ada".to_owned())]
        );
        Ok(())
    }
}
