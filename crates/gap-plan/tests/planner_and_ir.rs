//! Ownership resolution (`RUN-011`), IR structure & serde (`RUN-010`), selection flattening,
//! variable-independence, refused operations, and cost-before-execution (`RUN-005`, `GQL-040`,
//! `GQL-041`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{crm_uid, shop_uid, two_branch_snapshot, validate_op};
use gap_core::config::LimitsConfig;
use gap_core::error::ErrorCode;
use gap_core::ids::{ApprovalId, BranchId};
use gap_plan::{
    ArgValue, Fetch, GateNode, JoinCardinality, JoinKeyPair, JoinNode, JoinPlanStrategy,
    MutateNode, Plan, PlanConfig, PlanCost, PlanNode, Pushdown, SelectionDirective, SelectionField,
    TraverseNode, plan, plan_hash, plan_query, plan_to_value,
};
use gap_ql::{CostModelConfig, compute_operation_cost};
use serde_json::json;

// spec: RUN-010, RUN-013
#[test]
fn run_010_ir_nodes_roundtrip_serde_and_serde_json_map_sorts_keys() {
    // Verify `serde_json::Map` in this workspace does NOT enable `preserve_order`: keys inserted
    // out of order iterate in lexicographical `BTreeMap` order (`Brief §3.5`).
    let mut raw_map = serde_json::Map::new();
    raw_map.insert("zeta".to_owned(), json!(1));
    raw_map.insert("alpha".to_owned(), json!(2));
    raw_map.insert("mu".to_owned(), json!(3));
    let keys: Vec<&str> = raw_map.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["alpha", "mu", "zeta"]);

    let shop: BranchId = "shop".parse().expect("shop");
    let crm: BranchId = "crm".parse().expect("crm");
    let fetch_a = Fetch {
        branch: shop.clone(),
        branch_uid: shop_uid(),
        operation: "customer".to_owned(),
        arguments: BTreeMap::from([("id".to_owned(), ArgValue::Variable("cid".to_owned()))]),
        selection: vec![SelectionField::leaf("id")],
        response_key: "customer".to_owned(),
        pushdown: Pushdown {
            limit: Some(10),
            order_by: vec!["id ASC".to_owned()],
            predicate: Some(json!({"op": "EQ", "field": "active", "value": true})),
        },
    };
    let fetch_b = Fetch {
        branch: crm.clone(),
        branch_uid: crm_uid(),
        operation: "lead".to_owned(),
        arguments: BTreeMap::from([("id".to_owned(), ArgValue::Literal(json!("L-1")))]),
        selection: vec![SelectionField::leaf("email")],
        response_key: "lead".to_owned(),
        pushdown: Pushdown::default(),
    };

    let join_node = PlanNode::Join(Box::new(JoinNode {
        lhs: PlanNode::Fetch(fetch_a.clone()),
        rhs: PlanNode::Fetch(fetch_b.clone()),
        keys: vec![JoinKeyPair {
            from: "id".to_owned(),
            to: "customerId".to_owned(),
        }],
        cardinality: JoinCardinality::OneToMany,
        strategy: JoinPlanStrategy::ForeachBatch,
    }));
    let traverse_node = PlanNode::Traverse(Box::new(TraverseNode {
        arguments: BTreeMap::from([("maxDepth".to_owned(), ArgValue::Literal(json!(2)))]),
        selection: vec![SelectionField::leaf("truncated")],
        response_key: "traverse".to_owned(),
    }));
    let mutate_node = PlanNode::Mutate(Box::new(MutateNode {
        branch: shop.clone(),
        branch_uid: shop_uid(),
        operation: "createOrder".to_owned(),
        arguments: BTreeMap::from([("sku".to_owned(), ArgValue::Literal(json!("SKU-1")))]),
        selection: vec![SelectionField::leaf("id")],
        response_key: "createOrder".to_owned(),
        dry_run: true,
    }));
    let gate_node = PlanNode::Gate(Box::new(GateNode {
        approval: Some(ApprovalId::from_unix_ms(1_700_000_000_099, [0x09; 10])),
        summary: "shop.createOrder(sku=SKU-1)".to_owned(),
        child: mutate_node,
    }));
    let flatten_node = PlanNode::Flatten {
        path: vec!["customer".to_owned(), "leads".to_owned()],
        child: Box::new(join_node),
    };
    let seq_node = PlanNode::Sequence {
        nodes: vec![flatten_node, traverse_node, gate_node],
    };
    let sample_plan = Plan {
        root: PlanNode::Parallel {
            nodes: vec![PlanNode::Fetch(fetch_a), seq_node],
        },
        operation_name: Some("Composite".to_owned()),
        cost: PlanCost {
            planned_cost: 42,
            effective_budget: Some(100),
            warnings: vec!["sample warning".to_owned()],
        },
        branches: vec![crm, shop],
        policy_version: 7,
    };

    let json_val = plan_to_value(&sample_plan);
    assert!(
        json_val.is_object(),
        "plan_to_value must produce JSON object"
    );
    let encoded = serde_json::to_string(&json_val).expect("serialize plan json");
    let decoded: Plan = serde_json::from_str(&encoded).expect("deserialize plan json");
    assert_eq!(decoded, sample_plan);
    let h = plan_hash(&sample_plan);
    assert_eq!(h.len(), 64);
    assert!(
        h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
}

