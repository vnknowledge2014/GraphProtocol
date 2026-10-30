//! `foreach` checks (`NGC-020`, `NGC-026`, `INV-18`).

use super::{Verdict, not_applicable, not_covered, pass};
use crate::ngc_kit::kit::{
    Collected, Kit, canon_sorted, clip, field, first_difference, key_json, same_value,
};
use crate::ngc_kit::profile::ReadProbe;

/// Offset and stride that keep the caller's indices distinct from the positions of the sets (`NGC-096` (a)).
const INDEX_BASE: u32 = 100;
const INDEX_STRIDE: u32 = 7;

/// What the foreach checks need before they can run.
struct Setup<'a> {
    read: &'a ReadProbe,
    /// `limits.max_batch_size`: sets per request.
    batch: usize,
}

fn setup<'a>(kit: &'a Kit<'_>) -> Result<Setup<'a>, Verdict> {
    let max_batch = kit.desc.limits.as_ref().map_or(0, |l| l.max_batch_size);
    if max_batch == 0 {
        return Err(not_applicable(
            "limits.max_batch_size is 0: the connector does not support foreach (NGC-015)",
        ));
    }
    let Some(read) = kit.profile.read.as_ref() else {
        return Err(not_covered(
            "foreach is supported but the profile has no [read] section",
        ));
    };
    Ok(Setup {
        read,
        batch: usize::try_from(max_batch).unwrap_or(usize::MAX),
    })
}

/// Runs one `foreach` call over `(index, key)` pairs.
async fn run(kit: &Kit<'_>, read: &ReadProbe, items: &[(u32, &toml::Value)]) -> Collected {
    let mut req = kit.query_request(&read.operation, &read.arguments);
    req.foreach = items
        .iter()
        .map(|(index, key)| kit.variable_set(&read.key_var, *index, key))
        .collect();
    kit.collect(req).await
}

fn index_of(position: usize) -> u32 {
    INDEX_BASE + INDEX_STRIDE * u32::try_from(position).unwrap_or(0)
}

/// Rows of `index` as a sorted canonical list.
fn rows_at(got: &Collected, index: u32) -> Vec<String> {
    canon_sorted(got.rows.get(&index).map(Vec::as_slice).unwrap_or_default())
}

/// `foreach.correctness` (`NGC-020`).
pub(crate) async fn correctness(kit: &Kit<'_>, notes: &mut Vec<String>) -> Verdict {
    let setup = match setup(kit) {
        Ok(s) => s,
        Err(skip) => return skip,
    };
    let (read, batch) = (setup.read, setup.batch);
    let n = batch.min(read.keys.len());
    let picked: Vec<(u32, &toml::Value)> = read
        .keys
        .iter()
        .take(n)
        .enumerate()
        .map(|(i, k)| (index_of(i), k))
        .collect();
    let indices: Vec<u32> = picked.iter().map(|(i, _)| *i).collect();

    // (a) one batch: every index closes exactly once, no stray chunks, one Complete.
    let together = run(kit, read, &picked).await;
    let problems = together.foreach_problems(&indices);
    if !problems.is_empty() {
        return Err(format!(
            "foreach stream shape (NGC-020): {}",
            problems.join("; ")
        ));
    }
    for (idx, key) in &picked {
        if let Some(err) = together.index_errors.get(idx) {
            return Err(format!(
                "index {idx}: an existing key {key} came back with an error: {}",
                clip(&err.message)
            ));
        }
        if let Some(row_key) = &read.row_key {
            let want = key_json(key);
            for row in together
                .rows
                .get(idx)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                if !field(row, row_key).is_some_and(|got| same_value(&got, &want)) {
                    return Err(format!(
                        "index {idx} (key {key}) received a row whose `{row_key}` is not the key: rows were attributed to the wrong index"
                    ));
                }
            }
        }
    }

    // (b) independence: every key alone yields the same rows as inside the batch.
    let mut solo_rows = Vec::with_capacity(picked.len());
    for (idx, key) in &picked {
        let solo = run(kit, read, &[(0, *key)]).await;
        let problems = solo.foreach_problems(&[0]);
        if !problems.is_empty() || solo.index_errors.contains_key(&0) {
            return Err(format!("key {key} alone: {}", problems.join("; ")));
        }
        let alone = rows_at(&solo, 0);
        let batched = rows_at(&together, *idx);
        if alone != batched {
            return Err(format!(
                "key {key}: the rows differ between a solo call and a batch of {n} ({}); foreach indices must be evaluated independently (NGC-020)",
                first_difference(&alone, &batched)
            ));
        }
        solo_rows.push(alone);
    }
    if n < 2 {
        notes.push(
            "limits.max_batch_size is 1: co-batching independence was not exercised".to_owned(),
        );
    }

    // (c) the order of the sets does not change any index's rows.
    if n >= 2 {
        let reversed: Vec<(u32, &toml::Value)> = picked
            .iter()
            .rev()
            .enumerate()
            .map(|(i, (_, key))| (index_of(i) + 1_000, *key))
            .collect();
        let rev_indices: Vec<u32> = reversed.iter().map(|(i, _)| *i).collect();
        let got = run(kit, read, &reversed).await;
        let problems = got.foreach_problems(&rev_indices);
        if !problems.is_empty() {
            return Err(format!("reversed foreach: {}", problems.join("; ")));
        }
        for ((idx, key), position) in reversed.iter().zip((0..picked.len()).rev()) {
            if rows_at(&got, *idx) != solo_rows[position] {
                return Err(format!(
                    "key {key}: rows depend on the position of the set in the request (NGC-020)"
                ));
            }
        }

        // (d) two indices carrying equal values both receive the rows.
        let first = picked[0].1;
        let twice = run(kit, read, &[(1, first), (2, first)]).await;
        let problems = twice.foreach_problems(&[1, 2]);
        if !problems.is_empty() {
            return Err(format!("duplicate values: {}", problems.join("; ")));
        }
        for idx in [1, 2] {
            if rows_at(&twice, idx) != solo_rows[0] {
                return Err(format!(
                    "key {first} sent twice: index {idx} did not receive the same rows as a single call"
                ));
            }
        }
    }

    // (e) a key without rows closes empty and without error.
    if let Some(absent) = &read.absent_key {
        let mut items = vec![(1, absent)];
        if n >= 2 {
            items.push((2, picked[0].1));
        }
        let ids: Vec<u32> = items.iter().map(|(i, _)| *i).collect();
        let got = run(kit, read, &items).await;
        let problems = got.foreach_problems(&ids);
        if !problems.is_empty() {
            return Err(format!("absent key: {}", problems.join("; ")));
        }
        if got.index_errors.contains_key(&1) || got.rows.get(&1).is_some_and(|r| !r.is_empty()) {
            return Err(format!(
                "absent key {absent}: expected an empty, error-free closing chunk"
            ));
        }
        if n >= 2 && rows_at(&got, 2) != solo_rows[0] {
            return Err("an absent key changed the rows of another index".to_owned());
        }
    } else {
        notes.push(
            "profile has no read.absent_key: empty-index closing was not exercised".to_owned(),
        );
    }

    // (f) partial failure stays on its index.
    if let Some(bad) = &read.failing_key {
        let mut items = vec![(1, bad)];
        for (offset, (_, key)) in picked
            .iter()
            .take(batch.saturating_sub(1).min(2))
            .enumerate()
        {
            items.push((2 + u32::try_from(offset).unwrap_or(0), *key));
        }
        let ids: Vec<u32> = items.iter().map(|(i, _)| *i).collect();
        let got = run(kit, read, &items).await;
        if got.failure.is_some() {
            return Err(format!(
                "a failing key aborted the whole call instead of failing only its index (NGC-020): {:?}",
                got.failure.as_ref().map(|f| f.code)
            ));
        }
        let problems = got.foreach_problems(&ids);
        if !problems.is_empty() {
            return Err(format!("failing key: {}", problems.join("; ")));
        }
        if !got.index_errors.contains_key(&1) {
            return Err(format!(
                "failing key {bad}: its index closed without an error"
            ));
        }
        for (offset, idx) in ids.iter().skip(1).enumerate() {
            if got.index_errors.contains_key(idx) {
                return Err(format!(
                    "index {idx} failed although only index 1 carried the failing key (NGC-020)"
                ));
            }
            if rows_at(&got, *idx) != solo_rows[offset] {
                return Err(format!(
                    "index {idx}: rows changed because another index failed (NGC-020)"
                ));
            }
        }
    } else {
        notes.push("profile has no read.failing_key: partial failure was not exercised".to_owned());
    }
    pass()
}

