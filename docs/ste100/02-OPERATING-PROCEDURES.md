# Standard Operating Procedures `[PROCEDURAL]`

## 1. Build and Check the Gateway `[PROCEDURAL]`

> **NOTE:** Run every command below from the GAP repository root.

1. Run the reproducible offline build script to compile the `gap` binary.
2. Verify configuration syntax and OpenAPI paths with `./target/dist/gap config check`.
3. Run preflight diagnostics with `./target/dist/gap doctor`.
4. Verify each command exits with code `0`.

```bash
./dist/build-reproducible.sh --offline
./target/dist/gap config check examples/walking-skeleton/gap.toml
./target/dist/gap doctor --config examples/walking-skeleton/gap.toml
```

## 2. Start Demo Branches and the Gateway `[PROCEDURAL]`

> **CAUTION:** Confirm ports `9101`, `9102`, and `8080` stay free before you start the processes.

1. Start the OpenAPI REST fixture on port `9101`.
2. Start the MCP fixture on port `9102`.
3. Start the GAP `gateway` on port `8080`.
4. Check `/healthz` and `/readyz` and verify HTTP status `200`.

```bash
python3 examples/walking-skeleton/fixtures/rest_fixture.py --port 9101 &
python3 examples/walking-skeleton/fixtures/mcp_fixture.py --port 9102 &
./target/dist/gap serve --config examples/walking-skeleton/gap.toml
curl -sS -i http://127.0.0.1:8080/healthz
curl -sS -i http://127.0.0.1:8080/readyz
```

## 3. Send a Cross-Branch GAP-QL Query `[PROCEDURAL]`

> **NOTE:** The sample `gap.toml` file hashes raw key `gap_dev_skeleton_key_01` with BLAKE3.

1. Set `Content-Type` to `application/json` and `Accept` to `application/graphql-response+json`.
2. Set header `Authorization` to `Bearer gap_dev_skeleton_key_01`.
3. Send one typed GraphQL query joining `shop` and `notes` to `/graphql`.
4. Inspect the JSON `data` object and confirm zero entries in `errors`.

```bash
curl -sS http://127.0.0.1:8080/graphql \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/graphql-response+json' \
  -H 'Authorization: Bearer gap_dev_skeleton_key_01' \
  -d '{"query":"{ customer(id: 1) { id name tier } orders(customer_id: 1) { id status } customer_notes(customer_id: 1) }"}'
```

## 4. Discover Operations Over the MCP Facade `[PROCEDURAL]`

1. Send `tools/call` for `gap_describe` with `overview: true` to read `L0` branch summaries.
2. Send `tools/call` for `gap_search` to list top matching operations in `L1`.
3. Send `tools/call` for `gap_describe` with `names` to fetch exact `L2` field definitions.
4. Send `tools/call` for `gap_query` to run your typed GraphQL query.

```bash
curl -sS http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: tools/call' \
  -H 'Mcp-Name: gap_describe' \
  -H 'Authorization: Bearer gap_dev_skeleton_key_01' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"gap_describe","arguments":{"overview":true}}}'
```

## 5. Connect an Agent Host `[PROCEDURAL]`

1. Point HTTP hosts at `http://127.0.0.1:8080/mcp` with bearer key `gap_dev_skeleton_key_01`.
2. If your host uses local `stdio`, start `./target/dist/gap serve --stdio --config gap.toml`.
3. Open [the host guide directory](../hosts/index.md) for Antigravity IDE, Claude Code, and Cursor snippets.
