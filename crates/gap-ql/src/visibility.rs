//! Caller-scoped schema visibility projection (`GQL-062`), filtered GraphQL introspection
//! (`GQL-024`), and oracle-free validation helpers (`spec/02-gap-ql.md` §§4, 7, 9).

use std::collections::{HashMap, HashSet};

use apollo_compiler::Name;
use apollo_compiler::Schema;
use apollo_compiler::ast::Value;
use apollo_compiler::introspection::{check_max_depth, partial_execute};
use apollo_compiler::schema::ExtendedType;
use apollo_compiler::validation::Valid;
use gap_core::error::{ErrorCode, GapError, GapResult};

use crate::validate::validate_against_projected_schema;

/// Caller visibility predicate evaluated during schema projection, validation, self-repair hint
/// generation, and GraphQL introspection (`GQL-062`, `GQL-070`, `INV-05`).
///
/// Elements for which this predicate returns `false` behave identically to elements that do not
/// exist in the supergraph schema (zero existence oracle).
pub trait Visibility: Send + Sync {
    /// Returns `true` if the named GraphQL type is visible to the caller (`Discover` action).
    fn can_see_type(&self, type_name: &str) -> bool {
        let _ = type_name;
        true
    }

    /// Returns `true` if `field_name` on object or interface `parent_type` is visible.
    fn can_see_field(&self, parent_type: &str, field_name: &str) -> bool;

    /// Returns `true` if `arg_name` on `parent_type.field_name` is visible.
    fn can_see_argument(&self, parent_type: &str, field_name: &str, arg_name: &str) -> bool {
        let _ = (parent_type, field_name, arg_name);
        true
    }

    /// Returns `true` if `input_field` on `input_type` is visible.
    fn can_see_input_field(&self, input_type: &str, input_field: &str) -> bool {
        let _ = (input_type, input_field);
        true
    }

    /// Returns `true` if `enum_value` on `enum_type` is visible.
    fn can_see_enum_value(&self, enum_type: &str, enum_value: &str) -> bool {
        let _ = (enum_type, enum_value);
        true
    }

    /// Returns `true` if the caller holds `admin:schema`, allowing `PROPOSED`, `VERIFIED`, and
    /// `DEPRECATED` `@join` fields to appear in introspection (`GQL-031`, `INV-06`).
    fn has_admin_schema(&self) -> bool {
        false
    }

    /// Optional hook returning up to 3 already-visibility-filtered `__search` candidate field names
    /// when edit-distance `<= 2` produces additional suggestions (`GQL-070`).
    fn search_visible_names(&self, parent_type: &str, unknown_name: &str) -> Vec<String> {
        let _ = (parent_type, unknown_name);
        Vec::new()
    }
}

/// Permissive [`Visibility`] implementation that allows all types, fields, arguments, input fields,
/// and enum values (`has_admin_schema` defaults to `false` unless constructed with
/// [`Self::with_admin_schema`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AllowAllVisibility {
    admin_schema: bool,
}

impl AllowAllVisibility {
    /// Constructs an [`AllowAllVisibility`] with explicit `admin:schema` privilege (`GQL-031`).
    #[must_use]
    pub const fn with_admin_schema(admin_schema: bool) -> Self {
        Self { admin_schema }
    }
}

impl Visibility for AllowAllVisibility {
    fn can_see_field(&self, _parent_type: &str, _field_name: &str) -> bool {
        true
    }

    fn has_admin_schema(&self) -> bool {
        self.admin_schema
    }
}

/// Closure-backed [`Visibility`] adapter over `Fn(&str, &str) -> bool` (`(parent_type, field_name)`).
pub struct FnVisibility<F> {
    predicate: F,
    admin_schema: bool,
}

impl<F> FnVisibility<F>
where
    F: Fn(&str, &str) -> bool + Send + Sync,
{
    /// Wraps a `(parent_type, field_name) -> bool` closure as a [`Visibility`] implementation.
    #[must_use]
    pub const fn new(predicate: F) -> Self {
        Self {
            predicate,
            admin_schema: false,
        }
    }

    /// Sets whether the caller holds `admin:schema` (`GQL-031`).
    #[must_use]
    pub const fn with_admin_schema(mut self, admin_schema: bool) -> Self {
        self.admin_schema = admin_schema;
        self
    }
}

