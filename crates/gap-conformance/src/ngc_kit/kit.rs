//! Shared machinery of the checks: call context, query collection with stream-shape validation, row helpers.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use gap_core::error::NgcErrorInfo;
use gap_ngc_sdk::connector::NgcResult;
use gap_ngc_sdk::error::{prost_struct_to_json_map, prost_value_to_json};
use gap_ngc_sdk::grpc::GrpcClient;
use gap_proto::ngc::v1::{
    CallContext, Complete, ConnectorDescription, DescribeRequest, DescribeResponse, MutateRequest,
    MutateResponse, NgcError, Principal, PrincipalKind, QueryRequest, QueryResponse, VariableSet,
    query_response,
};
use prost::Message;
use prost_types::Struct;
use serde_json::Value;

use super::profile::{EffectProbe, Profile, table_to_struct, toml_to_prost_value};

/// Tenant used by ordinary kit calls.
pub(crate) const KIT_TENANT: &str = "kit-tenant-a";
/// A second tenant, for the tenant-binding checks (`NGC-022`, `NGC-030`).
pub(crate) const KIT_OTHER_TENANT: &str = "kit-tenant-b";

/// Everything the checks share for one run.
pub(crate) struct Kit<'a> {
    pub(crate) client: &'a GrpcClient,
    pub(crate) profile: &'a Profile,
    pub(crate) desc: &'a ConnectorDescription,
    nonce: u64,
    seq: AtomicU64,
    /// A credential-looking value sent as `authorization`; it must never come back in an error (`NGC-061`, `INV-15`).
    pub(crate) canary: String,
}

