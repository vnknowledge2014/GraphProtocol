//! TDD integration and property tests for `gap_snapshot::compose` (`RUN-009`, `RUN-011`, `GQL-004`, `GQL-030`, `GQL-032`, `NGC-011`).

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use apollo_compiler::Schema;
use apollo_compiler::ast::{Definition, Value as AstValue};
use apollo_compiler::schema::{DirectiveList, ExtendedType};
use gap_core::error::ErrorCode;
use gap_core::ids::{BranchId, BranchUid, RESERVED_BUILTIN_TYPES};
use gap_core::snapshot::{
    BranchStatus, Endpoint, FieldCoordinate, FieldOwnership, OpKind as CoreOpKind, Protocol,
    SideEffectLevel, TrustTier,
};
use gap_proto::ngc::v1::{
    Capabilities as ProtoCapabilities, ConnectorDescription, Limits as ProtoLimits, MetaSummary,
    OpKind as ProtoOpKind, OperationDescriptor as ProtoOp, PredicateOp, SideEffectHint,
};
use gap_snapshot::{BranchInput, compose};
use proptest::prelude::*;

fn uid(seed: u8) -> BranchUid {
    BranchUid::from_unix_ms(1_761_900_000_000 + u64::from(seed), [seed; 10])
}

fn endpoint(name: &str) -> Endpoint {
    Endpoint {
        uri: format!("https://{name}.internal/ngc"),
        transport: "grpc".to_owned(),
    }
}

fn prost_json_obj(pairs: &[(&str, serde_json::Value)]) -> prost_types::Struct {
    fn to_prost(v: &serde_json::Value) -> prost_types::Value {
        use prost_types::value::Kind;
        let kind = match v {
            serde_json::Value::Null => Kind::NullValue(0),
            serde_json::Value::Bool(b) => Kind::BoolValue(*b),
            serde_json::Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(s) => Kind::StringValue(s.clone()),
            serde_json::Value::Array(arr) => Kind::ListValue(prost_types::ListValue {
                values: arr.iter().map(to_prost).collect(),
            }),
            serde_json::Value::Object(map) => Kind::StructValue(prost_types::Struct {
                fields: map
                    .iter()
                    .map(|(k, val)| (k.clone(), to_prost(val)))
                    .collect(),
            }),
        };
        prost_types::Value { kind: Some(kind) }
    }

    prost_types::Struct {
        fields: pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), to_prost(v)))
            .collect(),
    }
}

fn proto_op(name: &str, kind: ProtoOpKind, description: &str) -> ProtoOp {
    let mut annotations = HashMap::new();
    annotations.insert("source".to_owned(), "fixture".to_owned());
    ProtoOp {
        name: name.to_owned(),
        kind: kind as i32,
        description: description.to_owned(),
        input_schema: Some(prost_json_obj(&[(
            "type",
            serde_json::Value::String("object".to_owned()),
        )])),
        output_schema: Some(prost_json_obj(&[(
            "type",
            serde_json::Value::String("object".to_owned()),
        )])),
        side_effect_hint: match kind {
            ProtoOpKind::Query | ProtoOpKind::Subscription => SideEffectHint::SeNone as i32,
            _ => SideEffectHint::SeReversible as i32,
        },
        idempotent_hint: matches!(kind, ProtoOpKind::Query | ProtoOpKind::Subscription),
        supports_dry_run: false,
        compensation_operation: String::new(),
        examples: Vec::new(),
        cost_weight: 2,
        pagination: 0,
        key_fields: Vec::new(),
        annotations,
    }
}

fn make_branch(
    slug: &str,
    seed: u8,
    trust: TrustTier,
    status: BranchStatus,
    sdl: &str,
    ops: Vec<ProtoOp>,
) -> BranchInput {
    BranchInput {
        id: BranchId::new(slug).expect("valid branch slug"),
        uid: uid(seed),
        protocol: Protocol::Rest,
        trust,
        endpoint: endpoint(slug),
        description: ConnectorDescription {
            connector_id: slug.to_owned(),
            connector_version: "0.1.0".to_owned(),
            ngc_version: "gap/0.1".to_owned(),
            protocol: gap_proto::ngc::v1::Protocol::Rest as i32,
            spec_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_owned(),
            capabilities: Some(ProtoCapabilities {
                foreach_batch: true,
                predicate_ops: vec![PredicateOp::PEq as i32, PredicateOp::PIn as i32],
                order_by: true,
                pagination: 2,
                native_traverse: false,
                subscribe: false,
                tasks: false,
                dry_run: false,
                idempotency: true,
                sketch: false,
                explain: true,
                as_of: false,
            }),
            limits: Some(ProtoLimits {
                max_batch_size: 50,
                max_in_flight: 32,
                max_page_size: 200,
                default_timeout_ms: 3_000,
                max_response_bytes: 2 * 1024 * 1024,
            }),
            schema_sdl: sdl.to_owned(),
            operations: ops,
            meta: Some(MetaSummary {
                node_categories: Vec::new(),
                edge_predicates: Vec::new(),
                synopsis_hint: format!(
                    "{slug} branch progressive-disclosure summary with multibyte €€€ padding past ninety bytes total length!"
                ),
            }),
            auth: None,
        },
        status,
    }
}

/// Exact SDL generated by `adapters/gap-ngc-openapi` for `tests/fixtures/openapi/shop.openapi.json`.
const SHOP_OPENAPI_SDL: &str = r#"
type Blob {
  id: Int!
  payload: String!
}

type Customer {
  active: Boolean!
  credit_cents: Int!
  id: Int!
  name: String!
  region: Customers_Region!
}

enum Customers_Region {
  eu
  us
  apac
}

