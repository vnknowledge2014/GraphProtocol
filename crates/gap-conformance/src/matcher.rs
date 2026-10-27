//! Subset matcher and normalized `ObservedResponse` (`spec/02-gap-ql.md` §11).

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use thiserror::Error;

/// First-difference diagnostic returned when `expect` does not subset-match `observed`
/// (`spec/02-gap-ql.md` §11).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("mismatch at `{path}`: {reason}")]
pub struct MatchMismatch {
    /// JSONPath-style location of the first difference (rooted at `$`).
    pub path: String,
    /// Human-readable description of the mismatch.
    pub reason: String,
}

/// Normalized response envelope matched against a fixture's `expect` object
/// (`spec/02-gap-ql.md` §11).
#[derive(Debug, Clone, PartialEq)]
pub struct ObservedResponse {
    /// HTTP status code (e.g. `200`, `400`, `401`).
    pub status: u16,
    /// GraphQL `data` field (`Value::Null` when absent).
    pub data: Value,
    /// GraphQL `errors` field (`Value::Array([])` when absent).
    pub errors: Value,
    /// GraphQL `extensions` field (`Value::Object({})` when absent).
    pub extensions: Value,
    /// Lower-cased HTTP response headers.
    pub headers: BTreeMap<String, String>,
    /// Derived flag: `true` iff `data` is `null` or was absent from the response body.
    pub data_null: bool,
}

impl ObservedResponse {
    /// Constructs an [`ObservedResponse`] from HTTP status, header pairs, and optional parsed
    /// JSON response object (`spec/02-gap-ql.md` §11).
    ///
    /// Server-supplied `dataNull` fields in `body_obj` are ignored; `data_null` is always derived
    /// from whether `data` is `null` or absent.
    #[must_use]
    pub fn from_parts<I, K, V>(
        status: u16,
        headers: I,
        body_obj: Option<&Map<String, Value>>,
    ) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let data = body_obj
            .and_then(|obj| obj.get("data"))
            .cloned()
            .unwrap_or(Value::Null);
        let errors = body_obj
            .and_then(|obj| obj.get("errors"))
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let extensions = body_obj
            .and_then(|obj| obj.get("extensions"))
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));

        let mut normalized_headers = BTreeMap::<String, String>::new();
        for (name, value) in headers {
            let lower = name.as_ref().to_ascii_lowercase();
            let val = value.as_ref();
            normalized_headers
                .entry(lower)
                .and_modify(|existing| {
                    existing.push_str(", ");
                    existing.push_str(val);
                })
                .or_insert_with(|| val.to_owned());
        }

        let data_null = data.is_null();
        Self {
            status,
            data,
            errors,
            extensions,
            headers: normalized_headers,
            data_null,
        }
    }

    /// Converts this [`ObservedResponse`] into the canonical JSON object matched by [`subset_match`].
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut root = Map::with_capacity(6);
        root.insert("status".to_owned(), Value::from(self.status));
        root.insert("data".to_owned(), self.data.clone());
        root.insert("errors".to_owned(), self.errors.clone());
        root.insert("extensions".to_owned(), self.extensions.clone());

        let mut headers_obj = Map::with_capacity(self.headers.len());
        for (k, v) in &self.headers {
            headers_obj.insert(k.clone(), Value::String(v.clone()));
        }
        root.insert("headers".to_owned(), Value::Object(headers_obj));
        root.insert("dataNull".to_owned(), Value::Bool(self.data_null));
        Value::Object(root)
    }

    /// Matches `expected` against this [`ObservedResponse`] using [`subset_match`].
    ///
    /// # Errors
    ///
    /// Returns [`MatchMismatch`] carrying the JSON path of the first difference.
    pub fn matches(&self, expected: &Value) -> Result<(), MatchMismatch> {
        subset_match(expected, &self.to_value())
    }
}