impl<'a> Kit<'a> {
    pub(crate) fn new(
        client: &'a GrpcClient,
        profile: &'a Profile,
        desc: &'a ConnectorDescription,
    ) -> Self {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
            u64::try_from(d.as_nanos() & 0xffff_ffff_ffff).unwrap_or(0)
        });
        Self {
            client,
            profile,
            desc,
            nonce,
            seq: AtomicU64::new(0),
            canary: format!("kit-canary-{nonce:x}"),
        }
    }

    pub(crate) fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// A key that is unique across runs (idempotency keys must not collide with an earlier run).
    pub(crate) fn unique(&self, label: &str) -> String {
        format!("kit-{:x}-{label}-{}", self.nonce, self.next_seq())
    }

    /// Call context with an absolute deadline `deadline_in_ms` from now (may be negative: already past).
    pub(crate) fn ctx(
        &self,
        tenant: &str,
        deadline_in_ms: i64,
        idempotency_key: &str,
    ) -> CallContext {
        CallContext {
            tenant_id: tenant.to_owned(),
            request_id: self.unique("req"),
            deadline_unix_ms: Self::now_ms().saturating_add(deadline_in_ms),
            principal: Some(Principal {
                id: "gap-conformance-kit".to_owned(),
                kind: PrincipalKind::Service as i32,
                ..Principal::default()
            }),
            idempotency_key: idempotency_key.to_owned(),
            ..CallContext::default()
        }
    }

    /// Ordinary call context: the kit's tenant and the profile's call timeout.
    pub(crate) fn call_ctx(&self) -> CallContext {
        let timeout = i64::try_from(self.profile.timing.call_timeout_ms).unwrap_or(i64::MAX);
        self.ctx(KIT_TENANT, timeout, "")
    }

    pub(crate) fn metadata(&self) -> BTreeMap<String, String> {
        BTreeMap::from([(
            "authorization".to_owned(),
            format!("Bearer {}", self.canary),
        )])
    }

    pub(crate) async fn describe(&self, known_spec_hash: &str) -> NgcResult<DescribeResponse> {
        self.client
            .describe_with_metadata(
                DescribeRequest {
                    known_spec_hash: known_spec_hash.to_owned(),
                },
                &self.metadata(),
            )
            .await
    }

    pub(crate) fn query_request(&self, operation: &str, args: &toml::Table) -> QueryRequest {
        QueryRequest {
            ctx: Some(self.call_ctx()),
            operation: operation.to_owned(),
            arguments: Some(table_to_struct(args)),
            ..QueryRequest::default()
        }
    }

    pub(crate) async fn mutate(
        &self,
        operation: &str,
        args: &toml::Table,
        idempotency_key: &str,
        tenant: &str,
        dry_run: bool,
    ) -> NgcResult<MutateResponse> {
        let timeout = i64::try_from(self.profile.timing.call_timeout_ms).unwrap_or(i64::MAX);
        self.client
            .mutate_with_metadata(
                MutateRequest {
                    ctx: Some(self.ctx(tenant, timeout, idempotency_key)),
                    operation: operation.to_owned(),
                    arguments: Some(table_to_struct(args)),
                    dry_run,
                    ..MutateRequest::default()
                },
                &self.metadata(),
            )
            .await
    }

    /// Runs `req` to the end (bounded by the profile's call timeout) and records its shape.
    pub(crate) async fn collect(&self, req: QueryRequest) -> Collected {
        let mut out = Collected::default();
        let timeout = Duration::from_millis(self.profile.timing.call_timeout_ms);
        let drained = tokio::time::timeout(timeout, async {
            let mut stream = match self.client.query_with_metadata(req, &self.metadata()).await {
                Ok(stream) => stream,
                Err(err) => {
                    out.failure = Some(err);
                    return;
                }
            };
            while let Some(item) = stream.next().await {
                match item {
                    Ok(resp) => out.absorb(resp),
                    Err(err) => {
                        out.failure = Some(err);
                        return;
                    }
                }
            }
        })
        .await;
        if drained.is_err() {
            out.timed_out = true;
            out.violations
                .push("the response stream did not end within the call timeout".to_owned());
        }
        out
    }

    /// Rows of a `Query` (single execution) or an error text.
    pub(crate) async fn rows_of(&self, req: QueryRequest) -> Result<Collected, String> {
        let got = self.collect(req).await;
        if let Some(failure) = &got.failure {
            return Err(format!("{:?}: {}", failure.code, failure.message()));
        }
        if got.timed_out {
            return Err("timed out".to_owned());
        }
        Ok(got)
    }

    /// Number of rows returned by an effect probe query.
    pub(crate) async fn count_effects(&self, probe: &EffectProbe) -> Result<usize, String> {
        let got = self
            .rows_of(self.query_request(&probe.operation, &probe.arguments))
            .await
            .map_err(|e| format!("effect probe `{}` failed: {e}", probe.operation))?;
        Ok(got.total_rows())
    }

    pub(crate) fn variable_set(&self, key_var: &str, index: u32, key: &toml::Value) -> VariableSet {
        let mut fields = BTreeMap::new();
        fields.insert(key_var.to_owned(), toml_to_prost_value(key));
        VariableSet {
            index,
            values: Some(Struct { fields }),
        }
    }
}

/// What one `Query` call produced, with protocol violations noted as they happen.
#[derive(Debug, Default)]
pub(crate) struct Collected {
    /// Call-level failure: the call did not start or the stream ended with an error.
    pub(crate) failure: Option<NgcErrorInfo>,
    /// Rows by `foreach_index`, in arrival order.
    pub(crate) rows: BTreeMap<u32, Vec<Struct>>,
    /// Closing chunks seen per index.
    pub(crate) closes: BTreeMap<u32, u32>,
    /// Per-index errors (carried by the closing chunk).
    pub(crate) index_errors: BTreeMap<u32, NgcError>,
    /// `Complete` messages seen.
    pub(crate) completes: Vec<Complete>,
    /// Shape violations noticed while reading.
    pub(crate) violations: Vec<String>,
    /// Encoded size of all rows.
    pub(crate) row_bytes: usize,
    /// The stream did not end in time.
    pub(crate) timed_out: bool,
}

