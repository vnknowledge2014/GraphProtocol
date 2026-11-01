//! Configuration loading (`ServerFile`), deterministic branch UID derivation, and gateway assembly
//! (`ARC-002`, `ARC-007`, `OPS-001`, `OPS-003`, `INV-01`, `INV-15`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gap_core::config::{BranchProtocol, BranchSpec, ConfigError, GapConfig, SecretRef};
use gap_core::error::ErrorCode;
use gap_core::ids::{BranchId, BranchUid};
use gap_core::secret::SecretString;
use gap_core::traits::BoxFuture;
use gap_exec::BranchClient;
use gap_mcp::inbound::{FacadeConfigError, McpFacade};
use gap_ngc_sdk::connector::Connector;
use gap_ngc_sdk::core::{ServerConfig as SdkServerConfig, ServerCore};
use gap_ngc_sdk::testing::in_memory_grpc;
use gap_proto::ngc::v1::{ConnectorDescription, DescribeRequest};
use gap_snapshot::{CompositionWarning, PublishError, SnapshotHandle};
use gap_transport::{AppState, ConfigValidationError, TransportEngine};
use thiserror::Error;

/// Opaque guard keeping background connector tasks or child processes alive for the lifetime of [`Assembled`].
pub type BranchKeepAlive = Box<dyn Send + Sync + 'static>;

/// Startup failure while reading or validating `gap.toml` and its referenced local files (`INV-15`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ServerConfigError {
    /// Core `gap.toml` parse, environment override, or schema range validation failed.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A file referenced by a branch entry (such as `branches[i].openapi.spec`) is missing or unreadable.
    #[error("invalid branch file at `{field_path}` (`{resolved_path}`): {reason}")]
    BranchFile {
        /// Configuration key path naming the offending branch field (e.g. `branches[0].openapi.spec`).
        field_path: String,
        /// Resolved filesystem path relative to the `gap.toml` directory.
        resolved_path: String,
        /// Sanitized I/O failure description (never contains file contents or secrets).
        reason: String,
    },
    /// Two branches resolved to the same [`BranchUid`].
    #[error("duplicate resolved branch uid `{uid}` at `{field_path}`")]
    DuplicateBranchUid {
        /// Offending branch key path.
        field_path: String,
        /// Colliding branch UUIDv7.
        uid: BranchUid,
    },
}

/// Validated `gap.toml` paired with its base directory and pre-read OpenAPI specification bytes.
#[derive(Clone)]
pub struct ServerFile {
    config: GapConfig,
    config_path: PathBuf,
    base_dir: PathBuf,
    openapi_documents: BTreeMap<BranchId, Vec<u8>>,
    env_overrides: Option<BTreeMap<String, String>>,
}

impl fmt::Debug for ServerFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerFile")
            .field("config_path", &self.config_path)
            .field("base_dir", &self.base_dir)
            .field("profile", &self.config.profile)
            .field("branches", &self.config.branches.len())
            .field("openapi_documents", &self.openapi_documents.len())
            .finish_non_exhaustive()
    }
}

