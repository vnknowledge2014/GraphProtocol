//! Sandboxed, allow-listed CLI [`Connector`] implementation (`CMP-060`, `SEC-061`, `SEC-070`, `NGC-082`).

use std::collections::BTreeMap;
use std::future::{Future, ready};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gap_core::error::{NgcErrorCode, NgcErrorInfo};
use gap_ngc_sdk::connector::{BoxStream, CallScope, Connector, NgcResult};
use gap_ngc_sdk::foreach::{ForeachConfig, foreach};
use gap_ngc_sdk::json::{from_json_str, to_json_string};
use gap_proto::NGC_WIRE_VERSION;
use gap_proto::ngc::v1::{
    CallContext, Capabilities, ConnectorDescription, DescribeRequest, DescribeResponse,
    HealthRequest, HealthResponse, MutateRequest, MutateResponse, MutationOutcome, OpKind,
    OperationDescriptor, Protocol, QueryRequest, QueryResponse, RowSetChunk, ServingStatus,
    SideEffectHint, VariableSet,
};
use gap_sec::authn::crypto::{ct_eq, hex_encode, sha256};
use gap_sec::authn::json::JsonValue;
use gap_sec::sandbox::{SandboxError, SandboxLauncher, SandboxProfile};

use crate::template::ArgvTemplate;

/// Gateway caller authentication mode enforced by [`CliConnector`] (`NGC-082`).
///
/// `ctx.principal` carried inside protobuf `CallContext` is informational only and MUST NOT be
/// used as the sole authentication of GAP.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CliGatewayAuth {
    /// Permitted only on isolated local stdio / unit test benches where no shared token is configured.
    #[default]
    AllowLocalStdioUnauthenticated,
    /// Requires a matching gateway bearer credential inside [`CallScope::metadata`] (`authorization`).
    RequireBearerToken {
        /// Shared secret token presented as `Bearer <token>` in call metadata.
        expected_token: String,
    },
}

/// Declarative binding from an NGC operation name to a sandboxed CLI binary invocation (`CMP-060`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOperationSpec {
    /// Operation identifier exposed in `Describe`.
    pub name: String,
    /// Query vs Mutation.
    pub kind: OpKind,
    /// Static untrusted description shown in `Describe`.
    pub description: String,
    /// Deterministically declared side-effect classification (`ADR-015`).
    pub side_effect: SideEffectHint,
    /// Whether the CLI command is idempotent.
    pub idempotent: bool,
    /// Whether the template accepts `dry_run` on mutations.
    pub supports_dry_run: bool,
    /// Absolute path to the allow-listed executable (`SEC-061`).
    pub binary_path: PathBuf,
    /// Direct `argv` template (`CMP-060`: never passes through a shell).
    pub template: ArgvTemplate,
    /// Base sandbox profile applied per invocation (`SEC-070`).
    pub sandbox_profile: SandboxProfile,
    /// Host environment source map (only allow-listed keys are copied).
    pub env_source: BTreeMap<String, String>,
}

/// Configuration for [`CliConnector`].
#[derive(Debug, Clone)]
pub struct CliConnectorConfig {
    /// Connector instance identifier.
    pub connector_id: String,
    /// Connector semantic version.
    pub connector_version: String,
    /// GraphQL SDL schema snippet exported in `Describe`.
    pub schema_sdl: String,
    /// Sandbox launcher pre-loaded with [`gap_sec::manifest::ExecAllowList`].
    pub launcher: Arc<SandboxLauncher>,
    /// Gateway authentication rule (`NGC-082`).
    pub auth: CliGatewayAuth,
    /// Operation bindings indexed by operation name.
    pub operations: BTreeMap<String, CliOperationSpec>,
}

/// Allow-listed, sandboxed CLI Native Graph Connector (`CMP-060`, `SEC-070`, `NGC-082`).
#[derive(Debug, Clone)]
pub struct CliConnector {
    config: Arc<CliConnectorConfig>,
    spec_hash: String,
}

impl CliConnector {
    /// Constructs a [`CliConnector`] and computes its deterministic `spec_hash`.
    #[must_use]
    pub fn new(config: CliConnectorConfig) -> Self {
        let mut sig_input = format!(
            "{}:{}:{}",
            config.connector_id, config.connector_version, config.schema_sdl
        );
        for (name, op) in &config.operations {
            sig_input.push(';');
            sig_input.push_str(name);
            sig_input.push(':');
            sig_input.push_str(&op.binary_path.to_string_lossy());
        }
        let spec_hash = hex_encode(&sha256(sig_input.as_bytes()));
        Self {
            config: Arc::new(config),
            spec_hash,
        }
    }

