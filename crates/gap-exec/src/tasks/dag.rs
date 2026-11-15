//! Step-graph validation, topological wave scheduling, and peer hop/loop checks (`RUN-082`, `RUN-083`, `INV-20`).

use std::collections::{BTreeMap, VecDeque};

use gap_core::context::RequestContext;
use gap_core::error::{ErrorCode, GapError, GapResult};

use super::types::{TaskStepSpec, TaskStrategy};

/// Validates task step dependencies for `strategy` and returns execution waves of step indices.
///
/// Rejects empty submissions, duplicate step IDs, missing `depends_on` targets, strategy constraint
/// violations, and cyclic `DAG` graphs (`ErrorCode::CycleUnbounded`) at submission time (`RUN-082`).
///
/// # Errors
///
/// Returns [`ErrorCode::CycleUnbounded`] if a `DAG` contains a cycle, or
/// [`ErrorCode::ValidationError`] on empty, duplicate, unknown, or strategy-invalid dependencies.
pub fn validate_submission_steps(
    strategy: TaskStrategy,
    steps: &[TaskStepSpec],
) -> GapResult<Vec<Vec<usize>>> {
    if steps.is_empty() {
        return Err(GapError::new(
            ErrorCode::ValidationError,
            "task submission requires at least one step",
        ));
    }

    let mut id_to_idx = BTreeMap::new();
    for (idx, step) in steps.iter().enumerate() {
        if step.id.trim().is_empty() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("task step at index {idx} must have a non-empty id"),
            ));
        }
        if id_to_idx.insert(step.id.as_str(), idx).is_some() {
            return Err(GapError::new(
                ErrorCode::ValidationError,
                format!("duplicate task step id `{}`", step.id),
            ));
        }
    }

    for step in steps {
        for dep in &step.depends_on {
            if !id_to_idx.contains_key(dep.as_str()) {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("task step `{}` depends on unknown step `{dep}`", step.id),
                ));
            }
            if dep == &step.id {
                return Err(GapError::new(
                    ErrorCode::CycleUnbounded,
                    format!("task step `{}` cannot depend on itself", step.id),
                ));
            }
        }
    }

    match strategy {
        TaskStrategy::Single => {
            if steps.len() != 1 {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!(
                        "SINGLE task strategy requires exactly 1 step, got {}",
                        steps.len()
                    ),
                ));
            }
            if !steps[0].depends_on.is_empty() {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    "SINGLE task step cannot declare dependsOn edges",
                ));
            }
            Ok(vec![vec![0]])
        }
        TaskStrategy::Parallel => {
            for step in steps {
                if !step.depends_on.is_empty() {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "PARALLEL task step `{}` cannot declare dependsOn edges",
                            step.id
                        ),
                    ));
                }
            }
            Ok(vec![(0..steps.len()).collect()])
        }
        TaskStrategy::Sequential => {
            for (idx, step) in steps.iter().enumerate() {
                let valid = if idx == 0 {
                    step.depends_on.is_empty()
                } else {
                    step.depends_on.is_empty()
                        || (step.depends_on.len() == 1 && step.depends_on[0] == steps[idx - 1].id)
                };
                if !valid {
                    return Err(GapError::new(
                        ErrorCode::ValidationError,
                        format!(
                            "SEQUENTIAL task step `{}` may only depend on the preceding step",
                            step.id
                        ),
                    ));
                }
            }
            Ok((0..steps.len()).map(|i| vec![i]).collect())
        }
        TaskStrategy::Dag => topological_waves(steps, &id_to_idx),
    }
}

fn topological_waves(
    steps: &[TaskStepSpec],
    id_to_idx: &BTreeMap<&str, usize>,
) -> GapResult<Vec<Vec<usize>>> {
    let n = steps.len();
    let mut in_degree = vec![0usize; n];
    let mut outgoing = vec![Vec::new(); n];

    for (idx, step) in steps.iter().enumerate() {
        let mut seen_deps = Vec::with_capacity(step.depends_on.len());
        for dep_id in &step.depends_on {
            let Some(&dep_idx) = id_to_idx.get(dep_id.as_str()) else {
                return Err(GapError::new(
                    ErrorCode::ValidationError,
                    format!("unknown dependency `{dep_id}`"),
                ));
            };
            if seen_deps.contains(&dep_idx) {
                continue;
            }
            seen_deps.push(dep_idx);
            outgoing[dep_idx].push(idx);
            in_degree[idx] += 1;
        }
    }

    let mut ready = VecDeque::new();
    for (idx, &deg) in in_degree.iter().enumerate() {
        if deg == 0 {
            ready.push_back(idx);
        }
    }

    let mut waves = Vec::new();
    let mut visited = 0usize;
    while !ready.is_empty() {
        let wave_len = ready.len();
        let mut wave = Vec::with_capacity(wave_len);
        for _ in 0..wave_len {
            if let Some(u) = ready.pop_front() {
                wave.push(u);
                visited += 1;
                for &v in &outgoing[u] {
                    in_degree[v] -= 1;
                    if in_degree[v] == 0 {
                        ready.push_back(v);
                    }
                }
            }
        }
        wave.sort_unstable();
        waves.push(wave);
    }

    if visited != n {
        return Err(GapError::new(
            ErrorCode::CycleUnbounded,
            "cyclic dependency graph detected in DAG task submission",
        ));
    }

    Ok(waves)
}

/// Computes the downstream [`RequestContext`] for a delegated hop (`hop + 1`, appends `node_id` to
/// `trace.path`), enforcing `max_hops` and peer-loop detection (`RUN-083`, `INV-20`, `NGC-042`).
///
/// # Errors
///
/// Returns [`ErrorCode::LoopDetected`] if `node_id` is already present in `ctx.trace.path`, or
/// [`ErrorCode::HopLimit`] if advancing the hop exceeds `max_hops`.
pub fn advance_delegated_hop(
    ctx: &RequestContext,
    node_id: &str,
    max_hops: u8,
) -> GapResult<RequestContext> {
    ctx.advance_peer_hop(node_id, max_hops)
}
