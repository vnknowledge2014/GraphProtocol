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

## 6. Start Dual-Stack HTTP/2 and HTTP/3 (QUIC) `[PROCEDURAL]`

> **CAUTION:** Production profile `prod` rejects HTTP/3 startup unless `[server]` defines explicit `tls_cert` and `tls_key` files (`OPS-005`).

1. Configure `server.http3_listen` in `gap.toml` to a valid UDP socket address such as `127.0.0.1:8443`.
2. Set `server.tls_cert` and `server.tls_key` to PEM secret references in `prod` environments.
3. If you run in local `dev` profile, set `server.tls_self_signed_dev = true` for ephemeral TLS certificates.
4. Start the dual-stack `gateway` process with your updated configuration file.
5. Check TCP HTTP responses for the `Alt-Svc: h3=":8443"; ma=86400` header (`OPS-005`).
6. Send test `/graphql` or `/mcp` requests over HTTP/3 QUIC with ALPN `h3` (`max_early_data_size = 0`).

```bash
export GAP_SERVER__HTTP3_LISTEN="127.0.0.1:8443"
export GAP_SERVER__TLS_SELF_SIGNED_DEV="true"
./target/dist/gap serve --config gap.toml
```

## 7. Connect AG-UI Frontends and System-1 Routers `[PROCEDURAL]`

> **NOTE:** `/ag-ui` spotlights `T2` outputs inside `<gap:untrusted>` tags and pauses unapproved mutations via `RUN_FINISHED` interrupts (`CMP-027`, `CMP-028`).

1. Send `GET` to `/ag-ui/capabilities` and verify JSON fields `protocol`, `version`, and `supportedEvents`.
2. Send `POST` `RunAgentInput` JSON to `/ag-ui` and read `RUN_STARTED`, `STATE_SNAPSHOT`, and `RUN_FINISHED` events.
3. Configure `[router].provider` in `gap.toml` as `baseline`, `receptron_laya`, `jev`, or `gemini_decision_1`.
4. Set `[router].api_key_secret` to an environment variable reference and never commit raw keys (`INV-15`).
5. Run `./target/dist/gap config check gap.toml` to verify router configuration and egress rules (`RTE-049`).

```bash
curl -sS http://127.0.0.1:8080/ag-ui/capabilities \
  -H 'Authorization: Bearer gap_dev_skeleton_key_01'
```
