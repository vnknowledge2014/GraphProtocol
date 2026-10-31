//! Per-branch `ConnectorLayer` v0 (`NGC-020`, `NGC-025`, `NGC-071`, `INV-02`, `INV-12`):
//! object-safe [`BranchClient`], [`InProcess`] server-core adapter, and lock-free
//! [`ConnectorRegistry`] with non-queueing per-branch bulkheads.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use gap_core::error::{ErrorCode, GapError, GapResult, NgcErrorCode, NgcErrorInfo};
use gap_core::ids::BranchUid;
use gap_core::snapshot::{BranchDescriptor, Snapshot};
use gap_ngc_sdk::connector::{
    BoxStream, CallScope, Connector, NgcResult, is_sensitive_metadata_key,
};
use gap_ngc_sdk::core::{ServerConfig, ServerCore};
use gap_ngc_sdk::grpc::GrpcClient;
use gap_proto::ngc::v1::{QueryRequest, QueryResponse};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Object-safe NGC branch client trait (`NGC-020`, `NGC-025`, `Brief §3.1`).
///
/// Boxes the returned future because `gap_ngc_sdk::connector::Connector` uses RPITIT and is not
/// `dyn`-compatible.
pub trait BranchClient: Send + Sync + 'static {
    /// Opens a streaming `Query` RPC against this branch under `scope` (`deadline`, `cancel`, `metadata`).
    fn query(
        &self,
        request: QueryRequest,
        scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send + '_>>;
}

impl BranchClient for GrpcClient {
    fn query(
        &self,
        _request: QueryRequest,
        _scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send + '_>> {
        Box::pin(async move {
            Err(NgcErrorInfo::new(
                NgcErrorCode::Unimplemented,
                "GrpcClient::query stub",
            ))
        })
    }
}

/// In-process [`BranchClient`] adapter over [`ServerCore<C>`] for socket-free unit and integration tests (`Brief §3.1`).
pub struct InProcess<C> {
    core: Arc<ServerCore<C>>,
}

impl<C> Clone for InProcess<C> {
    fn clone(&self) -> Self {
        Self {
            core: Arc::clone(&self.core),
        }
    }
}

impl<C> fmt::Debug for InProcess<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InProcess").finish_non_exhaustive()
    }
}

impl<C: Connector> InProcess<C> {
    /// Wraps an existing [`Arc<ServerCore<C>>`].
    #[must_use]
    pub const fn new(core: Arc<ServerCore<C>>) -> Self {
        Self { core }
    }

    /// Wraps `connector` in a fresh [`ServerCore`] with `config`.
    #[must_use]
    pub fn from_connector(connector: C, config: ServerConfig) -> Self {
        Self {
            core: Arc::new(ServerCore::new(connector, config)),
        }
    }

    /// Borrows the underlying [`ServerCore`].
    #[must_use]
    pub const fn core(&self) -> &Arc<ServerCore<C>> {
        &self.core
    }
}

impl<C: Connector> BranchClient for InProcess<C> {
    fn query(
        &self,
        _request: QueryRequest,
        _scope: CallScope,
    ) -> Pin<Box<dyn Future<Output = NgcResult<BoxStream<QueryResponse>>> + Send + '_>> {
        Box::pin(async move {
            Err(NgcErrorInfo::new(
                NgcErrorCode::Unimplemented,
                "InProcess::query stub",
            ))
        })
    }
}

struct RedactedMeta<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for RedactedMeta<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (k, v) in self.0 {
            if is_sensitive_metadata_key(k) {
                map.entry(k, &"***");
            } else {
                map.entry(k, v);
            }
        }
        map.finish()
    }
}

/// Immutable per-branch client and non-queueing bulkhead entry inside [`ConnectorRegistry`] (`INV-02`, `NGC-071`).
#[derive(Clone)]
pub struct RegisteredBranch {
    client: Arc<dyn BranchClient>,
    bulkhead: Arc<Semaphore>,
    max_in_flight: u32,
    metadata: BTreeMap<String, String>,
}

impl fmt::Debug for RegisteredBranch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredBranch")
            .field("max_in_flight", &self.max_in_flight)
            .field("available_permits", &self.bulkhead.available_permits())
            .field("metadata", &RedactedMeta(&self.metadata))
            .finish()
    }
}

impl RegisteredBranch {
    /// Creates a [`RegisteredBranch`] with `client` and a non-queueing bulkhead of `max_in_flight` permits.
    #[must_use]
    pub fn new(client: Arc<dyn BranchClient>, max_in_flight: u32) -> Self {
        Self::with_metadata(client, max_in_flight, BTreeMap::new())
    }

    /// Creates a [`RegisteredBranch`] with static per-branch call metadata (e.g. vault-resolved connector credentials; redacted in `Debug` and error sanitisation, `INV-15`).
    #[must_use]
    pub fn with_metadata(
        client: Arc<dyn BranchClient>,
        max_in_flight: u32,
        metadata: BTreeMap<String, String>,
    ) -> Self {
        let permits = usize::try_from(max_in_flight).unwrap_or(usize::MAX);
        Self {
            client,
            bulkhead: Arc::new(Semaphore::new(permits)),
            max_in_flight,
            metadata,
        }
    }

    /// Multiplexed client handle for this branch.
    #[must_use]
    pub fn client(&self) -> &Arc<dyn BranchClient> {
        &self.client
    }

    /// Configured `max_in_flight` bulkhead capacity (`NGC-071`).
    #[must_use]
    pub const fn max_in_flight(&self) -> u32 {
        self.max_in_flight
    }

    /// Currently available bulkhead permits.
    #[must_use]
    pub fn available_permits(&self) -> usize {
        self.bulkhead.available_permits()
    }

