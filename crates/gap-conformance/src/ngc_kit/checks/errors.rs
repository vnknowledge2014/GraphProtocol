//! Error-mapping and response-size checks (`NGC-023`, `NGC-027`, `NGC-060`, `NGC-061`, `INV-15`).

use std::collections::BTreeSet;

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::error::ngc_error_from_proto;
use gap_proto::ngc::v1::{Cmp, Expr, Page, expr};

use super::{Verdict, not_applicable, not_covered, pass};
use crate::ngc_kit::kit::{Collected, KIT_TENANT, Kit, canon, clip};
use crate::ngc_kit::profile::toml_to_prost_value;

/// `PredicateOp` leaf operators `P_EQ ..= P_EXISTS` (the combinators `AND`/`OR`/`NOT` are `Expr` nodes, not operators).
const LEAF_PREDICATE_OPS: std::ops::RangeInclusive<i32> = 1..=10;
/// Longest `NgcError.message` (`proto/gap/ngc/v1/ngc.proto`).
const MAX_MESSAGE_CHARS: usize = 300;
/// An operation name no connector has.
const UNKNOWN_OPERATION: &str = "kit_no_such_operation";
/// Upper bound on pages followed while draining a truncated answer.
const MAX_DRAIN_PAGES: usize = 100;

/// Records `err` for the credential scan, labelled with the probe that produced it.
fn record(seen: &mut Vec<(String, NgcErrorInfo)>, label: &str, err: &NgcErrorInfo) {
    seen.push((label.to_owned(), err.clone()));
}

fn code_in(err: &NgcErrorInfo, allowed: &[NgcErrorCode]) -> bool {
    allowed.contains(&err.code)
}

/// `errors.mapping` (`NGC-023`, `NGC-060`, `NGC-061`, `INV-15`).
pub(crate) async fn mapping(kit: &Kit<'_>, notes: &mut Vec<String>) -> Verdict {
    let Some(read) = kit.profile.read.as_ref() else {
        return not_covered("the profile has no [read] section to derive error probes from");
    };
    let mut seen: Vec<(String, NgcErrorInfo)> = Vec::new();

    // An unknown operation is refused, not served and not an internal error.
    let unknown_ok = [
        NgcErrorCode::NotFound,
        NgcErrorCode::InvalidArgument,
        NgcErrorCode::Unimplemented,
    ];
    let got = kit
        .collect(kit.query_request(UNKNOWN_OPERATION, &toml::Table::new()))
        .await;
    match &got.failure {
        None => return Err("a Query for an unknown operation succeeded".to_owned()),
        Some(err) if code_in(err, &unknown_ok) => record(&mut seen, "unknown Query operation", err),
        Some(err) => {
            return Err(format!(
                "a Query for an unknown operation failed {:?}; expected NOT_FOUND, INVALID_ARGUMENT or UNIMPLEMENTED (NGC-060)",
                err.code
            ));
        }
    }
    match kit
        .mutate(
            UNKNOWN_OPERATION,
            &toml::Table::new(),
            "",
            KIT_TENANT,
            false,
        )
        .await
    {
        Ok(_) => return Err("a Mutate for an unknown operation succeeded".to_owned()),
        Err(err) if code_in(&err, &unknown_ok) => {
            record(&mut seen, "unknown Mutate operation", &err)
        }
        Err(err) => {
            return Err(format!(
                "a Mutate for an unknown operation failed {:?}; expected NOT_FOUND, INVALID_ARGUMENT or UNIMPLEMENTED (NGC-060)",
                err.code
            ));
        }
    }

    // A request the connector cannot accept is INVALID_ARGUMENT.
    match kit
        .profile
        .errors
        .as_ref()
        .and_then(|e| e.invalid_query.as_ref())
    {
        Some(bad) => {
            let got = kit
                .collect(kit.query_request(&bad.operation, &bad.arguments))
                .await;
            match &got.failure {
                None => {
                    return Err(format!(
                        "operation `{}` accepted arguments the profile marks invalid",
                        bad.operation
                    ));
                }
                Some(err) if err.code == NgcErrorCode::InvalidArgument => {
                    record(&mut seen, "invalid query", err);
                }
                Some(err) => {
                    return Err(format!(
                        "invalid arguments failed {:?} instead of INVALID_ARGUMENT (NGC-060, NGC-063)",
                        err.code
                    ));
                }
            }
        }
        None => notes.push(
            "profile has no errors.invalid_query: INVALID_ARGUMENT mapping was not checked"
                .to_owned(),
        ),
    }

    // NGC-023: an operator that was not advertised is rejected, never ignored.
    let advertised: BTreeSet<i32> = kit
        .desc
        .capabilities
        .as_ref()
        .map(|c| c.predicate_ops.iter().copied().collect())
        .unwrap_or_default();
    if let Some(op) = LEAF_PREDICATE_OPS
        .clone()
        .find(|op| !advertised.contains(op))
    {
        let path = read.row_key.clone().unwrap_or_else(|| read.key_var.clone());
        let mut req = kit.query_request(&read.operation, &read.arguments);
        req.predicate = Some(Expr {
            node: Some(expr::Node::Cmp(Cmp {
                path,
                op,
                value: read.keys.first().map(toml_to_prost_value),
            })),
        });
        let got = kit.collect(req).await;
        match &got.failure {
            None => {
                return Err(format!(
                    "predicate operator {op} was not advertised in capabilities.predicate_ops but the query was served: it must fail INVALID_ARGUMENT, never be ignored (NGC-023)"
                ));
            }
            Some(err) if err.code == NgcErrorCode::InvalidArgument => {
                record(&mut seen, "unadvertised predicate", err);
            }
            Some(err) => {
                return Err(format!(
                    "an unadvertised predicate operator failed {:?} instead of INVALID_ARGUMENT (NGC-023)",
                    err.code
                ));
            }
        }
    } else {
        notes.push(
            "every leaf predicate operator is advertised: NGC-023 was not exercised".to_owned(),
        );
    }

    // A per-index error is also an error message that must stay clean.
    if let (Some(bad), Some(limits)) = (&read.failing_key, kit.desc.limits.as_ref())
        && limits.max_batch_size > 0
    {
        let mut req = kit.query_request(&read.operation, &read.arguments);
        req.foreach = vec![kit.variable_set(&read.key_var, 1, bad)];
        let got: Collected = kit.collect(req).await;
        if let Some(err) = got.index_errors.get(&1) {
            if err.message.chars().count() > MAX_MESSAGE_CHARS {
                return Err(format!(
                    "a per-index error message is {} characters, over the {MAX_MESSAGE_CHARS} limit (NGC-061)",
                    err.message.chars().count()
                ));
            }
            record(&mut seen, "per-index error", &ngc_error_from_proto(err));
        }
    }

    // Nothing that comes back may carry the caller's credential or a stack trace (NGC-061, INV-15).
    for (label, err) in &seen {
        let details = err
            .details
            .as_ref()
            .and_then(|d| serde_json::to_string(d).ok())
            .unwrap_or_default();
        if err.message().contains(&kit.canary) || details.contains(&kit.canary) {
            return Err(format!(
                "{label}: the error echoes the caller's credential (NGC-061, INV-15)"
            ));
        }
        let text = format!("{} {details}", err.message());
        if [
            "stack backtrace",
            "panicked at",
            "RUST_BACKTRACE",
            "\n   at ",
        ]
        .iter()
        .any(|marker| text.contains(marker))
        {
            return Err(format!(
                "{label}: the error message looks like a stack trace: {} (NGC-061)",
                clip(err.message())
            ));
        }
    }
    pass()
}

