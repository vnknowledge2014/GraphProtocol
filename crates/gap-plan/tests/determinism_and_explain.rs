//! Determinism (`RUN-013`: 1,000 repeated plannings, pinned golden `plan_hash`, two-process
//! agreement, document-order invariance) and `PlanExplanation` golden JSON (`RUN-010`, `spec/02` §4).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::process::Command;

use common::{two_branch_snapshot, validate_op};
use gap_plan::{PlanConfig, explain, plan, plan_hash};
use serde_json::json;

const CANONICAL_TWO_BRANCH_QUERY: &str = r#"
    query CustomerAndLead($cid: ID!, $lid: ID!) {
        customer(id: $cid) {
            id
            name
            orders(first: 5) {
                id
                totalCents
            }
        }
        lead(id: $lid) {
            id
            email
        }
    }
"#;

/// Pinned `blake3-256` hex digest of `CANONICAL_TWO_BRANCH_QUERY` over `two_branch_snapshot()`.
/// Changing `Plan` serialization or ordering requires an intentional update to this golden value.
const PINNED_TWO_BRANCH_PLAN_HASH: &str =
    "d6a6b2f6c1bb87e91db42099923d60b69596ef931214ccbe7b74c5bb8c6c9930";

fn build_canonical_two_branch_plan() -> gap_plan::Plan {
    let (snap, schema) = two_branch_snapshot();
    let vars = json!({ "cid": "cust-100", "lid": "lead-200" });
    let doc = validate_op(
        &schema,
        CANONICAL_TWO_BRANCH_QUERY,
        Some("CustomerAndLead"),
        vars.as_object().unwrap(),
    );
    plan(&snap, &doc, &PlanConfig::default()).expect("canonical two-branch plan")
}

