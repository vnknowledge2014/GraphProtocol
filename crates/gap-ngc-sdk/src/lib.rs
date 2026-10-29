//! Native Graph Connector (`NGC`) SDK, traits, and in-process/gRPC/HTTP/stdio connector utilities (`L2`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod connector;
pub mod core;
pub mod error;
pub mod foreach;
pub mod grpc;
pub mod http;
pub mod json;
pub mod mock;
pub mod stdio;
pub mod testing;