impl ServerFile {
    /// Loads `gap.toml` from `path` with process `GAP_*` environment overrides and verifies all
    /// referenced branch files exist and are readable.
    ///
    /// # Errors
    ///
    /// Returns [`ServerConfigError`] if `gap.toml` cannot be read/validated or any referenced OpenAPI
    /// document is missing or unreadable.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ServerConfigError> {
        let path_ref = path.as_ref();
        let config = GapConfig::load_from_path_with_process_env(path_ref)?;
        Self::from_loaded(path_ref, config, None)
    }

    /// Loads `gap.toml` from `path` using an explicit `env_vars` iterator for both `GAP_*` scalar
    /// overrides and `env://` [`SecretRef`] resolution (used for deterministic tests without mutating
    /// process environment variables).
    ///
    /// # Errors
    ///
    /// Returns [`ServerConfigError`] if loading, overriding, or branch-file verification fails.
    pub fn load_with_env<I, K, V>(
        path: impl AsRef<Path>,
        env_vars: I,
    ) -> Result<Self, ServerConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let path_ref = path.as_ref();
        let env_map: BTreeMap<String, String> = env_vars
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let raw_toml = fs::read_to_string(path_ref).map_err(|err| ConfigError::Io {
            path: path_ref.display().to_string(),
            reason: err.kind().to_string(),
        })?;
        let config = GapConfig::from_toml_and_env(
            &raw_toml,
            env_map.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        )?;
        Self::from_loaded(path_ref, config, Some(env_map))
    }

    fn from_loaded(
        path_ref: &Path,
        config: GapConfig,
        env_overrides: Option<BTreeMap<String, String>>,
    ) -> Result<Self, ServerConfigError> {
        let config_path = path_ref.to_path_buf();
        let base_dir = match path_ref.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };

        let mut seen_uids = BTreeSet::new();
        let mut openapi_documents = BTreeMap::new();

        for (idx, branch) in config.branches.iter().enumerate() {
            let uid = branch.uid.unwrap_or_else(|| derive_branch_uid(&branch.id));
            if !seen_uids.insert(uid) {
                return Err(ServerConfigError::DuplicateBranchUid {
                    field_path: format!("branches[{idx}].uid"),
                    uid,
                });
            }

            if branch.protocol == BranchProtocol::Openapi {
                let field_path = format!("branches[{idx}].openapi.spec");
                let Some(openapi) = branch.openapi.as_ref() else {
                    return Err(ServerConfigError::Config(ConfigError::InvalidEntry {
                        path: format!("branches[{idx}].openapi"),
                        reason: "missing `[branches.openapi]` table".to_owned(),
                    }));
                };
                let spec_ref = Path::new(&openapi.spec);
                let resolved = if spec_ref.is_absolute() {
                    spec_ref.to_path_buf()
                } else {
                    base_dir.join(spec_ref)
                };
                let resolved_path = resolved.display().to_string();

                let metadata =
                    fs::metadata(&resolved).map_err(|err| ServerConfigError::BranchFile {
                        field_path: field_path.clone(),
                        resolved_path: resolved_path.clone(),
                        reason: err.kind().to_string(),
                    })?;
                if !metadata.is_file() {
                    return Err(ServerConfigError::BranchFile {
                        field_path,
                        resolved_path,
                        reason: "path is not a regular file".to_owned(),
                    });
                }
                let bytes = fs::read(&resolved).map_err(|err| ServerConfigError::BranchFile {
                    field_path,
                    resolved_path,
                    reason: err.kind().to_string(),
                })?;
                openapi_documents.insert(branch.id.clone(), bytes);
            }
        }

        Ok(Self {
            config,
            config_path,
            base_dir,
            openapi_documents,
            env_overrides,
        })
    }

    /// Borrows the validated [`GapConfig`].
    #[must_use]
    pub const fn config(&self) -> &GapConfig {
        &self.config
    }

    /// Path to the loaded `gap.toml` file.
    #[must_use]
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Directory against which relative branch file paths are resolved.
    #[must_use]
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Pre-validated OpenAPI specification document bytes for `branch_id`, if it is an OpenAPI branch.
    #[must_use]
    pub fn openapi_document(&self, branch_id: &BranchId) -> Option<&[u8]> {
        self.openapi_documents.get(branch_id).map(Vec::as_slice)
    }

    /// Resolves a [`SecretRef`] into a redacted [`SecretString`] without ever echoing secret material
    /// in errors (`INV-15`).
    ///
    /// # Errors
    ///
    /// Returns [`BranchBuildError::SecretResolution`] when the referenced variable/file is missing,
    /// unreadable, empty, or uses an unsupported vault provider.
    pub fn resolve_secret(
        &self,
        secret_ref: &SecretRef,
        field_path: &str,
    ) -> Result<SecretString, BranchBuildError> {
        let reference = secret_ref.to_string();
        match secret_ref {
            SecretRef::Env { var } => {
                let raw = match &self.env_overrides {
                    Some(overrides) => overrides.get(var).cloned(),
                    None => std::env::var(var).ok(),
                };
                let Some(val) = raw else {
                    return Err(BranchBuildError::SecretResolution {
                        field_path: field_path.to_owned(),
                        reference,
                        reason: "environment variable is not set".to_owned(),
                    });
                };
                if val.is_empty() {
                    return Err(BranchBuildError::SecretResolution {
                        field_path: field_path.to_owned(),
                        reference,
                        reason: "environment variable is empty".to_owned(),
                    });
                }
                Ok(SecretString::new(val))
            }
            SecretRef::File { path } => {
                let metadata =
                    fs::metadata(path).map_err(|err| BranchBuildError::SecretResolution {
                        field_path: field_path.to_owned(),
                        reference: reference.clone(),
                        reason: err.kind().to_string(),
                    })?;
                if !metadata.is_file() {
                    return Err(BranchBuildError::SecretResolution {
                        field_path: field_path.to_owned(),
                        reference,
                        reason: "secret path is not a regular file".to_owned(),
                    });
                }
                let raw =
                    fs::read_to_string(path).map_err(|err| BranchBuildError::SecretResolution {
                        field_path: field_path.to_owned(),
                        reference: reference.clone(),
                        reason: err.kind().to_string(),
                    })?;
                let trimmed = raw.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    return Err(BranchBuildError::SecretResolution {
                        field_path: field_path.to_owned(),
                        reference,
                        reason: "secret file is empty".to_owned(),
                    });
                }
                Ok(SecretString::new(trimmed.to_owned()))
            }
            SecretRef::Vault { .. } => Err(BranchBuildError::SecretResolution {
                field_path: field_path.to_owned(),
                reference,
                reason: "vault secret providers are not available in M1 (WP-M6-07)".to_owned(),
            }),
        }
    }
}

