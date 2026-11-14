//! Dynamic gRPC / Protobuf `NativeGraphConnector` adapter (`L4`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod codec;
pub mod config;
pub mod connector;
pub mod reflect;
pub mod schema;

pub use config::{
    AdminClassificationPolicy, DeclaredSideEffect, DescriptorSource, GrpcConfig, GrpcCredential,
    TrustTier, attach_credentials, ensure_credential_transport_safe,
};
pub use connector::GrpcConnector;
pub use reflect::{GrpcTransport, LoadedDescriptors, TonicChannelTransport, load_descriptors};
pub use schema::{ExtractedGrpcMethod, GrpcSchemaBundle, extract_grpc_schema};