/// `limits.response_size` (`NGC-027`).
pub(crate) async fn response_size(kit: &Kit<'_>, _notes: &mut Vec<String>) -> Verdict {
    let max_bytes = kit.desc.limits.as_ref().map_or(0, |l| l.max_response_bytes);
    if max_bytes == 0 {
        return not_applicable("limits.max_response_bytes is 0: the connector declares no cap");
    }
    let Some(size) = kit.profile.size.as_ref() else {
        return not_covered("a response cap is declared but the profile has no [size] section");
    };
    let got = kit
        .rows_of(kit.query_request(&size.operation, &size.arguments))
        .await
        .map_err(|e| format!("the [size] operation failed: {e}"))?;
    let complete = got
        .complete()
        .ok_or_else(|| "the answer did not end with exactly one Complete".to_owned())?;
    let bytes = u64::try_from(got.row_bytes).unwrap_or(u64::MAX);
    if bytes > max_bytes {
        return Err(format!(
            "{bytes} bytes of rows exceed limits.max_response_bytes {max_bytes} (NGC-027)"
        ));
    }
    let rows = u64::try_from(got.total_rows()).unwrap_or(u64::MAX);
    if rows >= size.expected_total_rows {
        return Err(format!(
            "the [size] operation returned all {rows} rows within the cap; the profile must name an answer larger than {max_bytes} bytes"
        ));
    }
    if !complete.truncated {
        return Err(format!(
            "only {rows} of {} rows came back but the response is not flagged truncated (NGC-027)",
            size.expected_total_rows
        ));
    }
    if size.resumable {
        let mut all: Vec<String> = got.all_rows().into_iter().map(canon).collect();
        let mut cursor = complete.next_cursor.clone();
        if cursor.is_empty() {
            return Err(
                "the answer is truncated and the profile says it is resumable, but next_cursor is empty (NGC-027)"
                    .to_owned(),
            );
        }
        for _ in 0..MAX_DRAIN_PAGES {
            let mut req = kit.query_request(&size.operation, &size.arguments);
            req.page = Some(Page {
                limit: 0,
                cursor: cursor.clone(),
                offset: 0,
            });
            let next = kit
                .rows_of(req)
                .await
                .map_err(|e| format!("following next_cursor failed: {e}"))?;
            if u64::try_from(next.row_bytes).unwrap_or(u64::MAX) > max_bytes {
                return Err(format!(
                    "a resumed page exceeds limits.max_response_bytes {max_bytes} (NGC-027)"
                ));
            }
            all.extend(next.all_rows().into_iter().map(canon));
            cursor = next
                .complete()
                .map(|c| c.next_cursor.clone())
                .unwrap_or_default();
            if cursor.is_empty() {
                break;
            }
        }
        let distinct: BTreeSet<&String> = all.iter().collect();
        if u64::try_from(distinct.len()).unwrap_or(u64::MAX) != size.expected_total_rows
            || distinct.len() != all.len()
        {
            return Err(format!(
                "draining next_cursor produced {} rows ({} distinct), expected {} distinct (NGC-027)",
                all.len(),
                distinct.len(),
                size.expected_total_rows
            ));
        }
    }
    pass()
}