/// Derives a deterministic RFC 9562 UUIDv7 [`BranchUid`] from `id` via domain-separated `blake3` (`Brief §3.1`).
#[must_use]
pub fn derive_branch_uid(id: &BranchId) -> BranchUid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gap.branch-uid.v1:");
    hasher.update(id.as_str().as_bytes());
    let digest = *hasher.finalize().as_bytes();

    let mut ts_bytes = [0_u8; 8];
    ts_bytes[2..8].copy_from_slice(&digest[0..6]);
    let unix_ms = u64::from_be_bytes(ts_bytes);

    let mut rand_bytes = [0_u8; 10];
    rand_bytes.copy_from_slice(&digest[6..16]);

    BranchUid::from_unix_ms(unix_ms, rand_bytes)
}

/// Failure building or describing a single branch at startup (`Brief §3.2`, `INV-15`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BranchBuildError {
    /// Resolving a [`SecretRef`] failed (`INV-15`: `reference` is the URI scheme/name, never the value).
    #[error("failed to resolve secret `{reference}` for `{field_path}`: {reason}")]
    SecretResolution {
        /// Config path (e.g. `branches[0].openapi.auth.secret`).
        field_path: String,
        /// Redacted secret URI (`env://VAR` / `file:///path` / `secret://...`).
        reference: String,
        /// Sanitized reason.
        reason: String,
    },
    /// Connector construction failed before `Describe`.
    #[error("failed to build connector for branch `{branch_id}`: {reason}")]
    ConnectorInit {
        /// Offending branch slug.
        branch_id: BranchId,
        /// Sanitized diagnostic message.
        reason: String,
    },
    /// Startup `Describe` RPC failed or returned an empty description.
    #[error("startup Describe failed for branch `{branch_id}`: {reason}")]
    Describe {
        /// Offending branch slug.
        branch_id: BranchId,
        /// Sanitized diagnostic message.
        reason: String,
    },
}

/// Runtime artifacts produced for one branch by a [`BranchFactory`] (`Brief §3.2`).
pub struct BranchRuntime {
    /// Multiplexed NGC branch client registered in [`gap_exec::ConnectorRegistry`].
    pub client: Arc<dyn BranchClient>,
    /// Connector `Describe` payload supplied to `gap_snapshot::compose`.
    pub description: ConnectorDescription,
    /// Opaque keep-alive handle retained by [`Assembled`].
    pub handle: BranchKeepAlive,
}

impl fmt::Debug for BranchRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BranchRuntime")
            .field("connector_id", &self.description.connector_id)
            .field("operations", &self.description.operations.len())
            .finish_non_exhaustive()
    }
}

/// Object-safe asynchronous factory constructing one [`BranchRuntime`] per [`BranchSpec`] (`Brief §3.2`).
pub trait BranchFactory: Send + Sync {
    /// Builds and describes `branch` (`branches[branch_index]`).
    fn build_branch<'a>(
        &'a self,
        server_file: &'a ServerFile,
        branch_index: usize,
        branch: &'a BranchSpec,
        uid: BranchUid,
    ) -> BoxFuture<'a, Result<BranchRuntime, BranchBuildError>>;
}

/// Production [`BranchFactory`] constructing embedded `OpenApiConnector` and `McpConnector` instances
/// over `ServerCore` and `in_memory_grpc` (`OPS-003` (c)).
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultBranchFactory;

impl BranchFactory for DefaultBranchFactory {
    fn build_branch<'a>(
        &'a self,
        _server_file: &'a ServerFile,
        _branch_index: usize,
        branch: &'a BranchSpec,
        _uid: BranchUid,
    ) -> BoxFuture<'a, Result<BranchRuntime, BranchBuildError>> {
        let branch_id = branch.id.clone();
        Box::pin(async move {
            Err(BranchBuildError::ConnectorInit {
                branch_id,
                reason: "not yet implemented".to_owned(),
            })
        })
    }
}

