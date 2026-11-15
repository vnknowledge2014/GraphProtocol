//! Allow-listed, sandboxed CLI process `NGC` connector (`CMP-060`, `SEC-061`, `SEC-070`, `NGC-082`, `WP-M6-09`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod connector;
pub mod template;

#[cfg(test)]
mod tests;

pub use connector::{CliConnector, CliConnectorConfig, CliGatewayAuth, CliOperationSpec};
pub use template::{ArgConstraint, ArgvTemplate, TemplateToken};