    /// Validates gateway call authentication metadata strictly independent of `ctx.principal` (`NGC-082`).
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] with `UNAUTHENTICATED` when gateway credentials are required and missing or invalid.
    pub fn verify_call_auth_metadata(
        &self,
        metadata: &BTreeMap<String, String>,
        _ctx_informational: Option<&CallContext>,
    ) -> NgcResult<()> {
        match &self.config.auth {
            CliGatewayAuth::AllowLocalStdioUnauthenticated => Ok(()),
            CliGatewayAuth::RequireBearerToken { expected_token } => {
                let raw_header = metadata
                    .get("authorization")
                    .or_else(|| metadata.get("Authorization"))
                    .ok_or_else(|| {
                        NgcErrorInfo::new(
                            NgcErrorCode::Unauthenticated,
                            "missing gateway authorization metadata (ctx.principal alone is informational, NGC-082)",
                        )
                    })?;
                let presented = raw_header
                    .strip_prefix("Bearer ")
                    .or_else(|| raw_header.strip_prefix("bearer "))
                    .ok_or_else(|| {
                        NgcErrorInfo::new(
                            NgcErrorCode::Unauthenticated,
                            "authorization metadata must use Bearer scheme",
                        )
                    })?;
                if !ct_eq(presented.as_bytes(), expected_token.as_bytes()) {
                    return Err(NgcErrorInfo::new(
                        NgcErrorCode::Unauthenticated,
                        "invalid gateway authorization token",
                    ));
                }
                Ok(())
            }
        }
    }

    /// Synchronous helper implementing `Describe` (`NGC-010`, `NGC-082`).
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] if caller gateway authentication fails.
    pub fn describe_with_metadata(
        &self,
        request: DescribeRequest,
        metadata: &BTreeMap<String, String>,
    ) -> NgcResult<DescribeResponse> {
        self.verify_call_auth_metadata(metadata, None)?;
        if !request.known_spec_hash.is_empty()
            && ct_eq(
                request.known_spec_hash.as_bytes(),
                self.spec_hash.as_bytes(),
            )
        {
            return Ok(DescribeResponse {
                not_modified: true,
                description: None,
            });
        }

        let operations = self
            .config
            .operations
            .values()
            .map(|op| OperationDescriptor {
                name: op.name.clone(),
                kind: op.kind as i32,
                description: op.description.clone(),
                input_schema: None,
                output_schema: None,
                side_effect_hint: op.side_effect as i32,
                idempotent_hint: op.idempotent,
                supports_dry_run: op.supports_dry_run,
                compensation_operation: String::new(),
                examples: Vec::new(),
                cost_weight: 1,
                pagination: 0,
                key_fields: Vec::new(),
                annotations: std::collections::HashMap::new(),
            })
            .collect();

        Ok(DescribeResponse {
            not_modified: false,
            description: Some(ConnectorDescription {
                connector_id: self.config.connector_id.clone(),
                connector_version: self.config.connector_version.clone(),
                ngc_version: NGC_WIRE_VERSION.to_owned(),
                protocol: Protocol::Cli as i32,
                spec_hash: self.spec_hash.clone(),
                capabilities: Some(Capabilities {
                    foreach_batch: true,
                    dry_run: true,
                    ..Capabilities::default()
                }),
                limits: None,
                schema_sdl: self.config.schema_sdl.clone(),
                operations,
                meta: None,
                auth: None,
            }),
        })
    }

    /// Synchronously executes one CLI operation into a parsed [`JsonValue`] (`CMP-060`, `NGC-082`).
    ///
    /// # Errors
    ///
    /// Returns [`NgcErrorInfo`] if auth fails, operation is unknown, arguments violate the template,
    /// or sandbox execution fails/times out/exceeds caps.
    pub fn execute_json_with_metadata(
        &self,
        operation: &str,
        arguments: &BTreeMap<String, JsonValue>,
        metadata: &BTreeMap<String, String>,
        ctx: Option<&CallContext>,
        max_timeout: Option<Duration>,
    ) -> NgcResult<JsonValue> {
        self.verify_call_auth_metadata(metadata, ctx)?;
        let op = self.config.operations.get(operation).ok_or_else(|| {
            NgcErrorInfo::new(
                NgcErrorCode::NotFound,
                format!("unknown CLI operation `{operation}`"),
            )
        })?;
        execute_cli_spec(&self.config.launcher, op, arguments, max_timeout)
    }
}

