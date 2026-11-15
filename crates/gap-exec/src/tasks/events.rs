//! Per-task monotonic event hub supporting persisted replay, live broadcast, bounded backpressure,
//! and SSE `Last-Event-ID` resumption (`RUN-051`, `RUN-080`, `INV-02`, `INV-13`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::{TaskId, TenantScope};
use gap_store::StoreError;
use gap_store::tasks::TaskStore;
use tokio::sync::{broadcast, mpsc};

use super::types::TaskEventView;

/// Bounded per-subscriber channel capacity (`RUN-051`).
pub const TASK_EVENT_STREAM_BUFFER: usize = 64;

const BROADCAST_CAPACITY: usize = 128;

/// Parses an HTTP `Last-Event-ID` header value into the next `from_seq` cursor (`seq + 1`), falling
/// back to `fallback_from_seq` when absent or empty (`RUN-080`).
///
/// # Errors
///
/// Returns [`ErrorCode::ValidationError`] if `last_event_id` is non-empty and not an unsigned integer.
pub fn parse_sse_last_event_id(
    last_event_id: Option<&str>,
    fallback_from_seq: u64,
) -> GapResult<u64> {
    let Some(raw) = last_event_id else {
        return Ok(fallback_from_seq);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(fallback_from_seq);
    }
    let last_seq = trimmed.parse::<u64>().map_err(|_| {
        GapError::new(
            ErrorCode::ValidationError,
            format!("invalid SSE Last-Event-ID `{trimmed}`"),
        )
    })?;
    Ok(last_seq.saturating_add(1))
}

/// Bounded subscription stream yielding persisted history (`seq >= from_seq`) followed by live events.
#[derive(Debug)]
pub struct TaskEventStream {
    rx: mpsc::Receiver<TaskEventView>,
}

impl TaskEventStream {
    /// Receives the next strictly ordered [`TaskEventView`], or `None` once the task reaches a
    /// terminal state (or the hub shuts down).
    pub async fn recv(&mut self) -> Option<TaskEventView> {
        self.rx.recv().await
    }

    /// Unwraps the bounded underlying `mpsc::Receiver`.
    #[must_use]
    pub fn into_receiver(self) -> mpsc::Receiver<TaskEventView> {
        self.rx
    }
}

type EventChannelMap = HashMap<(String, TaskId), broadcast::Sender<TaskEventView>>;

/// Partitioned pub/sub coordinator for [`TaskEventView`] replay + follow (`RUN-080`, `INV-09`, `INV-13`).
#[derive(Debug, Default, Clone)]
pub struct TaskEventHub {
    channels: Arc<Mutex<EventChannelMap>>,
}

impl TaskEventHub {
    /// Creates an empty [`TaskEventHub`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn channel_for(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
    ) -> broadcast::Sender<TaskEventView> {
        let key = (tenant.tenant().as_str().to_owned(), task_id);
        let mut guard = match self.channels.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(tx) = guard.get(&key) {
            return tx.clone();
        }
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        guard.insert(key, tx.clone());
        tx
    }

    fn remove_if_terminal(&self, tenant: &TenantScope, task_id: TaskId, event: &TaskEventView) {
        if !event.state.is_terminal() {
            return;
        }
        if let Ok(mut guard) = self.channels.lock() {
            guard.remove(&(tenant.tenant().as_str().to_owned(), task_id));
        }
    }

    /// Publishes an already-persisted [`TaskEventView`] to active subscribers (`RUN-080`).
    pub fn publish(&self, tenant: &TenantScope, event: TaskEventView) {
        let task_id = event.task_id;
        let tx_opt = {
            let guard = match self.channels.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard
                .get(&(tenant.tenant().as_str().to_owned(), task_id))
                .cloned()
        };
        if let Some(tx) = tx_opt {
            let _ = tx.send(event.clone());
        }
        self.remove_if_terminal(tenant, task_id, &event);
    }

    /// Replays all persisted events with `seq >= from_seq` and seamlessly follows live events via
    /// a bounded `64`-item channel (`RUN-051`, `RUN-080`, `INV-13`).
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::TaskNotFound`] if `task_id` does not exist in `tenant`, or
    /// [`ErrorCode::Internal`] on store errors.
    pub fn subscribe(
        &self,
        store: &dyn TaskStore,
        tenant: &TenantScope,
        task_id: TaskId,
        from_seq: u64,
    ) -> GapResult<TaskEventStream> {
        let sender = self.channel_for(tenant, task_id);
        // Subscribe BEFORE reading persisted history so concurrent transitions cannot be missed.
        let mut live_rx = sender.subscribe();

        let record = store
            .get_task(tenant, task_id)
            .map_err(map_store_err)?
            .ok_or_else(|| {
                GapError::new(
                    ErrorCode::TaskNotFound,
                    format!("task `{task_id}` not found"),
                )
            })?;
        let history = store
            .events_from(tenant, task_id, from_seq)
            .map_err(map_store_err)?;

        let already_terminal = record.status.is_terminal();
        let (tx, rx) = mpsc::channel(TASK_EVENT_STREAM_BUFFER);

        tokio::spawn(async move {
            let mut last_sent_seq = from_seq.saturating_sub(1);
            let mut saw_terminal = false;

            for rec in history {
                let evt = TaskEventView::from_record(&rec);
                if evt.seq <= last_sent_seq {
                    continue;
                }
                last_sent_seq = evt.seq;
                if evt.state.is_terminal() {
                    saw_terminal = true;
                }
                if tx.send(evt).await.is_err() {
                    return;
                }
            }

            if saw_terminal || already_terminal {
                return;
            }

            loop {
                match live_rx.recv().await {
                    Ok(evt) => {
                        if evt.seq <= last_sent_seq {
                            continue;
                        }
                        last_sent_seq = evt.seq;
                        let is_term = evt.state.is_terminal();
                        if tx.send(evt).await.is_err() {
                            return;
                        }
                        if is_term {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return;
                    }
                }
            }
        });

        Ok(TaskEventStream { rx })
    }
}

fn map_store_err(err: StoreError) -> GapError {
    match err {
        StoreError::NotFound => GapError::new(ErrorCode::TaskNotFound, "task not found"),
        StoreError::Conflict => {
            GapError::new(ErrorCode::TaskInvalidState, "invalid task transition")
        }
        StoreError::Invalid { field, reason } => GapError::new(
            ErrorCode::ValidationError,
            format!("invalid task store field `{field}`: {reason}"),
        ),
        other => GapError::new(ErrorCode::Internal, format!("task store failure: {other}")),
    }
}