type Order {
  customer_id: Int!
  id: Int!
  metadata: JSON
  status: Orders_Status!
  total_cents: Int!
}

enum Orders_Status {
  pending
  paid
  shipped
}

type Product {
  price_cents: Int!
  sku: String!
  tags: [String!]!
  title: String!
}

type SlowStatus {
  ok: Boolean!
}

type Query {
  blobs(limit: Int, offset: Int): [Blob!]!
  customers(limit: Int, offset: Int, sort: String, region: Customers_Region, ids: [Int!]): [Customer!]!
  customer(id: Int!, X_Request_Tag: String): Customer
  orders(limit: Int, offset: Int, status: [Orders_Status!]): [Order!]!
  order(id: Int!): Order
  product(sku: String!): Product
  slow: SlowStatus!
}
"#;

fn count_ast_type_definitions(sdl: &str) -> BTreeMap<String, usize> {
    let ast = gap_ql::schema::parse_sdl_ast(sdl, "composed.graphql").expect("parse composed SDL");
    let mut counts = BTreeMap::new();
    for def in &ast.definitions {
        let maybe_name = match def {
            Definition::ScalarTypeDefinition(d) => Some(d.name.as_str()),
            Definition::ObjectTypeDefinition(d) => Some(d.name.as_str()),
            Definition::InterfaceTypeDefinition(d) => Some(d.name.as_str()),
            Definition::UnionTypeDefinition(d) => Some(d.name.as_str()),
            Definition::EnumTypeDefinition(d) => Some(d.name.as_str()),
            Definition::InputObjectTypeDefinition(d) => Some(d.name.as_str()),
            Definition::ScalarTypeExtension(d) => Some(d.name.as_str()),
            Definition::ObjectTypeExtension(d) => Some(d.name.as_str()),
            Definition::InterfaceTypeExtension(d) => Some(d.name.as_str()),
            Definition::UnionTypeExtension(d) => Some(d.name.as_str()),
            Definition::EnumTypeExtension(d) => Some(d.name.as_str()),
            Definition::InputObjectTypeExtension(d) => Some(d.name.as_str()),
            _ => None,
        };
        if let Some(name) = maybe_name {
            *counts.entry(name.to_owned()).or_insert(0) += 1;
        }
    }
    counts
}

fn extract_branch_uid_from_obj_directives(directives: &DirectiveList) -> Option<BranchUid> {
    let dir = directives.get("branch")?;
    if let Some(AstValue::String(raw_uid)) = dir.specified_argument_by_name("uid").map(|v| &**v) {
        return Some(BranchUid::parse(raw_uid).expect("valid @branch uid"));
    }
    None
}

fn extract_branch_uid_from_field_directives(
    directives: &apollo_compiler::ast::DirectiveList,
) -> Option<BranchUid> {
    let dir = directives.get("branch")?;
    if let Some(AstValue::String(raw_uid)) = dir.specified_argument_by_name("uid").map(|v| &**v) {
        return Some(BranchUid::parse(raw_uid).expect("valid @branch uid"));
    }
    None
}

fn ownership_oracle_from_schema(schema: &Schema) -> FieldOwnership {
    let mut map = BTreeMap::new();
    for (type_name, ext_ty) in &schema.types {
        if type_name.as_str().starts_with("__") {
            continue;
        }
        let ExtendedType::Object(obj) = ext_ty else {
            continue;
        };
        let obj_uid = extract_branch_uid_from_obj_directives(&obj.directives);
        for (field_name, field_def) in &obj.fields {
            if field_name.as_str().starts_with("__") {
                continue;
            }
            if let Some(owner_uid) =
                extract_branch_uid_from_field_directives(&field_def.directives).or(obj_uid)
            {
                map.insert(
                    FieldCoordinate::new(type_name.as_str(), field_name.as_str()),
                    owner_uid,
                );
            }
        }
    }
    FieldOwnership::new(map)
}

// spec: RUN-009, GQL-004
#[test]
fn compose_zero_branches_is_valid_and_contains_builtins_once() {
    let comp = compose(&[]).expect("zero-branch composition must succeed");
    assert!(comp.branches.is_empty());
    assert!(comp.ownership.is_empty());
    assert!(comp.warnings.is_empty());

    // Standalone SDL re-parses and validates in apollo-compiler without errors.
    Schema::parse_and_validate(&comp.sdl, "zero.graphql")
        .expect("zero-branch SDL must pass apollo-compiler validation");

    let counts = count_ast_type_definitions(&comp.sdl);
    for &builtin in RESERVED_BUILTIN_TYPES {
        assert_eq!(
            counts.get(builtin).copied(),
            Some(1),
            "built-in type `{builtin}` must appear exactly once in composed SDL"
        );
    }

    // Programmatic __search and __metaGraph are present on the compiled Schema.
    let ExtendedType::Object(query_obj) = &comp.schema.types["Query"] else {
        panic!("Query must be an Object");
    };
    assert!(query_obj.fields.contains_key("gap"));
    assert!(query_obj.fields.contains_key("__search"));
    assert!(query_obj.fields.contains_key("__metaGraph"));
}