impl Connector for CliConnector {
    fn describe(
        &self,
        request: DescribeRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<DescribeResponse>> + Send {
        ready(self.describe_with_metadata(request, &scope.metadata))
    }

    fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send {
        if let Err(err) = self.verify_call_auth_metadata(&scope.metadata, request.ctx.as_ref()) {
            return ready(Err(err));
        }
        let Some(op) = self.config.operations.get(&request.operation).cloned() else {
            return ready(Err(NgcErrorInfo::new(
                NgcErrorCode::NotFound,
                format!("unknown CLI query operation `{}`", request.operation),
            )));
        };
        if op.kind == OpKind::Mutation {
            return ready(Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!(
                    "operation `{}` is a mutation, use Mutate",
                    request.operation
                ),
            )));
        }

        let launcher = Arc::clone(&self.config.launcher);
        let remaining = scope
            .deadline
            .into_std()
            .checked_duration_since(Instant::now());
        let sets = if request.foreach.is_empty() {
            vec![VariableSet {
                index: 0,
                values: request.arguments,
            }]
        } else {
            request.foreach
        };

        let run_res = foreach(sets, ForeachConfig::new(16, 4), &scope, move |batch| {
            let mut results = Vec::with_capacity(batch.len());
            for item in batch {
                let outcome = to_json_string(&item).and_then(|json_str| {
                    let parsed = JsonValue::parse(&json_str).map_err(|err| {
                        NgcErrorInfo::new(
                            NgcErrorCode::InvalidArgument,
                            format!("invalid query argument struct: {err}"),
                        )
                    })?;
                    let empty = BTreeMap::new();
                    let args_map = parsed.as_object().unwrap_or(&empty);
                    let json_out = execute_cli_spec(&launcher, &op, args_map, remaining)?;
                    let chunk = parse_rows_chunk(&json_out)?;
                    Ok(chunk.rows)
                });
                results.push(outcome);
            }
            ready(Ok(results))
        });

        ready(run_res.map(gap_ngc_sdk::foreach::ForeachRun::into_query_stream))
    }

    fn mutate(
        &self,
        request: MutateRequest,
        scope: CallScope,
    ) -> impl Future<Output = NgcResult<MutateResponse>> + Send {
        if let Err(err) = self.verify_call_auth_metadata(&scope.metadata, request.ctx.as_ref()) {
            return ready(Err(err));
        }
        let Some(op) = self.config.operations.get(&request.operation) else {
            return ready(Err(NgcErrorInfo::new(
                NgcErrorCode::NotFound,
                format!("unknown CLI mutate operation `{}`", request.operation),
            )));
        };
        if request.dry_run && !op.supports_dry_run {
            return ready(Err(NgcErrorInfo::new(
                NgcErrorCode::InvalidArgument,
                format!("operation `{}` does not support dry_run", request.operation),
            )));
        }

        let req_json = match to_json_string(&request) {
            Ok(s) => s,
            Err(err) => return ready(Err(err)),
        };
        let mut args_map = match JsonValue::parse(&req_json) {
            Ok(JsonValue::Object(mut root)) => match root.remove("arguments") {
                Some(JsonValue::Object(map)) => map,
                _ => BTreeMap::new(),
            },
            _ => BTreeMap::new(),
        };
        if request.dry_run {
            args_map.insert("dry_run".to_owned(), JsonValue::Bool(true));
        }

        let remaining = scope
            .deadline
            .into_std()
            .checked_duration_since(Instant::now());
        let res = execute_cli_spec(&self.config.launcher, op, &args_map, remaining).and_then(
            |json_out| {
                let mut wrapper = BTreeMap::new();
                let result_obj = match json_out {
                    JsonValue::Object(map) => JsonValue::Object(map),
                    other => {
                        let mut wrap = BTreeMap::new();
                        wrap.insert("value".to_owned(), other);
                        JsonValue::Object(wrap)
                    }
                };
                wrapper.insert("result".to_owned(), result_obj);
                let mut resp: MutateResponse =
                    from_json_str(&JsonValue::Object(wrapper).to_jcs_string())?;
                resp.outcome = if request.dry_run {
                    MutationOutcome::OutcomeDryRun as i32
                } else {
                    MutationOutcome::OutcomeApplied as i32
                };
                Ok(resp)
            },
        );
        ready(res)
    }

    fn health(
        &self,
        _request: HealthRequest,
        _scope: CallScope,
    ) -> impl Future<Output = NgcResult<HealthResponse>> + Send {
        let report = gap_sec::sandbox::doctor_check();
        let all_allowlisted = self.config.operations.values().all(|op| {
            self.config
                .launcher
                .allow_list()
                .verify_before_launch(&op.binary_path)
                .is_ok()
        });
        let status = if all_allowlisted && report.binary_present {
            ServingStatus::Serving
        } else if all_allowlisted {
            ServingStatus::Degraded
        } else {
            ServingStatus::NotServing
        };
        ready(Ok(HealthResponse {
            status: status as i32,
            version: NGC_WIRE_VERSION.to_owned(),
            details: None,
        }))
    }
}