/// Checks whether `expected` subset-matches `observed` per `spec/02-gap-ql.md` §11.
///
/// - **Objects:** every key in `expected` must exist in `observed` and match recursively;
///   unlisted keys in `observed` are ignored.
/// - **Scalars and `null`:** equal (`Number`s compare by mathematical value).
/// - **Arrays:** ordered by default (exact length and index-wise recursive match).
/// - **Unordered arrays:** `{"unordered": true, "items": [...]}` matches a `Value::Array` of the
///   same length iff a one-to-one bipartite pairing exists (computed via Kuhn's augmenting paths).
///
/// # Errors
///
/// Returns [`MatchMismatch`] carrying the JSON path of the first difference.
pub fn subset_match(expected: &Value, observed: &Value) -> Result<(), MatchMismatch> {
    subset_match_at(expected, observed, "$")
}

fn subset_match_at(expected: &Value, observed: &Value, path: &str) -> Result<(), MatchMismatch> {
    if let Some(expected_items) = as_unordered_wrapper(expected) {
        return match_unordered_array(expected_items, observed, path);
    }

    match (expected, observed) {
        (Value::Null, Value::Null) => Ok(()),
        (Value::Bool(exp), Value::Bool(obs)) if exp == obs => Ok(()),
        (Value::String(exp), Value::String(obs)) if exp == obs => Ok(()),
        (Value::Number(exp), Value::Number(obs)) if numbers_equal(exp, obs) => Ok(()),
        (Value::Array(exp_items), Value::Array(obs_items)) => {
            if exp_items.len() != obs_items.len() {
                return Err(MatchMismatch {
                    path: path.to_owned(),
                    reason: format!(
                        "expected array of length {}, got length {}",
                        exp_items.len(),
                        obs_items.len()
                    ),
                });
            }
            for (idx, (exp_item, obs_item)) in exp_items.iter().zip(obs_items.iter()).enumerate() {
                let child_path = format!("{path}[{idx}]");
                subset_match_at(exp_item, obs_item, &child_path)?;
            }
            Ok(())
        }
        (Value::Object(exp_obj), Value::Object(obs_obj)) => {
            for (key, exp_val) in exp_obj {
                let child_path = join_object_path(path, key);
                let Some(obs_val) = obs_obj.get(key) else {
                    return Err(MatchMismatch {
                        path: child_path,
                        reason: format!("missing expected key `{key}`"),
                    });
                };
                subset_match_at(exp_val, obs_val, &child_path)?;
            }
            Ok(())
        }
        _ => Err(MatchMismatch {
            path: path.to_owned(),
            reason: format!("expected {expected}, got {observed}"),
        }),
    }
}

fn as_unordered_wrapper(value: &Value) -> Option<&[Value]> {
    let obj = value.as_object()?;
    if obj.len() != 2 {
        return None;
    }
    if obj.get("unordered") != Some(&Value::Bool(true)) {
        return None;
    }
    obj.get("items")?.as_array().map(Vec::as_slice)
}

fn match_unordered_array(
    expected_items: &[Value],
    observed: &Value,
    path: &str,
) -> Result<(), MatchMismatch> {
    let Some(obs_items) = observed.as_array() else {
        return Err(MatchMismatch {
            path: path.to_owned(),
            reason: format!(
                "expected unordered array of length {}, got {observed}",
                expected_items.len()
            ),
        });
    };

    let n = expected_items.len();
    if n != obs_items.len() {
        return Err(MatchMismatch {
            path: path.to_owned(),
            reason: format!(
                "expected unordered array of length {n}, got length {}",
                obs_items.len()
            ),
        });
    }

    let mut adj: Vec<Vec<usize>> = Vec::with_capacity(n);
    for exp_item in expected_items {
        let mut candidates = Vec::new();
        for (obs_idx, obs_item) in obs_items.iter().enumerate() {
            if subset_match_at(exp_item, obs_item, "$").is_ok() {
                candidates.push(obs_idx);
            }
        }
        adj.push(candidates);
    }

    // `matched_obs[j]` holds the index `i` in `expected_items` currently paired with `obs_items[j]`.
    let mut matched_obs: Vec<Option<usize>> = vec![None; n];
    for (exp_idx, exp_item) in expected_items.iter().enumerate() {
        let mut visited = vec![false; n];
        if !try_kuhn(exp_idx, &adj, &mut visited, &mut matched_obs) {
            return Err(MatchMismatch {
                path: format!("{path}.items[{exp_idx}]"),
                reason: format!(
                    "no one-to-one pairing in unordered array for expected item {exp_item}"
                ),
            });
        }
    }

    Ok(())
}

