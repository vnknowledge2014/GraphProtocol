//! Durable storage traits (`ContextStore`, `TaskStore`, `AuditLog`, `IdempotencyStore`) and backends (`L1`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod audit;
pub mod error;
pub mod feedback;
pub mod idempotency;
pub mod sqlite;
pub mod store;
pub mod surreal;
pub mod tasks;
pub mod types;

pub use audit::{AuditChain, AuditLog};
pub use error::StoreError;
pub use gap_core::clock::{Clock, Timestamp};
pub use gap_core::ids::{TaskId, TenantId, TenantScope};
pub use idempotency::IdempotencyStore;
pub use sqlite::{Durability, FORMAT_VERSION, Migration, SqliteConfig, SqliteContextStore};
pub use store::{ContextMaintenance, ContextStore};
pub use tasks::TaskStore;
pub use types::{
    BaseScope, BatchResult, Caller, Direction, Edge, EdgeId, EdgeResolution, EdgeType, GraphCounts,
    NewEdge, NewNode, Node, NodeId, NodeType, Provenance, PurgePolicy, PurgeReport, Scope,
    ScopeSpec, TraverseQuery, TraverseResume, TraverseResult, TraverseStats, TraverseStrategy,
    TraverseWarning,
};
