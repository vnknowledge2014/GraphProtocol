//! Immutable supergraph snapshot builder, `ArcSwap` publisher, off-thread reclaimer, and attribution-aware auto-rollback (`L3`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod canonical;
pub mod compose;
pub mod diff;
pub mod groups;
pub mod incremental;
pub mod publisher;
pub mod rollback;

pub use canonical::{
    CanonicalError, canonical_json, canonical_number, canonical_string, cmp_utf16_code_units,
};
pub use compose::{BranchInput, Composition, CompositionError, CompositionWarning, compose};