// spec: RUN-013, RUN-010
#[test]
fn run_013_1000_repeated_plannings_produce_single_pinned_golden_plan_hash() {
    let (snap, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();
    let vars = json!({ "cid": "cust-100", "lid": "lead-200" });
    let doc = validate_op(
        &schema,
        CANONICAL_TWO_BRANCH_QUERY,
        Some("CustomerAndLead"),
        vars.as_object().unwrap(),
    );

    let first_plan = plan(&snap, &doc, &cfg).expect("initial plan");
    let first_hash = plan_hash(&first_plan);
    assert_eq!(first_hash, PINNED_TWO_BRANCH_PLAN_HASH);

    for iter in 1..1_000 {
        let p = plan(&snap, &doc, &cfg).expect("repeated plan");
        let h = plan_hash(&p);
        assert_eq!(h, first_hash, "plan_hash diverged at iteration {iter}");
    }
}

// spec: RUN-013
#[test]
fn run_013_subprocess_probe_entry() {
    if std::env::var_os("GAP_PLAN_SUBPROCESS_PROBE").is_some() {
        let compiled = build_canonical_two_branch_plan();
        println!("GAP_PLAN_SUBPROCESS_HASH={}", plan_hash(&compiled));
    }
}

// spec: RUN-013
#[test]
fn run_013_two_processes_agree_on_plan_hash() {
    let parent_hash = plan_hash(&build_canonical_two_branch_plan());
    assert_eq!(parent_hash, PINNED_TWO_BRANCH_PLAN_HASH);

    let exe = std::env::current_exe().expect("locate current test executable");
    let mut child_hashes = Vec::with_capacity(2);
    for run_idx in 0..2 {
        let output = Command::new(&exe)
            .env("GAP_PLAN_SUBPROCESS_PROBE", "1")
            .args(["--exact", "run_013_subprocess_probe_entry", "--nocapture"])
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn child process {run_idx}: {e}"));
        assert!(
            output.status.success(),
            "child process {run_idx} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("utf-8 child stdout");
        let hash_line = stdout
            .lines()
            .find_map(|line| line.strip_prefix("GAP_PLAN_SUBPROCESS_HASH="))
            .unwrap_or_else(|| {
                panic!("missing GAP_PLAN_SUBPROCESS_HASH in child stdout: {stdout}")
            });
        child_hashes.push(hash_line.trim().to_owned());
    }

    assert_eq!(
        child_hashes[0], child_hashes[1],
        "two fresh OS processes disagreed on plan_hash"
    );
    assert_eq!(child_hashes[0], parent_hash);
}

// spec: RUN-013
#[test]
fn run_013_independent_root_fields_in_any_document_order_produce_identical_plan_and_hash() {
    let (snap, schema) = two_branch_snapshot();
    let cfg = PlanConfig::default();

    let permutations = [
        r#"query Q { customer(id: "c1") { id } order(id: "o1") { sku } lead(id: "l1") { email } }"#,
        r#"query Q { customer(id: "c1") { id } lead(id: "l1") { email } order(id: "o1") { sku } }"#,
        r#"query Q { order(id: "o1") { sku } customer(id: "c1") { id } lead(id: "l1") { email } }"#,
        r#"query Q { order(id: "o1") { sku } lead(id: "l1") { email } customer(id: "c1") { id } }"#,
        r#"query Q { lead(id: "l1") { email } customer(id: "c1") { id } order(id: "o1") { sku } }"#,
        r#"query Q { lead(id: "l1") { email } order(id: "o1") { sku } customer(id: "c1") { id } }"#,
    ];

    let baseline_doc = validate_op(&schema, permutations[0], Some("Q"), &serde_json::Map::new());
    let baseline_plan = plan(&snap, &baseline_doc, &cfg).expect("baseline plan");
    let baseline_hash = plan_hash(&baseline_plan);

    for (idx, query) in permutations.iter().enumerate().skip(1) {
        let doc = validate_op(&schema, query, Some("Q"), &serde_json::Map::new());
        let p = plan(&snap, &doc, &cfg).expect("permutation plan");
        assert_eq!(
            p, baseline_plan,
            "permutation {idx} produced different Plan"
        );
        assert_eq!(
            plan_hash(&p),
            baseline_hash,
            "permutation {idx} produced different plan_hash"
        );
    }
}

// spec: RUN-010, RUN-013, GQL-041
#[test]
fn run_010_explain_matches_two_branch_golden_json_without_execution() {
    let compiled = build_canonical_two_branch_plan();
    let explanation = explain(&compiled);
    let explanation_json = serde_json::to_value(&explanation).expect("serialize PlanExplanation");

    // Cost verification:
    // customer: weight 2 + 1 * (id(1) + name(1) + orders(first: 5 -> weight 2 + 5 * (id(1) + totalCents(2)) = 17)) = 21
    // lead: weight 3 + 1 * (id(1) + email(1)) = 5
    // Total = 26
    let expected = json!({
        "plan": {
            "branches": ["crm", "shop"],
            "cost": {
                "planned_cost": 26
            },
            "operation_name": "CustomerAndLead",
            "policy_version": 7,
            "root": {
                "kind": "parallel",
                "nodes": [
                    {
                        "kind": "fetch",
                        "arguments": {
                            "id": { "kind": "variable", "value": "lid" }
                        },
                        "branch": "crm",
                        "branch_uid": "018bcfe5-6c02-7020-8202-020202020202",
                        "operation": "lead",
                        "pushdown": {},
                        "response_key": "lead",
                        "selection": [
                            { "name": "id" },
                            { "name": "email" }
                        ]
                    },
                    {
                        "kind": "fetch",
                        "arguments": {
                            "id": { "kind": "variable", "value": "cid" }
                        },
                        "branch": "shop",
                        "branch_uid": "018bcfe5-6c01-7010-8101-010101010101",
                        "operation": "customer",
                        "pushdown": {},
                        "response_key": "customer",
                        "selection": [
                            { "name": "id" },
                            { "name": "name" },
                            {
                                "name": "orders",
                                "children": [
                                    { "name": "id" },
                                    { "name": "totalCents" }
                                ]
                            }
                        ]
                    }
                ]
            }
        },
        "branchesTouched": ["crm", "shop"],
        "estimatedCost": 26,
        "pushdowns": [
            "crm.lead:projection",
            "shop.customer:projection"
        ],
        "warnings": []
    });

    assert_eq!(explanation_json, expected);
}
