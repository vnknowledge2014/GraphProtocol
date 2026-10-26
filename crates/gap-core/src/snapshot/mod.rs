//! Immutable `Snapshot` and branch/operation descriptor models (`WP-M0-08`, `WP-M1-05`).

/// Immutable supergraph snapshot handle (`spec/01-architecture.md` §4, `ARC-005`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Monotonically increasing snapshot generation (`spec/00-conventions.md` §6).
    pub generation: u64,
}

impl Snapshot {
    /// Constructs an empty bootstrap snapshot with the given `generation`.
    #[must_use]
    pub const fn empty(generation: u64) -> Self {
        Self { generation }
    }
}
