//! Upstream GraphQL SDL / Introspection normalization, branch `<ns>_<Type>` namespacing, Relay connection detection, and Federation `_entities` detection (`spec/09-compat.md` §5.4, `NGC-012`, `NGC-013`, `GQL-004`).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use apollo_compiler::Schema;
use apollo_compiler::ast::Type as AstType;
use apollo_compiler::schema::ExtendedType;
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_core::ids::{is_allowed_graphql_identifier, is_reserved_builtin_type};
use gap_ngc_sdk::connector::NgcResult;
use gap_ngc_sdk::error::json_map_to_prost_struct;
use gap_proto::ngc::v1::{OpKind, OperationDescriptor, PaginationKind, SideEffectHint};
use serde_json::{Map, Value, json};

use crate::config::{GraphqlConfig, MutationSideEffectOverride, UpstreamSchemaSource};

/// Standard GraphQL introspection query sent when `UpstreamSchemaSource::LiveIntrospection` is configured.
pub const INTROSPECTION_QUERY: &str = r#"
query GapIntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    subscriptionType { name }
    types {
      kind
      name
      description
      fields(includeDeprecated: true) {
        name
        description
        args {
          name
          description
          type { ...TypeRef }
          defaultValue
        }
        type { ...TypeRef }
      }
      inputFields {
        name
        description
        type { ...TypeRef }
        defaultValue
      }
      interfaces { name }
      enumValues(includeDeprecated: true) {
        name
        description
      }
      possibleTypes { name }
    }
  }
}

fragment TypeRef on __Type {
  kind
  name
  ofType {
    kind
    name
    ofType {
      kind
      name
      ofType {
        kind
        name
        ofType {
          kind
          name
          ofType {
            kind
            name
            ofType {
              kind
              name
            }
          }
        }
      }
    }
  }
}
"#;

const BUILTIN_SCALARS: &[&str] = &["String", "Int", "Float", "Boolean", "ID"];
const JSON_SCHEMA_DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// Relay Connection metadata detected on an upstream Query field (`spec/09-compat.md` §5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayConnectionInfo {
    /// Upstream connection type name (e.g., `"ProductConnection"`).
    pub connection_type: String,
    /// Upstream edge type name (e.g., `"ProductEdge"`).
    pub edge_type: String,
    /// Upstream unwrapped node type name (e.g., `"Product"`).
    pub node_type: String,
    /// Upstream `pageInfo` type name (e.g., `"PageInfo"`).
    pub page_info_type: String,
    /// Query field accepts Relay `first` argument.
    pub has_first_arg: bool,
    /// Query field accepts Relay `after` argument.
    pub has_after_arg: bool,
}

/// Extracted GraphQL argument definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphqlArgDef {
    /// Argument name.
    pub name: String,
    /// Upstream GraphQL type string (e.g., `"ID!"`, `"[String!]"`, `"ReserveStockInput!"`).
    pub upstream_type: String,
    /// Branch-namespaced GraphQL type string.
    pub branch_type: String,
    /// True if top-level type is non-null (`!`).
    pub required: bool,
}

/// Compiled metadata for one extracted root GraphQL Query or Mutation field.
#[derive(Debug, Clone)]
pub struct ExtractedGraphqlOp {
    /// Operation name exposed to GAP (matches root GraphQL field name).
    pub name: String,
    /// Query vs Mutation.
    pub kind: OpKind,
    /// Ordered arguments of this root field.
    pub args: Vec<GraphqlArgDef>,
    /// Upstream unwrapped return type name (e.g. `"Product"` or `"ProductConnection"`).
    pub return_named_type: String,
    /// True when the return type is an Object/Interface/Union supporting subfields; false for scalars/enums.
    pub returns_composite: bool,
    /// True when top-level return is a list.
    pub returns_list: bool,
    /// Present when this operation returns a Relay Connection (`edges { node cursor } pageInfo`).
    pub relay: Option<RelayConnectionInfo>,
    /// Present when Apollo `_entities(representations: $reps)` batching applies for `foreach`.
    pub entity_batch: Option<crate::config::EntityBatchMapping>,
    /// Prebuilt NGC `OperationDescriptor`.
    pub descriptor: OperationDescriptor,
}