fn try_kuhn(
    exp_idx: usize,
    adj: &[Vec<usize>],
    visited: &mut [bool],
    matched_obs: &mut [Option<usize>],
) -> bool {
    for &obs_idx in &adj[exp_idx] {
        if visited[obs_idx] {
            continue;
        }
        visited[obs_idx] = true;
        let can_assign = match matched_obs[obs_idx] {
            None => true,
            Some(prev_exp_idx) => try_kuhn(prev_exp_idx, adj, visited, matched_obs),
        };
        if can_assign {
            matched_obs[obs_idx] = Some(exp_idx);
            return true;
        }
    }
    false
}

fn numbers_equal(a: &serde_json::Number, b: &serde_json::Number) -> bool {
    match (as_exact_i128(a), as_exact_i128(b)) {
        (Some(ia), Some(ib)) => ia == ib,
        (Some(int_val), None) => b.as_f64().is_some_and(|f| i128_equals_f64(int_val, f)),
        (None, Some(int_val)) => a.as_f64().is_some_and(|f| i128_equals_f64(int_val, f)),
        (None, None) => match (a.as_f64(), b.as_f64()) {
            (Some(fa), Some(fb)) => fa == fb,
            _ => false,
        },
    }
}

fn as_exact_i128(n: &serde_json::Number) -> Option<i128> {
    if let Some(u) = n.as_u64() {
        Some(i128::from(u))
    } else {
        n.as_i64().map(i128::from)
    }
}

/// Exact mathematical comparison between an integer in `[i64::MIN, u64::MAX]` and an `f64`.
///
/// Both `-2^63` (`-9_223_372_036_854_775_808.0`) and `2^64` (`18_446_744_073_709_551_616.0`) are
/// exact powers of two in IEEE-754 `f64`, and any finite `f64` in `[-2^63, 2^64)` with zero
/// fractional part represents an exact integer that converts losslessly via `as i128`.
fn i128_equals_f64(int_val: i128, float_val: f64) -> bool {
    const MIN_BOUND: f64 = -9_223_372_036_854_775_808.0; // -2^63 (exact in f64)
    const MAX_EXCL_BOUND: f64 = 18_446_744_073_709_551_616.0; // 2^64 (exact in f64)
    if !float_val.is_finite() || float_val.fract() != 0.0 {
        return false;
    }
    if !(MIN_BOUND..MAX_EXCL_BOUND).contains(&float_val) {
        return false;
    }
    #[allow(clippy::cast_possible_truncation)]
    let float_as_i128 = float_val as i128;
    int_val == float_as_i128
}

fn join_object_path(parent: &str, key: &str) -> String {
    if is_simple_identifier(key) {
        format!("{parent}.{key}")
    } else {
        format!("{parent}[{key:?}]")
    }
}

