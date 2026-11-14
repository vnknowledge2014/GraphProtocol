//! Parameterized PostgreSQL and SQLite `NativeGraphConnector` adapter (`L4`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod config;
pub mod connector;
pub mod cursor;
pub mod driver;
pub mod sketch;
pub mod template;

pub use config::{ColumnDef, Dialect, OperationConfig, OperationKind, SqlConfig, SqlType};
pub use connector::SqlConnector;
pub use driver::{Driver, DriverError, SqlParam, SqlRow, SqlValue, SqliteDriver};