// spec: RUN-009, RUN-011, NGC-011
#[test]
fn compose_golden_shop_and_crm_branches_with_colliding_customer_type() {
    let shop_ops = vec![
        proto_op("blobs", ProtoOpKind::Query, "List binary-like text blobs"),
        proto_op("customers", ProtoOpKind::Query, "List customers"),
        proto_op(
            "customer",
            ProtoOpKind::Query,
            "Fetch single customer by id",
        ),
        proto_op("orders", ProtoOpKind::Query, "List orders"),
        proto_op("order", ProtoOpKind::Query, "Fetch single order by id"),
        proto_op("product", ProtoOpKind::Query, "Fetch single product by SKU"),
        proto_op("slow", ProtoOpKind::Query, "Slow probe endpoint"),
    ];
    let shop = make_branch(
        "shop",
        1,
        TrustTier::T1,
        BranchStatus::Active,
        SHOP_OPENAPI_SDL,
        shop_ops,
    );

    let crm_sdl = r#"
        enum Tier {
          FREE
          PRO
          ENTERPRISE
        }

        type Customer {
          id: ID!
          email: String!
          tier: Tier!
          createdAt: DateTime!
        }

        type Query {
          crmCustomer(id: ID!): Customer
        }

        type Mutation {
          promoteCustomer(id: ID!, tier: Tier!): Customer!
        }
    "#;
    let crm = make_branch(
        "crm",
        2,
        TrustTier::T0,
        BranchStatus::Active,
        crm_sdl,
        vec![
            proto_op("crmCustomer", ProtoOpKind::Query, "Lookup CRM customer"),
            proto_op(
                "promoteCustomer",
                ProtoOpKind::Mutation,
                "Promote CRM customer tier",
            ),
        ],
    );

    let comp = compose(&[shop.clone(), crm.clone()]).expect("shop + crm must compose");
    Schema::parse_and_validate(&comp.sdl, "shop_crm.graphql")
        .expect("composed shop+crm SDL must pass standalone validation");

    let counts = count_ast_type_definitions(&comp.sdl);

    // Every built-in type appears once.
    for &builtin in RESERVED_BUILTIN_TYPES {
        assert_eq!(
            counts.get(builtin).copied(),
            Some(1),
            "built-in `{builtin}` must appear once"
        );
    }

    // Both namespaced Customer types and all other branch types exist once; un-prefixed names do not exist.
    for expected in [
        "shop_Blob",
        "shop_Customer",
        "shop_Customers_Region",
        "shop_Order",
        "shop_Orders_Status",
        "shop_Product",
        "shop_SlowStatus",
        "crm_Customer",
        "crm_Tier",
    ] {
        assert_eq!(
            counts.get(expected).copied(),
            Some(1),
            "namespaced type `{expected}` must appear once"
        );
    }
    for unprefixed in [
        "Blob",
        "Customer",
        "Customers_Region",
        "Order",
        "Orders_Status",
        "Product",
        "SlowStatus",
        "Tier",
    ] {
        assert!(
            !counts.contains_key(unprefixed),
            "un-prefixed branch type `{unprefixed}` must not exist in supergraph"
        );
    }

    // Check @branch on every branch root field and every branch object type.
    let ExtendedType::Object(query_obj) = &comp.schema.types["Query"] else {
        panic!("Query object");
    };
    for f in [
        "blobs",
        "customers",
        "customer",
        "orders",
        "order",
        "product",
        "slow",
    ] {
        assert_eq!(
            extract_branch_uid_from_field_directives(&query_obj.fields[f].directives),
            Some(shop.uid),
            "Query.{f} must carry shop @branch"
        );
        assert_eq!(
            comp.ownership.owner(&FieldCoordinate::new("Query", f)),
            Some(shop.uid)
        );
    }
    assert_eq!(
        extract_branch_uid_from_field_directives(&query_obj.fields["crmCustomer"].directives),
        Some(crm.uid)
    );

    let ExtendedType::Object(mut_obj) = &comp.schema.types["Mutation"] else {
        panic!("Mutation object");
    };
    assert_eq!(
        extract_branch_uid_from_field_directives(&mut_obj.fields["promoteCustomer"].directives),
        Some(crm.uid)
    );

    for (obj_name, expected_uid) in [
        ("shop_Blob", shop.uid),
        ("shop_Customer", shop.uid),
        ("shop_Order", shop.uid),
        ("shop_Product", shop.uid),
        ("shop_SlowStatus", shop.uid),
        ("crm_Customer", crm.uid),
    ] {
        let ExtendedType::Object(obj) = &comp.schema.types[obj_name] else {
            panic!("{obj_name} must be Object");
        };
        assert_eq!(
            extract_branch_uid_from_obj_directives(&obj.directives),
            Some(expected_uid),
            "{obj_name} must carry @branch"
        );
    }

    assert_eq!(comp.ownership, ownership_oracle_from_schema(&comp.schema));

    // Check sorted BranchDescriptors (crm before shop) and OperationDescriptor fields.
    assert_eq!(comp.branches.len(), 2);
    assert_eq!(comp.branches[0].id.as_str(), "crm");
    assert_eq!(comp.branches[1].id.as_str(), "shop");

    let shop_desc = &comp.branches[1];
    assert_eq!(shop_desc.operations.len(), 7);
    assert!(shop_desc.synopsis.len() <= 90);
    assert!(shop_desc.capabilities.foreach_batch);
    assert_eq!(shop_desc.capabilities.predicate_ops, vec!["EQ", "IN"]);
    assert_eq!(shop_desc.limits.max_batch_size, 50);
    assert_eq!(shop_desc.limits.max_in_flight, 32);
    assert_eq!(shop_desc.limits.timeout_ms, 3_000);
    let customer_op = shop_desc
        .operations
        .iter()
        .find(|o| o.id.as_str() == "shop.customer")
        .expect("shop.customer op");
    assert_eq!(customer_op.kind, CoreOpKind::Query);
    assert!(customer_op.sdl.contains("shop_Customer"));
    assert_eq!(customer_op.group.as_str().len(), 32);
}