fn is_simple_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    // spec: GQL-002 (VER-010)
    #[test]
    fn observed_response_derives_defaults_and_ignores_server_data_null() {
        let server_body = json!({
            "errors": [{"message": "unknown field", "extensions": {"code": "GAP_UNKNOWN_FIELD"}}],
            "dataNull": false
        });
        let obs = ObservedResponse::from_parts(
            200,
            [
                ("Content-Type", "application/graphql-response+json"),
                ("X-Trace-Id", "abc"),
                ("X-Multi", "one"),
                ("x-multi", "two"),
            ],
            server_body.as_object(),
        );
        assert_eq!(obs.status, 200);
        assert_eq!(obs.data, Value::Null);
        assert!(obs.data_null);
        assert_eq!(obs.extensions, json!({}));
        assert_eq!(
            obs.headers.get("content-type").map(String::as_str),
            Some("application/graphql-response+json")
        );
        assert_eq!(
            obs.headers.get("x-trace-id").map(String::as_str),
            Some("abc")
        );
        assert_eq!(
            obs.headers.get("x-multi").map(String::as_str),
            Some("one, two")
        );

        let expected = json!({
            "status": 200,
            "errors": [{"extensions": {"code": "GAP_UNKNOWN_FIELD"}}],
            "dataNull": true,
            "headers": {"content-type": "application/graphql-response+json"}
        });
        obs.matches(&expected).expect("subset match should succeed");
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn observed_response_sets_data_null_false_when_data_present_even_if_server_claims_true() {
        let server_body = json!({
            "data": {"billing": {"total": 42}},
            "dataNull": true
        });
        let obs = ObservedResponse::from_parts(
            200,
            std::iter::empty::<(&str, &str)>(),
            server_body.as_object(),
        );
        assert!(!obs.data_null);
        assert_eq!(obs.errors, json!([]));
        assert_eq!(obs.extensions, json!({}));

        let err = obs
            .matches(&json!({"dataNull": true}))
            .expect_err("dataNull must be false when data is non-null");
        assert_eq!(err.path, "$.dataNull");
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn observed_response_with_absent_json_body_populates_all_defaults() {
        let obs = ObservedResponse::from_parts(502, [("Server", "envoy")], None);
        assert_eq!(
            obs.to_value(),
            json!({
                "status": 502,
                "data": null,
                "errors": [],
                "extensions": {},
                "headers": {"server": "envoy"},
                "dataNull": true
            })
        );
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn scalars_and_numbers_compare_by_value_preserving_64bit_integer_precision() {
        subset_match(&json!(1), &json!(1.0)).expect("1 == 1.0");
        subset_match(&json!(1.0), &json!(1)).expect("1.0 == 1");
        subset_match(&json!(0), &json!(-0.0)).expect("0 == -0.0");
        subset_match(&json!(-42), &json!(-42.0)).expect("-42 == -42.0");
        subset_match(&json!(1.25), &json!(1.25)).expect("1.25 == 1.25");
        subset_match(&json!(u64::MAX), &json!(u64::MAX)).expect("u64::MAX == u64::MAX");

        assert!(subset_match(&json!(1), &json!(1.5)).is_err());
        assert!(subset_match(&json!(1.5), &json!(1)).is_err());
        assert!(subset_match(&json!(1.25), &json!(1.5)).is_err());
        // 2^53 and 2^53 + 1 collide if cast naively to f64; exact comparison must distinguish them.
        assert!(
            subset_match(
                &json!(9_007_199_254_740_992_u64),
                &json!(9_007_199_254_740_993_u64)
            )
            .is_err()
        );
        assert!(
            subset_match(
                &json!(9_007_199_254_740_993_u64),
                &json!(9_007_199_254_740_992.0_f64)
            )
            .is_err()
        );
        assert!(
            subset_match(
                &json!(-9_007_199_254_740_992_i64),
                &json!(-9_007_199_254_740_993_i64)
            )
            .is_err()
        );
        assert!(subset_match(&json!(u64::MAX), &json!(u64::MAX - 1)).is_err());
        assert!(subset_match(&json!(0), &json!(1e30_f64)).is_err());
        assert!(subset_match(&json!(0), &json!(-1e30_f64)).is_err());

        assert!(subset_match(&json!(true), &json!(false)).is_err());
        assert!(subset_match(&json!("1"), &json!(1)).is_err());
        assert!(subset_match(&json!(null), &json!(0)).is_err());
        assert!(subset_match(&json!(0), &json!(null)).is_err());
    }

    // spec: GQL-002, GQL-070, GQL-062 (VER-010)
    #[test]
    fn spec_section_11_example_fixture_matches_and_reports_exact_mismatch_paths() {
        let obs = ObservedResponse::from_parts(
            200,
            [("Content-Type", "application/graphql-response+json")],
            json!({
                "data": null,
                "errors": [{
                    "message": "Cannot query field `invoce` on type `Billing`.",
                    "locations": [{"line": 1, "column": 13}],
                    "extensions": {
                        "code": "GAP_UNKNOWN_FIELD",
                        "class": "request",
                        "retry": {"retryable": false},
                        "suggestion": {"didYouMean": ["invoice"]}
                    }
                }]
            })
            .as_object(),
        );

        let expect = json!({
            "status": 200,
            "errors": [{
                "extensions": {
                    "code": "GAP_UNKNOWN_FIELD",
                    "suggestion": {"didYouMean": ["invoice"]}
                }
            }],
            "dataNull": true
        });
        obs.matches(&expect).expect("spec §11 example must match");

        let wrong_code = json!({
            "status": 200,
            "errors": [{"extensions": {"code": "GAP_FORBIDDEN"}}]
        });
        let err = obs.matches(&wrong_code).expect_err("wrong code must fail");
        assert_eq!(err.path, "$.errors[0].extensions.code");

        let missing_header = json!({
            "headers": {"x-missing-header": "1"}
        });
        let hdr_err = obs
            .matches(&missing_header)
            .expect_err("missing header must fail");
        assert_eq!(hdr_err.path, "$.headers[\"x-missing-header\"]");
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn ordered_arrays_reject_prefix_subsequence_supersequence_and_reordering() {
        let observed = json!([{"id": "1"}, {"id": "2"}, {"id": "3"}]);

        // Prefix must fail (not a prefix match).
        let prefix_err = subset_match(&json!([{"id": "1"}, {"id": "2"}]), &observed)
            .expect_err("prefix must fail");
        assert_eq!(prefix_err.path, "$");

        // Subsequence must fail.
        let subseq_err = subset_match(&json!([{"id": "1"}, {"id": "3"}]), &observed)
            .expect_err("subsequence must fail");
        assert_eq!(subseq_err.path, "$");

        // Longer expected array must fail.
        let longer_err = subset_match(
            &json!([{"id": "1"}, {"id": "2"}, {"id": "3"}, {"id": "4"}]),
            &observed,
        )
        .expect_err("longer array must fail");
        assert_eq!(longer_err.path, "$");

        // Reordered same-length array must fail at first differing index.
        let reorder_err = subset_match(&json!([{"id": "1"}, {"id": "3"}, {"id": "2"}]), &observed)
            .expect_err("reordered array must fail");
        assert_eq!(reorder_err.path, "$[1].id");
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn unordered_wrapper_uses_bipartite_augmenting_paths_where_greedy_scan_fails() {
        // Greedy trap (2x2):
        //   expect[0] = {"a": 1} matches BOTH observed[0] = {"a": 1, "b": 2} and observed[1] = {"a": 1}.
        //   expect[1] = {"a": 1, "b": 2} matches ONLY observed[0].
        // A greedy scan assigns expect[0] -> observed[0] and then fails on expect[1]!
        let expected_2x2 = json!({
            "unordered": true,
            "items": [
                {"a": 1},
                {"a": 1, "b": 2}
            ]
        });
        let observed_2x2 = json!([
            {"a": 1, "b": 2},
            {"a": 1}
        ]);
        subset_match(&expected_2x2, &observed_2x2)
            .expect("Kuhn augmenting path must find valid 2x2 pairing");

        // Cascading greedy trap (3x3): requires two augmenting steps.
        let expected_3x3 = json!({
            "unordered": true,
            "items": [
                {"a": 1},
                {"a": 1, "b": 2},
                {"a": 1, "b": 2, "c": 3}
            ]
        });
        let observed_3x3 = json!([
            {"a": 1, "b": 2, "c": 3},
            {"a": 1, "b": 2},
            {"a": 1}
        ]);
        subset_match(&expected_3x3, &observed_3x3)
            .expect("Kuhn augmenting path must find valid 3x3 pairing");
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn unordered_wrapper_enforces_one_to_one_bijection_and_length_and_type() {
        // Hall marriage condition violation: two identical specific items cannot both pair with
        // the single observed item carrying `"b": 2`.
        let hall_violated = json!({
            "unordered": true,
            "items": [
                {"a": 1, "b": 2},
                {"a": 1, "b": 2}
            ]
        });
        let observed = json!([
            {"a": 1, "b": 2},
            {"a": 1}
        ]);
        let err = subset_match(&hall_violated, &observed)
            .expect_err("non-bijective unordered match must fail");
        assert_eq!(err.path, "$.items[1]");

        // Duplicate scalar multiplicities must match 1-to-1.
        assert!(
            subset_match(
                &json!({"unordered": true, "items": [1, 1, 2]}),
                &json!([1, 2, 2])
            )
            .is_err()
        );
        subset_match(
            &json!({"unordered": true, "items": [1, 1, 2]}),
            &json!([2, 1, 1]),
        )
        .expect("matching multiplicities succeed");

        // Length mismatch on unordered wrapper must fail at wrapper path.
        let len_err = subset_match(
            &json!({"unordered": true, "items": [1, 2]}),
            &json!([1, 2, 3]),
        )
        .expect_err("length mismatch must fail");
        assert_eq!(len_err.path, "$");

        // Non-array observed value must fail at wrapper path.
        let type_err = subset_match(&json!({"unordered": true, "items": [1]}), &json!({"0": 1}))
            .expect_err("non-array observed value must fail");
        assert_eq!(type_err.path, "$");

        // `{"unordered": false, "items": [...]}` is NOT the unordered wrapper; it matches as a plain object.
        subset_match(
            &json!({"unordered": false, "items": [1, 2]}),
            &json!({"unordered": false, "items": [1, 2], "extra": true}),
        )
        .expect("unordered:false is an ordinary object");
        assert!(
            subset_match(
                &json!({"unordered": false, "items": [1, 2]}),
                &json!([2, 1])
            )
            .is_err()
        );

        // 3-key object with `"unordered": true` and `"items"` plus extra key is NOT the 2-key wrapper.
        assert!(
            subset_match(
                &json!({"unordered": true, "items": [1], "extra": 0}),
                &json!([1])
            )
            .is_err()
        );
    }

    fn arb_json_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            (-1_000_000_i64..=1_000_000_i64).prop_map(Value::from),
            "[a-zA-Z0-9_-]{0,10}".prop_map(Value::String),
        ];
        leaf.prop_recursive(3, 32, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..=5).prop_map(Value::Array),
                // Prefix generated object keys with `k_` so arbitrary objects never accidentally
                // synthesize the reserved `{"unordered": true, "items": [...]}` wrapper.
                prop::collection::btree_map("k_[a-z0-9]{1,6}", inner, 0..=5).prop_map(|m| {
                    let mut map = Map::with_capacity(m.len());
                    for (k, v) in m {
                        map.insert(k, v);
                    }
                    Value::Object(map)
                }),
            ]
        })
    }

    fn weaken_value(val: &Value, seed: u64) -> Value {
        match val {
            Value::Object(obj) => {
                let mut out = Map::new();
                for (idx, (k, v)) in obj.iter().enumerate() {
                    let bit = (seed.wrapping_shr(idx as u32)) & 1;
                    if bit == 0 {
                        out.insert(k.clone(), weaken_value(v, seed.rotate_left(7)));
                    }
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(idx, item)| weaken_value(item, seed.wrapping_add(idx as u64)))
                    .collect(),
            ),
            _ => val.clone(),
        }
    }

    proptest! {
        // spec: GQL-002 (VER-010)
        #[test]
        fn prop_reflexivity(val in arb_json_value()) {
            prop_assert_eq!(subset_match(&val, &val), Ok(()));
        }

        // spec: GQL-002 (VER-010)
        #[test]
        fn prop_subset_weakening_still_matches(val in arb_json_value(), mask in any::<u64>()) {
            let weakened = weaken_value(&val, mask);
            prop_assert_eq!(subset_match(&weakened, &val), Ok(()));
        }

        // spec: GQL-002 (VER-010)
        #[test]
        fn prop_adding_absent_key_fails(
            entries in prop::collection::btree_map("k_[a-z0-9]{1,6}", arb_json_value(), 0..=6),
            absent_suffix in "[a-z0-9]{1,6}",
            absent_val in arb_json_value(),
        ) {
            let mut obs_map = Map::new();
            for (k, v) in entries {
                obs_map.insert(k, v);
            }
            let absent_key = format!("absent_{absent_suffix}");
            prop_assume!(!obs_map.contains_key(&absent_key));

            let mut exp_map = obs_map.clone();
            exp_map.insert(absent_key.clone(), absent_val);

            let err = subset_match(&Value::Object(exp_map), &Value::Object(obs_map))
                .expect_err("adding an absent key to expected object must fail");
            prop_assert_eq!(err.path, format!("$.{absent_key}"));
        }

        // spec: GQL-002 (VER-010)
        #[test]
        fn prop_unordered_is_permutation_invariant(
            items in prop::collection::vec(arb_json_value(), 0..=7),
            mask in any::<u64>(),
            perm_seed in any::<u64>(),
        ) {
            let expected_items: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(i, v)| weaken_value(v, mask.wrapping_add(i as u64)))
                .collect();

            let mut permuted = items;
            let len = permuted.len();
            let mut state = perm_seed | 1;
            for i in (1..len).rev() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let j = (state as usize) % (i + 1);
                permuted.swap(i, j);
            }

            let wrapper = json!({
                "unordered": true,
                "items": expected_items,
            });
            prop_assert_eq!(subset_match(&wrapper, &Value::Array(permuted)), Ok(()));
        }
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn a1_negative_zero_and_positive_zero_floats_and_integer_zero_match() {
        let neg_zero: Value = serde_json::from_str("-0.0").expect("parse -0.0");
        let pos_zero: Value = serde_json::from_str("0.0").expect("parse 0.0");
        let int_zero: Value = serde_json::from_str("0").expect("parse 0");

        // Sanity-check that serde_json preserved distinct IEEE-754 sign bits for -0.0 and +0.0.
        assert_ne!(
            neg_zero.as_f64().expect("neg f64").to_bits(),
            pos_zero.as_f64().expect("pos f64").to_bits()
        );

        assert_eq!(subset_match(&neg_zero, &pos_zero), Ok(()));
        assert_eq!(subset_match(&pos_zero, &neg_zero), Ok(()));
        assert_eq!(subset_match(&int_zero, &neg_zero), Ok(()));
        assert_eq!(subset_match(&neg_zero, &int_zero), Ok(()));
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn a2a_mismatch_under_leading_underscore_key_uses_dot_path() {
        let err = subset_match(&json!({"_x": 1}), &json!({"_x": 2}))
            .expect_err("value mismatch under `_x` must fail");
        assert_eq!(err.path, "$._x");
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn a2b_mismatch_under_digit_prefixed_key_uses_bracket_path() {
        let err = subset_match(&json!({"1a": 1}), &json!({"1a": 2}))
            .expect_err("value mismatch under `1a` must fail");
        assert_eq!(err.path, "$[\"1a\"]");
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn a2c_mismatch_under_hyphenated_key_uses_bracket_path() {
        let err = subset_match(&json!({"a-b": 1}), &json!({"a-b": 2}))
            .expect_err("value mismatch under `a-b` must fail");
        assert_eq!(err.path, "$[\"a-b\"]");
    }

    // spec: GQL-002 (VER-010)
    #[test]
    fn a2d_mismatch_under_empty_key_uses_bracket_path() {
        let err = subset_match(&json!({"": 1}), &json!({"": 2}))
            .expect_err("value mismatch under empty key must fail");
        assert_eq!(err.path, "$[\"\"]");
    }
}
