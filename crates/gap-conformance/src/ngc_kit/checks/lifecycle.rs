//! Deadline and cancellation checks (`NGC-025`, `INV-12`, `NGC-093` (a), (b)).
//!
//! Both checks prove two things: the call ends the way the spec says, and the capacity it held comes back. The second part
//! matters because an SDK-based server drops the connector future by itself; only a connector that keeps working for callers
//! that are gone (or holds its own gate) is caught by watching capacity.

use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::join_all;
use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_proto::ngc::v1::QueryRequest;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::{Verdict, not_covered, pass};
use crate::ngc_kit::kit::{KIT_TENANT, Kit};
use crate::ngc_kit::profile::SlowProbe;

/// How long a freshly started slow call is watched for a refusal before it counts as running.
const SETTLE: Duration = Duration::from_millis(50);
/// Pause between capacity probes.
const RETRY_PAUSE: Duration = Duration::from_millis(50);

/// First thing a slow call reported.
#[derive(Debug)]
enum Event {
    /// `RESOURCE_EXHAUSTED`: the connector had no room.
    Refused,
    /// Any other failure.
    Failed(NgcErrorInfo),
    /// The stream produced an item and is still going.
    Streaming,
    /// The stream ended by itself (the operation is not slow).
    Ended,
}

impl Event {
    fn from_error(err: NgcErrorInfo) -> Self {
        if err.code == NgcErrorCode::ResourceExhausted {
            Self::Refused
        } else {
            Self::Failed(err)
        }
    }
}

/// A slow call running in its own task; aborting the task drops the stream, which is a client cancel.
struct SlowCall {
    handle: JoinHandle<()>,
    first: oneshot::Receiver<Event>,
}

impl SlowCall {
    /// First event within `wait`, or `None` while the call is still running without having said anything.
    async fn first_within(&mut self, wait: Duration) -> Option<Event> {
        match tokio::time::timeout(wait, &mut self.first).await {
            Ok(Ok(event)) => Some(event),
            Ok(Err(_)) => Some(Event::Ended),
            Err(_) => None,
        }
    }

    /// Cancel: drop the stream.
    fn cancel(self) {
        self.handle.abort();
    }
}

fn spawn_slow(kit: &Kit<'_>, slow: &SlowProbe, deadline_in_ms: i64) -> SlowCall {
    let mut req: QueryRequest = kit.query_request(&slow.operation, &slow.arguments);
    req.ctx = Some(kit.ctx(KIT_TENANT, deadline_in_ms, ""));
    let client = kit.client.clone();
    let metadata = kit.metadata();
    let (tx, first) = oneshot::channel();
    let handle = tokio::spawn(async move {
        match client.query_with_metadata(req, &metadata).await {
            Err(err) => {
                let _ = tx.send(Event::from_error(err));
            }
            Ok(mut stream) => {
                match stream.next().await {
                    Some(Err(err)) => {
                        let _ = tx.send(Event::from_error(err));
                        return;
                    }
                    Some(Ok(_)) => {
                        let _ = tx.send(Event::Streaming);
                    }
                    None => {
                        let _ = tx.send(Event::Ended);
                        return;
                    }
                }
                while stream.next().await.is_some() {}
            }
        }
    });
    SlowCall { handle, first }
}

fn slow_probe<'a>(kit: &'a Kit<'_>) -> Result<(&'a SlowProbe, usize), Verdict> {
    let Some(slow) = kit.profile.slow.as_ref() else {
        return Err(not_covered(
            "the profile has no [slow] section: deadlines and cancellation cannot be probed",
        ));
    };
    let max_in_flight = kit
        .desc
        .limits
        .as_ref()
        .map_or(1, |l| l.max_in_flight)
        .max(1);
    let calls = slow.calls.min(max_in_flight);
    Ok((slow, usize::try_from(calls).unwrap_or(1)))
}

fn grace(kit: &Kit<'_>) -> Duration {
    Duration::from_millis(kit.profile.timing.grace_ms)
}

/// Opens `calls` slow calls with a long deadline until none is refused (or the grace period is over), then cancels them.
///
/// This is the capacity proof: if the connector still works for callers that left, it keeps its slots and refuses the new ones.
async fn capacity_returns(kit: &Kit<'_>, slow: &SlowProbe, calls: usize) -> Result<(), String> {
    let long = i64::try_from(kit.profile.timing.call_timeout_ms).unwrap_or(i64::MAX);
    let started = tokio::time::Instant::now();
    loop {
        let mut batch: Vec<SlowCall> = (0..calls).map(|_| spawn_slow(kit, slow, long)).collect();
        let mut refused = 0_usize;
        for call in &mut batch {
            if matches!(call.first_within(SETTLE).await, Some(Event::Refused)) {
                refused += 1;
            }
        }
        for call in batch {
            call.cancel();
        }
        if refused == 0 {
            return Ok(());
        }
        if started.elapsed() >= grace(kit) {
            return Err(format!(
                "{refused} of {calls} slow calls were still refused (RESOURCE_EXHAUSTED) {} ms after the previous ones ended: the connector did not release the capacity",
                grace(kit).as_millis()
            ));
        }
        tokio::time::sleep(RETRY_PAUSE).await;
    }
}

