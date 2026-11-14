//! Upstream GraphQL endpoint `NativeGraphConnector` adapter (`L4`).
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
pub mod rewrite;
pub mod schema;
pub mod transport;

pub use config::{
    EntityBatchMapping, GraphqlConfig, GraphqlCredential, MutationSideEffectOverride,
    UpstreamSchemaSource, resolve_auth_headers, validate_endpoint,
};
pub use connector::GraphqlConnector;
pub use rewrite::{
    PreparedGraphqlCall, build_aliased_batch_call, build_entities_batch_call, build_single_call,
    rewrite_executable_document_types,
};
pub use schema::{
    ExtractedGraphqlOp, GraphqlArgDef, GraphqlSchemaBundle, INTROSPECTION_QUERY,
    RelayConnectionInfo, build_bundle_from_sdl, extract_graphql_schema, introspection_to_sdl,
};
pub use transport::{
    GraphqlHttpRequest, GraphqlTransport, GraphqlWireResponse, ReqwestGraphqlTransport,
    interpret_wire_response,
};