// spec: RUN-009
#[test]
fn compose_all_24_permutations_of_four_branches_are_byte_identical() {
    let b1 = make_branch(
        "alpha",
        10,
        TrustTier::T1,
        BranchStatus::Active,
        "type Rec { id: ID! }\ntype Query { alphaRec: Rec }",
        vec![proto_op("alphaRec", ProtoOpKind::Query, "alpha")],
    );
    let b2 = make_branch(
        "beta-api",
        11,
        TrustTier::T1,
        BranchStatus::Active,
        "type Rec { id: ID! }\ntype Query { betaRec: Rec }",
        vec![proto_op("betaRec", ProtoOpKind::Query, "beta")],
    );
    let b3 = make_branch(
        "gamma",
        12,
        TrustTier::T0,
        BranchStatus::Drift,
        "type Item { k: String! }\ntype Mutation { gammaPut(k: String!): Item! }",
        vec![proto_op("gammaPut", ProtoOpKind::Mutation, "gamma")],
    );
    let b4 = make_branch(
        "delta",
        13,
        TrustTier::T2,
        BranchStatus::Quarantined,
        "type Ev { seq: Int! }\ntype Subscription { deltaWatch: Ev! }",
        vec![proto_op(
            "deltaWatch",
            ProtoOpKind::Subscription,
            "delta stream",
        )],
    );

    let items = [b1, b2, b3, b4];
    let baseline = compose(&items).expect("baseline compose");

    // Generate all 24 permutations of [0, 1, 2, 3].
    let mut perms = Vec::with_capacity(24);
    for i in 0..4 {
        for j in 0..4 {
            if j == i {
                continue;
            }
            for k in 0..4 {
                if k == i || k == j {
                    continue;
                }
                let l = 6 - i - j - k;
                perms.push([i, j, k, l]);
            }
        }
    }
    assert_eq!(perms.len(), 24);

    for perm in perms {
        let permuted = [
            items[perm[0]].clone(),
            items[perm[1]].clone(),
            items[perm[2]].clone(),
            items[perm[3]].clone(),
        ];
        let got = compose(&permuted).expect("permutation compose");
        assert_eq!(
            got.sdl, baseline.sdl,
            "SDL differed for permutation {perm:?}"
        );
        assert_eq!(
            got.branch_set_hash, baseline.branch_set_hash,
            "branch_set_hash differed for permutation {perm:?}"
        );
        assert_eq!(got.ownership, baseline.ownership);
        assert_eq!(got.branches, baseline.branches);
    }
}

// spec: RUN-009
#[test]
fn compose_rejects_duplicate_branch_id_and_duplicate_branch_uid() {
    let b1 = make_branch(
        "dup-id",
        21,
        TrustTier::T1,
        BranchStatus::Active,
        "type Query { f1: String }",
        vec![proto_op("f1", ProtoOpKind::Query, "f1")],
    );
    let b2 = make_branch(
        "dup-id",
        22,
        TrustTier::T1,
        BranchStatus::Active,
        "type Query { f2: String }",
        vec![proto_op("f2", ProtoOpKind::Query, "f2")],
    );
    let err_id = compose(&[b1.clone(), b2]).expect_err("duplicate BranchId must fail");
    assert_eq!(err_id.code(), ErrorCode::ValidationError);
    assert!(err_id.message().contains("dup-id"));

    let mut b3 = make_branch(
        "other-id",
        23,
        TrustTier::T1,
        BranchStatus::Active,
        "type Query { f3: String }",
        vec![proto_op("f3", ProtoOpKind::Query, "f3")],
    );
    b3.uid = b1.uid;
    let err_uid = compose(&[b1, b3]).expect_err("duplicate BranchUid must fail");
    assert_eq!(err_uid.code(), ErrorCode::ValidationError);
}