impl Collected {
    fn absorb(&mut self, resp: QueryResponse) {
        match resp.chunk {
            Some(query_response::Chunk::Rows(chunk)) => {
                if !self.completes.is_empty() {
                    self.violations
                        .push("a row chunk arrived after Complete".to_owned());
                }
                let idx = chunk.foreach_index;
                if self.closes.get(&idx).copied().unwrap_or(0) > 0 {
                    self.violations.push(format!(
                        "a chunk for index {idx} arrived after its closing chunk"
                    ));
                }
                self.row_bytes += chunk.rows.iter().map(Message::encoded_len).sum::<usize>();
                self.rows.entry(idx).or_default().extend(chunk.rows);
                if let Some(err) = chunk.error {
                    if !chunk.last_for_index {
                        self.violations.push(format!(
                            "index {idx}: an error travels on a chunk that is not the closing chunk"
                        ));
                    }
                    self.index_errors.insert(idx, err);
                }
                if chunk.last_for_index {
                    *self.closes.entry(idx).or_insert(0) += 1;
                }
            }
            Some(query_response::Chunk::Complete(done)) => self.completes.push(done),
            None => self
                .violations
                .push("a QueryResponse without a chunk".to_owned()),
        }
    }

    /// All rows, in index order then arrival order.
    pub(crate) fn all_rows(&self) -> Vec<&Struct> {
        self.rows.values().flatten().collect()
    }

    pub(crate) fn total_rows(&self) -> usize {
        self.rows.values().map(Vec::len).sum()
    }

    /// The single `Complete`, if exactly one arrived.
    pub(crate) fn complete(&self) -> Option<&Complete> {
        if self.completes.len() == 1 {
            self.completes.first()
        } else {
            None
        }
    }

    /// Shape problems of a `foreach` call over `expected` indices (`NGC-020`).
    pub(crate) fn foreach_problems(&self, expected: &[u32]) -> Vec<String> {
        let mut problems = self.violations.clone();
        if let Some(f) = &self.failure {
            problems.push(format!("call failed: {:?} {}", f.code, f.message()));
        }
        for idx in expected {
            match self.closes.get(idx).copied().unwrap_or(0) {
                1 => {}
                0 => problems.push(format!("index {idx} never received a closing chunk")),
                n => problems.push(format!("index {idx} received {n} closing chunks")),
            }
        }
        for idx in self.rows.keys().chain(self.closes.keys()) {
            if !expected.contains(idx) {
                problems.push(format!(
                    "a chunk carries index {idx}, which was not requested"
                ));
            }
        }
        if self.completes.len() != 1 {
            problems.push(format!(
                "expected exactly one Complete, got {}",
                self.completes.len()
            ));
        }
        problems.sort();
        problems.dedup();
        problems
    }
}

/// Canonical text of a row (keys sorted by the map type).
pub(crate) fn canon(row: &Struct) -> String {
    Value::Object(prost_struct_to_json_map(row)).to_string()
}

pub(crate) fn canon_all<'a>(rows: impl IntoIterator<Item = &'a Struct>) -> Vec<String> {
    rows.into_iter().map(canon).collect()
}

pub(crate) fn canon_sorted<'a>(rows: impl IntoIterator<Item = &'a Struct>) -> Vec<String> {
    let mut v = canon_all(rows);
    v.sort();
    v
}

/// A field of a row as JSON.
pub(crate) fn field(row: &Struct, name: &str) -> Option<Value> {
    row.fields.get(name).map(prost_value_to_json)
}

/// A TOML key as the JSON a row would carry for it.
pub(crate) fn key_json(key: &toml::Value) -> Value {
    prost_value_to_json(&toml_to_prost_value(key))
}

/// First differing detail between two canonical row lists, for failure messages.
pub(crate) fn first_difference(left: &[String], right: &[String]) -> String {
    if left.len() != right.len() {
        return format!("{} rows vs {} rows", left.len(), right.len());
    }
    left.iter()
        .zip(right)
        .position(|(l, r)| l != r)
        .map_or_else(
            || "identical".to_owned(),
            |i| format!("row {i}: {} vs {}", left[i], right[i]),
        )
}

/// Compares two JSON values, numbers by value.
pub(crate) fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(l, r)| same_value(l, r))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| same_value(v, w)))
        }
        _ => a == b,
    }
}

/// Orders two JSON scalars (numbers by value, strings lexicographically); `None` if not comparable.
pub(crate) fn compare_scalars(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Truncates a diagnostic to a readable length.
pub(crate) fn clip(text: &str) -> String {
    const MAX: usize = 240;
    if text.chars().count() <= MAX {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(MAX).collect();
        format!("{cut}...")
    }
}