impl<F> Visibility for FnVisibility<F>
where
    F: Fn(&str, &str) -> bool + Send + Sync,
{
    fn can_see_field(&self, parent_type: &str, field_name: &str) -> bool {
        (self.predicate)(parent_type, field_name)
    }

    fn has_admin_schema(&self) -> bool {
        self.admin_schema
    }
}

/// Set-based [`Visibility`] policy hiding explicit types, fields, arguments, input fields, and
/// enum values while optionally supplying visible `__search` fallback hits.
#[derive(Debug, Default, Clone)]
pub struct HiddenSetVisibility {
    hidden_types: HashSet<String>,
    hidden_fields: HashSet<(String, String)>,
    hidden_arguments: HashSet<(String, String, String)>,
    hidden_input_fields: HashSet<(String, String)>,
    hidden_enum_values: HashSet<(String, String)>,
    search_hits: HashMap<String, Vec<String>>,
    admin_schema: bool,
}

impl HiddenSetVisibility {
    /// Creates an empty [`HiddenSetVisibility`] where all elements are initially visible.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks `type_name` hidden from the caller.
    #[must_use]
    pub fn hide_type(mut self, type_name: impl Into<String>) -> Self {
        self.hidden_types.insert(type_name.into());
        self
    }

    /// Marks `parent_type.field_name` hidden from the caller.
    #[must_use]
    pub fn hide_field(
        mut self,
        parent_type: impl Into<String>,
        field_name: impl Into<String>,
    ) -> Self {
        self.hidden_fields
            .insert((parent_type.into(), field_name.into()));
        self
    }

    /// Marks `parent_type.field_name(arg_name:)` hidden from the caller.
    #[must_use]
    pub fn hide_argument(
        mut self,
        parent_type: impl Into<String>,
        field_name: impl Into<String>,
        arg_name: impl Into<String>,
    ) -> Self {
        self.hidden_arguments
            .insert((parent_type.into(), field_name.into(), arg_name.into()));
        self
    }

    /// Marks `input_type.input_field` hidden from the caller.
    #[must_use]
    pub fn hide_input_field(
        mut self,
        input_type: impl Into<String>,
        input_field: impl Into<String>,
    ) -> Self {
        self.hidden_input_fields
            .insert((input_type.into(), input_field.into()));
        self
    }

    /// Marks `enum_type.enum_value` hidden from the caller.
    #[must_use]
    pub fn hide_enum_value(
        mut self,
        enum_type: impl Into<String>,
        enum_value: impl Into<String>,
    ) -> Self {
        self.hidden_enum_values
            .insert((enum_type.into(), enum_value.into()));
        self
    }

    /// Registers fallback visible search hits for `parent_type` (`GQL-070`).
    #[must_use]
    pub fn with_search_hits(mut self, parent_type: impl Into<String>, hits: Vec<String>) -> Self {
        self.search_hits.insert(parent_type.into(), hits);
        self
    }

    /// Grants or revokes `admin:schema` (`GQL-031`).
    #[must_use]
    pub const fn with_admin_schema(mut self, admin_schema: bool) -> Self {
        self.admin_schema = admin_schema;
        self
    }
}

impl Visibility for HiddenSetVisibility {
    fn can_see_type(&self, type_name: &str) -> bool {
        !self.hidden_types.contains(type_name)
    }

    fn can_see_field(&self, parent_type: &str, field_name: &str) -> bool {
        !self
            .hidden_fields
            .contains(&(parent_type.to_owned(), field_name.to_owned()))
    }

    fn can_see_argument(&self, parent_type: &str, field_name: &str, arg_name: &str) -> bool {
        !self.hidden_arguments.contains(&(
            parent_type.to_owned(),
            field_name.to_owned(),
            arg_name.to_owned(),
        ))
    }

    fn can_see_input_field(&self, input_type: &str, input_field: &str) -> bool {
        !self
            .hidden_input_fields
            .contains(&(input_type.to_owned(), input_field.to_owned()))
    }

    fn can_see_enum_value(&self, enum_type: &str, enum_value: &str) -> bool {
        !self
            .hidden_enum_values
            .contains(&(enum_type.to_owned(), enum_value.to_owned()))
    }