// spec: RUN-009, GQL-004
#[test]
fn compose_namespaces_all_type_reference_positions_and_preserves_builtins_and_directive_strings() {
    let sdl = r#"
        scalar Money

        enum Status {
          DRAFT
          LIVE
        }

        interface NodeLike {
          id: ID!
          status: Status!
        }

        interface Auditable implements NodeLike {
          id: ID!
          status: Status!
          updatedAt: DateTime!
        }

        type Item implements NodeLike & Auditable @key(fields: "id") {
          id: ID!
          status: Status!
          updatedAt: DateTime!
          price: Money!
          matrix(tags: [Status!]!): [[Item!]!]!
          raw: JSON
          page: PageInfo!
        }

        type Bundle {
          title: String!
          items: [Item!]!
        }

        union Searchable = Item | Bundle

        input ItemFilter {
          status: Status
          minPrice: Money
          nested: [ItemFilter!]
          after: Cursor
        }

        type Query {
          searchCatalog(filter: ItemFilter!): [Searchable!]!
        }

        type Mutation {
          saveItem(filter: ItemFilter!): Item! @compensate(with: "Item") @auth(scopes: ["Item"])
        }

        type Subscription {
          watchCatalog(status: Status): Searchable!
        }
    "#;

    let branch = make_branch(
        "catalog-v2",
        30,
        TrustTier::T1,
        BranchStatus::Active,
        sdl,
        vec![
            proto_op("searchCatalog", ProtoOpKind::Query, "search"),
            proto_op("saveItem", ProtoOpKind::Mutation, "save"),
            proto_op("watchCatalog", ProtoOpKind::Subscription, "watch"),
        ],
    );

    let comp = compose(&[branch]).expect("catalog-v2 must compose and validate");
    Schema::parse_and_validate(&comp.sdl, "catalog.graphql")
        .expect("namespaced catalog SDL must validate");

    // Check union members were rewritten.
    let ExtendedType::Union(union_ty) = &comp.schema.types["catalog_v2_Searchable"] else {
        panic!("expected union catalog_v2_Searchable");
    };
    assert!(union_ty.members.contains("catalog_v2_Item"));
    assert!(union_ty.members.contains("catalog_v2_Bundle"));
    assert_eq!(union_ty.members.len(), 2);

    // Check interface implements and object implements were rewritten.
    let ExtendedType::Interface(auditable) = &comp.schema.types["catalog_v2_Auditable"] else {
        panic!("expected interface catalog_v2_Auditable");
    };
    assert!(
        auditable
            .implements_interfaces
            .contains("catalog_v2_NodeLike")
    );
    assert_eq!(
        auditable.fields["updatedAt"].ty.inner_named_type().as_str(),
        "DateTime"
    );

    let ExtendedType::Object(item_obj) = &comp.schema.types["catalog_v2_Item"] else {
        panic!("expected object catalog_v2_Item");
    };
    assert!(
        item_obj
            .implements_interfaces
            .contains("catalog_v2_NodeLike")
    );
    assert!(
        item_obj
            .implements_interfaces
            .contains("catalog_v2_Auditable")
    );
    assert_eq!(
        item_obj.fields["matrix"].ty.to_string(),
        "[[catalog_v2_Item!]!]!"
    );
    assert_eq!(
        item_obj.fields["matrix"].arguments[0].ty.to_string(),
        "[catalog_v2_Status!]!"
    );
    // Built-ins and standard scalars remain untouched.
    assert_eq!(item_obj.fields["id"].ty.to_string(), "ID!");
    assert_eq!(item_obj.fields["raw"].ty.to_string(), "JSON");
    assert_eq!(item_obj.fields["page"].ty.to_string(), "PageInfo!");

    // Check input object fields.
    let ExtendedType::InputObject(filter_in) = &comp.schema.types["catalog_v2_ItemFilter"] else {
        panic!("expected input catalog_v2_ItemFilter");
    };
    assert_eq!(
        filter_in.fields["status"].ty.to_string(),
        "catalog_v2_Status"
    );
    assert_eq!(
        filter_in.fields["minPrice"].ty.to_string(),
        "catalog_v2_Money"
    );
    assert_eq!(
        filter_in.fields["nested"].ty.to_string(),
        "[catalog_v2_ItemFilter!]"
    );
    assert_eq!(filter_in.fields["after"].ty.to_string(), "Cursor");

    // Directive string arguments that happen to match a type name MUST NOT be rewritten.
    let ExtendedType::Object(mut_obj) = &comp.schema.types["Mutation"] else {
        panic!("Mutation");
    };
    let comp_dir = mut_obj.fields["saveItem"]
        .directives
        .get("compensate")
        .expect("@compensate preserved");
    assert_eq!(
        comp_dir
            .specified_argument_by_name("with")
            .map(|v| v.to_string()),
        Some("\"Item\"".to_owned())
    );
}

// spec: GQL-004, RUN-009
#[test]
fn compose_rejects_reserved_names_with_gap_reserved_name() {
    let cases: &[(&str, Vec<ProtoOp>)] = &[
        (
            "type Query { gap: String }",
            vec![proto_op("gap", ProtoOpKind::Query, "reserved root")],
        ),
        (
            "type Query { __search: String }",
            vec![proto_op("__search", ProtoOpKind::Query, "reserved search")],
        ),
        (
            "type Query { __metaGraph: String }",
            vec![proto_op(
                "__metaGraph",
                ProtoOpKind::Query,
                "reserved metagraph",
            )],
        ),
        (
            "type __Hidden { x: Int }\ntype Query { ok: String }",
            vec![proto_op("ok", ProtoOpKind::Query, "ok")],
        ),
        (
            "type Safe { __dunder: Int }\ntype Query { ok: Safe }",
            vec![proto_op("ok", ProtoOpKind::Query, "ok")],
        ),
        (
            "type SearchResult { x: Int }\ntype Query { ok: String }",
            vec![proto_op("ok", ProtoOpKind::Query, "builtin SearchResult")],
        ),
        (
            "type GapNode { x: Int }\ntype Query { ok: String }",
            vec![proto_op("ok", ProtoOpKind::Query, "builtin GapNode")],
        ),
        (
            "scalar String\ntype Query { ok: Int }",
            vec![proto_op("ok", ProtoOpKind::Query, "scalar String")],
        ),
    ];

    for (idx, (sdl, ops)) in cases.iter().enumerate() {
        let b = make_branch(
            "bad-branch",
            40 + u8::try_from(idx).expect("u8"),
            TrustTier::T1,
            BranchStatus::Active,
            sdl,
            ops.clone(),
        );
        let err = compose(&[b]).expect_err(&format!("case {idx} (`{sdl}`) must fail"));
        assert_eq!(
            err.code(),
            ErrorCode::ReservedName,
            "case {idx} (`{sdl}`) expected GAP_RESERVED_NAME, got {err:?}"
        );
    }
}

