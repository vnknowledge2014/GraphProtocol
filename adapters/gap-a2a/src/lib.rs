//! Inbound `A2A v1.0` binding (`HttpModule`) and outbound peer-agent `NGC` connector (`L4`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod card;
pub mod inbound;
pub mod outbound;
pub mod wire;

pub use card::{
    AgentCapabilities, AgentCard, AgentCardConfig, AgentCardSignature, AgentExtension,
    AgentInterface, AgentProtocolBinding, AgentSkill, build_agent_card, sign_agent_card,
    verify_agent_card_signatures,
};
pub use inbound::{
    A2aFacade, A2aFacadeConfig, A2aTaskBackend, InMemoryA2aBackend, InboundSendRequest,
};
pub use outbound::{
    A2aOutboundEnvelope, A2aPeerConfig, A2aPeerConnector, A2aPeerTransport,
    DEFAULT_POLL_INITIAL_MS, DEFAULT_POLL_MAX_MS, SCOPE_HOP_HEADER, SCOPE_TRACE_PATH_HEADER,
};
pub use wire::{
    A2A_CONTENT_TYPE, A2A_EXTENSIONS_HEADER, A2A_PROTOCOL_VERSION, A2A_VERSION_HEADER, A2aArtifact,
    A2aMessage, A2aPart, A2aRole, A2aStreamEvent, A2aTask, A2aTaskState, A2aTaskStatus,
    DEFAULT_A2A_MAX_HOPS, GAP_TRACE_EXT_URI, TraceExtension, encode_http_sse_stream,
    encode_jsonrpc_sse_stream,
};

#[cfg(test)]
mod tests;
