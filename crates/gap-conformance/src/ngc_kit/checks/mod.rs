//! The checks of the NGC connector kit (`NGC-091`).
//!
//! Every check is `async fn(&Kit, &mut Vec<String>) -> Verdict`: `Ok(None)` passes, `Ok(Some(skip))` did not run, `Err(text)`
//! fails with a reason that names the requirement. `notes` receives observations that are not failures.

use super::kit::{Kit, clip};
use super::outcome::{Outcome, SkipReason, id};

pub(crate) mod describe;
pub(crate) mod errors;
pub(crate) mod foreach;
pub(crate) mod lifecycle;
pub(crate) mod mutate;
pub(crate) mod paging;

/// `Ok(None)` = pass, `Ok(Some(_))` = skipped, `Err(_)` = fail.
pub(crate) type Verdict = Result<Option<SkipReason>, String>;

pub(crate) fn pass() -> Verdict {
    Ok(None)
}

pub(crate) fn not_applicable(why: impl Into<String>) -> Verdict {
    Ok(Some(SkipReason::NotApplicable(why.into())))
}

pub(crate) fn not_covered(why: impl Into<String>) -> Verdict {
    Ok(Some(SkipReason::NotCovered(why.into())))
}

/// Converts a verdict into the public outcome (failure texts are clipped to a readable length).
pub(crate) fn finish(verdict: Verdict) -> Outcome {
    match verdict {
        Ok(None) => Outcome::Pass,
        Ok(Some(skip)) => Outcome::Skip(skip),
        Err(why) => Outcome::Fail(clip(&why)),
    }
}

/// Runs the check named `check_id`.
pub(crate) async fn run_one(kit: &Kit<'_>, check_id: &str, notes: &mut Vec<String>) -> Verdict {
    match check_id {
        id::DESCRIBE_VALID => describe::valid(kit, notes).await,
        id::DESCRIBE_STABLE => describe::stable(kit, notes).await,
        id::HEALTH => describe::health(kit, notes).await,
        id::FOREACH_CORRECTNESS => foreach::correctness(kit, notes).await,
        id::FOREACH_N_PLUS_ONE => foreach::n_plus_one(kit, notes).await,
        id::QUERY_ORDER => paging::order(kit, notes).await,
        id::DEADLINE => lifecycle::deadline(kit, notes).await,
        id::CANCEL => lifecycle::cancel(kit, notes).await,
        id::MUTATE_IDEMPOTENCY => mutate::idempotency(kit, notes).await,
        id::MUTATE_DRY_RUN => mutate::dry_run(kit, notes).await,
        id::PAGE_STABILITY => paging::stability(kit, notes).await,
        id::ERRORS_MAPPING => errors::mapping(kit, notes).await,
        id::LIMITS_RESPONSE_SIZE => errors::response_size(kit, notes).await,
        other => Err(format!("the kit has no implementation for check `{other}`")),
    }
}