// spec: RUN-009, RUN-011
#[test]
fn compose_rejects_ambiguous_root_field_and_colliding_namespaced_types_naming_both_branches() {
    // 1. Two branches defining the same root field `Query.lookupUser`.
    let b1 = make_branch(
        "auth-svc",
        50,
        TrustTier::T1,
        BranchStatus::Active,
        "type Query { lookupUser(id: ID!): String }",
        vec![proto_op("lookupUser", ProtoOpKind::Query, "auth lookup")],
    );
    let b2 = make_branch(
        "billing-svc",
        51,
        TrustTier::T1,
        BranchStatus::Active,
        "type Query { lookupUser(id: ID!): String }",
        vec![proto_op("lookupUser", ProtoOpKind::Query, "billing lookup")],
    );
    let err_field = compose(&[b1, b2]).expect_err("duplicate root field must fail");
    assert_eq!(err_field.code(), ErrorCode::AmbiguousField);
    assert!(
        err_field.message().contains("auth-svc") && err_field.message().contains("billing-svc"),
        "error message must name both branches: {}",
        err_field.message()
    );
    assert!(err_field.message().contains("lookupUser"));

    // 2. Branch `ab` defining `c_Record` vs branch `ab_c` defining `Record` -> both produce `ab_c_Record`.
    let left = make_branch(
        "ab",
        52,
        TrustTier::T1,
        BranchStatus::Active,
        "type c_Record { id: ID! }\ntype Query { leftOp: c_Record }",
        vec![proto_op("leftOp", ProtoOpKind::Query, "left")],
    );
    let right = make_branch(
        "ab_c",
        53,
        TrustTier::T1,
        BranchStatus::Active,
        "type Record { id: ID! }\ntype Query { rightOp: Record }",
        vec![proto_op("rightOp", ProtoOpKind::Query, "right")],
    );
    let err_type =
        compose(&[left.clone(), right]).expect_err("colliding namespaced type must fail");
    assert_eq!(err_type.code(), ErrorCode::AmbiguousField);
    assert!(
        err_type.message().contains("ab") && err_type.message().contains("ab_c"),
        "error message must name both branches: {}",
        err_type.message()
    );
    assert!(err_type.message().contains("ab_c_Record"));

    // 3. Hyphenated `ab-c` defining `Record` vs `ab` defining `c_Record` -> also `ab_c_Record`.
    let right_hyphen = make_branch(
        "ab-c",
        54,
        TrustTier::T1,
        BranchStatus::Active,
        "type Record { id: ID! }\ntype Query { rightHyphenOp: Record }",
        vec![proto_op(
            "rightHyphenOp",
            ProtoOpKind::Query,
            "right hyphen",
        )],
    );
    let err_hyphen =
        compose(&[left, right_hyphen]).expect_err("hyphen/underscore type collision must fail");
    assert_eq!(err_hyphen.code(), ErrorCode::AmbiguousField);
    assert!(
        err_hyphen.message().contains("ab") && err_hyphen.message().contains("ab-c"),
        "error message must name both branches: {}",
        err_hyphen.message()
    );
}

// spec: RUN-009
#[test]
fn compose_rejects_operation_and_root_field_mismatch_both_ways() {
    // Direction 1: root field in SDL without matching OperationDescriptor.
    let missing_op = make_branch(
        "inv-svc",
        60,
        TrustTier::T1,
        BranchStatus::Active,
        "type Query { listed: String, orphanField: Int }",
        vec![proto_op("listed", ProtoOpKind::Query, "listed")],
    );
    let err1 = compose(&[missing_op]).expect_err("root field without operation must fail");
    assert_eq!(err1.code(), ErrorCode::ValidationError);
    assert!(
        err1.message().contains("inv-svc") && err1.message().contains("orphanField"),
        "unexpected message: {}",
        err1.message()
    );

    // Direction 2: OperationDescriptor without matching root field in SDL.
    let extra_op = make_branch(
        "pay-svc",
        61,
        TrustTier::T1,
        BranchStatus::Active,
        "type Query { listed: String }",
        vec![
            proto_op("listed", ProtoOpKind::Query, "listed"),
            proto_op("ghostOp", ProtoOpKind::Query, "ghost"),
        ],
    );
    let err2 = compose(&[extra_op]).expect_err("operation without root field must fail");
    assert_eq!(err2.code(), ErrorCode::ValidationError);
    assert!(
        err2.message().contains("pay-svc") && err2.message().contains("ghostOp"),
        "unexpected message: {}",
        err2.message()
    );

    // Direction 3: OperationDescriptor kind (MUTATION) mismatches SDL root type (Query).
    let wrong_kind = make_branch(
        "kind-svc",
        62,
        TrustTier::T1,
        BranchStatus::Active,
        "type Query { doThing: String }",
        vec![proto_op("doThing", ProtoOpKind::Mutation, "wrong kind")],
    );
    let err3 = compose(&[wrong_kind]).expect_err("kind mismatch must fail");
    assert_eq!(err3.code(), ErrorCode::ValidationError);
    assert!(
        err3.message().contains("kind-svc") && err3.message().contains("doThing"),
        "unexpected message: {}",
        err3.message()
    );
}

// spec: RUN-009, GQL-030
#[test]
fn compose_strips_authored_composer_directives_with_warnings_and_attaches_authoritative_branch() {
    let sdl = r#"
        type Account @branch(id: "spoofed", uid: "0195f000-0000-7000-8000-000000000099", protocol: SQL, trust: T0) @provenance(source: "evil", descHash: "00") {
          id: ID!
          partner: String @join(to: "crm.crmCustomer", on: [{ from: "id", to: "id" }], cardinality: ONE_TO_ONE, status: ACTIVE)
        }

        type Query {
          account(id: ID!): Account @branch(id: "spoofed", uid: "0195f000-0000-7000-8000-000000000099", protocol: SQL, trust: T0) @capabilityGroup(id: "00000000000000000000000000000000") @provenance(source: "evil", descHash: "00")
        }
    "#;
    let branch = make_branch(
        "acct",
        70,
        TrustTier::T1,
        BranchStatus::Active,
        sdl,
        vec![proto_op("account", ProtoOpKind::Query, "get account")],
    );

    let comp =
        compose(std::slice::from_ref(&branch)).expect("acct branch must compose after stripping");
    assert!(
        comp.warnings.len() >= 5,
        "expected warnings for @branch (x2), @provenance (x2), @capabilityGroup, @join(status:); got {:?}",
        comp.warnings
    );
    assert!(
        comp.warnings
            .iter()
            .all(|w| w.code == ErrorCode::DirectiveIgnored && w.branch_id == branch.id)
    );
    assert!(
        !comp.sdl.contains("spoofed"),
        "authored @branch must never survive into composed SDL"
    );

    // Authoritative @branch on Query.account and acct_Account.
    let ExtendedType::Object(query_obj) = &comp.schema.types["Query"] else {
        panic!("Query");
    };
    let field_dirs = &query_obj.fields["account"].directives;
    assert_eq!(
        extract_branch_uid_from_field_directives(field_dirs),
        Some(branch.uid)
    );
    assert!(!field_dirs.has("capabilityGroup"));
    assert!(!field_dirs.has("provenance"));

    let ExtendedType::Object(acct_obj) = &comp.schema.types["acct_Account"] else {
        panic!("acct_Account");
    };
    assert_eq!(
        extract_branch_uid_from_obj_directives(&acct_obj.directives),
        Some(branch.uid)
    );
    assert!(!acct_obj.directives.has("provenance"));

    // @join status was reset to PROPOSED, not ACTIVE.
    let join_dir = acct_obj.fields["partner"]
        .directives
        .get("join")
        .expect("@join retained without ACTIVE status");
    if let Some(status_val) = join_dir.specified_argument_by_name("status") {
        assert_eq!(status_val.to_string(), "PROPOSED");
    }
}

