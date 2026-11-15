//! Mutation pipeline: side-effect policy, approval gate, Ed25519 `ApprovalProof`, idempotency,
//! dry-run, and mutation audit persistence (`RUN-070`..`RUN-075`, `NGC-030`..`NGC-033`, `INV-11`).

pub mod approval;
pub mod pipeline;

#[cfg(test)]
mod tests;

pub use approval::{
    ApprovalDecisionOutcome, ApprovalGate, ApprovalReplayGuard, PendingApproval,
    canonical_args_hash, deterministic_approval_summary, sign_approval_proof,
    verify_approval_proof,
};
pub use pipeline::{
    IDEMPOTENCY_POLL_INTERVAL_MS, MutationExecutionRequest, MutationExecutionResult,
    MutationPipeline, MutationPipelineConfig, MutationTarget, SdkMutationTarget,
    json_to_prost_struct, prost_struct_to_json,
};
