//! GAP-QL strict GraphQL profile parser, validator, directives, and static complexity analyzer (`L1`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod cost;
pub mod limits;
pub mod schema;
pub mod suggest;
pub mod validate;
pub mod visibility;