// spec: RUN-009, GQL-032 (ADR-015)
#[test]
fn compose_forces_t2_side_effect_to_unknown_in_sdl_and_descriptor() {
    let t2_sdl = r#"
        type ResultBox {
          ok: Boolean!
        }

        type Query {
          untrustedRead: ResultBox @sideEffect(level: NONE, idempotent: true, dryRun: true)
        }

        type Mutation {
          untrustedWrite(payload: String!): ResultBox! @sideEffect(level: REVERSIBLE, idempotent: true, dryRun: true)
        }
    "#;
    let mut read_op = proto_op("untrustedRead", ProtoOpKind::Query, "claims read-only");
    read_op.side_effect_hint = SideEffectHint::SeNone as i32;
    read_op.idempotent_hint = true;

    let mut write_op = proto_op("untrustedWrite", ProtoOpKind::Mutation, "claims reversible");
    write_op.side_effect_hint = SideEffectHint::SeReversible as i32;
    write_op.idempotent_hint = true;

    let t2_branch = make_branch(
        "untrusted-mcp",
        80,
        TrustTier::T2,
        BranchStatus::Quarantined,
        t2_sdl,
        vec![read_op, write_op],
    );

    let comp = compose(&[t2_branch]).expect("T2 branch must compose");
    for (root_ty, field_name) in [("Query", "untrustedRead"), ("Mutation", "untrustedWrite")] {
        let ExtendedType::Object(obj) = &comp.schema.types[root_ty] else {
            panic!("{root_ty}");
        };
        let se_dir = obj.fields[field_name]
            .directives
            .get("sideEffect")
            .expect("T2 root field must carry @sideEffect");
        assert_eq!(
            se_dir
                .specified_argument_by_name("level")
                .map(|v| v.to_string()),
            Some("UNKNOWN".to_owned()),
            "{root_ty}.{field_name} on T2 branch must force level: UNKNOWN"
        );
    }

    for op_desc in &comp.branches[0].operations {
        assert_eq!(op_desc.side_effect.level, SideEffectLevel::Unknown);
        assert!(!op_desc.side_effect.verified);
        assert!(op_desc.side_effect.requires_approval);
    }
}

// spec: RUN-009, NGC-011
#[test]
fn compose_desc_hash_hashes_five_fields_ignores_connector_spec_hash_and_branch_set_hash_tracks_status()
 {
    let base_branch = make_branch(
        "hash-svc",
        90,
        TrustTier::T1,
        BranchStatus::Active,
        "type Query { fetchItem(id: ID!): String }",
        vec![proto_op(
            "fetchItem",
            ProtoOpKind::Query,
            "baseline description",
        )],
    );
    let base_comp = compose(std::slice::from_ref(&base_branch)).expect("base compose");
    let base_op_hash = base_comp.branches[0].operations[0].desc_hash;
    let base_pinned_hash = base_comp.branches[0].pinned_desc_hash;

    // 1. Connector's `spec_hash` (and any fake hash string) MUST NOT be trusted for `desc_hash` / `pinned_desc_hash`.
    let mut spoofed_spec = base_branch.clone();
    spoofed_spec.description.spec_hash =
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned();
    let spoofed_comp = compose(&[spoofed_spec]).expect("spoofed spec_hash compose");
    assert_eq!(
        spoofed_comp.branches[0].operations[0].desc_hash, base_op_hash,
        "desc_hash must be computed by GAP and ignore ConnectorDescription.spec_hash"
    );
    assert_eq!(
        spoofed_comp.branches[0].pinned_desc_hash, base_pinned_hash,
        "pinned_desc_hash must ignore ConnectorDescription.spec_hash"
    );
    assert_eq!(spoofed_comp.branches[0].spec_hash, [0xff; 32]);

    // 2. Changing each of the 5 hashed fields (`name`, `description`, `input_schema`, `output_schema`, `annotations`) changes `desc_hash`.
    let mut by_name = base_branch.clone();
    by_name.description.schema_sdl = "type Query { fetchItemRenamed(id: ID!): String }".to_owned();
    by_name.description.operations[0].name = "fetchItemRenamed".to_owned();

    let mut by_desc = base_branch.clone();
    by_desc.description.operations[0].description = "modified description".to_owned();

    let mut by_input = base_branch.clone();
    by_input.description.operations[0].input_schema = Some(prost_json_obj(&[
        ("type", serde_json::Value::String("object".to_owned())),
        ("additionalProperties", serde_json::Value::Bool(false)),
    ]));

    let mut by_output = base_branch.clone();
    by_output.description.operations[0].output_schema = Some(prost_json_obj(&[(
        "type",
        serde_json::Value::String("string".to_owned()),
    )]));

    let mut by_annot = base_branch.clone();
    by_annot.description.operations[0]
        .annotations
        .insert("extra".to_owned(), "changed".to_owned());

    for (label, variant) in [
        ("name", by_name),
        ("description", by_desc),
        ("input_schema", by_input),
        ("output_schema", by_output),
        ("annotations", by_annot),
    ] {
        let c = compose(&[variant]).expect("variant compose");
        let h = c.branches[0].operations[0].desc_hash;
        assert_ne!(
            h, base_op_hash,
            "desc_hash must change when `{label}` changes"
        );
    }

    // 3. `branch_set_hash` changes with `BranchStatus`.
    let mut seen_hashes = std::collections::BTreeSet::new();
    for status in [
        BranchStatus::Active,
        BranchStatus::Drift,
        BranchStatus::Quarantined,
        BranchStatus::Disabled,
    ] {
        let mut b = base_branch.clone();
        b.status = status;
        let c = compose(&[b]).expect("status compose");
        assert!(
            seen_hashes.insert(c.branch_set_hash),
            "branch_set_hash must change for status {status:?}"
        );
    }
}

