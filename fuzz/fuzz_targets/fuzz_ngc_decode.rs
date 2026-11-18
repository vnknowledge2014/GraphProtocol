//! Fuzz target for NGC Protobuf wire decoders (`WP-M1-03`, `WP-M7-06`, `NGC-010`).
#![no_main]
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use gap_proto::ngc::v1::{
    DescribeResponse, MutateRequest, MutateResponse, NgcError, QueryRequest, QueryResponse,
    TaskStatus,
};
use libfuzzer_sys::fuzz_target;
use prost::Message as _;

const MAX_FRAME_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_FRAME_BYTES {
        return;
    }
    let _ = DescribeResponse::decode(data);
    let _ = QueryRequest::decode(data);
    let _ = QueryResponse::decode(data);
    let _ = MutateRequest::decode(data);
    let _ = MutateResponse::decode(data);
    let _ = TaskStatus::decode(data);
    let _ = NgcError::decode(data);
});