// spec: RUN-011
#[test]
fn run_011_unknown_or_unowned_root_field_is_rejected_with_gap_unknown_field() {
    let (snap, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();

    // 1. `orphanMetric` exists on `Query` in SDL, so `ValidatedOperation` succeeds, but has NO
    //    owning branch in `snapshot.ownership` -> must fail with `GAP_UNKNOWN_FIELD` (never guess!).
    let unowned = validate_op(&schema, "{ orphanMetric }", None, &serde_json::Map::new());
    let err = plan(&snap, &unowned, &cfg).expect_err("unowned field must fail");
    assert_eq!(err.code(), ErrorCode::UnknownField);

    // 2. Completely nonexistent root field via `plan_query` -> `GAP_UNKNOWN_FIELD`.
    let missing_err = plan_query(
        &snap,
        "{ nonExistentRootField { id } }",
        None,
        &serde_json::Map::new(),
        &cfg,
    )
    .expect_err("unknown root field must fail");
    assert_eq!(missing_err.code(), ErrorCode::UnknownField);
}

// spec: RUN-011, RUN-010
#[test]
fn run_011_branch_owned_root_field_routes_only_to_its_owner_and_single_fetch_is_unwrapped() {
    let (snap, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();

    // Query `lead` owned by `crm` (the SECOND branch in `snap.branches` after `shop`!).
    // If a broken planner picked the first branch instead of consulting `snap.ownership`, this fails.
    let doc = validate_op(
        &schema,
        r#"{ lead(id: "lead-7") { id email } }"#,
        None,
        &serde_json::Map::new(),
    );
    let compiled = plan(&snap, &doc, &cfg).expect("single-branch crm query must plan");

    assert_eq!(compiled.operation_name, None);
    assert_eq!(compiled.policy_version, 7);
    assert_eq!(compiled.branches, vec![BranchId::new("crm").expect("crm")]);

    // Single root field MUST NOT be wrapped in `PlanNode::Parallel`.
    let PlanNode::Fetch(fetch) = &compiled.root else {
        panic!(
            "expected unwrapped PlanNode::Fetch, got {:?}",
            compiled.root
        );
    };
    assert_eq!(fetch.branch.as_str(), "crm");
    assert_eq!(fetch.branch_uid, crm_uid());
    assert_eq!(fetch.operation, "lead");
    assert_eq!(fetch.response_key, "lead");
    assert_eq!(
        fetch.arguments,
        BTreeMap::from([("id".to_owned(), ArgValue::Literal(json!("lead-7")))])
    );
    assert_eq!(
        fetch.selection,
        vec![SelectionField::leaf("id"), SelectionField::leaf("email")]
    );
    assert!(fetch.pushdown.is_empty());
}

// spec: RUN-011, RUN-013
#[test]
fn run_011_multiple_root_fields_on_same_branch_remain_separate_sorted_fetches() {
    let (snap, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();

    // Three root fields all owned by `shop`, written in reverse order of `(branch, op, response_key)`.
    let doc = validate_op(
        &schema,
        r#"
        query ShopBundle {
            order(id: "o-9") { id sku }
            vip: customer(id: "c-2") { id }
            customer(id: "c-1") { name }
        }
        "#,
        Some("ShopBundle"),
        &serde_json::Map::new(),
    );
    let compiled = plan(&snap, &doc, &cfg).expect("same-branch multi-root query must plan");
    assert_eq!(compiled.operation_name.as_deref(), Some("ShopBundle"));
    assert_eq!(
        compiled.branches,
        vec![BranchId::new("shop").expect("shop")]
    );

    let PlanNode::Parallel { nodes } = &compiled.root else {
        panic!(
            "expected Parallel for 3 root fetches, got {:?}",
            compiled.root
        );
    };
    assert_eq!(
        nodes.len(),
        3,
        "same-branch root fields must stay separate Fetch nodes"
    );

    let keys: Vec<(&str, &str, &str)> = nodes
        .iter()
        .map(|n| match n {
            PlanNode::Fetch(f) => (
                f.branch.as_str(),
                f.operation.as_str(),
                f.response_key.as_str(),
            ),
            other => panic!("expected Fetch child, got {other:?}"),
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            ("shop", "customer", "customer"),
            ("shop", "customer", "vip"),
            ("shop", "order", "order"),
        ]
    );
}

// spec: RUN-010, RUN-011, RUN-013
#[test]
fn run_010_aliases_variables_nested_selections_typename_fragments_and_directives() {
    let (snap, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();

    let query = r#"
        query LeadDetail(
            $lid: ID!,
            $tag: String!,
            $reg: String!,
            $hidePhone: Boolean!,
            $includeCompany: Boolean!
        ) {
            ...RootLeadSpread
        }

        fragment RootLeadSpread on Query {
            primaryLead: lead(
                id: $lid,
                tags: [$tag, "priority"],
                filter: { region: $reg, active: true }
            ) {
                __typename
                leadId: id
                ...LeadContactFields
                ... on crm_Lead @include(if: $includeCompany) {
                    score
                    company {
                        id
                        companyName: name
                        __typename
                    }
                }
                ... {
                    phone @skip(if: $hidePhone)
                }
            }
        }

        fragment LeadContactFields on crm_Lead {
            email @include(if: true)
        }
    "#;

    let vars_a = json!({
        "lid": "lead-alpha",
        "tag": "enterprise",
        "reg": "US",
        "hidePhone": true,
        "includeCompany": false
    });
    let vars_b = json!({
        "lid": "lead-omega",
        "tag": "smb",
        "reg": "VN",
        "hidePhone": false,
        "includeCompany": true
    });

    let validated_a = validate_op(
        &schema,
        query,
        Some("LeadDetail"),
        vars_a.as_object().unwrap(),
    );
    let validated_b = validate_op(
        &schema,
        query,
        Some("LeadDetail"),
        vars_b.as_object().unwrap(),
    );

    let plan_a = plan(&snap, &validated_a, &cfg).expect("plan with vars_a");
    let plan_b = plan(&snap, &validated_b, &cfg).expect("plan with vars_b");

    // Variable-independence: changing runtime variable values MUST NOT alter the compiled Plan!
    assert_eq!(plan_a, plan_b);
    assert_eq!(plan_hash(&plan_a), plan_hash(&plan_b));

    let PlanNode::Fetch(fetch) = &plan_a.root else {
        panic!(
            "expected single Fetch from root fragment spread, got {:?}",
            plan_a.root
        );
    };
    assert_eq!(fetch.branch.as_str(), "crm");
    assert_eq!(fetch.operation, "lead");
    assert_eq!(fetch.response_key, "primaryLead");

    // Arguments preserve `ArgValue::Variable` (including nested inside List and Object).
    assert_eq!(
        fetch.arguments.get("id"),
        Some(&ArgValue::Variable("lid".to_owned()))
    );
    assert_eq!(
        fetch.arguments.get("tags"),
        Some(&ArgValue::List(vec![
            ArgValue::Variable("tag".to_owned()),
            ArgValue::Literal(json!("priority")),
        ]))
    );
    assert_eq!(
        fetch.arguments.get("filter"),
        Some(&ArgValue::Object(BTreeMap::from([
            ("active".to_owned(), ArgValue::Literal(json!(true))),
            ("region".to_owned(), ArgValue::Variable("reg".to_owned())),
        ])))
    );

    // Flattened selection tree preserves exact document order, `__typename`, aliases, nested
    // children, and `@skip` / `@include` directives (including inherited fragment-level `@include`).
    assert_eq!(
        fetch.selection,
        vec![
            SelectionField::leaf("__typename"),
            SelectionField {
                name: "id".to_owned(),
                alias: Some("leadId".to_owned()),
                directives: Vec::new(),
                children: Vec::new(),
            },
            SelectionField {
                name: "email".to_owned(),
                alias: None,
                directives: vec![SelectionDirective {
                    name: "include".to_owned(),
                    if_arg: ArgValue::Literal(json!(true)),
                }],
                children: Vec::new(),
            },
            SelectionField {
                name: "score".to_owned(),
                alias: None,
                directives: vec![SelectionDirective {
                    name: "include".to_owned(),
                    if_arg: ArgValue::Variable("includeCompany".to_owned()),
                }],
                children: Vec::new(),
            },
            SelectionField {
                name: "company".to_owned(),
                alias: None,
                directives: vec![SelectionDirective {
                    name: "include".to_owned(),
                    if_arg: ArgValue::Variable("includeCompany".to_owned()),
                }],
                children: vec![
                    SelectionField::leaf("id"),
                    SelectionField {
                        name: "name".to_owned(),
                        alias: Some("companyName".to_owned()),
                        directives: Vec::new(),
                        children: Vec::new(),
                    },
                    SelectionField::leaf("__typename"),
                ],
            },
            SelectionField {
                name: "phone".to_owned(),
                alias: None,
                directives: vec![SelectionDirective {
                    name: "skip".to_owned(),
                    if_arg: ArgValue::Variable("hidePhone".to_owned()),
                }],
                children: Vec::new(),
            },
        ]
    );
}

// spec: RUN-010, RUN-011
#[test]
fn run_011_gap_discovery_introspection_mutations_and_subscriptions_are_refused() {
    let (snap, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();

    let refused_docs = [
        "{ gap { metaGraph { generation } } }",
        r#"{ __search(query: "customer") { abstained } }"#,
        "{ __metaGraph { generation } }",
        "{ __schema { queryType { name } } }",
        "{ __typename }",
        r#"mutation { createOrder(sku: "SKU-9") { id } }"#,
        r#"subscription { gapTaskEvents(taskId: "018bcfe5-6c01-7010-8101-010101010101") { seq } }"#,
    ];

    for query in refused_docs {
        let doc = validate_op(&schema, query, None, &serde_json::Map::new());
        let err = plan(&snap, &doc, &cfg).unwrap_err();
        assert_eq!(
            err.code(),
            ErrorCode::UnsupportedFeature,
            "expected UnsupportedFeature for `{query}`, got {err:?}"
        );
    }
}

// spec: RUN-005, GQL-040, GQL-041
#[test]
fn run_005_over_budget_documents_are_refused_before_any_callee_and_cost_matches_gap_ql() {
    let (snap, schema) = two_branch_snapshot();
    let callee_invocations = AtomicUsize::new(0);

    let run_pipeline = |doc: &gap_ql::ValidatedOperation,
                        cfg: &PlanConfig|
     -> Result<Plan, gap_core::error::GapError> {
        let compiled = plan(&snap, doc, cfg)?;
        // Simulated downstream connector dispatch: must NEVER be reached when cost exceeds budget.
        callee_invocations.fetch_add(1, Ordering::SeqCst);
        Ok(compiled)
    };

    // Query cost breakdown (`TWO_BRANCH_SDL`):
    // orders (no `first` -> warning + default_list_size 100):
    //   weight 2 + 100 * (id(1) + totalCents(2)) = 302
    // lead: weight 3 + 1 * (id(1) + email(1)) = 5
    // Total = 307
    let query = r#"{ orders { id totalCents } lead(id: "L-1") { id email } }"#;
    let doc = validate_op(&schema, query, None, &serde_json::Map::new());

    let expected_cost = compute_operation_cost(
        &doc,
        &LimitsConfig::default(),
        None,
        CostModelConfig::default(),
    )
    .expect("compute_operation_cost");
    assert_eq!(expected_cost.planned_cost, 307);
    assert!(!expected_cost.warnings.is_empty());

    // 1. Over system `limits.max_cost_units = 200` -> `GAP_COMPLEXITY_EXCEEDED`, callee count == 0
    let mut sys_tight = PlanConfig::default();
    sys_tight.limits.max_cost_units = 200;
    let sys_err = run_pipeline(&doc, &sys_tight).expect_err("307 > 200 max_cost_units");
    assert_eq!(sys_err.code(), ErrorCode::ComplexityExceeded);
    assert_eq!(callee_invocations.load(Ordering::SeqCst), 0);

    // 2. Over `@budget(costUnits: 150)` -> `GAP_BUDGET_EXCEEDED`, callee count == 0
    let budgeted_doc = validate_op(
        &schema,
        r#"query @budget(costUnits: 150) { orders { id totalCents } lead(id: "L-1") { id email } }"#,
        None,
        &serde_json::Map::new(),
    );
    let dir_err =
        run_pipeline(&budgeted_doc, &PlanConfig::default()).expect_err("307 > 150 @budget");
    assert_eq!(dir_err.code(), ErrorCode::BudgetExceeded);
    assert_eq!(callee_invocations.load(Ordering::SeqCst), 0);

    // 3. Over `request_budget_cost_units = Some(250)` -> `GAP_BUDGET_EXCEEDED`, callee count == 0
    let req_tight = PlanConfig {
        request_budget_cost_units: Some(250),
        ..PlanConfig::default()
    };
    let req_err = run_pipeline(&doc, &req_tight).expect_err("307 > 250 request budget");
    assert_eq!(req_err.code(), ErrorCode::BudgetExceeded);
    assert_eq!(callee_invocations.load(Ordering::SeqCst), 0);

    // 4. Within budget -> succeeds and `plan.cost` matches `gap_ql::cost` exactly.
    let ok_plan = run_pipeline(&doc, &PlanConfig::default()).expect("within default 10,000 cap");
    assert_eq!(callee_invocations.load(Ordering::SeqCst), 1);
    assert_eq!(ok_plan.cost.planned_cost, expected_cost.planned_cost);
    assert_eq!(
        ok_plan.cost.effective_budget,
        expected_cost.effective_budget
    );
    assert_eq!(ok_plan.cost.warnings, expected_cost.warnings);
}