fn execute_cli_spec(
    launcher: &SandboxLauncher,
    op: &CliOperationSpec,
    arguments: &BTreeMap<String, JsonValue>,
    max_timeout: Option<Duration>,
) -> NgcResult<JsonValue> {
    let argv = op.template.render(arguments)?;
    let mut profile = op.sandbox_profile.clone();
    if let Some(deadline_budget) = max_timeout {
        profile.timeout = profile.timeout.min(deadline_budget);
    }
    let out = launcher
        .launch(&op.binary_path, &argv, &profile, &op.env_source)
        .map_err(map_sandbox_error)?;

    if out.status_code != 0 {
        let stderr_text = String::from_utf8_lossy(&out.stderr);
        let excerpt: String = stderr_text.chars().take(200).collect();
        return Err(NgcErrorInfo::new(
            NgcErrorCode::Unavailable,
            format!(
                "CLI operation `{}` exited {}: {excerpt}",
                op.name, out.status_code
            ),
        ));
    }

    let raw_stdout = std::str::from_utf8(&out.stdout).map_err(|_| {
        NgcErrorInfo::new(
            NgcErrorCode::Internal,
            "CLI process returned non-UTF-8 stdout",
        )
    })?;
    if raw_stdout.trim().is_empty() {
        return Ok(JsonValue::Array(Vec::new()));
    }
    JsonValue::parse(raw_stdout.trim()).map_err(|err| {
        NgcErrorInfo::new(
            NgcErrorCode::Internal,
            format!("CLI stdout was not valid JSON: {err}"),
        )
    })
}

fn parse_rows_chunk(json_out: &JsonValue) -> NgcResult<RowSetChunk> {
    let rows_val = match json_out {
        JsonValue::Array(items) => JsonValue::Array(items.clone()),
        JsonValue::Object(map) => JsonValue::Array(vec![JsonValue::Object(map.clone())]),
        other => {
            let mut row = BTreeMap::new();
            row.insert("value".to_owned(), other.clone());
            JsonValue::Array(vec![JsonValue::Object(row)])
        }
    };
    let mut root = BTreeMap::new();
    root.insert("rows".to_owned(), rows_val);
    from_json_str::<RowSetChunk>(&JsonValue::Object(root).to_jcs_string())
}

fn map_sandbox_error(err: SandboxError) -> NgcErrorInfo {
    match err {
        SandboxError::AllowList(e) => {
            NgcErrorInfo::new(NgcErrorCode::PermissionDenied, e.to_string())
        }
        SandboxError::UnavailableFailClosed { .. } => {
            NgcErrorInfo::new(NgcErrorCode::PermissionDenied, err.to_string())
        }
        SandboxError::TimedOut { .. } => {
            NgcErrorInfo::new(NgcErrorCode::DeadlineExceeded, err.to_string())
        }
        SandboxError::OutputLimitExceeded { .. } => {
            NgcErrorInfo::new(NgcErrorCode::ResourceExhausted, err.to_string())
        }
        SandboxError::WorkDirInvalid(_)
        | SandboxError::SpawnFailed(_)
        | SandboxError::NonZeroExit { .. } => {
            NgcErrorInfo::new(NgcErrorCode::Unavailable, err.to_string())
        }
    }
}