/// Complete normalized GraphQL schema bundle (`spec/09-compat.md` §5.4).
#[derive(Debug, Clone)]
pub struct GraphqlSchemaBundle {
    /// Normalized branch-namespaced GAP-QL SDL (`<ns>_<Type>`).
    pub schema_sdl: String,
    /// Deterministic `blake3` hash of the canonical upstream schema.
    pub spec_hash: String,
    /// Upstream root `Query` type name (usually `"Query"`).
    pub upstream_query_root: Option<String>,
    /// Upstream root `Mutation` type name (usually `"Mutation"`).
    pub upstream_mutation_root: Option<String>,
    /// Map from branch-namespaced type `<ns>_<Type>` to upstream type `<Type>`.
    pub branch_to_upstream_type: BTreeMap<String, String>,
    /// Map from upstream type `<Type>` to branch-namespaced `<ns>_<Type>`.
    pub upstream_to_branch_type: BTreeMap<String, String>,
    /// Extracted operations keyed by operation name.
    pub operations: BTreeMap<String, ExtractedGraphqlOp>,
    /// Ordered NGC operation descriptors.
    pub descriptors: Vec<OperationDescriptor>,
    /// Whether upstream supports Apollo Federation `_entities(representations: [_Any!]!): [_Entity]!`.
    pub supports_federation_entities: bool,
    /// Upstream object types belonging to Federation `_Entity` union.
    pub federation_entity_types: BTreeSet<String>,
    /// Parsed upstream `apollo_compiler::Schema`.
    pub upstream_schema: Schema,
}

