//! Reusable `NativeGraphConnector` black-box conformance kit (`WP-M1-04`, `NGC-091`).
//!
//! [`run_kit`] drives a connector through its gRPC binding and reports, per check, [`Outcome::Pass`], [`Outcome::Fail`] or
//! [`Outcome::Skip`]. A skip is either `NotApplicable` (the capability is not advertised) or `NotCovered` (advertised, but the
//! [`Profile`] gives the kit nothing to probe it with); a connector may claim "NGC-compatible" only if
//! [`KitReport::compatible`] holds: no failure and no uncovered capability.
//!
//! What to probe (operations, keys, a never-ending operation, a mutation) is connector-specific data in a profile,
//! `conformance/ngc/<name>.kit.toml`; see `conformance/ngc/README.md`. The kit proves itself against
//! [`reference::ReferenceConnector`]: the good connector passes every check and each [`reference::Flaw`] fails the
//! checks it targets (see the `selftest` module).

mod checks;
mod kit;
pub mod outcome;
pub mod profile;
pub mod reference;
#[cfg(test)]
mod selftest;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use gap_ngc_sdk::grpc::GrpcClient;
use gap_proto::ngc::v1::{ConnectorDescription, DescribeRequest};

pub use outcome::{CHECKS, CheckInfo, CheckResult, KitReport, Outcome, SkipReason, id};
pub use profile::{Profile, ProfileError};

/// Seconds the kit waits for a TCP connection to the connector.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Error from [`connect`].
#[derive(Debug, thiserror::Error)]
#[error("cannot use connector address `{addr}`: {reason}")]
pub struct ConnectError {
    addr: String,
    reason: String,
}

/// Builds a client for the connector at `addr` (`host:port` or `http://host:port`); it connects on first use.
///
/// Only plain-text gRPC is supported: the kit is meant for a loopback or private address (`NGC-001` requires TLS off-host).
///
/// # Errors
///
/// [`ConnectError`] for an `https://` or otherwise unsupported or malformed address.
pub fn connect(addr: &str) -> Result<GrpcClient, ConnectError> {
    let fail = |reason: &str| ConnectError {
        addr: addr.to_owned(),
        reason: reason.to_owned(),
    };
    let trimmed = addr.trim();
    if trimmed.starts_with("https://") {
        return Err(fail(
            "TLS endpoints are not supported by the kit; use plain-text gRPC on a loopback or private address",
        ));
    }
    let url = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("http://{trimmed}")
    };
    if !url.starts_with("http://") {
        return Err(fail("expected `host:port` or `http://host:port`"));
    }
    let endpoint = tonic::transport::Endpoint::from_shared(url)
        .map_err(|e| fail(&e.to_string()))?
        .connect_timeout(CONNECT_TIMEOUT);
    Ok(GrpcClient::from_channel(endpoint.connect_lazy()))
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Report for a connector whose `Describe` could not even be read: nothing else can be probed.
fn describe_unavailable(profile: &Profile, why: &str) -> KitReport {
    let results = CHECKS
        .iter()
        .map(|info| CheckResult {
            info: *info,
            outcome: if info.id == id::DESCRIBE_VALID {
                Outcome::Fail(why.to_owned())
            } else {
                Outcome::Skip(SkipReason::NotCovered(
                    "Describe failed, so nothing else can be probed".to_owned(),
                ))
            },
            notes: Vec::new(),
            elapsed_ms: 0,
        })
        .collect();
    KitReport {
        profile: profile.name.clone(),
        connector_id: String::new(),
        results,
    }
}

/// Runs every check of the kit against `client`, in [`CHECKS`] order.
///
/// Checks run one after the other: several of them change connector state (a mutation, held calls) and must not overlap.
pub async fn run_kit(client: &GrpcClient, profile: &Profile) -> KitReport {
    run_selected(client, profile, None).await
}

/// Like [`run_kit`], optionally restricted to the checks named in `only` (in [`CHECKS`] order); the report then holds only
/// those. Running one check on a fresh connector shows what that check alone proves, without the damage earlier checks did.
pub(crate) async fn run_selected(
    client: &GrpcClient,
    profile: &Profile,
    only: Option<&[&str]>,
) -> KitReport {
    let desc: ConnectorDescription = match client
        .describe_with_metadata(DescribeRequest::default(), &BTreeMap::new())
        .await
    {
        Ok(resp) => match resp.description {
            Some(desc) => desc,
            None => {
                return describe_unavailable(
                    profile,
                    "Describe without known_spec_hash returned no description",
                );
            }
        },
        Err(err) => {
            return describe_unavailable(
                profile,
                &format!("Describe failed {:?}: {}", err.code, err.message()),
            );
        }
    };

    let kit = kit::Kit::new(client, profile, &desc);
    let mut results = Vec::with_capacity(CHECKS.len());
    for info in &CHECKS {
        if only.is_some_and(|ids| !ids.contains(&info.id)) {
            continue;
        }
        let started = Instant::now();
        let mut notes = Vec::new();
        let verdict = checks::run_one(&kit, info.id, &mut notes).await;
        results.push(CheckResult {
            info: *info,
            outcome: checks::finish(verdict),
            notes,
            elapsed_ms: elapsed_ms(started),
        });
    }
    KitReport {
        profile: profile.name.clone(),
        connector_id: desc.connector_id.clone(),
        results,
    }
}