/// Wraps any [`Connector`] in [`ServerCore`] + [`in_memory_grpc`] and executes the startup `Describe` call.
///
/// # Errors
///
/// Returns [`BranchBuildError::Describe`] naming `branch_id` if `Describe` fails or omits `description`.
pub async fn wrap_connector<C: Connector>(
    branch_id: &BranchId,
    connector: C,
) -> Result<BranchRuntime, BranchBuildError> {
    let core = Arc::new(ServerCore::new(connector, SdkServerConfig::default()));
    let (grpc_client, grpc_handle) = in_memory_grpc(core);
    let response = grpc_client
        .describe(DescribeRequest::default())
        .await
        .map_err(|err| BranchBuildError::Describe {
            branch_id: branch_id.clone(),
            reason: err.message().to_owned(),
        })?;
    let description = response
        .description
        .ok_or_else(|| BranchBuildError::Describe {
            branch_id: branch_id.clone(),
            reason: "connector returned empty DescribeResponse".to_owned(),
        })?;
    Ok(BranchRuntime {
        client: Arc::new(grpc_client),
        description,
        handle: Box::new(grpc_handle),
    })
}

/// Startup failure during [`assemble`] (`Brief §3.2`, `INV-15`).
#[derive(Debug, Error)]
pub enum AssembleError {
    /// Configuration or local file validation error.
    #[error(transparent)]
    Config(#[from] ServerConfigError),
    /// Building or describing a branch failed.
    #[error(transparent)]
    Branch(#[from] BranchBuildError),
    /// Supergraph schema composition failed (`RUN-009`, `RUN-011`).
    #[error("supergraph composition failed (branch={branch}, code={code}): {message}")]
    Composition {
        /// Offending branch slug (`"<supergraph>"` when cross-branch).
        branch: String,
        /// Stable GAP [`ErrorCode`].
        code: ErrorCode,
        /// Human-readable diagnostic.
        message: String,
    },
    /// Publishing the initial snapshot failed.
    #[error(transparent)]
    SnapshotPublish(#[from] PublishError),
    /// Deriving [`gap_transport::TransportConfig`] failed validation.
    #[error(transparent)]
    Transport(#[from] ConfigValidationError),
    /// Constructing [`McpFacade`] failed validation.
    #[error(transparent)]
    Facade(#[from] FacadeConfigError),
}

/// Fully wired, ready-to-serve gateway runtime (`Brief §3.2`).
pub struct Assembled {
    /// Shared transport state with `McpFacade` mounted under `/mcp`.
    pub app_state: Arc<AppState>,
    /// Wait-free `ArcSwap` handle pinned per request (`INV-01`).
    pub snapshot_handle: Arc<SnapshotHandle>,
    /// Inbound MCP `2026-07-28` / `2025-11-25` facade.
    pub facade: McpFacade,
    /// In-process `GraphqlEngine` executing `Operation -> Authz -> Plan -> Exec`.
    pub engine: TransportEngine,
    /// Shared atomic drain flag toggled on first shutdown signal (`RUN-090`).
    pub draining: Arc<AtomicBool>,
    /// Validated `GapConfig` used to assemble this runtime.
    pub config: GapConfig,
    /// Non-fatal composition warnings recorded at startup.
    pub warnings: Vec<CompositionWarning>,
    #[allow(dead_code)]
    branch_handles: Vec<BranchKeepAlive>,
}

impl fmt::Debug for Assembled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Assembled")
            .field("app_state", &self.app_state)
            .field("generation", &self.snapshot_handle.generation())
            .field("draining", &self.draining)
            .field("warnings", &self.warnings.len())
            .finish_non_exhaustive()
    }
}

/// Assembles [`Assembled`] from `server_file` using `factory` (`Brief §3.2`).
///
/// # Errors
///
/// Returns [`AssembleError`] if any branch fails to build/describe, supergraph composition fails, or
/// transport/facade validation fails.
pub async fn assemble(
    _server_file: &ServerFile,
    _factory: &dyn BranchFactory,
) -> Result<Assembled, AssembleError> {
    Err(AssembleError::Config(ServerConfigError::Config(
        ConfigError::Parse("not yet implemented".to_owned()),
    )))
}

/// Assembles [`Assembled`] from `server_file` using [`DefaultBranchFactory`].
///
/// # Errors
///
/// Returns [`AssembleError`] on any branch, composition, or configuration error.
pub async fn assemble_default(server_file: &ServerFile) -> Result<Assembled, AssembleError> {
    assemble(server_file, &DefaultBranchFactory).await
}