/// Converts [`UpstreamSchemaSource`] JSON or SDL into a normalized [`GraphqlSchemaBundle`].
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` if SDL/introspection JSON is malformed or `namespace` is empty.
pub fn extract_graphql_schema(
    source: &UpstreamSchemaSource,
    config: &GraphqlConfig,
) -> NgcResult<GraphqlSchemaBundle> {
    let raw_sdl = match source {
        UpstreamSchemaSource::Sdl(sdl) => sdl.clone(),
        UpstreamSchemaSource::IntrospectionJson(val) => introspection_to_sdl(val)?,
        UpstreamSchemaSource::LiveIntrospection => {
            return Err(NgcErrorInfo::new(
                NgcErrorCode::FailedPrecondition,
                "LiveIntrospection requires fetching __schema JSON before extraction",
            ));
        }
    };
    build_bundle_from_sdl(&raw_sdl, config)
}

/// Parses an upstream GraphQL SDL document into a [`GraphqlSchemaBundle`] (`spec/09-compat.md` §5.4).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` when SDL fails parsing or `config.branch_namespace` is invalid.
pub fn build_bundle_from_sdl(
    upstream_sdl: &str,
    config: &GraphqlConfig,
) -> NgcResult<GraphqlSchemaBundle> {
    let ns = sanitize_namespace(&config.branch_namespace);
    if ns.is_empty() {
        return Err(NgcErrorInfo::new(
            NgcErrorCode::InvalidArgument,
            "GraphqlConfig.branch_namespace must contain at least one ASCII alphanumeric character",
        ));
    }

    let mut parser = apollo_compiler::parser::Parser::new()
        .recursion_limit(256)
        .token_limit(100_000);
    let upstream_schema = parser
        .parse_schema(upstream_sdl, "upstream.graphql")
        .map_err(|with_errs| {
            NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("failed to parse upstream GraphQL SDL: {}", with_errs.errors),
            )
        })?;

    let spec_hash = blake3::hash(upstream_sdl.as_bytes()).to_hex().to_string();

    let query_root_name = upstream_schema
        .schema_definition
        .query
        .as_ref()
        .map(|n| n.name.to_string())
        .or_else(|| {
            upstream_schema
                .types
                .contains_key("Query")
                .then(|| "Query".to_owned())
        });
    let mutation_root_name = upstream_schema
        .schema_definition
        .mutation
        .as_ref()
        .map(|n| n.name.to_string())
        .or_else(|| {
            upstream_schema
                .types
                .contains_key("Mutation")
                .then(|| "Mutation".to_owned())
        });
    let subscription_root_name = upstream_schema
        .schema_definition
        .subscription
        .as_ref()
        .map(|n| n.name.to_string())
        .or_else(|| {
            upstream_schema
                .types
                .contains_key("Subscription")
                .then(|| "Subscription".to_owned())
        });

    let mut root_names = BTreeSet::new();
    if let Some(q) = &query_root_name {
        root_names.insert(q.as_str());
    }
    if let Some(m) = &mutation_root_name {
        root_names.insert(m.as_str());
    }
    if let Some(s) = &subscription_root_name {
        root_names.insert(s.as_str());
    }

    // Build bidirectional type namespace mapping (<ns>_<Type>).
    let mut upstream_to_branch_type = BTreeMap::new();
    let mut branch_to_upstream_type = BTreeMap::new();
    let mut used_branch_types = BTreeSet::new();

    for (name, ext_ty) in &upstream_schema.types {
        let up_name = name.as_str();
        if up_name.starts_with("__")
            || BUILTIN_SCALARS.contains(&up_name)
            || root_names.contains(up_name)
            || is_federation_internal_type(up_name)
        {
            continue;
        }
        // Keep GAP `JSON` scalar unprefixed if already JSON scalar, otherwise prefix cleanly.
        if matches!(ext_ty, ExtendedType::Scalar(_)) && up_name == "JSON" {
            upstream_to_branch_type.insert("JSON".to_owned(), "JSON".to_owned());
            branch_to_upstream_type.insert("JSON".to_owned(), "JSON".to_owned());
            continue;
        }
        let br_name = make_branch_type_name(&ns, up_name, &mut used_branch_types);
        upstream_to_branch_type.insert(up_name.to_owned(), br_name.clone());
        branch_to_upstream_type.insert(br_name, up_name.to_owned());
    }

    // Check Federation `_entities` support.
    let federation_entity_types = detect_federation_entities(&upstream_schema);
    let supports_federation_entities = !federation_entity_types.is_empty()
        && query_root_name
            .as_deref()
            .and_then(|q| upstream_schema.get_object(q))
            .is_some_and(|obj| obj.fields.contains_key("_entities"));

    let mut operations = BTreeMap::new();
    let mut descriptors = Vec::new();
    let mut query_sdl_fields = Vec::new();
    let mut mutation_sdl_fields = Vec::new();

    if let Some(q_root) = &query_root_name
        && let Some(q_obj) = upstream_schema.get_object(q_root.as_str())
    {
        for (fname, fcomp) in &q_obj.fields {
            let op_name = fname.as_str();
            if op_name == "_entities" || op_name == "_service" || op_name.starts_with("__") {
                continue;
            }
            let op = extract_root_operation(
                op_name,
                OpKind::Query,
                fcomp,
                &upstream_schema,
                &upstream_to_branch_type,
                supports_federation_entities,
                &federation_entity_types,
                config,
            );
            query_sdl_fields.push(format_field_sdl(fcomp, &upstream_to_branch_type));
            descriptors.push(op.descriptor.clone());
            operations.insert(op_name.to_owned(), op);
        }
    }

    if !config.read_only
        && let Some(m_root) = &mutation_root_name
        && let Some(m_obj) = upstream_schema.get_object(m_root.as_str())
    {
        for (fname, fcomp) in &m_obj.fields {
            let op_name = fname.as_str();
            if op_name.starts_with("__") {
                continue;
            }
            let op = extract_root_operation(
                op_name,
                OpKind::Mutation,
                fcomp,
                &upstream_schema,
                &upstream_to_branch_type,
                supports_federation_entities,
                &federation_entity_types,
                config,
            );
            mutation_sdl_fields.push(format_field_sdl(fcomp, &upstream_to_branch_type));
            descriptors.push(op.descriptor.clone());
            operations.insert(op_name.to_owned(), op);
        }
    }

    let schema_sdl = render_namespaced_sdl(
        &upstream_schema,
        &root_names,
        &upstream_to_branch_type,
        &query_sdl_fields,
        &mutation_sdl_fields,
    );

    Ok(GraphqlSchemaBundle {
        schema_sdl,
        spec_hash,
        upstream_query_root: query_root_name,
        upstream_mutation_root: mutation_root_name,
        branch_to_upstream_type,
        upstream_to_branch_type,
        operations,
        descriptors,
        supports_federation_entities,
        federation_entity_types,
        upstream_schema,
    })
}

fn sanitize_namespace(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if ch == '_' && !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    if out.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        out.insert(0, 'b');
    }
    out
}

