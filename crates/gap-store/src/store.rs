//! Object-safe synchronous [`ContextStore`] and [`ContextMaintenance`] traits (`INV-09`, `INV-19`, `ING-064`..`ING-067`).

use gap_core::clock::Timestamp;
use gap_core::ids::{TaskId, TenantScope};

use crate::error::StoreError;
use crate::types::{
    BatchResult, Caller, Edge, EdgeId, GraphCounts, NewEdge, NewNode, Node, NodeId, PurgePolicy,
    PurgeReport, TraverseQuery, TraverseResult,
};

/// Tenant-scoped temporal context graph store (`ADR-020`, `ADR-025`, `INV-09`, `INV-19`).
///
/// Every method requires [`TenantScope`] as its first argument; there is no tenant-unscoped
/// read or write entry point (`INV-09`). All operations are synchronous (`m2-01-design.md` §1);
/// async callers invoke them through a bounded blocking pool (`spawn_blocking`).
pub trait ContextStore: Send + Sync {
    /// Inserts or updates a single node in `tenant`, returning its [`NodeId`].
    ///
    /// Single-item wrapper over [`Self::put_batch`].
    fn put_node(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        node: NewNode,
    ) -> Result<NodeId, StoreError> {
        let mut out = self.put_batch(tenant, writer, std::slice::from_ref(&node), &[])?;
        out.node_ids.pop().ok_or(StoreError::Backend)
    }

    /// Inserts or updates a single directed edge in `tenant`, returning its [`EdgeId`].
    ///
    /// Single-item wrapper over [`Self::put_batch`].
    fn put_edge(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        edge: NewEdge,
    ) -> Result<EdgeId, StoreError> {
        let mut out = self.put_batch(tenant, writer, &[], std::slice::from_ref(&edge))?;
        out.edge_ids.pop().ok_or(StoreError::Backend)
    }

    /// Atomically applies `nodes` and then `edges` in a single transaction (`<= 10,000` items total).
    ///
    /// If any item fails validation, visibility checks, or quota limits, nothing is persisted.
    fn put_batch(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        nodes: &[NewNode],
        edges: &[NewEdge],
    ) -> Result<BatchResult, StoreError>;

    /// Fetches a node visible to `caller` at `as_of` (`None` uses the injected clock's `now`).
    ///
    /// Invisible and non-existent nodes both return `Ok(None)` (`ING-065`).
    fn get_node(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        id: &NodeId,
        as_of: Option<Timestamp>,
    ) -> Result<Option<Node>, StoreError>;

    /// Fetches an edge visible to `caller` (both endpoints visible) at `as_of`.
    ///
    /// Invisible and non-existent edges both return `Ok(None)` (`ING-065`).
    fn get_edge(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        id: &EdgeId,
        as_of: Option<Timestamp>,
    ) -> Result<Option<Edge>, StoreError>;

    /// Closes (`valid_to = now`) a current node visible to `writer` and all of its current
    /// incident edges in a single transaction (`ADR-020`, `ING-062`).
    ///
    /// Returns [`StoreError::NotFound`] identically for missing and invisible nodes (`ING-065`).
    fn tombstone_node(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        id: &NodeId,
    ) -> Result<(), StoreError>;

    /// Hard-deletes all versions of a node visible to `writer` and all incident edges (`ING-066`).
    ///
    /// Returns [`StoreError::NotFound`] identically for missing and invisible nodes (`ING-065`).
    fn delete_node(
        &self,
        tenant: &TenantScope,
        writer: &Caller,
        id: &NodeId,
    ) -> Result<(), StoreError>;

    /// Counts visible nodes and edges (both endpoints visible) in `tenant` at `as_of`.
    fn counts(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        as_of: Option<Timestamp>,
    ) -> Result<GraphCounts, StoreError>;

    /// Executes a bounded graph walk (`BFS`, `DFS`, `SHORTEST`, or `WEIGHTED`) over visible nodes
    /// and edges (`ING-064`, `ING-067`).
    fn traverse(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        query: &TraverseQuery,
    ) -> Result<TraverseResult, StoreError>;

    /// Searches visible nodes in `tenant` at `as_of` over `label` and `summary`, ranked by BM25
    /// with deterministic tie-breaking by `NodeId` (`limit` in `1..=100`).
    fn search_nodes(
        &self,
        tenant: &TenantScope,
        caller: &Caller,
        query: &str,
        limit: u32,
        as_of: Option<Timestamp>,
    ) -> Result<Vec<Node>, StoreError>;
}

/// Lifecycle maintenance operations driven by the gateway / task engine (`ING-066`).
pub trait ContextMaintenance: Send + Sync {
    /// Closes (`valid_to = now`) all current `Task`-scoped (and `Ephemeral(Task)`) nodes and their
    /// current incident edges belonging to `task_id` within `tenant`, returning the number of
    /// tombstoned nodes (`ING-066`).
    fn tombstone_task_nodes(
        &self,
        tenant: &TenantScope,
        task_id: TaskId,
    ) -> Result<u64, StoreError>;

    /// Hard-deletes expired `Ephemeral` nodes and `Task` nodes tombstoned longer ago than
    /// `policy.task_retention` across all tenants, returning aggregate counts only (`ING-066`, `INV-09`).
    fn purge_expired(&self, policy: &PurgePolicy) -> Result<PurgeReport, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::{ContextMaintenance, ContextStore};

    // spec: INV-09, INV-19
    #[test]
    fn inv_09_context_store_and_maintenance_traits_are_object_safe() {
        fn _assert_dyn_store(_s: &dyn ContextStore) {}
        fn _assert_dyn_maint(_m: &dyn ContextMaintenance) {}
    }
}