// spec: RUN-009, GQL-004
#[test]
fn compose_hostile_branch_10000_types_deep_wrappers_and_1mib_description_terminates_cleanly() {
    // 1. 10,000 types + 1 MiB operation description: must complete in bounded time with Ok or Err (no stack overflow / hang).
    let start = Instant::now();
    let mut large_sdl = String::with_capacity(200_000);
    for i in 0..10_000 {
        large_sdl.push_str(&format!("type T{i} {{ v: Int }}\n"));
    }
    large_sdl.push_str("type Query { rootOp: T0 }\n");

    let one_mib_desc = "A".repeat(1024 * 1024);
    let large_branch = make_branch(
        "hostile-big",
        100,
        TrustTier::T2,
        BranchStatus::Quarantined,
        &large_sdl,
        vec![proto_op("rootOp", ProtoOpKind::Query, &one_mib_desc)],
    );
    let large_res = compose(&[large_branch]);
    assert!(
        start.elapsed().as_secs() < 15,
        "10,000-type + 1 MiB branch took too long: {:?}",
        start.elapsed()
    );
    match large_res {
        Ok(comp) => assert_eq!(comp.branches.len(), 1),
        Err(err) => assert!(matches!(
            err.code(),
            ErrorCode::ParseError | ErrorCode::ValidationError | ErrorCode::ComplexityExceeded
        )),
    }

    // 2. Deeply nested list wrappers (`[[[...]]]` 96 levels deep): must terminate cleanly with Err, never stack overflow.
    let open = "[".repeat(96);
    let close = "]".repeat(96);
    let deep_sdl = format!("type Leaf {{ x: Int }}\ntype Query {{ deep: {open}Leaf{close} }}\n");
    let deep_branch = make_branch(
        "hostile-deep",
        101,
        TrustTier::T2,
        BranchStatus::Quarantined,
        &deep_sdl,
        vec![proto_op("deep", ProtoOpKind::Query, "deep wrapper")],
    );
    let deep_res = compose(&[deep_branch]);
    assert!(
        deep_res.is_err(),
        "96-level nested list wrapper must be rejected cleanly"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    // spec: RUN-009, RUN-011
    #[test]
    fn prop_ownership_map_matches_schema_branch_directives(
        branch_count in 1_usize..=5,
        obj_counts in prop::collection::vec(1_usize..=3, 5),
        field_counts in prop::collection::vec(1_usize..=3, 5),
        op_counts in prop::collection::vec(1_usize..=3, 5),
    ) {
        let mut branches = Vec::with_capacity(branch_count);
        for b_idx in 0..branch_count {
            let slug = format!("br-{b_idx}");
            let mut sdl = String::new();
            for o_idx in 0..obj_counts[b_idx] {
                sdl.push_str(&format!("type Entity{o_idx} {{\n"));
                for f_idx in 0..field_counts[b_idx] {
                    sdl.push_str(&format!("  f{f_idx}: String!\n"));
                }
                sdl.push_str("}\n");
            }

            let mut query_fields = Vec::new();
            let mut mutation_fields = Vec::new();
            let mut ops = Vec::new();
            for op_idx in 0..op_counts[b_idx] {
                let op_name = format!("b{b_idx}Op{op_idx}");
                let target_obj = format!("Entity{}", op_idx % obj_counts[b_idx]);
                if op_idx % 2 == 0 {
                    query_fields.push(format!("  {op_name}: {target_obj}"));
                    ops.push(proto_op(&op_name, ProtoOpKind::Query, "gen query"));
                } else {
                    mutation_fields.push(format!("  {op_name}(v: String): {target_obj}!"));
                    ops.push(proto_op(&op_name, ProtoOpKind::Mutation, "gen mutation"));
                }
            }
            if !query_fields.is_empty() {
                sdl.push_str(&format!("type Query {{\n{}\n}}\n", query_fields.join("\n")));
            }
            if !mutation_fields.is_empty() {
                sdl.push_str(&format!("type Mutation {{\n{}\n}}\n", mutation_fields.join("\n")));
            }

            let trust = match b_idx % 3 {
                0 => TrustTier::T0,
                1 => TrustTier::T1,
                _ => TrustTier::T2,
            };
            branches.push(make_branch(
                &slug,
                u8::try_from(110 + b_idx).expect("u8"),
                trust,
                BranchStatus::Active,
                &sdl,
                ops,
            ));
        }

        let comp = compose(&branches).expect("generated multi-branch schema must compose");
        let expected_ownership = ownership_oracle_from_schema(&comp.schema);
        prop_assert_eq!(comp.ownership, expected_ownership);
    }
}
