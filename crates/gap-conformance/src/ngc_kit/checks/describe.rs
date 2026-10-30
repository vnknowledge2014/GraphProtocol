//! `Describe` and `Health` checks (`NGC-010`, `NGC-012`, `NGC-013`, `NGC-015`, `NGC-070`).

use std::collections::BTreeSet;
use std::time::Duration;

use gap_proto::ngc::v1::{HealthRequest, ServingStatus};
use prost_types::{Struct, value::Kind};

use super::{Verdict, pass};
use crate::ngc_kit::kit::{Kit, clip};

/// Latency samples for the optional `Describe` budget (the `NGC-010` SLO is a p95).
const DESCRIBE_LATENCY_SAMPLES: usize = 20;

fn valid_operation_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// First `$ref` that is not local (`#/...`), searching nested structs and lists (`NGC-013`).
fn non_local_ref(schema: &Struct) -> Option<String> {
    for (key, value) in &schema.fields {
        if key == "$ref"
            && let Some(Kind::StringValue(target)) = &value.kind
            && !target.starts_with('#')
        {
            return Some(target.clone());
        }
        match &value.kind {
            Some(Kind::StructValue(inner)) => {
                if let Some(found) = non_local_ref(inner) {
                    return Some(found);
                }
            }
            Some(Kind::ListValue(list)) => {
                for item in &list.values {
                    if let Some(Kind::StructValue(inner)) = &item.kind
                        && let Some(found) = non_local_ref(inner)
                    {
                        return Some(found);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// `describe.valid`: identity, SDL, operation names, local `$ref`s, coherent limits.
pub(crate) async fn valid(kit: &Kit<'_>, _notes: &mut Vec<String>) -> Verdict {
    let d = kit.desc;
    if d.connector_id.trim().is_empty() {
        return Err("connector_id is empty".to_owned());
    }
    if !d.ngc_version.starts_with("gap/") {
        return Err(format!(
            "ngc_version `{}` is not of the form `gap/<major>.<minor>`",
            clip(&d.ngc_version)
        ));
    }
    gap_ql::schema::with_gap_builtins(&d.schema_sdl).map_err(|e| {
        format!(
            "schema_sdl is not valid branch SDL (NGC-012): {}",
            clip(&e.to_string())
        )
    })?;
    if d.operations.is_empty() {
        return Err("Describe lists no operations".to_owned());
    }
    let mut seen = BTreeSet::new();
    for op in &d.operations {
        if !valid_operation_name(&op.name) {
            return Err(format!(
                "operation name `{}` does not match [A-Za-z_][A-Za-z0-9_]* (NGC-012)",
                clip(&op.name)
            ));
        }
        if !seen.insert(op.name.as_str()) {
            return Err(format!("operation `{}` is listed twice", op.name));
        }
        for (which, schema) in [
            ("input_schema", &op.input_schema),
            ("output_schema", &op.output_schema),
        ] {
            if let Some(target) = schema.as_ref().and_then(non_local_ref) {
                return Err(format!(
                    "operation `{}` {which} has a non-local $ref `{}` (NGC-013: remote refs are an SSRF vector)",
                    op.name,
                    clip(&target)
                ));
            }
        }
    }
    let (Some(caps), Some(limits)) = (d.capabilities.as_ref(), d.limits.as_ref()) else {
        return Err("Describe carries no capabilities or no limits".to_owned());
    };
    if caps.foreach_batch && limits.max_batch_size == 0 {
        return Err(
            "foreach_batch is advertised but limits.max_batch_size is 0, which means no foreach (NGC-015)"
                .to_owned(),
        );
    }
    if limits.max_in_flight == 0 {
        return Err(
            "limits.max_in_flight is 0; a connector enforces its own cap (NGC-071)".to_owned(),
        );
    }
    pass()
}

/// `describe.stable`: idempotent, and `not_modified` only for the hash the connector issued.
pub(crate) async fn stable(kit: &Kit<'_>, _notes: &mut Vec<String>) -> Verdict {
    let mut full = Vec::new();
    for round in 1..=2 {
        let resp = kit
            .describe("")
            .await
            .map_err(|e| format!("Describe #{round} failed: {:?} {}", e.code, e.message()))?;
        match (resp.not_modified, resp.description) {
            (false, Some(desc)) => full.push(desc),
            _ => {
                return Err(format!(
                    "Describe #{round} without known_spec_hash returned no description"
                ));
            }
        }
    }
    let (first, second) = (&full[0], &full[1]);
    if first.spec_hash != second.spec_hash {
        return Err(format!(
            "spec_hash changed between two Describe calls (`{}` then `{}`): Describe must be idempotent (NGC-010)",
            clip(&first.spec_hash),
            clip(&second.spec_hash)
        ));
    }
    if first != second {
        return Err(
            "two Describe calls returned different descriptions: Describe must be side-effect free and idempotent (NGC-010)"
                .to_owned(),
        );
    }
    let same = kit
        .describe(&first.spec_hash)
        .await
        .map_err(|e| format!("Describe with the current hash failed: {:?}", e.code))?;
    if !same.not_modified && same.description.as_ref() != Some(first) {
        return Err(
            "with the current known_spec_hash Describe returned neither not_modified nor the same description"
                .to_owned(),
        );
    }
    let stale = kit
        .describe("kit-never-issued-hash")
        .await
        .map_err(|e| format!("Describe with a stale hash failed: {:?}", e.code))?;
    if stale.not_modified || stale.description.is_none() {
        return Err(
            "Describe answered not_modified for a spec hash it never issued (NGC-010)".to_owned(),
        );
    }
    if let Some(budget_ms) = kit.profile.timing.describe_budget_ms {
        let mut samples = Vec::with_capacity(DESCRIBE_LATENCY_SAMPLES);
        for _ in 0..DESCRIBE_LATENCY_SAMPLES {
            let started = tokio::time::Instant::now();
            kit.describe("")
                .await
                .map_err(|e| format!("Describe failed during the latency probe: {:?}", e.code))?;
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        let p95 = samples[(DESCRIBE_LATENCY_SAMPLES * 95).div_ceil(100) - 1];
        if p95 > Duration::from_millis(budget_ms) {
            return Err(format!(
                "Describe p95 is {} ms, over the profile budget of {budget_ms} ms (NGC-010)",
                p95.as_millis()
            ));
        }
    }
    pass()
}

/// `health`: `SERVING` (or `DEGRADED`) within the budget (`NGC-070`).
pub(crate) async fn health(kit: &Kit<'_>, notes: &mut Vec<String>) -> Verdict {
    let started = tokio::time::Instant::now();
    let resp = kit
        .client
        .health(HealthRequest {})
        .await
        .map_err(|e| format!("Health failed: {:?} {}", e.code, e.message()))?;
    let took = started.elapsed();
    let budget = Duration::from_millis(kit.profile.timing.health_budget_ms);
    if took > budget {
        return Err(format!(
            "Health took {} ms, over the {} ms budget (NGC-070)",
            took.as_millis(),
            budget.as_millis()
        ));
    }
    match ServingStatus::try_from(resp.status) {
        Ok(ServingStatus::Serving) => pass(),
        Ok(ServingStatus::Degraded) => {
            notes.push("Health reports DEGRADED (reduced capacity, NGC-070)".to_owned());
            pass()
        }
        other => Err(format!(
            "Health reports {other:?}; the kit needs a SERVING connector (NGC-070)"
        )),
    }
}