    /// Static outbound call metadata attached to calls targeting this branch.
    #[must_use]
    pub const fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }
}

/// Builder for an immutable [`ConnectorRegistry`] (`INV-02`).
#[derive(Debug, Default)]
pub struct ConnectorRegistryBuilder {
    entries: BTreeMap<BranchUid, RegisteredBranch>,
}

impl ConnectorRegistryBuilder {
    /// Registers `client` for `uid` with bulkhead capacity `max_in_flight`.
    #[must_use]
    pub fn register(
        mut self,
        uid: BranchUid,
        client: Arc<dyn BranchClient>,
        max_in_flight: u32,
    ) -> Self {
        self.entries
            .insert(uid, RegisteredBranch::new(client, max_in_flight));
        self
    }

    /// Registers `client` for `uid` with bulkhead capacity `max_in_flight` and static call `metadata`.
    #[must_use]
    pub fn register_with_metadata(
        mut self,
        uid: BranchUid,
        client: Arc<dyn BranchClient>,
        max_in_flight: u32,
        metadata: BTreeMap<String, String>,
    ) -> Self {
        self.entries.insert(
            uid,
            RegisteredBranch::with_metadata(client, max_in_flight, metadata),
        );
        self
    }

    /// Registers `client` for `branch`, sizing its bulkhead to `branch.limits.max_in_flight`.
    #[must_use]
    pub fn register_branch(
        self,
        branch: &BranchDescriptor,
        client: Arc<dyn BranchClient>,
    ) -> Self {
        self.register(branch.uid, client, branch.limits.max_in_flight)
    }

    /// Finalizes the immutable [`ConnectorRegistry`].
    #[must_use]
    pub fn build(self) -> ConnectorRegistry {
        ConnectorRegistry::new(self.entries)
    }
}

/// Immutable per-branch client registry (`INV-02`: `BTreeMap<BranchUid, RegisteredBranch>`, zero locks on read path).
///
/// # Connection Pooling (`v0`)
///
/// In `WP-M1-07`, pooling is one multiplexed [`BranchClient`] per [`BranchUid`] (for example, an
/// HTTP/2-multiplexed `tonic` [`GrpcClient`]), bounded by a non-queueing `tokio::sync::Semaphore`
/// sized to `BranchDescriptor.limits.max_in_flight` and acquired with `try_acquire_owned`.
#[derive(Debug, Clone, Default)]
pub struct ConnectorRegistry {
    entries: BTreeMap<BranchUid, RegisteredBranch>,
}

impl ConnectorRegistry {
    /// Constructs an empty [`ConnectorRegistry`].
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Wraps a pre-built map of [`RegisteredBranch`] entries.
    #[must_use]
    pub const fn new(entries: BTreeMap<BranchUid, RegisteredBranch>) -> Self {
        Self { entries }
    }

    /// Returns a [`ConnectorRegistryBuilder`].
    #[must_use]
    pub fn builder() -> ConnectorRegistryBuilder {
        ConnectorRegistryBuilder::default()
    }

    /// Builds a [`ConnectorRegistry`] by pairing `clients` (`BTreeMap<BranchUid, Arc<dyn BranchClient>>`)
    /// with `BranchDescriptor.limits.max_in_flight` from `snapshot` (`Brief §3.1`).
    ///
    /// Clients whose [`BranchUid`] is absent from `snapshot.branches` fall back to
    /// `BranchLimits::default().max_in_flight`.
    #[must_use]
    pub fn from_snapshot(
        snapshot: &Snapshot,
        clients: BTreeMap<BranchUid, Arc<dyn BranchClient>>,
    ) -> Self {
        let mut entries = BTreeMap::new();
        for (uid, client) in clients {
            let max_in_flight = snapshot.branches.get(&uid).map_or_else(
                || gap_core::snapshot::BranchLimits::default().max_in_flight,
                |b| b.limits.max_in_flight,
            );
            entries.insert(uid, RegisteredBranch::new(client, max_in_flight));
        }
        Self { entries }
    }

    /// Looks up a [`RegisteredBranch`] by [`BranchUid`] without acquiring any lock (`INV-02`).
    #[must_use]
    pub fn get(&self, uid: &BranchUid) -> Option<&RegisteredBranch> {
        self.entries.get(uid)
    }

    /// Looks up the multiplexed [`BranchClient`] for `uid` (`INV-02`).
    #[must_use]
    pub fn client(&self, uid: &BranchUid) -> Option<&Arc<dyn BranchClient>> {
        self.entries.get(uid).map(RegisteredBranch::client)
    }

    /// Returns available bulkhead permits for `uid`.
    #[must_use]
    pub fn available_permits(&self, uid: &BranchUid) -> Option<usize> {
        self.entries.get(uid).map(RegisteredBranch::available_permits)
    }

    /// Returns configured `max_in_flight` capacity for `uid`.
    #[must_use]
    pub fn max_in_flight(&self, uid: &BranchUid) -> Option<u32> {
        self.entries.get(uid).map(RegisteredBranch::max_in_flight)
    }

    /// Number of registered branches.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when no branches are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Attempts to acquire a per-branch bulkhead permit without queueing (`Brief §3.1`, `NGC-071`).
    ///
    /// # Errors
    ///
    /// - [`ErrorCode::BranchUnavailable`] if `uid` is not registered.
    /// - [`ErrorCode::Overloaded`] (`retryable: true`) if the branch bulkhead is saturated.
    pub fn try_acquire_branch(
        &self,
        uid: &BranchUid,
    ) -> GapResult<(&RegisteredBranch, OwnedSemaphorePermit)> {
        let _ = uid;
        Err(GapError::new(
            ErrorCode::Internal,
            "try_acquire_branch stub",
        ))
    }
}