    fn has_admin_schema(&self) -> bool {
        self.admin_schema
    }

    fn search_visible_names(&self, parent_type: &str, _unknown_name: &str) -> Vec<String> {
        self.search_hits
            .get(parent_type)
            .cloned()
            .unwrap_or_default()
    }
}

fn is_always_retained_type(type_name: &str) -> bool {
    matches!(type_name, "Int" | "Float" | "String" | "Boolean" | "ID")
        || type_name.starts_with("__")
}

fn join_field_visible_in_schema(
    directives: &apollo_compiler::ast::DirectiveList,
    allow_non_active_joins: bool,
) -> bool {
    let Some(join_dir) = directives.get("join") else {
        return true;
    };
    if allow_non_active_joins {
        return true;
    }
    for arg in &join_dir.arguments {
        if arg.name.as_str() == "status"
            && let Value::Enum(status) = &*arg.value
        {
            return status.as_str() == "ACTIVE";
        }
    }
    // `@join` status defaults to `PROPOSED` in `spec/02-gap-ql.md` §5 (`GQL-031`).
    false
}

/// Projects `schema` into a caller-scoped [`Valid<Schema>`] containing only types, fields,
/// arguments, input fields, and enum values visible under `visibility` (`GQL-062`, `GQL-031`).
///
/// When `allow_non_active_joins` is `false`, `@join` fields whose `status != ACTIVE` are also
/// excluded (`GQL-031`, `INV-06`).
#[must_use]
pub fn project_schema_with_join_mode(
    schema: &Valid<Schema>,
    visibility: &dyn Visibility,
    allow_non_active_joins: bool,
) -> Valid<Schema> {
    let mut projected: Schema = (**schema).clone();

    let query_root_name = projected
        .schema_definition
        .query
        .as_ref()
        .map(|c| c.name.as_str().to_owned())
        .unwrap_or_else(|| "Query".to_owned());

    // Step 1: Drop explicitly hidden non-root types.
    projected.types.retain(|ty_name, _| {
        let raw = ty_name.as_str();
        raw == query_root_name || is_always_retained_type(raw) || visibility.can_see_type(raw)
    });

    // Step 2: Fixed-point pruning of hidden members and broken references.
    loop {
        let mut changed = false;
        let known_types: HashSet<Name> = projected.types.keys().cloned().collect();
        let mut types_to_remove: HashSet<Name> = HashSet::new();

        for (ty_name, ext_ty) in &mut projected.types {
            let parent_str = ty_name.as_str();
            if parent_str.starts_with("__") {
                continue;
            }
            match ext_ty {
                ExtendedType::Scalar(_) => {}
                ExtendedType::Object(obj_node) => {
                    let obj = obj_node.make_mut();
                    let before_ifaces = obj.implements_interfaces.len();
                    obj.implements_interfaces
                        .retain(|iface| known_types.contains(&iface.name));
                    if obj.implements_interfaces.len() != before_ifaces {
                        changed = true;
                    }

                    let mut kept_fields = apollo_compiler::collections::IndexMap::default();
                    for (fname, fdef_comp) in &obj.fields {
                        let fstr = fname.as_str();
                        if !visibility.can_see_field(parent_str, fstr)
                            || !join_field_visible_in_schema(
                                &fdef_comp.directives,
                                allow_non_active_joins,
                            )
                            || !known_types.contains(fdef_comp.ty.inner_named_type())
                        {
                            changed = true;
                            continue;
                        }

                        let mut drop_field = false;
                        let mut new_args = Vec::with_capacity(fdef_comp.arguments.len());
                        let mut args_modified = false;
                        for arg in &fdef_comp.arguments {
                            let arg_visible =
                                visibility.can_see_argument(parent_str, fstr, arg.name.as_str());
                            let arg_type_exists = known_types.contains(arg.ty.inner_named_type());
                            if arg_visible && arg_type_exists {
                                new_args.push(arg.clone());
                            } else if arg.ty.is_non_null() && arg.default_value.is_none() {
                                // Required argument is hidden or its type was pruned -> field cannot be called.
                                drop_field = true;
                                break;
                            } else {
                                args_modified = true;
                            }
                        }
                        if drop_field {
                            changed = true;
                            continue;
                        }
                        if args_modified {
                            changed = true;
                            let mut comp = fdef_comp.clone();
                            comp.make_mut().arguments = new_args;
                            kept_fields.insert(fname.clone(), comp);
                        } else {
                            kept_fields.insert(fname.clone(), fdef_comp.clone());
                        }
                    }
                    obj.fields = kept_fields;

                    if obj.fields.is_empty() && parent_str != query_root_name {
                        types_to_remove.insert(ty_name.clone());
                    }
                }
                ExtendedType::Interface(iface_node) => {
                    let iface = iface_node.make_mut();
                    let before_ifaces = iface.implements_interfaces.len();
                    iface
                        .implements_interfaces
                        .retain(|sub| known_types.contains(&sub.name));
                    if iface.implements_interfaces.len() != before_ifaces {
                        changed = true;
                    }

                    let mut kept_fields = apollo_compiler::collections::IndexMap::default();
                    for (fname, fdef_comp) in &iface.fields {
                        let fstr = fname.as_str();
                        if !visibility.can_see_field(parent_str, fstr)
                            || !join_field_visible_in_schema(
                                &fdef_comp.directives,
                                allow_non_active_joins,
                            )
                            || !known_types.contains(fdef_comp.ty.inner_named_type())
                        {
                            changed = true;
                            continue;
                        }
                        let mut drop_field = false;
                        let mut new_args = Vec::with_capacity(fdef_comp.arguments.len());
                        let mut args_modified = false;
                        for arg in &fdef_comp.arguments {
                            let arg_visible =
                                visibility.can_see_argument(parent_str, fstr, arg.name.as_str());
                            let arg_type_exists = known_types.contains(arg.ty.inner_named_type());
                            if arg_visible && arg_type_exists {
                                new_args.push(arg.clone());
                            } else if arg.ty.is_non_null() && arg.default_value.is_none() {
                                drop_field = true;
                                break;
                            } else {
                                args_modified = true;
                            }
                        }
                        if drop_field {
                            changed = true;
                            continue;
                        }
                        if args_modified {
                            changed = true;
                            let mut comp = fdef_comp.clone();
                            comp.make_mut().arguments = new_args;
                            kept_fields.insert(fname.clone(), comp);
                        } else {
                            kept_fields.insert(fname.clone(), fdef_comp.clone());
                        }
                    }
                    iface.fields = kept_fields;

                    if iface.fields.is_empty() {
                        types_to_remove.insert(ty_name.clone());
                    }
                }
                ExtendedType::Union(union_node) => {
                    let u = union_node.make_mut();
                    let before = u.members.len();
                    u.members.retain(|m| known_types.contains(&m.name));
                    if u.members.len() != before {
                        changed = true;
                    }
                    if u.members.is_empty() {
                        types_to_remove.insert(ty_name.clone());
                    }
                }
                ExtendedType::Enum(enum_node) => {
                    let e = enum_node.make_mut();
                    let before = e.values.len();
                    e.values.retain(|vname, _| {
                        visibility.can_see_enum_value(parent_str, vname.as_str())
                    });
                    if e.values.len() != before {
                        changed = true;
                    }
                    if e.values.is_empty() {
                        types_to_remove.insert(ty_name.clone());
                    }
                }
                ExtendedType::InputObject(input_node) => {
                    let inp = input_node.make_mut();
                    let mut required_pruned = false;
                    let before = inp.fields.len();
                    inp.fields.retain(|fname, fdef| {
                        let keep = visibility.can_see_input_field(parent_str, fname.as_str())
                            && known_types.contains(fdef.ty.inner_named_type());
                        if !keep && fdef.ty.is_non_null() && fdef.default_value.is_none() {
                            required_pruned = true;
                        }
                        keep
                    });
                    if inp.fields.len() != before {
                        changed = true;
                    }
                    if required_pruned || inp.fields.is_empty() {
                        types_to_remove.insert(ty_name.clone());
                    }
                }
            }
        }

        if !types_to_remove.is_empty() {
            changed = true;
            projected.types.retain(|k, _| !types_to_remove.contains(k));
            let sdef = projected.schema_definition.make_mut();
            if let Some(m) = &sdef.mutation
                && !projected.types.contains_key(&m.name)
            {
                sdef.mutation = None;
            }
            if let Some(s) = &sdef.subscription
                && !projected.types.contains_key(&s.name)
            {
                sdef.subscription = None;
            }
        }

        if !changed {
            break;
        }
    }

    Valid::assume_valid(projected)
}