fn is_federation_internal_type(name: &str) -> bool {
    matches!(name, "_Any" | "_Entity" | "_Service" | "_FieldSet")
}

fn make_branch_type_name(ns: &str, raw_name: &str, used: &mut BTreeSet<String>) -> String {
    let trimmed = raw_name.trim_start_matches('_');
    let base = format!("{ns}_{trimmed}");
    let cand = if is_allowed_graphql_identifier(&base)
        && !is_reserved_builtin_type(&base)
        && !matches!(
            base.as_str(),
            "Query" | "Mutation" | "Subscription" | "String" | "Int" | "Float" | "Boolean" | "ID"
        ) {
        base
    } else {
        format!("gql_{base}")
    };
    if used.insert(cand.clone()) {
        return cand;
    }
    for i in 2_u32.. {
        let alt = format!("{cand}_{i}");
        if used.insert(alt.clone()) {
            return alt;
        }
    }
    cand
}

fn detect_federation_entities(schema: &Schema) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(ExtendedType::Union(u)) = schema.types.get("_Entity") {
        for member in &u.members {
            out.insert(member.name.to_string());
        }
    }
    out
}

fn unwrap_ast_type(ty: &AstType) -> (String, bool, bool) {
    fn walk(t: &AstType, is_list: bool, top_non_null: bool) -> (String, bool, bool) {
        match t {
            AstType::Named(n) => (n.to_string(), is_list, top_non_null),
            AstType::NonNullNamed(n) => (n.to_string(), is_list, top_non_null),
            AstType::List(inner) | AstType::NonNullList(inner) => walk(inner, true, top_non_null),
        }
    }
    let required = matches!(ty, AstType::NonNullNamed(_) | AstType::NonNullList(_));
    walk(ty, false, required)
}

fn format_ast_type(ty: &AstType, map: &BTreeMap<String, String>) -> String {
    match ty {
        AstType::Named(n) => map
            .get(n.as_str())
            .cloned()
            .unwrap_or_else(|| n.to_string()),
        AstType::NonNullNamed(n) => {
            let mapped = map
                .get(n.as_str())
                .cloned()
                .unwrap_or_else(|| n.to_string());
            format!("{mapped}!")
        }
        AstType::List(inner) => format!("[{}]", format_ast_type(inner, map)),
        AstType::NonNullList(inner) => format!("[{}]!", format_ast_type(inner, map)),
    }
}

fn format_field_sdl(
    field: &apollo_compiler::schema::Component<apollo_compiler::ast::FieldDefinition>,
    map: &BTreeMap<String, String>,
) -> String {
    let args = if field.arguments.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = field
            .arguments
            .iter()
            .map(|a| format!("{}: {}", a.name, format_ast_type(&a.ty, map)))
            .collect();
        format!("({})", parts.join(", "))
    };
    format!(
        "  {}{}: {}",
        field.name,
        args,
        format_ast_type(&field.ty, map)
    )
}

