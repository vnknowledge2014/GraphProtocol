//! Fuzz target for the GAP-QL request path (`WP-M1-02`, `GQL-001`, `GQL-002`): parse, the GAP structural
//! checks and full validation against a fixed schema must never panic, hang or allocate without bound on
//! arbitrary UTF-8 input. A parse error is a normal outcome.
#![no_main]
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::sync::OnceLock;

use gap_ql::{
    AllowAllVisibility, Schema, Valid, parse_executable_ast, validate_executable_reserved_names,
    validate_query_with_visibility, validate_unique_input_object_fields,
};
use libfuzzer_sys::fuzz_target;

/// Documents longer than this are skipped: the server refuses them before parsing (`max_request_body_bytes`).
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

const SCHEMA_SDL: &str = r#"
directive @cost(weight: Int, listSize: Int) on FIELD_DEFINITION
type Order { id: ID! sku: String! status: OrderStatus! lines(first: Int): [Line!]! }
type Line { id: ID! quantity: Int! }
enum OrderStatus { OPEN PAID SHIPPED }
input OrderFilter { status: OrderStatus minTotal: Int! = 0 }
type Node { id: ID! next: Node }
type Query {
  shopVersion: String
  order(id: ID!): Order
  orders(first: Int, status: OrderStatus, filter: OrderFilter): [Order!]!
  node(id: ID!): Node
}
"#;

fn schema() -> Option<&'static Valid<Schema>> {
    static SCHEMA: OnceLock<Option<Valid<Schema>>> = OnceLock::new();
    SCHEMA
        .get_or_init(|| Schema::parse_and_validate(SCHEMA_SDL, "fuzz.graphql").ok())
        .as_ref()
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if text.len() > MAX_DOCUMENT_BYTES {
        return;
    }
    if let Ok(ast_doc) = parse_executable_ast(text, "fuzz.graphql") {
        let _ = validate_executable_reserved_names(&ast_doc);
        let _ = validate_unique_input_object_fields(&ast_doc);
    }
    if let Some(schema) = schema() {
        let _ = validate_query_with_visibility(schema, text, &AllowAllVisibility::default());
    }
});
