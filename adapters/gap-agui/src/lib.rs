//! Inbound AG-UI v1.0 (`ag-ui-protocol/ag-ui`) event-stream and human-in-the-loop
//! interrupt facade mounted by `gap-server` at `/ag-ui` (`L4`, `ADR-027`, `CMP-025`..`CMP-028`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

pub mod compact;
pub mod events;
pub mod facade;
pub mod input;

pub use compact::compact_events;
pub use events::{
    AG_UI_PROTOCOL_VERSION, AgUiEvent, AgUiInterrupt, EventType, RunOutcome, encode_sse_stream,
};
pub use facade::{AgUiConfig, AgUiFacade, MAX_AGUI_REQUEST_BYTES};
pub use input::{AgUiMessage, InterruptResume, RunAgentInput, extract_gapql, extract_user_intent};