/// `foreach.n_plus_one` (`NGC-026`, `INV-18`, `VER-040`).
pub(crate) async fn n_plus_one(kit: &Kit<'_>, notes: &mut Vec<String>) -> Verdict {
    let setup = match setup(kit) {
        Ok(s) => s,
        Err(skip) => return skip,
    };
    let (read, batch) = (setup.read, setup.batch);
    let native = kit
        .desc
        .capabilities
        .as_ref()
        .is_some_and(|c| c.foreach_batch);
    let upstream_batch = read
        .upstream_batch
        .map_or(batch, |b| usize::try_from(b).unwrap_or(usize::MAX))
        .max(1);
    let bound_for = |distinct: usize| {
        if native {
            distinct.div_ceil(upstream_batch)
        } else {
            distinct
        }
    };
    if !native {
        notes.push(
            "foreach_batch is not advertised (emulated foreach): the bound is one upstream call per distinct key"
                .to_owned(),
        );
    }

    let n = batch.min(read.keys.len());
    let distinct: Vec<(u32, &toml::Value)> = read
        .keys
        .iter()
        .take(n)
        .enumerate()
        .map(|(i, k)| (index_of(i), k))
        .collect();
    let got = run(kit, read, &distinct).await;
    let calls = upstream_calls(&got)?;
    if calls > bound_for(n) {
        return Err(format!(
            "{n} distinct keys cost {calls} upstream calls, over the bound of {} (NGC-026, INV-18)",
            bound_for(n)
        ));
    }

    let m = batch / 2;
    if m >= 1 {
        let mut doubled: Vec<(u32, &toml::Value)> = Vec::new();
        for (position, (_, key)) in distinct.iter().take(m).enumerate() {
            doubled.push((index_of(2 * position), *key));
            doubled.push((index_of(2 * position + 1), *key));
        }
        let got = run(kit, read, &doubled).await;
        let calls = upstream_calls(&got)?;
        let distinct_count = doubled.len() / 2;
        if calls > bound_for(distinct_count) {
            return Err(format!(
                "{} sets over {distinct_count} distinct keys cost {calls} upstream calls, over the bound of {}: equal values must be evaluated once (NGC-026)",
                doubled.len(),
                bound_for(distinct_count)
            ));
        }
    }
    pass()
}

fn upstream_calls(got: &Collected) -> Result<usize, String> {
    let complete = got.complete().ok_or_else(|| {
        format!(
            "the foreach call did not end with exactly one Complete ({})",
            got.foreach_problems(&[]).join("; ")
        )
    })?;
    Ok(usize::try_from(complete.upstream_calls).unwrap_or(usize::MAX))
}
