//! Health polling for registered replicas (`NGC-070`, `RTE-073`).
//!
//! One tokio task per replica; every 5 s ± 20% jitter it calls `BranchClient::health`
//! bounded by 500 ms, then feeds the result to `TelemetrySlot::apply_health`.

use std::sync::Arc;
use std::time::Duration;

use gap_balance::slot::{HealthStatus, TelemetrySlot};
use gap_balance::time::Now;
use gap_proto::ngc::v1::{HealthRequest, ServingStatus};
use tokio::task::JoinSet;
use tokio::time::{Instant as TokioInstant, sleep};
use tokio_util::sync::CancellationToken;

use super::BranchClient;
use super::ConnectorRegistry;

/// Handle that owns all health-poller tasks. Dropping it cancels them.
pub struct HealthPollers {
    _cancel: CancellationToken,
    tasks: JoinSet<()>,
}

impl HealthPollers {
    /// Cancels all pollers and waits for them to exit.
    pub async fn join_all(mut self) {
        self._cancel.cancel();
        while self.tasks.join_next().await.is_some() {}
    }
}

impl Drop for HealthPollers {
    fn drop(&mut self) {
        self._cancel.cancel();
    }
}

/// Spawns one health-poller task per replica across all branches in `registry`.
pub fn spawn_health_pollers(
    registry: &ConnectorRegistry,
    parent_cancel: CancellationToken,
) -> HealthPollers {
    let local_cancel = CancellationToken::new();
    let mut tasks: JoinSet<()> = JoinSet::new();

    for branch in registry.entries.values() {
        for entry in &branch.replicas {
            let client = Arc::clone(&entry.replica.client);
            let slot = Arc::clone(&entry.replica.slot);
            let interval = registry.resilience.health_interval;
            let jitter_frac = registry.resilience.health_jitter;
            let health_timeout = registry.resilience.health_timeout;
            let epoch = registry.epoch;
            let cancel = local_cancel.clone();
            let parent = parent_cancel.clone();

            tasks.spawn(poll_one_replica(
                client,
                slot,
                interval,
                jitter_frac,
                health_timeout,
                epoch,
                cancel,
                parent,
            ));
        }
    }

    HealthPollers {
        _cancel: local_cancel,
        tasks,
    }
}

#[allow(clippy::too_many_arguments)]
async fn poll_one_replica(
    client: Arc<dyn BranchClient>,
    slot: Arc<TelemetrySlot>,
    interval: Duration,
    jitter_frac: f64,
    health_timeout: Duration,
    epoch: TokioInstant,
    cancel: CancellationToken,
    parent_cancel: CancellationToken,
) {
    let seed = (&*slot as *const TelemetrySlot as u64).wrapping_add(1);
    let mut rng = gap_balance::XorShift64::new(seed);

    loop {
        let base_ms = interval.as_millis() as u64;
        let jitter_ms = (base_ms as f64 * jitter_frac) as u64;
        let lo = base_ms.saturating_sub(jitter_ms);
        let range = jitter_ms.saturating_mul(2).max(1);
        let wait_ms = lo + rng.next_u64() % range;
        let wait = Duration::from_millis(wait_ms);

        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = parent_cancel.cancelled() => return,
            () = sleep(wait) => {}
        }

        let health_result = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = parent_cancel.cancelled() => return,
            res = tokio::time::timeout(health_timeout, client.health(HealthRequest {})) => {
                match res {
                    Ok(inner) => inner,
                    Err(_) => Err(gap_core::error::NgcErrorInfo::new(
                        gap_core::error::NgcErrorCode::DeadlineExceeded,
                        "health poll timed out",
                    )),
                }
            }
        };

        let now = {
            let elapsed = epoch.elapsed();
            let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            Now::from_ms(ms)
        };

        match health_result {
            Ok(resp) => {
                let status = match ServingStatus::try_from(resp.status) {
                    Ok(ServingStatus::Serving) => HealthStatus::Serving,
                    Ok(ServingStatus::Degraded) => HealthStatus::Degraded,
                    Ok(ServingStatus::NotServing) | Ok(ServingStatus::Unspecified) | Err(_) => {
                        HealthStatus::NotServing
                    }
                };
                let capacity_weight = extract_capacity_weight(&resp.details);
                slot.apply_health(status, capacity_weight, now);
            }
            Err(_) => {
                slot.apply_health(HealthStatus::NotServing, None, now);
            }
        }
    }
}

fn extract_capacity_weight(details: &Option<prost_types::Struct>) -> Option<f64> {
    let s = details.as_ref()?;
    let val = s.fields.get("capacity_weight")?;
    if let Some(prost_types::value::Kind::NumberValue(n)) = &val.kind {
        Some(*n)
    } else {
        None
    }
}