#[allow(clippy::too_many_arguments)]
fn extract_root_operation(
    op_name: &str,
    kind: OpKind,
    field: &apollo_compiler::schema::Component<apollo_compiler::ast::FieldDefinition>,
    schema: &Schema,
    type_map: &BTreeMap<String, String>,
    supports_federation_entities: bool,
    federation_entity_types: &BTreeSet<String>,
    config: &GraphqlConfig,
) -> ExtractedGraphqlOp {
    let empty_map = BTreeMap::new();
    let mut args = Vec::with_capacity(field.arguments.len());
    for arg in &field.arguments {
        let (_, _, required) = unwrap_ast_type(&arg.ty);
        args.push(GraphqlArgDef {
            name: arg.name.to_string(),
            upstream_type: format_ast_type(&arg.ty, &empty_map),
            branch_type: format_ast_type(&arg.ty, type_map),
            required,
        });
    }

    let (return_named_type, returns_list, _) = unwrap_ast_type(&field.ty);
    let returns_composite = matches!(
        schema.types.get(return_named_type.as_str()),
        Some(ExtendedType::Object(_) | ExtendedType::Interface(_) | ExtendedType::Union(_))
    );

    let relay = if kind == OpKind::Query && !returns_list {
        detect_relay_connection(schema, &return_named_type, &args)
    } else {
        None
    };

    let entity_batch = if kind == OpKind::Query && config.prefer_federation_entities {
        config.entity_mappings.get(op_name).cloned().or_else(|| {
            if supports_federation_entities
                && federation_entity_types.contains(&return_named_type)
                && !returns_list
                && !args.is_empty()
            {
                Some(crate::config::EntityBatchMapping {
                    upstream_typename: return_named_type.clone(),
                    key_fields: args.iter().map(|a| a.name.clone()).collect(),
                })
            } else {
                None
            }
        })
    } else {
        None
    };

    let pagination = if relay.is_some() {
        PaginationKind::PaginationCursor
    } else {
        PaginationKind::PaginationNone
    };

    let (side_effect_hint, idempotent_hint) = match kind {
        OpKind::Query => (SideEffectHint::SeNone, true),
        _ => match config.mutation_side_effects.get(op_name) {
            Some(MutationSideEffectOverride::Reversible) => (SideEffectHint::SeReversible, false),
            Some(MutationSideEffectOverride::Irreversible) => {
                (SideEffectHint::SeIrreversible, false)
            }
            None => (SideEffectHint::SideEffectUnspecified, false),
        },
    };

    let input_json_schema = build_input_json_schema(op_name, schema, field);
    let output_json_schema = build_type_json_schema(schema, &return_named_type);

    let input_struct = match &input_json_schema {
        Value::Object(m) => json_map_to_prost_struct(m),
        _ => prost_types::Struct::default(),
    };
    let output_struct = match &output_json_schema {
        Value::Object(m) => json_map_to_prost_struct(m),
        _ => prost_types::Struct::default(),
    };

    let mut annotations = HashMap::new();
    annotations.insert(
        "graphql.return_type".to_owned(),
        type_map
            .get(&return_named_type)
            .cloned()
            .unwrap_or_else(|| return_named_type.clone()),
    );
    annotations.insert(
        "graphql.upstream_type".to_owned(),
        return_named_type.clone(),
    );
    if let Some(r) = &relay {
        annotations.insert("graphql.relay_connection".to_owned(), "true".to_owned());
        annotations.insert("graphql.relay_node_type".to_owned(), r.node_type.clone());
    }
    annotations.insert(
        "batch".to_owned(),
        if entity_batch.is_some() {
            "apollo_entities".to_owned()
        } else {
            "aliased_root_fields".to_owned()
        },
    );

    let key_fields = if let Some(eb) = &entity_batch {
        eb.key_fields.clone()
    } else {
        args.iter().map(|a| a.name.clone()).collect()
    };

    let descriptor = OperationDescriptor {
        name: op_name.to_owned(),
        kind: kind.into(),
        description: field
            .description
            .as_ref()
            .map_or_else(|| format!("GraphQL field {op_name}"), |d| d.to_string()),
        input_schema: Some(input_struct),
        output_schema: Some(output_struct),
        side_effect_hint: side_effect_hint.into(),
        idempotent_hint,
        supports_dry_run: kind == OpKind::Mutation && config.supports_dry_run,
        compensation_operation: String::new(),
        examples: Vec::new(),
        cost_weight: 2,
        pagination: pagination.into(),
        key_fields,
        annotations,
    };

    ExtractedGraphqlOp {
        name: op_name.to_owned(),
        kind,
        args,
        return_named_type,
        returns_composite,
        returns_list,
        relay,
        entity_batch,
        descriptor,
    }
}

fn detect_relay_connection(
    schema: &Schema,
    type_name: &str,
    args: &[GraphqlArgDef],
) -> Option<RelayConnectionInfo> {
    let conn_obj = schema.get_object(type_name)?;
    let edges_field = conn_obj.fields.get("edges")?;
    let page_info_field = conn_obj.fields.get("pageInfo")?;

    let (edge_type_name, edges_is_list, _) = unwrap_ast_type(&edges_field.ty);
    if !edges_is_list {
        return None;
    }
    let edge_obj = schema.get_object(&edge_type_name)?;
    let node_field = edge_obj.fields.get("node")?;
    let _cursor_field = edge_obj.fields.get("cursor")?;
    let (node_type, _, _) = unwrap_ast_type(&node_field.ty);

    let (page_info_type_name, _, _) = unwrap_ast_type(&page_info_field.ty);
    let page_info_obj = schema.get_object(&page_info_type_name)?;
    page_info_obj.fields.get("hasNextPage")?;
    page_info_obj.fields.get("endCursor")?;

    let has_first_arg = args.iter().any(|a| a.name == "first");
    let has_after_arg = args.iter().any(|a| a.name == "after");

    Some(RelayConnectionInfo {
        connection_type: type_name.to_owned(),
        edge_type: edge_type_name,
        node_type,
        page_info_type: page_info_type_name,
        has_first_arg,
        has_after_arg,
    })
}

