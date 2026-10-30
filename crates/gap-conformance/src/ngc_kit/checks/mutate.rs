//! Idempotency and dry-run checks (`NGC-030`, `NGC-031`).

use gap_core::error::NgcErrorCode;
use gap_proto::ngc::v1::{MutateResponse, MutationOutcome};

use super::{Verdict, not_applicable, not_covered, pass};
use crate::ngc_kit::kit::{KIT_OTHER_TENANT, KIT_TENANT, Kit};
use crate::ngc_kit::profile::{EffectProbe, MutateProbe};

fn outcome_of(resp: &MutateResponse) -> Result<MutationOutcome, i32> {
    MutationOutcome::try_from(resp.outcome).map_err(|_| resp.outcome)
}

/// Effect count, if the profile supplies a probe.
async fn effects(kit: &Kit<'_>, probe: Option<&EffectProbe>) -> Result<Option<usize>, String> {
    match probe {
        Some(p) => kit.count_effects(p).await.map(Some),
        None => Ok(None),
    }
}

fn expect_effects(
    what: &str,
    got: Option<usize>,
    before: Option<usize>,
    added: usize,
) -> Result<(), String> {
    if let (Some(got), Some(before)) = (got, before)
        && got != before + added
    {
        return Err(format!(
            "{what}: the effect probe counts {got} effects, expected {} ({before} before + {added})",
            before + added
        ));
    }
    Ok(())
}

async fn applied(
    kit: &Kit<'_>,
    m: &MutateProbe,
    key: &str,
    tenant: &str,
    why: &str,
) -> Result<MutateResponse, String> {
    let resp = kit
        .mutate(&m.operation, &m.arguments, key, tenant, false)
        .await
        .map_err(|e| format!("{why}: Mutate failed {:?}: {}", e.code, e.message()))?;
    match outcome_of(&resp) {
        Ok(MutationOutcome::OutcomeApplied) => Ok(resp),
        other => Err(format!(
            "{why}: expected outcome APPLIED, got {other:?}{}",
            resp.error
                .as_ref()
                .map(|e| format!(" ({})", e.message))
                .unwrap_or_default()
        )),
    }
}

/// `mutate.idempotency` (`NGC-030`).
pub(crate) async fn idempotency(kit: &Kit<'_>, notes: &mut Vec<String>) -> Verdict {
    if !kit
        .desc
        .capabilities
        .as_ref()
        .is_some_and(|c| c.idempotency)
    {
        return not_applicable(
            "idempotency is not advertised: GAP treats every mutation of this connector as non-retryable (NGC-030)",
        );
    }
    let Some(m) = kit.profile.mutate.as_ref() else {
        return not_covered("idempotency is advertised but the profile has no [mutate] section");
    };
    let probe = m.probe.as_ref();
    if probe.is_none() {
        notes.push(
            "profile has no mutate.probe: \"applied at most once\" is checked through was_replayed only, not through the effect count"
                .to_owned(),
        );
    }
    let before = effects(kit, probe).await?;

    let key = kit.unique("idem");
    let first = applied(kit, m, &key, KIT_TENANT, "first call with a fresh key").await?;
    if first.was_replayed {
        return Err("a fresh idempotency key was reported as replayed".to_owned());
    }
    let again = applied(kit, m, &key, KIT_TENANT, "repeat with the same key").await?;
    if !again.was_replayed {
        return Err(
            "a repeat of (tenant, operation, idempotency_key) was not flagged was_replayed (NGC-030)"
                .to_owned(),
        );
    }
    if again.result != first.result {
        return Err("the replay returned a different result than the original call".to_owned());
    }
    expect_effects(
        "two calls with one key",
        effects(kit, probe).await?,
        before,
        1,
    )?;

    let other_key = kit.unique("idem");
    let fresh = applied(kit, m, &other_key, KIT_TENANT, "a different key").await?;
    if fresh.was_replayed {
        return Err("a different idempotency key was reported as replayed".to_owned());
    }
    expect_effects("a second key", effects(kit, probe).await?, before, 2)?;

    // The scope of a key is (tenant, operation, key): the same key under another tenant is a new request.
    let other_tenant = applied(
        kit,
        m,
        &key,
        KIT_OTHER_TENANT,
        "the same key under another tenant",
    )
    .await?;
    if other_tenant.was_replayed {
        return Err(
            "the same idempotency key under another tenant was replayed: keys must be scoped to the tenant (NGC-030)"
                .to_owned(),
        );
    }
    expect_effects(
        "the same key under another tenant",
        effects(kit, probe).await?,
        before,
        3,
    )?;
    pass()
}

/// `mutate.dry_run` (`NGC-031`).
pub(crate) async fn dry_run(kit: &Kit<'_>, notes: &mut Vec<String>) -> Verdict {
    let Some(m) = kit.profile.mutate.as_ref() else {
        return not_covered("the profile has no [mutate] section");
    };
    let probe = m.probe.as_ref();
    if probe.is_none() {
        notes.push(
            "profile has no mutate.probe: \"changes nothing\" is checked through key reuse only, not through the effect count"
                .to_owned(),
        );
    }
    let supports = kit.desc.capabilities.as_ref().is_some_and(|c| c.dry_run);
    let before = effects(kit, probe).await?;
    let key = kit.unique("dry");
    let dry = kit
        .mutate(&m.operation, &m.arguments, &key, KIT_TENANT, true)
        .await;

    if !supports {
        return match dry {
            Err(e) if e.code == NgcErrorCode::Unimplemented => {
                expect_effects(
                    "an unsupported dry run",
                    effects(kit, probe).await?,
                    before,
                    0,
                )?;
                pass()
            }
            Err(e) => Err(format!(
                "dry_run is not advertised and the call failed {:?} instead of UNIMPLEMENTED (NGC-031)",
                e.code
            )),
            Ok(resp) => Err(format!(
                "dry_run is not advertised but the call succeeded with outcome {:?}; it must fail UNIMPLEMENTED (NGC-031)",
                outcome_of(&resp)
            )),
        };
    }

    let resp = dry.map_err(|e| {
        format!(
            "dry_run is advertised but the call failed {:?}: {}",
            e.code,
            e.message()
        )
    })?;
    match outcome_of(&resp) {
        Ok(MutationOutcome::OutcomeDryRun) => {}
        other => {
            return Err(format!(
                "a dry run returned outcome {other:?} instead of DRY_RUN (NGC-031)"
            ));
        }
    }
    if resp.dry_run_report.is_none() {
        return Err("a DRY_RUN outcome carries no dry_run_report (NGC-031)".to_owned());
    }
    expect_effects("a dry run", effects(kit, probe).await?, before, 0)?;

    // The dry run must not have consumed the idempotency key.
    let real = applied(
        kit,
        m,
        &key,
        KIT_TENANT,
        "a real call with the key of the dry run",
    )
    .await?;
    if real.was_replayed {
        return Err(
            "a real call after a dry run with the same key was treated as a replay: the dry run was recorded as an execution (NGC-031)"
                .to_owned(),
        );
    }
    expect_effects(
        "the real call after a dry run",
        effects(kit, probe).await?,
        before,
        1,
    )?;
    pass()
}
