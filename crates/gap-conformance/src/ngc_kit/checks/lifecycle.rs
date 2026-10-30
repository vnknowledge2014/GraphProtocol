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
        drop(self);
    }
}

impl Drop for SlowCall {
    fn drop(&mut self) {
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

/// Opens `calls` slow calls with a long deadline and returns them once none is refused.
///
/// Refusals (`RESOURCE_EXHAUSTED`) are retried until the grace period is over: work the previous step cancelled may still be
/// draining, and a transport needs a moment to tell the connector. A connector that keeps working for callers that left (or
/// holds its own gate) never gives the slots back, so it is still refusing when the grace period ends. Dropping the returned
/// calls cancels them.
async fn open_all(kit: &Kit<'_>, slow: &SlowProbe, calls: usize) -> Result<Vec<SlowCall>, String> {
    let long = i64::try_from(kit.profile.timing.call_timeout_ms).unwrap_or(i64::MAX);
    let started = tokio::time::Instant::now();
    loop {
        let mut batch: Vec<SlowCall> = (0..calls).map(|_| spawn_slow(kit, slow, long)).collect();
        let mut refused = 0_usize;
        for call in &mut batch {
            match call.first_within(SETTLE).await {
                Some(Event::Refused) => refused += 1,
                Some(Event::Failed(err)) => {
                    return Err(format!(
                        "a slow call failed {:?}: {}",
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
        if refused == 0 {
            return Ok(batch);
        }
        drop(batch);
        if started.elapsed() >= grace(kit) {
            return Err(format!(
                "{refused} of {calls} slow calls were still refused (RESOURCE_EXHAUSTED) {} ms after the previous ones ended: the connector did not release the capacity",
                grace(kit).as_millis()
            ));
        }
        tokio::time::sleep(RETRY_PAUSE).await;
    }
}

/// The capacity proof: a fresh batch of slow calls is accepted, then cancelled again.
async fn capacity_returns(kit: &Kit<'_>, slow: &SlowProbe, calls: usize) -> Result<(), String> {
    open_all(kit, slow, calls).await.map(drop)
}

/// A call whose deadline passed must have failed `DEADLINE_EXCEEDED` within `wait`; anything else is explained here.
fn expect_deadline_exceeded(
    event: Option<Event>,
    what: &str,
    wait: Duration,
) -> Result<(), String> {
    match event {
        Some(Event::Failed(err)) if err.code == NgcErrorCode::DeadlineExceeded => Ok(()),
        Some(Event::Failed(err)) => Err(format!(
            "{what} failed {:?} instead of DEADLINE_EXCEEDED (NGC-025)",
            err.code
        )),
        Some(Event::Refused) => Err(format!(
            "{what} was refused (RESOURCE_EXHAUSTED) before the deadline test could start (is the connector busy?)"
        )),
        Some(other) => Err(format!(
            "{what} was accepted ({other:?}) instead of failing DEADLINE_EXCEEDED (NGC-025)"
        )),
        None => Err(format!(
            "{what} was still running {} ms later (NGC-025, INV-12)",
            wait.as_millis()
        )),
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
    expect_deadline_exceeded(
        past.first_within(grace(kit)).await,
        "a call whose deadline had passed",
        grace(kit),
    )?;
    past.cancel();

    // A deadline that passes while the call runs, for as many calls as the proof uses.
    let short = i64::try_from(timing.deadline_ms).unwrap_or(i64::MAX);
    let mut batch: Vec<SlowCall> = (0..calls).map(|_| spawn_slow(kit, slow, short)).collect();
    let events = join_all(batch.iter_mut().map(|c| c.first_within(wait))).await;
    for event in events {
        expect_deadline_exceeded(
            event,
            &format!("a call with a {} ms deadline", timing.deadline_ms),
            wait,
        )?;
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
    let held = open_all(kit, slow, calls)
        .await
        .map_err(|why| format!("before anything was cancelled, {why}"))?;
    drop(held);
    capacity_returns(kit, slow, calls)
        .await
        .map_err(|why| format!("after the client cancelled, {why} (NGC-025, INV-12)"))?;
    pass()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_millis(2_200);

    // spec: NGC-025, INV-12
    #[test]
    fn ngc_kit_deadline_verdict_accepts_only_deadline_exceeded() {
        let ok = expect_deadline_exceeded(
            Some(Event::Failed(NgcErrorInfo::new(
                NgcErrorCode::DeadlineExceeded,
                "late",
            ))),
            "a call",
            WAIT,
        );
        assert_eq!(ok, Ok(()));

        for code in [
            NgcErrorCode::Cancelled,
            NgcErrorCode::Internal,
            NgcErrorCode::Unavailable,
        ] {
            let err = expect_deadline_exceeded(
                Some(Event::Failed(NgcErrorInfo::new(code, "x"))),
                "a call",
                WAIT,
            )
            .expect_err("a wrong code must fail");
            assert!(err.contains("instead of DEADLINE_EXCEEDED"), "{err}");
        }
    }

    // spec: NGC-025, INV-12
    #[test]
    fn ngc_kit_deadline_verdict_explains_refusal_acceptance_and_a_call_that_never_ends() {
        let refused =
            expect_deadline_exceeded(Some(Event::Refused), "a call", WAIT).expect_err("refusal");
        assert!(refused.contains("refused"), "{refused}");
        for event in [Event::Streaming, Event::Ended] {
            let accepted =
                expect_deadline_exceeded(Some(event), "a call", WAIT).expect_err("accepted");
            assert!(accepted.contains("was accepted"), "{accepted}");
        }
        let hung = expect_deadline_exceeded(None, "a call", WAIT).expect_err("still running");
        assert!(hung.contains("still running 2200 ms later"), "{hung}");
    }
}