fn render_namespaced_sdl(
    schema: &Schema,
    root_names: &BTreeSet<&str>,
    type_map: &BTreeMap<String, String>,
    query_sdl_fields: &[String],
    mutation_sdl_fields: &[String],
) -> String {
    let mut out = String::new();
    let mut names: Vec<_> = schema.types.keys().collect();
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    for name in names {
        let up = name.as_str();
        if up.starts_with("__")
            || BUILTIN_SCALARS.contains(&up)
            || root_names.contains(up)
            || is_federation_internal_type(up)
        {
            continue;
        }
        let Some(br_name) = type_map.get(up) else {
            continue;
        };
        let Some(ext) = schema.types.get(up) else {
            continue;
        };
        match ext {
            ExtendedType::Scalar(_) => {
                if br_name != "JSON" {
                    out.push_str(&format!("scalar {br_name}\n\n"));
                }
            }
            ExtendedType::Enum(e) => {
                let vals: Vec<String> = e.values.keys().map(|v| format!("  {v}")).collect();
                out.push_str(&format!("enum {br_name} {{\n{}\n}}\n\n", vals.join("\n")));
            }
            ExtendedType::Union(u) => {
                let members: Vec<String> = u
                    .members
                    .iter()
                    .map(|m| {
                        type_map
                            .get(m.name.as_str())
                            .cloned()
                            .unwrap_or_else(|| m.name.to_string())
                    })
                    .collect();
                if !members.is_empty() {
                    out.push_str(&format!("union {br_name} = {}\n\n", members.join(" | ")));
                }
            }
            ExtendedType::Interface(i) => {
                let fields: Vec<String> = i
                    .fields
                    .values()
                    .map(|f| format_field_sdl(f, type_map))
                    .collect();
                if !fields.is_empty() {
                    out.push_str(&format!(
                        "interface {br_name} {{\n{}\n}}\n\n",
                        fields.join("\n")
                    ));
                }
            }
            ExtendedType::InputObject(inp) => {
                let fields: Vec<String> = inp
                    .fields
                    .values()
                    .map(|f| format!("  {}: {}", f.name, format_ast_type(&f.ty, type_map)))
                    .collect();
                if !fields.is_empty() {
                    out.push_str(&format!(
                        "input {br_name} {{\n{}\n}}\n\n",
                        fields.join("\n")
                    ));
                }
            }
            ExtendedType::Object(obj) => {
                let impls = if obj.implements_interfaces.is_empty() {
                    String::new()
                } else {
                    let mapped: Vec<String> = obj
                        .implements_interfaces
                        .iter()
                        .map(|n| {
                            type_map
                                .get(n.name.as_str())
                                .cloned()
                                .unwrap_or_else(|| n.name.to_string())
                        })
                        .collect();
                    format!(" implements {}", mapped.join(" & "))
                };
                let fields: Vec<String> = obj
                    .fields
                    .values()
                    .map(|f| format_field_sdl(f, type_map))
                    .collect();
                if !fields.is_empty() {
                    out.push_str(&format!(
                        "type {br_name}{impls} {{\n{}\n}}\n\n",
                        fields.join("\n")
                    ));
                }
            }
        }
    }

    if !query_sdl_fields.is_empty() {
        out.push_str(&format!(
            "type Query {{\n{}\n}}\n\n",
            query_sdl_fields.join("\n")
        ));
    }
    if !mutation_sdl_fields.is_empty() {
        out.push_str(&format!(
            "type Mutation {{\n{}\n}}\n\n",
            mutation_sdl_fields.join("\n")
        ));
    }
    out
}

