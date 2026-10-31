//! Shared helpers of the outbound integration tests.

pub mod connector;
pub mod real_child;
pub mod real_http;
pub mod wire;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use gap_core::error::NgcErrorInfo;
use gap_mcp::outbound::fake::{FakeMcpServer, FakeTool, ServerEra};
use gap_mcp::outbound::{HttpTransport, McpConfig, McpConnector, McpTransport, StdioTransport};
use gap_ngc_sdk::connector::{BoxStream, CallScope};
use gap_ngc_sdk::error::{json_map_to_prost_struct, prost_struct_to_json_map};
use gap_proto::ngc::v1::{Complete, QueryRequest, QueryResponse, VariableSet, query_response};
use serde_json::{Map, Value, json};
use tokio::time::Instant;

pub fn scope(seconds: u64) -> CallScope {
    CallScope::for_test(Instant::now() + Duration::from_secs(seconds))
}

pub fn args(value: &Value) -> Option<prost_types::Struct> {
    value.as_object().map(json_map_to_prost_struct)
}

pub fn query(op: &str, arguments: Value) -> QueryRequest {
    QueryRequest {
        operation: op.to_owned(),
        arguments: args(&arguments),
        ..QueryRequest::default()
    }
}

pub fn foreach_query(op: &str, sets: &[Value]) -> QueryRequest {
    let foreach = sets
        .iter()
        .enumerate()
        .map(|(i, v)| VariableSet {
            index: u32::try_from(i).unwrap_or(u32::MAX),
            values: args(v),
        })
        .collect();
    QueryRequest {
        operation: op.to_owned(),
        foreach,
        ..QueryRequest::default()
    }
}

#[derive(Debug, Default)]
pub struct Collected {
    pub rows: BTreeMap<u32, Vec<Value>>,
    pub errors: BTreeMap<u32, NgcErrorInfo>,
    pub closed: Vec<u32>,
    pub complete: Option<Complete>,
}

pub async fn collect(mut stream: BoxStream<QueryResponse>) -> Result<Collected, NgcErrorInfo> {
    let mut out = Collected::default();
    while let Some(item) = stream.next().await {
        match item?.chunk {
            Some(query_response::Chunk::Rows(chunk)) => {
                let entry = out.rows.entry(chunk.foreach_index).or_default();
                entry.extend(
                    chunk
                        .rows
                        .iter()
                        .map(|r| Value::Object(prost_struct_to_json_map(r))),
                );
                if let Some(err) = chunk.error {
                    out.errors.insert(
                        chunk.foreach_index,
                        gap_ngc_sdk::error::ngc_error_from_proto(&err),
                    );
                }
                if chunk.last_for_index {
                    out.closed.push(chunk.foreach_index);
                }
            }
            Some(query_response::Chunk::Complete(c)) => out.complete = Some(c),
            None => {}
        }
    }
    Ok(out)
}

/// A server with the given tools whose `tools/call` answers `structuredContent` echoing the arguments.
pub fn echo_server(era: ServerEra) -> FakeMcpServer {
    let server = FakeMcpServer::new(era);
    server.set_tools(vec![
        FakeTool::new("echo", json!({"type": "object", "properties": {"id": {"type": "integer"}, "tag": {"type": "string"}}, "required": ["id"]})),
        FakeTool::new("list", json!({"type": "object"})),
    ]);
    server.set_call_handler(|name, arguments| {
        use gap_mcp::outbound::fake::FakeCall;
        match name {
            "list" => FakeCall::Reply(json!({"structuredContent": [{"n": 1}, {"n": 2}, {"n": 3}]})),
            _ if arguments.get("id") == Some(&json!(666)) => FakeCall::Reply(
                json!({"isError": true, "content": [{"type": "text", "text": "boom"}]}),
            ),
            _ => FakeCall::Reply(json!({"structuredContent": {"echo": arguments}})),
        }
    });
    server
}

pub fn connector_over<T: McpTransport>(transport: T, cfg: McpConfig) -> McpConnector<T> {
    McpConnector::new(Arc::new(transport), cfg).expect("valid config")
}

pub fn http_connector(
    server: &FakeMcpServer,
    cfg: McpConfig,
) -> McpConnector<HttpTransport<gap_mcp::outbound::fake::FakeHttp>> {
    connector_over(server.http_transport(), cfg)
}

pub fn stdio_connector(server: &FakeMcpServer, cfg: McpConfig) -> McpConnector<StdioTransport> {
    connector_over(server.stdio_transport(), cfg)
}

#[allow(dead_code)]
pub fn obj(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}