/// `deadline`: a passed deadline ends the call with `DEADLINE_EXCEEDED` and the capacity comes back.
pub(crate) async fn deadline(kit: &Kit<'_>, notes: &mut Vec<String>) -> Verdict {
    let (slow, calls) = match slow_probe(kit) {
        Ok(found) => found,
        Err(skip) => return skip,
    };
    let timing = &kit.profile.timing;
    let wait = Duration::from_millis(timing.deadline_ms) + grace(kit);

    // A deadline already in the past.
    let mut past = spawn_slow(kit, slow, -1_000);
    match past.first_within(grace(kit)).await {
        Some(Event::Failed(err)) if err.code == NgcErrorCode::DeadlineExceeded => {}
        Some(Event::Failed(err)) => {
            return Err(format!(
                "a call whose deadline had passed failed {:?} instead of DEADLINE_EXCEEDED (NGC-025)",
                err.code
            ));
        }
        Some(other) => {
            return Err(format!(
                "a call whose deadline had passed was accepted ({other:?}) instead of failing DEADLINE_EXCEEDED (NGC-025)"
            ));
        }
        None => {
            return Err(
                "a call whose deadline had passed was still running after the grace period (INV-12)"
                    .to_owned(),
            );
        }
    }
    past.cancel();

    // A deadline that passes while the call runs, for as many calls as the proof uses.
    let short = i64::try_from(timing.deadline_ms).unwrap_or(i64::MAX);
    let mut batch: Vec<SlowCall> = (0..calls).map(|_| spawn_slow(kit, slow, short)).collect();
    let events = join_all(batch.iter_mut().map(|c| c.first_within(wait))).await;
    for event in events {
        match event {
            Some(Event::Failed(err)) if err.code == NgcErrorCode::DeadlineExceeded => {}
            Some(Event::Refused) => {
                return Err(
                    "the connector refused a slow call before the deadline test could start (is it busy?)"
                        .to_owned(),
                );
            }
            Some(other) => {
                return Err(format!(
                    "a call with a {} ms deadline ended as {other:?} instead of DEADLINE_EXCEEDED (NGC-025)",
                    timing.deadline_ms
                ));
            }
            None => {
                return Err(format!(
                    "a call with a {} ms deadline was still running {} ms later (NGC-025, INV-12)",
                    timing.deadline_ms,
                    wait.as_millis()
                ));
            }
        }
    }
    for call in batch {
        call.cancel();
    }

    let max_in_flight = kit.desc.limits.as_ref().map_or(0, |l| l.max_in_flight);
    if usize::try_from(max_in_flight).unwrap_or(usize::MAX) > calls {
        notes.push(format!(
            "capacity proof used {calls} of {max_in_flight} in-flight slots: it only shows that those slots came back"
        ));
    }
    capacity_returns(kit, slow, calls)
        .await
        .map_err(|why| format!("after the deadlines passed, {why} (NGC-025, INV-12)"))?;
    pass()
}

/// `cancel`: a client cancel stops the work and the capacity comes back.
pub(crate) async fn cancel(kit: &Kit<'_>, _notes: &mut Vec<String>) -> Verdict {
    let (slow, calls) = match slow_probe(kit) {
        Ok(found) => found,
        Err(skip) => return skip,
    };
    let long = i64::try_from(kit.profile.timing.call_timeout_ms).unwrap_or(i64::MAX);

    let mut held: Vec<SlowCall> = (0..calls).map(|_| spawn_slow(kit, slow, long)).collect();
    for call in &mut held {
        match call.first_within(SETTLE).await {
            Some(Event::Refused) => {
                return Err(
                    "the connector refused a slow call before anything was cancelled (is it busy?)"
                        .to_owned(),
                );
            }
            Some(Event::Failed(err)) => {
                return Err(format!(
                    "a slow call failed {:?} before it was cancelled: {}",
                    err.code,
                    err.message()
                ));
            }
            Some(Event::Ended) => {
                return Err(format!(
                    "operation `{}` ended by itself; the [slow] operation must stay open until cancelled",
                    slow.operation
                ));
            }
            Some(Event::Streaming) | None => {}
        }
    }
    for call in held {
        call.cancel();
    }
    capacity_returns(kit, slow, calls)
        .await
        .map_err(|why| format!("after the client cancelled, {why} (NGC-025, INV-12)"))?;
    pass()
}