fn build_input_json_schema(
    op_name: &str,
    schema: &Schema,
    field: &apollo_compiler::schema::Component<apollo_compiler::ast::FieldDefinition>,
) -> Value {
    let mut props = Map::new();
    let mut req = Vec::new();
    let mut visiting = BTreeSet::new();
    for arg in &field.arguments {
        let (_, _, non_null) = unwrap_ast_type(&arg.ty);
        if non_null {
            req.push(Value::String(arg.name.to_string()));
        }
        props.insert(
            arg.name.to_string(),
            ast_type_to_json_schema(schema, &arg.ty, &mut visiting),
        );
    }
    let mut root = Map::new();
    root.insert(
        "$schema".to_owned(),
        Value::String(JSON_SCHEMA_DRAFT_2020_12.to_owned()),
    );
    root.insert("title".to_owned(), Value::String(format!("{op_name}Args")));
    root.insert("type".to_owned(), Value::String("object".to_owned()));
    root.insert("properties".to_owned(), Value::Object(props));
    if !req.is_empty() {
        root.insert("required".to_owned(), Value::Array(req));
    }
    Value::Object(root)
}

fn build_type_json_schema(schema: &Schema, type_name: &str) -> Value {
    let mut visiting = BTreeSet::new();
    let mut v = named_type_to_json_schema(schema, type_name, &mut visiting);
    if let Value::Object(ref mut m) = v {
        m.insert(
            "$schema".to_owned(),
            Value::String(JSON_SCHEMA_DRAFT_2020_12.to_owned()),
        );
    }
    v
}

fn ast_type_to_json_schema(
    schema: &Schema,
    ty: &AstType,
    visiting: &mut BTreeSet<String>,
) -> Value {
    match ty {
        AstType::Named(n) | AstType::NonNullNamed(n) => {
            named_type_to_json_schema(schema, n.as_str(), visiting)
        }
        AstType::List(inner) | AstType::NonNullList(inner) => {
            json!({
                "type": "array",
                "items": ast_type_to_json_schema(schema, inner, visiting),
            })
        }
    }
}

fn named_type_to_json_schema(
    schema: &Schema,
    name: &str,
    visiting: &mut BTreeSet<String>,
) -> Value {
    match name {
        "String" | "ID" => json!({ "type": "string" }),
        "Int" => json!({ "type": "integer" }),
        "Float" => json!({ "type": "number" }),
        "Boolean" => json!({ "type": "boolean" }),
        "JSON" => json!({ "type": "object" }),
        other => match schema.types.get(other) {
            Some(ExtendedType::Scalar(_)) => json!({ "type": "string" }),
            Some(ExtendedType::Enum(e)) => {
                let vals: Vec<Value> = e
                    .values
                    .keys()
                    .map(|k| Value::String(k.to_string()))
                    .collect();
                json!({ "type": "string", "enum": vals })
            }
            Some(ExtendedType::InputObject(inp)) => {
                if !visiting.insert(other.to_owned()) {
                    return json!({ "type": "object" });
                }
                let mut props = Map::new();
                for f in inp.fields.values() {
                    props.insert(
                        f.name.to_string(),
                        ast_type_to_json_schema(schema, &f.ty, visiting),
                    );
                }
                visiting.remove(other);
                json!({ "type": "object", "properties": props })
            }
            Some(ExtendedType::Object(obj)) => {
                if !visiting.insert(other.to_owned()) {
                    return json!({ "type": "object" });
                }
                let mut props = Map::new();
                for f in obj.fields.values() {
                    props.insert(
                        f.name.to_string(),
                        ast_type_to_json_schema(schema, &f.ty, visiting),
                    );
                }
                visiting.remove(other);
                json!({ "type": "object", "properties": props })
            }
            _ => json!({ "type": "object" }),
        },
    }
}