/// Projects `schema` for executable query validation (`GQL-062` + `GQL-031`: non-`ACTIVE` joins
/// are never executable even for `admin:schema`).
#[must_use]
pub fn project_visible_schema(
    schema: &Valid<Schema>,
    visibility: &dyn Visibility,
) -> Valid<Schema> {
    project_schema_with_join_mode(schema, visibility, false)
}

/// Projects `schema` for GraphQL introspection (`GQL-024`, `GQL-031`, `GQL-062`: non-`ACTIVE`
/// joins are visible only when `visibility.has_admin_schema()` is `true`).
#[must_use]
pub fn project_introspection_schema(
    schema: &Valid<Schema>,
    visibility: &dyn Visibility,
) -> Valid<Schema> {
    project_schema_with_join_mode(schema, visibility, visibility.has_admin_schema())
}

/// Executes standard GraphQL introspection (`__schema`, `__type`, root `__typename`) against a
/// caller-filtered projection of `schema` (`GQL-024`, `GQL-031`, `GQL-062`).
pub fn execute_visible_introspection(
    schema: &Valid<Schema>,
    query: &str,
    operation_name: Option<&str>,
    variables: &serde_json::Map<String, serde_json::Value>,
    visibility: &dyn Visibility,
) -> GapResult<serde_json::Value> {
    let introspect_schema = project_introspection_schema(schema, visibility);
    let validated = validate_against_projected_schema(
        &introspect_schema,
        query,
        operation_name,
        variables,
        visibility,
    )
    .map_err(|mut errs| {
        if errs.is_empty() {
            GapError::new(
                ErrorCode::ValidationError,
                "introspection validation failed",
            )
        } else {
            errs.swap_remove(0)
        }
    })?;

    let Some(op) = validated.operation() else {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "missing operation for introspection",
        ));
    };
    if !op.operation_type.is_query() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "schema introspection is only supported on query operations",
        ));
    }

    check_max_depth(&validated.document, op).map_err(|req_err| {
        GapError::new(ErrorCode::DepthExceeded, req_err.message().to_string())
    })?;

    let resp = partial_execute(
        &introspect_schema,
        &introspect_schema.implementers_map(),
        &validated.document,
        op,
        &validated.coerced_variables,
    )
    .map_err(|req_err| GapError::new(ErrorCode::ValidationError, req_err.message().to_string()))?;

    serde_json::to_value(&resp).map_err(|ser_err| {
        GapError::new(
            ErrorCode::Internal,
            format!("failed to serialize introspection response: {ser_err}"),
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::schema::with_gap_builtins;
    use crate::validate::validate_query_with_visibility;

    // spec: GQL-062, GQL-070, GQL-072
    #[test]
    fn hidden_field_and_unknown_field_produce_byte_identical_errors_and_suggestions() {
        let schema_with_secret = with_gap_builtins(
            r#"
            type Query {
                billing: BillingQuery!
            }
            type BillingQuery {
                invoice(id: ID!): Invoice
                invoiceSummary(id: ID!): String
                secretInvoice(id: ID!): Invoice
            }
            type Invoice {
                id: ID!
                total: Float!
                internalCost: Float!
            }
            "#,
        )
        .expect("compose schema with secret");

        let schema_without_secret = with_gap_builtins(
            r#"
            type Query {
                billing: BillingQuery!
            }
            type BillingQuery {
                invoice(id: ID!): Invoice
                invoiceSummary(id: ID!): String
            }
            type Invoice {
                id: ID!
                total: Float!
            }
            "#,
        )
        .expect("compose schema without secret");

        let vis = HiddenSetVisibility::new()
            .hide_field("BillingQuery", "secretInvoice")
            .hide_field("Invoice", "internalCost")
            .with_search_hits(
                "BillingQuery",
                vec!["invoiceSummary".to_owned(), "secretInvoice".to_owned()],
            );

        // 1. Direct query to hidden field `secretInvoice` vs schema where `secretInvoice` does not exist
        let q_hidden = "{ billing { secretInvoice(id: \"1\") { id } } }";
        let err_hidden = validate_query_with_visibility(&schema_with_secret, q_hidden, &vis)
            .expect_err("hidden field must fail");
        let vis_for_absent = HiddenSetVisibility::new()
            .with_search_hits("BillingQuery", vec!["invoiceSummary".to_owned()]);
        let err_absent =
            validate_query_with_visibility(&schema_without_secret, q_hidden, &vis_for_absent)
                .expect_err("absent field must fail");

        assert_eq!(err_hidden, err_absent);
        assert_eq!(
            serde_json::to_vec(&err_hidden).unwrap(),
            serde_json::to_vec(&err_absent).unwrap(),
            "serialized JSON error bytes must be 100% identical"
        );

        // 2. Typo `invoce` gets `didYouMean: ["invoice", "invoiceSummary"]` and template `{ ... }`,
        //    and NEVER leaks `secretInvoice`.
        let q_typo = "{ billing { invoce(id: \"1\") { total } } }";
        let err_typo = validate_query_with_visibility(&schema_with_secret, q_typo, &vis)
            .expect_err("typo must fail");
        assert_eq!(err_typo.code(), ErrorCode::UnknownField);
        let sugg = err_typo.suggestion().expect("must carry suggestion");
        let dym = sugg.did_you_mean.as_ref().expect("didYouMean present");
        assert!(dym.contains(&"invoice".to_owned()));
        assert!(!dym.contains(&"secretInvoice".to_owned()));
        assert!(sugg.template.is_some());

        // 3. Typo 1-char away from hidden `internalCost` (`internalCos`) NEVER suggests `internalCost`
        let q_secret_typo = "{ billing { invoice(id: \"1\") { id internalCos } } }";
        let err_secret_typo =
            validate_query_with_visibility(&schema_with_secret, q_secret_typo, &vis)
                .expect_err("typo of hidden field must fail");
        if let Some(sugg) = err_secret_typo.suggestion()
            && let Some(dym) = &sugg.did_you_mean
        {
            assert!(
                !dym.contains(&"internalCost".to_owned()),
                "didYouMean leaked hidden field: {dym:?}"
            );
        }
        assert!(!err_secret_typo.message().contains("internalCost"));
    }

    // spec: GQL-070
    #[test]
    fn missing_and_unknown_arguments_populate_self_repair_suggestions() {
        let schema = with_gap_builtins(
            r#"
            type Query {
                billing: BillingQuery!
            }
            type BillingQuery {
                createNote(invoiceId: ID!, author: String!, body: String): String
            }
            "#,
        )
        .expect("compose");

        let missing_err = validate_query_with_visibility(
            &schema,
            "{ billing { createNote(invoiceId: \"1\") } }",
            &AllowAllVisibility::default(),
        )
        .expect_err("missing required arg `author`");
        assert_eq!(missing_err.code(), ErrorCode::MissingArgument);
        let missing_args = missing_err
            .suggestion()
            .and_then(|s| s.missing_arguments.as_ref())
            .expect("missingArguments populated");
        assert_eq!(missing_args, &vec!["author".to_owned()]);

        let unknown_arg_err = validate_query_with_visibility(
            &schema,
            "{ billing { createNote(invoiceId: \"1\", author: \"a\", bdy: \"hi\") } }",
            &AllowAllVisibility::default(),
        )
        .expect_err("unknown arg `bdy`");
        assert_eq!(unknown_arg_err.code(), ErrorCode::UnknownArgument);
        let dym = unknown_arg_err
            .suggestion()
            .and_then(|s| s.did_you_mean.as_ref())
            .expect("didYouMean for unknown arg");
        assert_eq!(dym, &vec!["body".to_owned()]);
    }

    // spec: GQL-024, GQL-062, INV-15
    #[test]
    fn introspection_filters_hidden_types_fields_arguments_and_enum_values() {
        let schema = with_gap_builtins(
            r#"
            enum Tier { PUBLIC CONFIDENTIAL }
            type SecretAudit { id: ID! }
            type Query {
                publicItem(tier: Tier = PUBLIC, debugToken: String): String
                secretAudit: SecretAudit
            }
            "#,
        )
        .expect("compose");

        let vis = HiddenSetVisibility::new()
            .hide_type("SecretAudit")
            .hide_argument("Query", "publicItem", "debugToken")
            .hide_enum_value("Tier", "CONFIDENTIAL");

        let resp = execute_visible_introspection(
            &schema,
            r#"{
                __schema {
                    types { name }
                    queryType {
                        fields {
                            name
                            args { name }
                        }
                    }
                }
                tierType: __type(name: "Tier") {
                    enumValues { name }
                }
            }"#,
            None,
            &serde_json::Map::new(),
            &vis,
        )
        .expect("introspection succeeds");

        let dumped = serde_json::to_string(&resp).expect("serialize");
        assert!(!dumped.contains("SecretAudit"));
        assert!(!dumped.contains("secretAudit"));
        assert!(!dumped.contains("debugToken"));
        assert!(!dumped.contains("CONFIDENTIAL"));
        assert!(dumped.contains("publicItem"));
        assert!(dumped.contains("PUBLIC"));
    }

    // spec: GQL-062, GQL-070, GQL-072
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn prop_hidden_field_errors_are_byte_identical_to_nonexistent_fields_and_leak_nothing(
            vis_names in prop::collection::btree_set("[a-z]{4,9}", 1..5),
            hid_names in prop::collection::btree_set("[a-z]{4,9}", 1..4),
            queried in "[a-z]{4,9}",
        ) {
            // Ensure visible and hidden field sets are disjoint and avoid `id`.
            let visible_fields: Vec<String> = vis_names
                .into_iter()
                .filter(|n| n != "id")
                .collect();
            prop_assume!(!visible_fields.is_empty());

            let visible_set: HashSet<&str> = visible_fields.iter().map(String::as_str).collect();
            let hidden_fields: Vec<String> = hid_names
                .into_iter()
                .filter(|n| n != "id" && !visible_set.contains(n.as_str()))
                .collect();
            prop_assume!(!hidden_fields.is_empty());
            prop_assume!(!visible_set.contains(queried.as_str()));

            let mut full_sdl = String::from("type Query { box: Box! }\ntype Box {\n");
            let mut public_sdl = String::from("type Query { box: Box! }\ntype Box {\n");
            for vf in &visible_fields {
                full_sdl.push_str(&format!("  {vf}: String\n"));
                public_sdl.push_str(&format!("  {vf}: String\n"));
            }
            for hf in &hidden_fields {
                full_sdl.push_str(&format!("  {hf}: String\n"));
            }
            full_sdl.push_str("}\n");
            public_sdl.push_str("}\n");

            let full_schema = with_gap_builtins(&full_sdl).unwrap();
            let public_schema = with_gap_builtins(&public_sdl).unwrap();

            let mut vis = HiddenSetVisibility::new();
            for hf in &hidden_fields {
                vis = vis.hide_field("Box", hf.clone());
            }

            // Test both querying the hidden field itself AND querying an arbitrary `queried` string.
            for probe in [hidden_fields[0].as_str(), queried.as_str()] {
                let q = format!("{{ box {{ {probe} }} }}");
                let err_hidden = validate_query_with_visibility(&full_schema, &q, &vis)
                    .expect_err("hidden/unknown field must fail");
                let err_absent = validate_query_with_visibility(
                    &public_schema,
                    &q,
                    &AllowAllVisibility::default(),
                )
                .expect_err("absent field must fail");

                prop_assert_eq!(&err_hidden, &err_absent);
                prop_assert_eq!(
                    serde_json::to_vec(&err_hidden).unwrap(),
                    serde_json::to_vec(&err_absent).unwrap()
                );

                if let Some(sugg) = err_hidden.suggestion() {
                    if let Some(dym) = &sugg.did_you_mean {
                        for item in dym {
                            prop_assert!(
                                visible_set.contains(item.as_str()),
                                "didYouMean suggested non-visible field `{item}`"
                            );
                        }
                    }
                    if let Some(tpl) = &sugg.template {
                        for hf in &hidden_fields {
                            prop_assert!(
                                !tpl.contains(hf.as_str()),
                                "template leaked hidden field `{hf}`: {tpl}"
                            );
                        }
                    }
                }
            }
        }
    }
}