/// Renders an upstream GraphQL `__schema` introspection JSON payload into SDL text (`spec/09-compat.md` §5.4).
///
/// # Errors
///
/// Returns `INVALID_ARGUMENT` / `UPSTREAM_ERROR` if the JSON lacks `__schema.types`.
pub fn introspection_to_sdl(val: &Value) -> NgcResult<String> {
    let schema_obj = val
        .get("data")
        .and_then(|d| d.get("__schema"))
        .or_else(|| val.get("__schema"))
        .unwrap_or(val);

    let types_arr = schema_obj
        .get("types")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                "introspection JSON missing `__schema.types` array",
            )
        })?;

    let q_name = schema_obj
        .get("queryType")
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("Query");
    let m_name = schema_obj
        .get("mutationType")
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str);
    let s_name = schema_obj
        .get("subscriptionType")
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str);

    let mut out = String::new();
    let mut needs_schema_block = false;
    if q_name != "Query"
        || m_name.is_some_and(|n| n != "Mutation")
        || s_name.is_some_and(|n| n != "Subscription")
    {
        needs_schema_block = true;
    }
    if needs_schema_block {
        out.push_str("schema {\n");
        out.push_str(&format!("  query: {q_name}\n"));
        if let Some(m) = m_name {
            out.push_str(&format!("  mutation: {m}\n"));
        }
        if let Some(s) = s_name {
            out.push_str(&format!("  subscription: {s}\n"));
        }
        out.push_str("}\n\n");
    }

    for ty in types_arr {
        let Some(name) = ty.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.starts_with("__") || BUILTIN_SCALARS.contains(&name) {
            continue;
        }
        let kind = ty.get("kind").and_then(Value::as_str).unwrap_or("");
        match kind {
            "SCALAR" => {
                out.push_str(&format!("scalar {name}\n\n"));
            }
            "ENUM" => {
                let vals = ty
                    .get("enumValues")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.get("name").and_then(Value::as_str))
                            .map(|n| format!("  {n}"))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !vals.is_empty() {
                    out.push_str(&format!("enum {name} {{\n{}\n}}\n\n", vals.join("\n")));
                }
            }
            "UNION" => {
                let members = ty
                    .get("possibleTypes")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| m.get("name").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !members.is_empty() {
                    out.push_str(&format!("union {name} = {}\n\n", members.join(" | ")));
                }
            }
            "INPUT_OBJECT" => {
                let fields = ty
                    .get("inputFields")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|f| {
                                let fnm = f.get("name").and_then(Value::as_str)?;
                                let fty = render_introspection_type_ref(f.get("type")?).ok()?;
                                Some(format!("  {fnm}: {fty}"))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !fields.is_empty() {
                    out.push_str(&format!("input {name} {{\n{}\n}}\n\n", fields.join("\n")));
                }
            }
            "INTERFACE" | "OBJECT" => {
                let kw = if kind == "INTERFACE" {
                    "interface"
                } else {
                    "type"
                };
                let impls = ty
                    .get("interfaces")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|i| i.get("name").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                    })
                    .filter(|v| !v.is_empty())
                    .map_or_else(String::new, |v| format!(" implements {}", v.join(" & ")));
                let fields = ty
                    .get("fields")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|f| {
                                let fnm = f.get("name").and_then(Value::as_str)?;
                                let fty = render_introspection_type_ref(f.get("type")?).ok()?;
                                let args = f
                                    .get("args")
                                    .and_then(Value::as_array)
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|arg| {
                                                let anm =
                                                    arg.get("name").and_then(Value::as_str)?;
                                                let aty =
                                                    render_introspection_type_ref(arg.get("type")?)
                                                        .ok()?;
                                                Some(format!("{anm}: {aty}"))
                                            })
                                            .collect::<Vec<_>>()
                                    })
                                    .filter(|v| !v.is_empty())
                                    .map_or_else(String::new, |v| format!("({})", v.join(", ")));
                                Some(format!("  {fnm}{args}: {fty}"))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !fields.is_empty() {
                    out.push_str(&format!(
                        "{kw} {name}{impls} {{\n{}\n}}\n\n",
                        fields.join("\n")
                    ));
                }
            }
            _ => {}
        }
    }

    Ok(out)
}

fn render_introspection_type_ref(t: &Value) -> NgcResult<String> {
    let kind = t.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "NON_NULL" => {
            let inner = t.get("ofType").ok_or_else(|| {
                NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "introspection NON_NULL type missing ofType",
                )
            })?;
            Ok(format!("{}!", render_introspection_type_ref(inner)?))
        }
        "LIST" => {
            let inner = t.get("ofType").ok_or_else(|| {
                NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "introspection LIST type missing ofType",
                )
            })?;
            Ok(format!("[{}]", render_introspection_type_ref(inner)?))
        }
        _ => t
            .get("name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                NgcErrorInfo::new(
                    NgcErrorCode::InvalidArgument,
                    "introspection named type missing `name`",
                )
            }),
    }
}
