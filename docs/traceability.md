# Spec-to-Test Traceability Matrix (`VER-001`..`VER-003`)

Generated mechanically by [`dist/generate-traceability.py`](../dist/generate-traceability.py) from `tools/spec_trace.py --strict`.

## Summary Metrics

| Metric | Value |
|---|---:|
| Total normative requirements | 362 |
| `MUST` / `MUST NOT` requirements | 89 |
| Non-`MUST` requirements (`SHOULD`/`MAY`) | 273 |
| Total test/conformance `spec:` markers | 2516 |
| Uncovered `MUST` requirements | 0 |
| Unknown `spec:` test markers | 0 |

## Coverage Breakdown by Specification File

| Specification File | Requirements | `MUST` | Tested | Uncovered `MUST` | Markers |
|---|---:|---:|---:|---:|---:|
| `spec/01-architecture.md` | 30 | 4 | 29 | 0 | 398 |
| `spec/02-gap-ql.md` | 27 | 15 | 27 | 0 | 369 |
| `spec/03-ngc.md` | 46 | 30 | 43 | 0 | 424 |
| `spec/04-routing.md` | 64 | 11 | 55 | 0 | 198 |
| `spec/05-ingestion.md` | 41 | 15 | 41 | 0 | 318 |
| `spec/06-runtime.md` | 47 | 2 | 45 | 0 | 218 |
| `spec/07-security.md` | 62 | 3 | 40 | 0 | 206 |
| `spec/08-operations.md` | 14 | 2 | 11 | 0 | 104 |
| `spec/09-compat.md` | 31 | 7 | 29 | 0 | 281 |

## Requirement-to-Test Mapping

### `spec/01-architecture.md`

| ID | Spec Line | Obligation | Markers | Primary Verification Locations |
|---|---:|---|---:|---|
| `ARC-001` | 52 | `MUST` | 3 | `crates/gap-testkit/src/lib.rs:105`, `crates/gap-testkit/src/port.rs:112`, `crates/gap-testkit/src/port.rs:137` |
| `ARC-002` | 53 | `MUST` | 2 | `crates/gap-server/tests/assemble.rs:201`, `crates/gap-server/tests/e2e.rs:219` |
| `ARC-007` | 54 | `MUST` | 14 | `adapters/gap-mcp/tests/inbound/wire.rs:20`, `crates/gap-conformance/src/driver.rs:793`, `crates/gap-core/src/traits.rs:440`, `crates/gap-server/tests/assemble.rs:201` (+10 more) |
| `ARC-003` | 55 | non-MUST | 5 | `crates/gap-s1/src/candle/mmap.rs:251`, `crates/gap-s1/src/candle/mmap.rs:271`, `crates/gap-s1/tests/golden/main.rs:187`, `crates/gap-s1/tests/golden/main.rs:435` (+1 more) |
| `ARC-004` | 56 | non-MUST | 0 | _none_ |
| `ARC-005` | 98 | non-MUST | 14 | `crates/gap-core/src/ids.rs:901`, `crates/gap-core/src/ids.rs:976`, `crates/gap-core/src/ids.rs:997`, `crates/gap-core/src/ord_map.rs:485` (+10 more) |
| `ARC-006` | 99 | `MUST` | 3 | `crates/gap-balance/tests/alloc_free.rs:93`, `crates/gap-core/src/snapshot/mod.rs:620`, `crates/gap-core/src/snapshot/mod.rs:722` |
| `INV-01` | 109 | non-MUST | 17 | `adapters/gap-mcp/tests/inbound/wire.rs:924`, `crates/gap-exec/tests/e2e_and_tower.rs:584`, `crates/gap-plan/tests/cache_and_hostile.rs:27`, `crates/gap-plan/tests/cache_and_hostile.rs:121` (+13 more) |
| `INV-02` | 110 | non-MUST | 33 | `crates/gap-balance/src/registry.rs:488`, `crates/gap-balance/src/time.rs:31`, `crates/gap-conformance/src/report.rs:294`, `crates/gap-conformance/src/report.rs:378` (+29 more) |
| `INV-03` | 111 | non-MUST | 1 | `crates/gap-transport/tests/engine_and_modules.rs:49` |
| `INV-04` | 112 | non-MUST | 1 | `crates/gap-sec/src/authz/tests.rs:40` |
| `INV-05` | 113 | non-MUST | 5 | `adapters/gap-mcp/tests/inbound/discovery.rs:199`, `crates/gap-index/src/lib.rs:1878`, `crates/gap-index/src/lib.rs:2059`, `crates/gap-route/tests/discovery_and_conformal.rs:616` (+1 more) |
| `INV-06` | 114 | non-MUST | 10 | `crates/gap-core/src/joingraph/mod.rs:344`, `crates/gap-core/src/joingraph/mod.rs:364`, `crates/gap-core/src/joingraph/mod.rs:408`, `crates/gap-exec/src/shadow/mod.rs:376` (+6 more) |
| `INV-07` | 115 | non-MUST | 1 | `adapters/gap-a2a/src/tests.rs:155` |
| `INV-08` | 116 | non-MUST | 4 | `crates/gap-ingest/src/drift/tests.rs:462`, `crates/gap-ingest/src/drift/tests.rs:541`, `tests/security/rugpull/rug_pull.rs:2`, `tests/security/rugpull/rug_pull.rs:138` |
| `INV-09` | 117 | non-MUST | 67 | `adapters/gap-a2a/src/tests.rs:290`, `crates/gap-balance/src/registry.rs:433`, `crates/gap-balance/tests/shuttle_balance.rs:491`, `crates/gap-core/src/context.rs:432` (+63 more) |
| `INV-10` | 118 | non-MUST | 1 | `crates/gap-balance/src/registry.rs:433` |
| `INV-11` | 119 | non-MUST | 1 | `crates/gap-exec/src/mutate/tests.rs:280` |
| `INV-12` | 120 | non-MUST | 26 | `adapters/gap-mcp/tests/outbound/connector.rs:272`, `adapters/gap-mcp/tests/outbound/connector.rs:337`, `crates/gap-conformance/src/ngc_kit/checks/lifecycle.rs:270`, `crates/gap-conformance/src/ngc_kit/checks/lifecycle.rs:298` (+22 more) |
| `INV-13` | 121 | non-MUST | 4 | `crates/gap-exec/src/tasks/tests.rs:332`, `crates/gap-exec/tests/task_crash_recovery.rs:93`, `crates/gap-store/src/tasks/tests.rs:24`, `crates/gap-store/src/tasks/tests.rs:266` |
| `INV-14` | 122 | non-MUST | 4 | `crates/gap-ingest/src/proposals/tests.rs:102`, `crates/gap-sec/src/audit/tests.rs:33`, `crates/gap-store/src/audit/tests.rs:34`, `crates/gap-store/src/audit/tests.rs:112` |
| `INV-15` | 123 | non-MUST | 117 | `adapters/gap-a2a/src/tests.rs:548`, `adapters/gap-mcp/src/outbound/config.rs:127`, `adapters/gap-mcp/src/outbound/error.rs:135`, `adapters/gap-mcp/src/outbound/launch.rs:315` (+113 more) |
| `INV-16` | 124 | non-MUST | 7 | `crates/gap-snapshot/src/publisher/mod.rs:1193`, `crates/gap-snapshot/src/publisher/mod.rs:1296`, `crates/gap-snapshot/src/rollback/mod.rs:511`, `crates/gap-snapshot/src/rollback/mod.rs:632` (+3 more) |
| `INV-17` | 125 | non-MUST | 8 | `crates/gap-core/src/traits.rs:440`, `crates/gap-route/src/calibration_job/tests.rs:168`, `crates/gap-route/src/s1_adapter/tests.rs:619`, `crates/gap-route/tests/discovery_and_conformal.rs:887` (+4 more) |
| `INV-18` | 126 | non-MUST | 15 | `adapters/gap-mcp/tests/outbound/connector.rs:373`, `adapters/gap-ngc-graphql/tests/graphql_connector.rs:591`, `adapters/gap-ngc-openapi/src/connector.rs:2720`, `crates/gap-conformance/tests/ngc_kit_mcp.rs:88` (+11 more) |
| `INV-19` | 127 | non-MUST | 19 | `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/src/sqlite/mod.rs:680`, `crates/gap-store/src/sqlite/mod.rs:874`, `crates/gap-store/src/store.rs:147` (+15 more) |
| `INV-20` | 128 | non-MUST | 3 | `adapters/gap-a2a/tests/loop_and_wire.rs:291`, `crates/gap-core/src/context.rs:495`, `crates/gap-exec/src/tasks/tests.rs:400` |
| `ARC-010` | 131 | non-MUST | 7 | `crates/gap-core/src/clock.rs:388`, `crates/gap-core/src/clock.rs:407`, `crates/gap-core/src/clock.rs:435`, `crates/gap-core/src/ids.rs:1014` (+3 more) |
| `ARC-011` | 132 | non-MUST | 4 | `crates/gap-core/src/traits.rs:463`, `crates/gap-transport/src/shed/mod.rs:599`, `crates/gap-transport/tests/admission.rs:145`, `crates/gap-transport/tests/deadlines_and_sse.rs:295` |
| `ARC-012` | 133 | non-MUST | 2 | `crates/gap-core/src/traits.rs:463`, `crates/gap-transport/tests/deadlines_and_sse.rs:295` |

### `spec/02-gap-ql.md`

| ID | Spec Line | Obligation | Markers | Primary Verification Locations |
|---|---:|---|---:|---|
| `GQL-001` | 6 | `MUST` | 22 | `conformance/a2a/003_data_task_query.json:8`, `conformance/gql/001_valid_list.json:7`, `conformance/gql/002_valid_variables.json:7`, `conformance/gql/003_valid_variable_supplied.json:7` (+18 more) |
| `GQL-002` | 7 | `MUST` | 112 | `conformance/a2a/002_text_resolve_typename.json:8`, `conformance/a2a/004_task_state_surface.json:8`, `conformance/gql/002_valid_variables.json:8`, `conformance/gql/003_valid_variable_supplied.json:8` (+108 more) |
| `GQL-003` | 8 | non-MUST | 52 | `adapters/gap-ngc-graphql/tests/graphql_connector.rs:900`, `conformance/gql/001_valid_list.json:8`, `conformance/gql/010_parse_unterminated.json:8`, `conformance/gql/011_parse_empty.json:8` (+48 more) |
| `GQL-004` | 9 | `MUST` | 18 | `adapters/gap-ngc-graphql/tests/graphql_connector.rs:433`, `adapters/gap-ngc-openapi/src/extract.rs:1217`, `adapters/gap-ngc-openapi/src/schema.rs:544`, `adapters/gap-ngc-openapi/src/schema.rs:601` (+14 more) |
| `GQL-010` | 149 | `MUST` | 7 | `conformance/gql/057_discovery_metagraph.json:7`, `crates/gap-exec/src/traverse/mod.rs:1312`, `crates/gap-plan/src/traverse/mod.rs:1262`, `crates/gap-plan/src/traverse/mod.rs:1303` (+3 more) |
| `GQL-011` | 150 | `MUST` | 2 | `adapters/gap-mcp/tests/inbound/discovery.rs:199`, `adapters/gap-mcp/tests/inbound/discovery.rs:272` |
| `GQL-012` | 151 | `MUST` | 1 | `crates/gap-route/tests/discovery_and_conformal.rs:474` |
| `GQL-020` | 163 | non-MUST | 4 | `adapters/gap-mcp/tests/inbound/discovery.rs:272`, `adapters/gap-mcp/tests/inbound/discovery.rs:344`, `conformance/mcp/08-gap-search-and-describe.json:4`, `crates/gap-route/tests/discovery_and_conformal.rs:616` |
| `GQL-021` | 164 | `MUST` | 3 | `adapters/gap-mcp/tests/inbound/discovery.rs:272`, `adapters/gap-mcp/tests/inbound/discovery.rs:344`, `crates/gap-route/tests/discovery_and_conformal.rs:616` |
| `GQL-022` | 165 | `MUST` | 7 | `adapters/gap-mcp/src/inbound/result.rs:207`, `adapters/gap-mcp/src/inbound/result.rs:231`, `adapters/gap-mcp/tests/inbound/discovery.rs:272`, `adapters/gap-mcp/tests/inbound/discovery.rs:344` (+3 more) |
| `GQL-023` | 166 | `MUST` | 4 | `adapters/gap-mcp/tests/inbound/discovery.rs:19`, `conformance/gql/057_discovery_metagraph.json:8`, `conformance/mcp/08-gap-search-and-describe.json:4`, `crates/gap-route/tests/discovery_and_conformal.rs:616` |
| `GQL-024` | 167 | non-MUST | 4 | `adapters/gap-mcp/tests/inbound/discovery.rs:84`, `conformance/gql/055_discovery_typename.json:8`, `conformance/gql/056_discovery_schema_introspection.json:7`, `crates/gap-ql/src/visibility.rs:740` |
| `GQL-030` | 197 | `MUST` | 3 | `crates/gap-core/src/error.rs:992`, `crates/gap-ql/src/directives.rs:760`, `crates/gap-snapshot/tests/compose.rs:971` |
| `GQL-031` | 198 | non-MUST | 2 | `crates/gap-core/src/joingraph/mod.rs:364`, `crates/gap-ql/src/directives.rs:845` |
| `GQL-032` | 199 | non-MUST | 2 | `crates/gap-ql/src/directives.rs:952`, `crates/gap-snapshot/tests/compose.rs:1041` |
| `GQL-050` | 216 | `MUST` | 23 | `conformance/a2a/001_agent_card_metagraph.json:8`, `conformance/a2a/005_trace_context_surface.json:8`, `conformance/gql/043_cost_scalar_root.json:8`, `conformance/gql/044_cost_object_field.json:8` (+19 more) |
| `GQL-051` | 217 | non-MUST | 6 | `crates/gap-exec/tests/e2e_and_tower.rs:436`, `crates/gap-exec/tests/errors_and_null_matrix.rs:31`, `crates/gap-exec/tests/errors_and_null_matrix.rs:352`, `crates/gap-exec/tests/errors_and_null_matrix.rs:444` (+2 more) |
| `GQL-060` | 220 | non-MUST | 3 | `crates/gap-core/src/config.rs:977`, `crates/gap-ql/src/lib.rs:313`, `crates/gap-ql/src/persisted.rs:506` |
| `GQL-061` | 221 | non-MUST | 9 | `adapters/gap-mcp/tests/inbound/discovery.rs:147`, `conformance/gql/051_persisted_hash_matches.json:7`, `conformance/gql/052_persisted_hash_mismatch.json:7`, `conformance/gql/053_persisted_hash_malformed.json:7` (+5 more) |
| `GQL-062` | 222 | `MUST` | 9 | `adapters/gap-mcp/tests/inbound/discovery.rs:84`, `adapters/gap-mcp/tests/inbound/discovery.rs:147`, `adapters/gap-mcp/tests/inbound/discovery.rs:199`, `adapters/gap-mcp/tests/inbound/discovery.rs:392` (+5 more) |
| `GQL-040` | 238 | `MUST` | 22 | `conformance/gql/037_limits_depth_at_maximum.json:7`, `conformance/gql/038_limits_depth_over_maximum.json:7`, `conformance/gql/039_limits_aliases_at_maximum.json:7`, `conformance/gql/040_limits_aliases_over_maximum.json:7` (+18 more) |
| `GQL-041` | 239 | non-MUST | 16 | `conformance/gql/043_cost_scalar_root.json:7`, `conformance/gql/044_cost_object_field.json:7`, `conformance/gql/045_cost_object_fields.json:7`, `conformance/gql/046_cost_list_first_below_list_size.json:7` (+12 more) |
| `GQL-042` | 240 | non-MUST | 4 | `crates/gap-core/src/joingraph/mod.rs:408`, `crates/gap-ingest/src/gates/mod.rs:2220`, `crates/gap-plan/src/join/mod.rs:590`, `crates/gap-ql/src/cost.rs:666` |
| `GQL-043` | 241 | non-MUST | 5 | `crates/gap-exec/src/traverse/mod.rs:1171`, `crates/gap-plan/src/traverse/mod.rs:1262`, `crates/gap-ql/src/validate.rs:1256`, `crates/gap-store/src/types.rs:1452` (+1 more) |
| `GQL-070` | 244 | `MUST` | 17 | `adapters/gap-mcp/tests/inbound/discovery.rs:84`, `conformance/gql/012_validation_unknown_field.json:8`, `conformance/gql/013_validation_unknown_nested_field.json:8`, `conformance/gql/015_validation_missing_argument.json:8` (+13 more) |
| `GQL-071` | 245 | non-MUST | 3 | `crates/gap-core/src/error.rs:963`, `crates/gap-core/src/error.rs:1015`, `crates/gap-ingest/src/proposals/tests.rs:770` |
| `GQL-072` | 246 | `MUST` | 9 | `crates/gap-conformance/tests/conformance_gql.rs:93`, `crates/gap-core/src/error.rs:963`, `crates/gap-core/src/error.rs:1015`, `crates/gap-core/src/error.rs:1073` (+5 more) |

### `spec/03-ngc.md`

| ID | Spec Line | Obligation | Markers | Primary Verification Locations |
|---|---:|---|---:|---|
| `NGC-001` | 7 | non-MUST | 16 | `crates/gap-conformance/src/ngc_kit/selftest.rs:154`, `crates/gap-ngc-sdk/tests/json_mapping.rs:813`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:358`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:985` (+12 more) |
| `NGC-002` | 8 | `MUST` | 7 | `crates/gap-ngc-sdk/tests/json_mapping.rs:38`, `crates/gap-ngc-sdk/tests/json_mapping.rs:208`, `crates/gap-ngc-sdk/tests/json_mapping.rs:813`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:358` (+3 more) |
| `NGC-003` | 9 | non-MUST | 10 | `crates/gap-ngc-sdk/tests/json_mapping.rs:38`, `crates/gap-ngc-sdk/tests/json_mapping.rs:813`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:358`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:985` (+6 more) |
| `NGC-004` | 10 | `MUST` | 2 | `crates/gap-proto/src/lib.rs:59`, `crates/gap-proto/tests/service_shape.rs:239` |
| `NGC-010` | 13 | `MUST` | 14 | `adapters/gap-mcp/src/outbound/map.rs:787`, `adapters/gap-mcp/tests/outbound/connector.rs:82`, `adapters/gap-ngc-openapi/src/connector.rs:1753`, `adapters/gap-ngc-openapi/src/connector.rs:5197` (+10 more) |
| `NGC-011` | 14 | `MUST` | 8 | `crates/gap-snapshot/src/canonical.rs:272`, `crates/gap-snapshot/src/canonical.rs:349`, `crates/gap-snapshot/src/canonical.rs:370`, `crates/gap-snapshot/src/canonical.rs:411` (+4 more) |
| `NGC-012` | 15 | `MUST` | 12 | `adapters/gap-mcp/src/outbound/map.rs:697`, `adapters/gap-mcp/tests/outbound/connector.rs:82`, `adapters/gap-ngc-openapi/src/connector.rs:1753`, `adapters/gap-ngc-openapi/src/extract.rs:1217` (+8 more) |
| `NGC-013` | 16 | `MUST` | 9 | `adapters/gap-ngc-openapi/src/connector.rs:1753`, `adapters/gap-ngc-openapi/src/document.rs:616`, `adapters/gap-ngc-openapi/src/document.rs:670`, `adapters/gap-ngc-openapi/src/schema.rs:544` (+5 more) |
| `NGC-014` | 17 | non-MUST | 1 | `adapters/gap-ngc-grpc/tests/grpc_connector.rs:623` |
| `NGC-015` | 18 | non-MUST | 5 | `adapters/gap-ngc-openapi/src/connector.rs:3419`, `crates/gap-conformance/src/ngc_kit/selftest.rs:71`, `crates/gap-core/src/joingraph/mod.rs:344`, `crates/gap-core/src/joingraph/mod.rs:384` (+1 more) |
| `NGC-016` | 19 | non-MUST | 0 | _none_ |
| `NGC-020` | 22 | `MUST` | 30 | `adapters/gap-mcp/tests/outbound/connector.rs:373`, `adapters/gap-mcp/tests/outbound/connector.rs:431`, `adapters/gap-ngc-graphql/tests/graphql_connector.rs:591`, `adapters/gap-ngc-graphql/tests/graphql_connector.rs:758` (+26 more) |
| `NGC-021` | 23 | `MUST` | 16 | `adapters/gap-mcp/tests/outbound/connector.rs:496`, `adapters/gap-ngc-openapi/src/connector.rs:2520`, `adapters/gap-ngc-openapi/src/connector.rs:3836`, `adapters/gap-ngc-openapi/src/connector.rs:4154` (+12 more) |
| `NGC-022` | 24 | `MUST` | 15 | `adapters/gap-mcp/tests/outbound/connector.rs:496`, `adapters/gap-ngc-openapi/src/connector.rs:2520`, `adapters/gap-ngc-openapi/src/connector.rs:4393`, `adapters/gap-ngc-openapi/src/connector.rs:4537` (+11 more) |
| `NGC-023` | 25 | `MUST` | 13 | `adapters/gap-mcp/tests/outbound/connector.rs:496`, `adapters/gap-mcp/tests/outbound/connector.rs:674`, `adapters/gap-ngc-openapi/src/connector.rs:2182`, `adapters/gap-ngc-openapi/src/connector.rs:4393` (+9 more) |
| `NGC-024` | 26 | `MUST` | 1 | `adapters/gap-ngc-sql/tests/sql_connector.rs:424` |
| `NGC-025` | 27 | `MUST` | 32 | `adapters/gap-mcp/tests/outbound/connector.rs:272`, `adapters/gap-mcp/tests/outbound/connector.rs:306`, `adapters/gap-mcp/tests/outbound/connector.rs:431`, `adapters/gap-ngc-graphql/tests/graphql_connector.rs:900` (+28 more) |
| `NGC-026` | 28 | `MUST` | 20 | `adapters/gap-mcp/tests/outbound/connector.rs:373`, `adapters/gap-ngc-graphql/tests/graphql_connector.rs:591`, `adapters/gap-ngc-grpc/tests/grpc_connector.rs:819`, `adapters/gap-ngc-openapi/src/connector.rs:2409` (+16 more) |
| `NGC-027` | 29 | `MUST` | 12 | `adapters/gap-mcp/tests/outbound/connector.rs:471`, `adapters/gap-ngc-openapi/src/connector.rs:2520`, `adapters/gap-ngc-openapi/src/connector.rs:4154`, `adapters/gap-ngc-openapi/src/fixture.rs:669` (+8 more) |
| `NGC-030` | 32 | `MUST` | 6 | `crates/gap-conformance/src/ngc_kit/selftest.rs:30`, `crates/gap-conformance/src/ngc_kit/selftest.rs:42`, `crates/gap-conformance/src/ngc_kit/selftest.rs:71`, `crates/gap-conformance/src/ngc_kit/selftest.rs:210` (+2 more) |
| `NGC-031` | 33 | `MUST` | 12 | `crates/gap-conformance/src/ngc_kit/selftest.rs:30`, `crates/gap-conformance/src/ngc_kit/selftest.rs:42`, `crates/gap-conformance/src/ngc_kit/selftest.rs:71`, `crates/gap-conformance/src/ngc_kit/selftest.rs:210` (+8 more) |
| `NGC-032` | 34 | `MUST` | 1 | `crates/gap-exec/src/mutate/tests.rs:333` |
| `NGC-033` | 35 | non-MUST | 2 | `crates/gap-exec/src/mutate/tests.rs:523`, `crates/gap-exec/src/saga/mod.rs:631` |
| `NGC-040` | 38 | non-MUST | 0 | _none_ |
| `NGC-041` | 39 | non-MUST | 1 | `adapters/gap-a2a/tests/loop_and_wire.rs:381` |
| `NGC-042` | 40 | `MUST` | 2 | `adapters/gap-a2a/tests/loop_and_wire.rs:291`, `crates/gap-core/src/context.rs:495` |
| `NGC-050` | 43 | `MUST` | 6 | `adapters/gap-ngc-sql/src/sketch.rs:139`, `adapters/gap-ngc-sql/src/sketch.rs:170`, `adapters/gap-ngc-sql/tests/sql_connector.rs:775`, `crates/gap-conformance/tests/ngc_kit_sql.rs:121` (+2 more) |
| `NGC-051` | 44 | `MUST` | 3 | `crates/gap-ingest/src/gates/join_verify/mod.rs:1272`, `crates/gap-ingest/src/gates/join_verify/mod.rs:1318`, `crates/gap-ingest/src/gates/join_verify/mod.rs:1374` |
| `NGC-052` | 45 | non-MUST | 1 | `crates/gap-ingest/src/gates/join_verify/mod.rs:1272` |
| `NGC-060` | 48 | `MUST` | 19 | `adapters/gap-mcp/src/outbound/error.rs:155`, `adapters/gap-mcp/src/outbound/map.rs:803`, `adapters/gap-mcp/tests/outbound/connector.rs:496`, `adapters/gap-mcp/tests/outbound/connector.rs:674` (+15 more) |
| `NGC-061` | 49 | `MUST` | 17 | `adapters/gap-mcp/src/outbound/error.rs:135`, `adapters/gap-ngc-openapi/src/connector.rs:1896`, `adapters/gap-ngc-openapi/src/connector.rs:2182`, `adapters/gap-ngc-openapi/src/connector.rs:3927` (+13 more) |
| `NGC-062` | 50 | non-MUST | 6 | `crates/gap-ngc-sdk/tests/error_and_scope.rs:129`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:694`, `crates/gap-ngc-sdk/tests/transports.rs:736`, `crates/gap-ngc-sdk/tests/transports.rs:1006` (+2 more) |
| `NGC-063` | 53 | non-MUST | 10 | `crates/gap-exec/src/exec/error.rs:410`, `crates/gap-exec/tests/errors_and_null_matrix.rs:352`, `crates/gap-ngc-sdk/tests/error_and_scope.rs:75`, `crates/gap-ngc-sdk/tests/error_and_scope.rs:110` (+6 more) |
| `NGC-070` | 54 | `MUST` | 12 | `adapters/gap-mcp/tests/outbound/connector.rs:628`, `adapters/gap-ngc-sql/tests/sql_connector.rs:220`, `crates/gap-balance/src/slot.rs:417`, `crates/gap-conformance/src/ngc_kit/selftest.rs:30` (+8 more) |
| `NGC-071` | 55 | `MUST` | 8 | `adapters/gap-mcp/tests/outbound/connector.rs:337`, `adapters/gap-ngc-openapi/src/connector.rs:2902`, `crates/gap-exec/src/connector_layer/mod.rs:1063`, `crates/gap-exec/tests/bounds_and_hostile.rs:75` (+4 more) |
| `NGC-072` | 56 | `MUST` | 3 | `adapters/gap-ngc-openapi/src/connector.rs:2902`, `adapters/gap-ngc-openapi/src/fixture.rs:669`, `crates/gap-balance/src/slot.rs:417` |
| `NGC-080` | 59 | non-MUST | 3 | `crates/gap-ngc-sdk/tests/error_and_scope.rs:281`, `crates/gap-ngc-sdk/tests/transports.rs:36`, `crates/gap-ngc-sdk/tests/transports.rs:736` |
| `NGC-081` | 60 | non-MUST | 1 | `crates/gap-sec/src/authn/tests.rs:676` |
| `NGC-082` | 61 | `MUST` | 2 | `adapters/gap-ngc-cli/src/tests.rs:153`, `tests/security/sandbox/sandbox_smoke.rs:282` |
| `NGC-090` | 64 | non-MUST | 5 | `adapters/gap-ngc-openapi/src/connector.rs:2902`, `crates/gap-ngc-sdk/tests/error_and_scope.rs:360`, `crates/gap-ngc-sdk/tests/foreach_props.rs:658`, `crates/gap-ngc-sdk/tests/foreach_props.rs:819` (+1 more) |
| `NGC-091` | 65 | non-MUST | 32 | `adapters/gap-ngc-openapi/src/connector.rs:3071`, `adapters/gap-ngc-openapi/src/fixture.rs:669`, `adapters/gap-ngc-openapi/src/upstream.rs:430`, `crates/gap-conformance/src/cli.rs:431` (+28 more) |
| `NGC-092` | 66 | non-MUST | 0 | _none_ |
| `NGC-093` | 67 | `MUST` | 23 | `crates/gap-ngc-sdk/tests/error_and_scope.rs:245`, `crates/gap-ngc-sdk/tests/error_and_scope.rs:438`, `crates/gap-ngc-sdk/tests/error_and_scope.rs:488`, `crates/gap-ngc-sdk/tests/error_and_scope.rs:534` (+19 more) |
| `NGC-094` | 68 | `MUST` | 6 | `crates/gap-ngc-sdk/tests/error_and_scope.rs:201`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:358`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:694`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:985` (+2 more) |
| `NGC-095` | 69 | `MUST` | 11 | `crates/gap-ngc-sdk/tests/transport_matrix.rs:358`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:694`, `crates/gap-ngc-sdk/tests/transport_matrix.rs:985`, `crates/gap-ngc-sdk/tests/transports.rs:383` (+7 more) |
| `NGC-096` | 70 | `MUST` | 9 | `crates/gap-conformance/tests/ngc_kit_sql.rs:121`, `crates/gap-ngc-sdk/tests/foreach_props.rs:190`, `crates/gap-ngc-sdk/tests/foreach_props.rs:253`, `crates/gap-ngc-sdk/tests/foreach_props.rs:395` (+5 more) |

### `spec/04-routing.md`

| ID | Spec Line | Obligation | Markers | Primary Verification Locations |
|---|---:|---|---:|---|
| `RTE-001` | 14 | non-MUST | 2 | `crates/gap-index/src/lib.rs:1949`, `crates/gap-index/src/lib.rs:2020` |
| `RTE-002` | 15 | non-MUST | 1 | `crates/gap-models/src/lib.rs:1222` |
| `RTE-003` | 16 | non-MUST | 1 | `crates/gap-index/src/lib.rs:1832` |
| `RTE-004` | 17 | non-MUST | 1 | `crates/gap-index/src/lib.rs:1849` |
| `RTE-005` | 18 | non-MUST | 2 | `crates/gap-index/src/lib.rs:1878`, `crates/gap-index/src/lib.rs:2020` |
| `RTE-006` | 19 | non-MUST | 2 | `crates/gap-index/src/lib.rs:1926`, `crates/gap-index/src/lib.rs:1949` |
| `RTE-010` | 22 | non-MUST | 2 | `crates/gap-index/src/lib.rs:2059`, `crates/gap-sec/src/authz/tests.rs:483` |
| `RTE-011` | 23 | non-MUST | 0 | _none_ |
| `RTE-020` | 26 | `MUST` | 3 | `crates/gap-route/tests/discovery_and_conformal.rs:160`, `crates/gap-s1/src/candle/sequence.rs:217`, `crates/gap-s1/tests/golden/main.rs:397` |
| `RTE-021` | 27 | non-MUST | 1 | `crates/gap-route/tests/discovery_and_conformal.rs:160` |
| `RTE-022` | 28 | non-MUST | 1 | `crates/gap-route/tests/discovery_and_conformal.rs:160` |
| `RTE-030` | 43 | non-MUST | 11 | `crates/gap-route/src/groups/mod.rs:901`, `crates/gap-route/src/groups/mod.rs:953`, `crates/gap-route/src/groups/mod.rs:1062`, `crates/gap-route/src/groups/mod.rs:1132` (+7 more) |
| `RTE-031` | 44 | `MUST` | 1 | `crates/gap-route/tests/discovery_and_conformal.rs:474` |
| `RTE-032` | 45 | non-MUST | 1 | `crates/gap-route/tests/discovery_and_conformal.rs:261` |
| `RTE-033` | 46 | non-MUST | 4 | `crates/gap-route/src/groups/mod.rs:901`, `crates/gap-route/src/groups/mod.rs:953`, `crates/gap-route/src/groups/mod.rs:1062`, `crates/gap-route/src/groups/mod.rs:1132` |
| `RTE-034` | 47 | non-MUST | 1 | `crates/gap-route/tests/discovery_and_conformal.rs:616` |
| `RTE-040` | 58 | non-MUST | 0 | _none_ |
| `RTE-041` | 59 | `MUST` | 8 | `crates/gap-route/tests/discovery_and_conformal.rs:887`, `crates/gap-s1/src/ort/tests.rs:26`, `crates/gap-s1/src/ort/tests.rs:104`, `crates/gap-s1/src/sidecar/tests.rs:52` (+4 more) |
| `RTE-042` | 60 | non-MUST | 8 | `crates/gap-route/src/s1_adapter/tests.rs:226`, `crates/gap-route/src/s1_adapter/tests.rs:421`, `crates/gap-route/tests/discovery_and_conformal.rs:261`, `crates/gap-s1/src/candle/sequence.rs:217` (+4 more) |
| `RTE-043` | 61 | non-MUST | 2 | `crates/gap-route/src/debias/tests.rs:35`, `crates/gap-route/src/debias/tests.rs:61` |
| `RTE-044` | 62 | non-MUST | 7 | `crates/gap-route/src/s1_adapter/tests.rs:619`, `crates/gap-s1/src/ort/tests.rs:104`, `crates/gap-s1/src/sidecar/tests.rs:52`, `crates/gap-s1/src/sidecar/tests.rs:133` (+3 more) |
| `RTE-045` | 63 | non-MUST | 2 | `crates/gap-route/src/s1_adapter/tests.rs:550`, `crates/gap-server/src/wiring_s1.rs:989` |
| `RTE-046` | 64 | non-MUST | 1 | `crates/gap-route/src/s1_adapter/tests.rs:498` |
| `RTE-047` | 65 | non-MUST | 2 | `crates/gap-s1/tests/decision_providers.rs:109`, `crates/gap-s1/tests/decision_providers.rs:197` |
| `RTE-048` | 66 | `MUST` | 1 | `crates/gap-s1/tests/decision_providers.rs:241` |
| `RTE-049` | 67 | `MUST` | 3 | `crates/gap-core/src/config.rs:1130`, `crates/gap-s1/tests/decision_providers.rs:241`, `crates/gap-server/tests/assemble.rs:767` |
| `RTE-060` | 71 | non-MUST | 0 | _none_ |
| `RTE-061` | 72 | non-MUST | 0 | _none_ |
| `RTE-062` | 75 | non-MUST | 4 | `crates/gap-cli/src/calibrate/mod.rs:431`, `crates/gap-cli/src/calibrate/mod.rs:490`, `crates/gap-route/src/calibration_job/tests.rs:89`, `crates/gap-route/tests/discovery_and_conformal.rs:887` |
| `RTE-063` | 76 | non-MUST | 0 | _none_ |
| `RTE-064` | 79 | non-MUST | 4 | `crates/gap-cli/src/calibrate/mod.rs:431`, `crates/gap-route/src/calibration_job/tests.rs:89`, `crates/gap-route/src/calibration_job/tests.rs:168`, `crates/gap-route/tests/discovery_and_conformal.rs:887` |
| `RTE-065` | 80 | non-MUST | 2 | `crates/gap-route/src/calibration_job/tests.rs:89`, `crates/gap-route/tests/discovery_and_conformal.rs:887` |
| `RTE-066` | 81 | non-MUST | 1 | `crates/gap-route/src/calibration_job/tests.rs:89` |
| `RTE-067` | 82 | `MUST` | 5 | `crates/gap-cli/src/calibrate/mod.rs:431`, `crates/gap-cli/src/calibrate/mod.rs:490`, `crates/gap-route/src/calibration_job/tests.rs:89`, `crates/gap-route/src/calibration_job/tests.rs:168` (+1 more) |
| `RTE-068` | 83 | non-MUST | 2 | `crates/gap-cli/src/calibrate/mod.rs:431`, `crates/gap-route/src/calibration_job/tests.rs:220` |
| `RTE-069` | 86 | non-MUST | 0 | _none_ |
| `RTE-070` | 115 | `MUST` | 9 | `crates/gap-balance/src/ewma.rs:217`, `crates/gap-balance/src/ewma.rs:236`, `crates/gap-balance/src/ewma.rs:263`, `crates/gap-balance/src/ewma.rs:344` (+5 more) |
| `RTE-071` | 116 | non-MUST | 3 | `crates/gap-balance/src/slot.rs:238`, `crates/gap-balance/src/slot.rs:276`, `crates/gap-balance/tests/shuttle_balance.rs:440` |
| `RTE-072` | 117 | non-MUST | 1 | `crates/gap-balance/src/registry.rs:433` |
| `RTE-077` | 119 | `MUST` | 10 | `crates/gap-balance/src/ewma.rs:217`, `crates/gap-balance/src/ewma.rs:236`, `crates/gap-balance/src/ewma.rs:263`, `crates/gap-balance/src/ewma.rs:292` (+6 more) |
| `RTE-078` | 120 | non-MUST | 6 | `crates/gap-balance/src/registry.rs:318`, `crates/gap-balance/src/registry.rs:388`, `crates/gap-balance/src/registry.rs:433`, `crates/gap-balance/src/registry.rs:488` (+2 more) |
| `RTE-073` | 123 | non-MUST | 3 | `crates/gap-balance/src/p2c.rs:384`, `crates/gap-balance/src/p2c.rs:742`, `crates/gap-balance/src/p2c.rs:1143` |
| `RTE-074` | 124 | non-MUST | 7 | `crates/gap-balance/src/p2c.rs:384`, `crates/gap-balance/src/p2c.rs:516`, `crates/gap-balance/src/p2c.rs:829`, `crates/gap-balance/src/p2c.rs:1008` (+3 more) |
| `RTE-075` | 125 | non-MUST | 1 | `crates/gap-balance/src/p2c.rs:384` |
| `RTE-076` | 126 | non-MUST | 0 | _none_ |
| `RTE-079` | 128 | `MUST` | 10 | `crates/gap-balance/src/p2c.rs:384`, `crates/gap-balance/src/p2c.rs:516`, `crates/gap-balance/src/p2c.rs:742`, `crates/gap-balance/src/p2c.rs:829` (+6 more) |
| `RTE-080` | 139 | non-MUST | 8 | `crates/gap-balance/src/breaker.rs:1326`, `crates/gap-balance/src/breaker.rs:2446`, `crates/gap-balance/src/breaker.rs:2583`, `crates/gap-balance/src/breaker.rs:2608` (+4 more) |
| `RTE-081` | 140 | non-MUST | 3 | `crates/gap-balance/src/breaker.rs:1326`, `tests/chaos/chaos.rs:171`, `tests/chaos/chaos.rs:386` |
| `RTE-082` | 141 | non-MUST | 4 | `crates/gap-balance/src/breaker.rs:1326`, `crates/gap-balance/src/breaker.rs:2854`, `crates/gap-balance/src/breaker.rs:2978`, `crates/gap-balance/tests/shuttle_balance.rs:264` |
| `RTE-083` | 142 | non-MUST | 4 | `crates/gap-balance/src/breaker.rs:1326`, `crates/gap-balance/src/breaker.rs:2686`, `crates/gap-balance/src/slot.rs:417`, `crates/gap-balance/tests/shuttle_balance.rs:323` |
| `RTE-084` | 144 | `MUST` | 12 | `crates/gap-balance/src/breaker.rs:1326`, `crates/gap-balance/src/breaker.rs:2446`, `crates/gap-balance/src/breaker.rs:2559`, `crates/gap-balance/src/breaker.rs:2583` (+8 more) |
| `RTE-085` | 145 | non-MUST | 12 | `crates/gap-balance/src/breaker.rs:1326`, `crates/gap-balance/src/breaker.rs:2446`, `crates/gap-balance/src/breaker.rs:2686`, `crates/gap-balance/src/breaker.rs:2728` (+8 more) |
| `RTE-086` | 146 | non-MUST | 1 | `crates/gap-balance/src/slot.rs:417` |
| `RTE-090` | 149 | non-MUST | 0 | _none_ |
| `RTE-091` | 150 | non-MUST | 3 | `crates/gap-balance/src/retry.rs:93`, `crates/gap-balance/src/retry.rs:142`, `crates/gap-balance/tests/shuttle_balance.rs:467` |
| `RTE-092` | 151 | non-MUST | 0 | _none_ |
| `RTE-093` | 152 | non-MUST | 3 | `crates/gap-balance/src/retry.rs:93`, `crates/gap-balance/src/retry.rs:142`, `crates/gap-balance/tests/shuttle_balance.rs:467` |
| `RTE-100` | 166 | non-MUST | 2 | `crates/gap-ingest/src/proposals/tests.rs:770`, `crates/gap-route/src/feedback/tests.rs:115` |
| `RTE-101` | 167 | non-MUST | 2 | `crates/gap-ingest/src/proposals/tests.rs:770`, `crates/gap-route/src/feedback/tests.rs:115` |
| `RTE-102` | 168 | `MUST` | 1 | `crates/gap-ingest/src/proposals/tests.rs:770` |
| `RTE-103` | 169 | non-MUST | 1 | `crates/gap-ingest/src/proposals/tests.rs:770` |
| `RTE-110` | 172 | non-MUST | 3 | `crates/gap-route/src/feedback/tests.rs:76`, `crates/gap-store/src/feedback/tests.rs:83`, `crates/gap-store/src/feedback/tests.rs:204` |
| `RTE-111` | 173 | non-MUST | 2 | `crates/gap-route/src/calibration_job/tests.rs:220`, `crates/gap-route/src/feedback/tests.rs:27` |
| `RTE-112` | 174 | non-MUST | 1 | `crates/gap-store/src/feedback/tests.rs:83` |

### `spec/05-ingestion.md`

| ID | Spec Line | Obligation | Markers | Primary Verification Locations |
|---|---:|---|---:|---|
| `ING-001` | 24 | non-MUST | 9 | `crates/gap-cli/src/proposals/mod.rs:431`, `crates/gap-cli/src/proposals/mod.rs:561`, `crates/gap-ingest/src/gates/mod.rs:2021`, `crates/gap-ingest/src/gates/mod.rs:2066` (+5 more) |
| `ING-002` | 25 | non-MUST | 1 | `crates/gap-ingest/src/proposals/tests.rs:303` |
| `ING-003` | 26 | non-MUST | 1 | `crates/gap-ingest/src/proposals/tests.rs:411` |
| `ING-010` | 42 | `MUST` | 19 | `adapters/gap-ngc-openapi/src/connector.rs:4850`, `adapters/gap-ngc-openapi/src/document.rs:586`, `adapters/gap-ngc-openapi/src/extract.rs:1139`, `adapters/gap-ngc-openapi/src/extract.rs:1217` (+15 more) |
| `ING-011` | 43 | `MUST` | 2 | `crates/gap-ingest/src/cisg/mod.rs:1297`, `crates/gap-ingest/src/fetch/mod.rs:152` |
| `ING-012` | 44 | `MUST` | 13 | `adapters/gap-ngc-openapi/src/document.rs:616`, `adapters/gap-ngc-openapi/src/document.rs:670`, `adapters/gap-ngc-openapi/src/document.rs:846`, `crates/gap-ingest/src/cisg/mod.rs:1297` (+9 more) |
| `ING-020` | 67 | non-MUST | 10 | `crates/gap-ingest/src/cisg/mod.rs:1272`, `crates/gap-ingest/src/extractors/graphql/mod.rs:1661`, `crates/gap-ingest/src/extractors/graphql/mod.rs:1746`, `crates/gap-ingest/src/extractors/grpc/mod.rs:2263` (+6 more) |
| `ING-021` | 68 | non-MUST | 4 | `crates/gap-ingest/src/cisg/mod.rs:1233`, `crates/gap-ingest/src/extractors/openapi/mod.rs:905`, `crates/gap-ingest/src/extractors/sql/mod.rs:1519`, `crates/gap-ingest/src/extractors/sql/mod.rs:1601` |
| `ING-030` | 71 | non-MUST | 2 | `crates/gap-ingest/src/enrich/mod.rs:1532`, `crates/gap-ingest/src/enrich/mod.rs:1729` |
| `ING-031` | 72 | non-MUST | 1 | `crates/gap-ingest/src/enrich/mod.rs:1548` |
| `ING-032` | 73 | non-MUST | 1 | `crates/gap-ingest/src/enrich/mod.rs:1592` |
| `ING-033` | 74 | non-MUST | 1 | `crates/gap-ingest/src/enrich/mod.rs:1621` |
| `ING-034` | 75 | non-MUST | 1 | `crates/gap-ingest/src/enrich/mod.rs:1657` |
| `ING-035` | 76 | `MUST` | 2 | `crates/gap-ingest/src/enrich/mod.rs:1682`, `crates/gap-ingest/src/proposals/tests.rs:411` |
| `ING-040` | 94 | non-MUST | 8 | `crates/gap-exec/src/shadow/mod.rs:376`, `crates/gap-ingest/src/gates/join_verify/mod.rs:1374`, `crates/gap-ingest/src/gates/join_verify/mod.rs:1410`, `crates/gap-ingest/src/gates/mod.rs:2066` (+4 more) |
| `ING-041` | 95 | non-MUST | 5 | `crates/gap-exec/src/shadow/mod.rs:376`, `crates/gap-exec/src/shadow/mod.rs:431`, `crates/gap-ingest/src/gates/join_verify/mod.rs:1410`, `crates/gap-ingest/src/gates/replay/mod.rs:539` (+1 more) |
| `ING-050` | 98 | non-MUST | 2 | `crates/gap-ingest/src/proposals/tests.rs:411`, `crates/gap-snapshot/src/publisher/mod.rs:1296` |
| `ING-051` | 99 | non-MUST | 3 | `crates/gap-cli/src/proposals/mod.rs:431`, `crates/gap-cli/src/proposals/mod.rs:561`, `crates/gap-ingest/src/proposals/tests.rs:529` |
| `ING-052` | 100 | non-MUST | 9 | `crates/gap-cli/src/commands/tests.rs:249`, `crates/gap-ingest/src/drift/tests.rs:180`, `crates/gap-ingest/src/drift/tests.rs:215`, `crates/gap-ingest/src/drift/tests.rs:302` (+5 more) |
| `ING-053` | 101 | non-MUST | 3 | `crates/gap-ingest/src/drift/tests.rs:380`, `crates/gap-ingest/src/drift/tests.rs:462`, `crates/gap-ingest/src/drift/tests.rs:541` |
| `ING-054` | 102 | non-MUST | 2 | `tests/security/rugpull/rug_pull.rs:2`, `tests/security/rugpull/rug_pull.rs:138` |
| `ING-055` | 103 | non-MUST | 1 | `crates/gap-snapshot/src/incremental/tests.rs:269` |
| `ING-060` | 109 | non-MUST | 2 | `adapters/gap-ngc-code/tests/code_indexer.rs:90`, `crates/gap-store/src/types.rs:1222` |
| `ING-061` | 110 | non-MUST | 1 | `adapters/gap-ngc-code/tests/code_indexer.rs:187` |
| `ING-062` | 111 | non-MUST | 10 | `adapters/gap-ngc-code/tests/code_indexer.rs:256`, `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/src/sqlite/mod.rs:550`, `crates/gap-store/tests/conformance_sqlite.rs:14` (+6 more) |
| `ING-063` | 112 | non-MUST | 3 | `adapters/gap-ngc-code/tests/code_indexer.rs:378`, `crates/gap-exec/src/traverse/mod.rs:962`, `crates/gap-store/tests/search.rs:30` |
| `ING-064` | 113 | non-MUST | 42 | `adapters/gap-ngc-code/tests/code_indexer.rs:534`, `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/src/sqlite/mod.rs:510`, `crates/gap-store/src/sqlite/mod.rs:550` (+38 more) |
| `ING-065` | 114 | `MUST` | 28 | `crates/gap-exec/src/traverse/mod.rs:1171`, `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/src/surreal/tests.rs:23`, `crates/gap-store/src/types.rs:1174` (+24 more) |
| `ING-066` | 115 | `MUST` | 18 | `crates/gap-cli/src/commands/tests.rs:389`, `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/src/surreal/tests.rs:23`, `crates/gap-store/src/types.rs:1256` (+14 more) |
| `ING-067` | 116 | `MUST` | 16 | `crates/gap-exec/src/traverse/mod.rs:1312`, `crates/gap-plan/src/traverse/mod.rs:1303`, `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/src/surreal/tests.rs:23` (+12 more) |
| `ING-070` | 119 | non-MUST | 2 | `crates/gap-ingest/src/worker/mod.rs:636`, `crates/gap-ingest/src/worker/mod.rs:684` |
| `ING-071` | 120 | non-MUST | 1 | `crates/gap-ingest/src/worker/mod.rs:712` |
| `ING-080` | 127 | non-MUST | 5 | `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/tests/conformance_sqlite.rs:14`, `crates/gap-store/tests/crud_temporal_batch.rs:229`, `crates/gap-store/tests/crud_temporal_batch.rs:1008` (+1 more) |
| `ING-081` | 128 | `MUST` | 5 | `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/tests/conformance_sqlite.rs:14`, `crates/gap-store/tests/crud_temporal_batch.rs:1008`, `crates/gap-store/tests/crud_temporal_batch.rs:1083` (+1 more) |
| `ING-082` | 129 | `MUST` | 12 | `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/tests/conformance_sqlite.rs:14`, `crates/gap-store/tests/crud_temporal_batch.rs:42`, `crates/gap-store/tests/crud_temporal_batch.rs:477` (+8 more) |
| `ING-083` | 130 | `MUST` | 10 | `crates/gap-exec/src/traverse/mod.rs:1171`, `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/tests/conformance_sqlite.rs:14`, `crates/gap-store/tests/maintenance.rs:255` (+6 more) |
| `ING-084` | 131 | `MUST` | 15 | `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/tests/conformance_sqlite.rs:14`, `crates/gap-store/tests/tenant_same_ids.rs:595`, `crates/gap-store/tests/traversal.rs:73` (+11 more) |
| `ING-085` | 132 | `MUST` | 11 | `crates/gap-core/src/cursor/mod.rs:613`, `crates/gap-exec/src/traverse/mod.rs:1060`, `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/tests/conformance_sqlite.rs:14` (+7 more) |
| `ING-086` | 133 | `MUST` | 10 | `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/src/sqlite/mod.rs:680`, `crates/gap-store/tests/backup_restore.rs:637`, `crates/gap-store/tests/conformance_sqlite.rs:14` (+6 more) |
| `ING-087` | 134 | `MUST` | 18 | `crates/gap-store/src/conformance.rs:900`, `crates/gap-store/src/sqlite/mod.rs:510`, `crates/gap-store/src/sqlite/mod.rs:680`, `crates/gap-store/src/sqlite/mod.rs:836` (+14 more) |
| `ING-088` | 135 | `MUST` | 9 | `crates/gap-store/src/sqlite/mod.rs:1216`, `crates/gap-store/src/sqlite/plan_tests.rs:799`, `crates/gap-store/src/sqlite/plan_tests.rs:817`, `crates/gap-store/src/sqlite/plan_tests.rs:1085` (+5 more) |

### `spec/06-runtime.md`

| ID | Spec Line | Obligation | Markers | Primary Verification Locations |
|---|---:|---|---:|---|
| `RUN-001` | 17 | `MUST` | 6 | `crates/gap-exec/tests/e2e_and_tower.rs:37`, `crates/gap-exec/tests/e2e_and_tower.rs:315`, `crates/gap-exec/tests/e2e_and_tower.rs:538`, `crates/gap-exec/tests/e2e_and_tower.rs:584` (+2 more) |
| `RUN-002` | 18 | non-MUST | 4 | `crates/gap-transport/src/gateway.rs:476`, `crates/gap-transport/tests/e2e_and_pinning.rs:25`, `crates/gap-transport/tests/gateway_and_security.rs:201`, `crates/gap-transport/tests/status_matrix.rs:485` |
| `RUN-003` | 28 | non-MUST | 8 | `crates/gap-snapshot/src/publisher/mod.rs:1193`, `crates/gap-snapshot/src/publisher/mod.rs:1246`, `crates/gap-snapshot/src/publisher/mod.rs:1296`, `crates/gap-snapshot/tests/handle.rs:106` (+4 more) |
| `RUN-004` | 29 | non-MUST | 3 | `crates/gap-snapshot/src/publisher/mod.rs:1193`, `crates/gap-snapshot/src/publisher/mod.rs:1246`, `crates/gap-snapshot/tests/handle.rs:427` |
| `RUN-005` | 30 | non-MUST | 2 | `crates/gap-plan/tests/cache_and_hostile.rs:27`, `crates/gap-plan/tests/planner_and_ir.rs:492` |
| `RUN-006` | 31 | non-MUST | 1 | `crates/gap-snapshot/src/publisher/mod.rs:1246` |
| `RUN-007` | 32 | non-MUST | 4 | `crates/gap-cli/src/commands/tests.rs:249`, `crates/gap-snapshot/src/rollback/mod.rs:511`, `crates/gap-snapshot/src/rollback/mod.rs:632`, `tests/swap_stress/swap_stress.rs:398` |
| `RUN-008` | 33 | non-MUST | 4 | `crates/gap-snapshot/src/rollback/mod.rs:511`, `tests/swap_stress/swap_stress.rs:115`, `tests/swap_stress/swap_stress.rs:234`, `tests/swap_stress/swap_stress.rs:398` |
| `RUN-009` | 34 | non-MUST | 22 | `crates/gap-core/src/error.rs:992`, `crates/gap-core/src/snapshot/mod.rs:685`, `crates/gap-snapshot/src/diff/tests.rs:88`, `crates/gap-snapshot/src/diff/tests.rs:108` (+18 more) |
| `RUN-010` | 37 | non-MUST | 9 | `crates/gap-plan/src/ir.rs:313`, `crates/gap-plan/src/join/mod.rs:660`, `crates/gap-plan/src/pushdown/mod.rs:599`, `crates/gap-plan/tests/determinism_and_explain.rs:48` (+5 more) |
| `RUN-011` | 38 | non-MUST | 11 | `crates/gap-core/src/error.rs:992`, `crates/gap-core/src/snapshot/mod.rs:685`, `crates/gap-plan/tests/planner_and_ir.rs:129`, `crates/gap-plan/tests/planner_and_ir.rs:153` (+7 more) |
| `RUN-012` | 39 | non-MUST | 5 | `crates/gap-plan/src/join/mod.rs:660`, `crates/gap-plan/src/pushdown/mod.rs:520`, `crates/gap-plan/src/pushdown/mod.rs:550`, `crates/gap-plan/src/pushdown/mod.rs:583` (+1 more) |
| `RUN-013` | 40 | non-MUST | 14 | `crates/gap-core/src/joingraph/mod.rs:364`, `crates/gap-plan/src/ir.rs:313`, `crates/gap-plan/src/join/mod.rs:463`, `crates/gap-plan/src/join/mod.rs:660` (+10 more) |
| `RUN-014` | 41 | non-MUST | 5 | `crates/gap-plan/tests/cache_and_hostile.rs:27`, `crates/gap-plan/tests/cache_and_hostile.rs:121`, `crates/gap-plan/tests/cache_and_hostile.rs:160`, `crates/gap-plan/tests/cache_and_hostile.rs:199` (+1 more) |
| `RUN-015` | 42 | non-MUST | 5 | `crates/gap-exec/tests/bounds_and_hostile.rs:28`, `crates/gap-exec/tests/bounds_and_hostile.rs:75`, `crates/gap-exec/tests/e2e_and_tower.rs:37`, `crates/gap-exec/tests/e2e_and_tower.rs:246` (+1 more) |
| `RUN-020` | 45 | non-MUST | 2 | `crates/gap-exec/src/join/mod.rs:1442`, `crates/gap-exec/src/join/mod.rs:1780` |
| `RUN-021` | 46 | non-MUST | 3 | `crates/gap-core/src/joingraph/mod.rs:384`, `crates/gap-exec/src/join/mod.rs:1506`, `crates/gap-plan/src/join/mod.rs:574` |
| `RUN-022` | 47 | non-MUST | 1 | `crates/gap-exec/src/join/mod.rs:1563` |
| `RUN-023` | 48 | non-MUST | 3 | `crates/gap-exec/src/join/mod.rs:1593`, `crates/gap-plan/src/join/mod.rs:551`, `crates/gap-plan/src/pushdown/mod.rs:599` |
| `RUN-024` | 49 | non-MUST | 4 | `crates/gap-exec/src/join/mod.rs:1663`, `crates/gap-exec/tests/e2e_and_tower.rs:436`, `crates/gap-exec/tests/errors_and_null_matrix.rs:31`, `tests/chaos/chaos.rs:263` |
| `RUN-025` | 50 | non-MUST | 6 | `crates/gap-core/src/cursor/mod.rs:588`, `crates/gap-core/src/cursor/mod.rs:613`, `crates/gap-core/src/cursor/mod.rs:647`, `crates/gap-exec/src/join/mod.rs:1707` (+2 more) |
| `RUN-026` | 51 | non-MUST | 1 | `crates/gap-exec/src/join/mod.rs:1707` |
| `RUN-027` | 52 | non-MUST | 3 | `crates/gap-core/src/joingraph/mod.rs:384`, `crates/gap-plan/src/join/mod.rs:463`, `crates/gap-plan/src/join/mod.rs:551` |
| `RUN-030` | 55 | non-MUST | 7 | `crates/gap-exec/tests/deadlines_and_cancel.rs:51`, `crates/gap-exec/tests/deadlines_and_cancel.rs:141`, `crates/gap-transport/tests/deadlines_and_sse.rs:59`, `crates/gap-transport/tests/deadlines_and_sse.rs:130` (+3 more) |
| `RUN-031` | 56 | non-MUST | 6 | `crates/gap-exec/tests/deadlines_and_cancel.rs:192`, `crates/gap-exec/tests/deadlines_and_cancel.rs:267`, `crates/gap-exec/tests/deadlines_and_cancel.rs:341`, `crates/gap-transport/tests/deadlines_and_sse.rs:130` (+2 more) |
| `RUN-032` | 57 | non-MUST | 2 | `crates/gap-exec/src/bulkhead/mod.rs:215`, `crates/gap-exec/src/bulkhead/mod.rs:252` |
| `RUN-040` | 58 | non-MUST | 16 | `crates/gap-transport/src/shed/mod.rs:539`, `crates/gap-transport/src/shed/mod.rs:582`, `crates/gap-transport/src/shed/mod.rs:599`, `crates/gap-transport/src/shed/mod.rs:671` (+12 more) |
| `RUN-050` | 59 | non-MUST | 8 | `crates/gap-sec/src/quota/tests.rs:12`, `crates/gap-transport/src/ratelimit/mod.rs:334`, `crates/gap-transport/src/ratelimit/mod.rs:369`, `crates/gap-transport/src/ratelimit/mod.rs:396` (+4 more) |
| `RUN-051` | 60 | non-MUST | 1 | `crates/gap-transport/tests/admission.rs:273` |
| `RUN-060` | 63 | `MUST` | 9 | `crates/gap-exec/src/exec/assembly.rs:1422`, `crates/gap-exec/tests/bounds_and_hostile.rs:193`, `crates/gap-exec/tests/bounds_and_hostile.rs:340`, `crates/gap-exec/tests/e2e_and_tower.rs:37` (+5 more) |
| `RUN-061` | 64 | non-MUST | 0 | _none_ |
| `RUN-062` | 65 | non-MUST | 0 | _none_ |
| `RUN-070` | 69 | non-MUST | 1 | `crates/gap-exec/src/mutate/tests.rs:280` |
| `RUN-071` | 70 | non-MUST | 1 | `crates/gap-exec/src/mutate/tests.rs:333` |
| `RUN-072` | 71 | non-MUST | 4 | `crates/gap-exec/src/mutate/tests.rs:523`, `crates/gap-store/src/idempotency/tests.rs:111`, `crates/gap-store/src/idempotency/tests.rs:120`, `crates/gap-store/src/idempotency/tests.rs:165` |
| `RUN-073` | 72 | non-MUST | 1 | `crates/gap-exec/src/mutate/tests.rs:523` |
| `RUN-074` | 73 | non-MUST | 1 | `crates/gap-exec/src/saga/mod.rs:631` |
| `RUN-075` | 74 | non-MUST | 2 | `crates/gap-exec/src/mutate/tests.rs:280`, `crates/gap-exec/src/mutate/tests.rs:523` |
| `RUN-080` | 84 | non-MUST | 6 | `crates/gap-cli/src/commands/tests.rs:389`, `crates/gap-exec/src/tasks/tests.rs:332`, `crates/gap-exec/tests/task_crash_recovery.rs:93`, `crates/gap-store/src/surreal/tests.rs:38` (+2 more) |
| `RUN-081` | 85 | non-MUST | 4 | `crates/gap-exec/tests/task_crash_recovery.rs:93`, `crates/gap-exec/tests/task_crash_recovery.rs:279`, `crates/gap-store/src/surreal/tests.rs:38`, `crates/gap-store/src/tasks/tests.rs:266` |
| `RUN-082` | 86 | non-MUST | 1 | `crates/gap-exec/src/tasks/tests.rs:210` |
| `RUN-083` | 87 | non-MUST | 1 | `crates/gap-exec/src/tasks/tests.rs:400` |
| `RUN-084` | 88 | non-MUST | 1 | `crates/gap-exec/src/tasks/tests.rs:529` |
| `RUN-085` | 89 | non-MUST | 3 | `crates/gap-exec/src/mutate/tests.rs:333`, `crates/gap-exec/src/tasks/tests.rs:598`, `crates/gap-store/src/tasks/tests.rs:266` |
| `RUN-086` | 90 | non-MUST | 3 | `crates/gap-exec/src/tasks/tests.rs:598`, `crates/gap-store/src/tasks/tests.rs:266`, `crates/gap-store/src/tasks/tests.rs:306` |
| `RUN-090` | 100 | non-MUST | 10 | `crates/gap-core/src/telemetry.rs:455`, `crates/gap-server/src/run.rs:652`, `crates/gap-server/tests/e2e.rs:599`, `crates/gap-server/tests/http3_dual_stack.rs:205` (+6 more) |
| `RUN-091` | 101 | non-MUST | 4 | `crates/gap-exec/tests/task_crash_recovery.rs:93`, `tests/soak/soak.rs:465`, `tests/soak/soak.rs:624`, `tests/upgrade/upgrade.rs:284` |

### `spec/07-security.md`

| ID | Spec Line | Obligation | Markers | Primary Verification Locations |
|---|---:|---|---:|---|
| `TM-01` | 13 | non-MUST | 0 | _none_ |
| `TM-02` | 14 | non-MUST | 0 | _none_ |
| `TM-03` | 15 | non-MUST | 0 | _none_ |
| `TM-04` | 16 | non-MUST | 0 | _none_ |
| `TM-05` | 17 | non-MUST | 1 | `crates/gap-sec/src/session_risk/tests.rs:19` |
| `TM-06` | 18 | non-MUST | 0 | _none_ |
| `TM-07` | 19 | non-MUST | 0 | _none_ |
| `TM-08` | 20 | non-MUST | 0 | _none_ |
| `TM-09` | 21 | non-MUST | 0 | _none_ |
| `TM-10` | 22 | non-MUST | 0 | _none_ |
| `TM-11` | 23 | non-MUST | 1 | `crates/gap-sec/src/quota/tests.rs:12` |
| `TM-12` | 24 | non-MUST | 0 | _none_ |
| `TM-13` | 25 | non-MUST | 0 | _none_ |
| `TM-14` | 26 | non-MUST | 4 | `crates/gap-route/src/calibration_job/tests.rs:168`, `crates/gap-route/src/feedback/tests.rs:76`, `crates/gap-store/src/feedback/tests.rs:83`, `crates/gap-store/src/feedback/tests.rs:204` |
| `TM-15` | 27 | non-MUST | 0 | _none_ |
| `TM-16` | 28 | non-MUST | 0 | _none_ |
| `TM-17` | 29 | non-MUST | 2 | `crates/gap-sec/src/audit/tests.rs:33`, `crates/gap-store/src/audit/tests.rs:112` |
| `TM-18` | 30 | non-MUST | 0 | _none_ |
| `TM-19` | 31 | non-MUST | 0 | _none_ |
| `TM-20` | 32 | non-MUST | 0 | _none_ |
| `TM-21` | 33 | non-MUST | 0 | _none_ |
| `TM-22` | 34 | non-MUST | 0 | _none_ |
| `SEC-001` | 37 | non-MUST | 12 | `crates/gap-core/src/config.rs:977`, `crates/gap-sec/src/authn/tests.rs:46`, `crates/gap-sec/src/authn/tests.rs:102`, `crates/gap-sec/src/authn/tests.rs:179` (+8 more) |
| `SEC-002` | 38 | non-MUST | 7 | `crates/gap-cli/tests/cli.rs:407`, `crates/gap-core/src/config.rs:977`, `crates/gap-sec/src/authn/tests.rs:431`, `crates/gap-server/tests/e2e.rs:690` (+3 more) |
| `SEC-003` | 39 | `MUST` | 1 | `crates/gap-sec/src/authn/tests.rs:432` |
| `SEC-004` | 40 | non-MUST | 6 | `crates/gap-sec/src/authn/tests.rs:103`, `crates/gap-sec/src/authn/tests.rs:433`, `crates/gap-server/src/run.rs:667`, `crates/gap-server/tests/e2e.rs:455` (+2 more) |
| `SEC-010` | 48 | `MUST` | 20 | `adapters/gap-a2a/src/tests.rs:290`, `crates/gap-conformance/src/offline.rs:442`, `crates/gap-conformance/tests/spawn.rs:123`, `crates/gap-core/src/context.rs:432` (+16 more) |
| `SEC-011` | 49 | `MUST` | 7 | `adapters/gap-a2a/src/tests.rs:548`, `adapters/gap-ngc-graphql/tests/graphql_connector.rs:1023`, `adapters/gap-ngc-grpc/tests/grpc_connector.rs:1038`, `crates/gap-sec/src/authn/tests.rs:396` (+3 more) |
| `SEC-012` | 50 | non-MUST | 2 | `crates/gap-sec/src/authn/tests.rs:675`, `crates/gap-sec/src/authz/tests.rs:243` |
| `SEC-020` | 53 | non-MUST | 4 | `crates/gap-cli/src/commands/tests.rs:90`, `crates/gap-cli/src/commands/tests.rs:310`, `crates/gap-sec/src/authz/tests.rs:40`, `crates/gap-sec/src/authz/tests.rs:170` |
| `SEC-021` | 54 | non-MUST | 2 | `crates/gap-sec/src/authz/tests.rs:40`, `crates/gap-sec/src/authz/tests.rs:297` |
| `SEC-022` | 55 | non-MUST | 1 | `crates/gap-sec/src/authz/tests.rs:40` |
| `SEC-023` | 56 | non-MUST | 1 | `crates/gap-sec/src/authz/tests.rs:297` |
| `SEC-024` | 57 | non-MUST | 1 | `crates/gap-sec/src/authz/tests.rs:542` |
| `SEC-040` | 72 | non-MUST | 20 | `crates/gap-cli/src/commands/tests.rs:90`, `crates/gap-conformance/src/cli.rs:973`, `crates/gap-conformance/src/driver.rs:832`, `crates/gap-conformance/src/driver.rs:1009` (+16 more) |
| `SEC-041` | 73 | non-MUST | 16 | `crates/gap-conformance/src/driver.rs:832`, `crates/gap-conformance/src/driver.rs:1063`, `crates/gap-conformance/src/driver.rs:1077`, `crates/gap-conformance/src/driver.rs:1107` (+12 more) |
| `SEC-042` | 74 | non-MUST | 5 | `crates/gap-ingest/src/enrich/mod.rs:1592`, `crates/gap-ingest/src/enrich/mod.rs:1621`, `crates/gap-sec/src/vault/tests.rs:191`, `tests/security/secrets/secret_canary.rs:2` (+1 more) |
| `SEC-050` | 77 | non-MUST | 3 | `crates/gap-sec/src/quota/tests.rs:89`, `crates/gap-transport/src/ratelimit/mod.rs:416`, `crates/gap-transport/tests/admission.rs:88` |
| `SEC-060` | 80 | non-MUST | 1 | `crates/gap-sec/src/manifest/tests.rs:48` |
| `SEC-061` | 81 | non-MUST | 6 | `adapters/gap-ngc-cli/src/tests.rs:51`, `crates/gap-sec/src/manifest/tests.rs:48`, `crates/gap-sec/src/manifest/tests.rs:185`, `tests/security/rugpull/rug_pull.rs:2` (+2 more) |
| `SEC-062` | 82 | non-MUST | 3 | `crates/gap-cli/src/commands/tests.rs:90`, `crates/gap-cli/tests/cli.rs:432`, `crates/gap-sec/src/manifest/tests.rs:48` |
| `SEC-070` | 85 | non-MUST | 9 | `adapters/gap-ngc-cli/src/tests.rs:51`, `crates/gap-cli/tests/cli.rs:210`, `crates/gap-sec/src/sandbox/tests.rs:46`, `crates/gap-sec/src/sandbox/tests.rs:88` (+5 more) |
| `SEC-071` | 86 | non-MUST | 4 | `crates/gap-sec/src/sandbox/tests.rs:88`, `tests/security/rugpull/rug_pull.rs:2`, `tests/security/rugpull/rug_pull.rs:205`, `tests/security/sandbox/sandbox_smoke.rs:85` |
| `SEC-072` | 87 | non-MUST | 7 | `crates/gap-cli/src/commands/tests.rs:90`, `crates/gap-cli/tests/cli.rs:262`, `crates/gap-sec/src/egress/tests.rs:328`, `crates/gap-sec/src/sandbox/tests.rs:46` (+3 more) |
| `SEC-075` | 90 | non-MUST | 9 | `adapters/gap-mcp/tests/outbound/real_http.rs:10`, `crates/gap-sec/src/egress/tests.rs:54`, `crates/gap-sec/src/egress/tests.rs:117`, `crates/gap-sec/src/egress/tests.rs:160` (+5 more) |
| `SEC-076` | 91 | non-MUST | 6 | `crates/gap-sec/src/egress/tests.rs:54`, `crates/gap-sec/src/egress/tests.rs:117`, `crates/gap-sec/src/egress/tests.rs:218`, `tests/security/ssrf/ssrf_egress.rs:2` (+2 more) |
| `SEC-080` | 94 | non-MUST | 4 | `adapters/gap-mcp/tests/inbound/tools.rs:15`, `adapters/gap-mcp/tests/inbound/tools.rs:541`, `crates/gap-ingest/src/lint/tests.rs:70`, `tests/security/injection/injection_corpus.rs:223` |
| `SEC-081` | 95 | non-MUST | 3 | `crates/gap-ingest/src/lint/tests.rs:70`, `crates/gap-ingest/src/lint/tests.rs:132`, `tests/security/injection/injection_corpus.rs:223` |
| `SEC-082` | 96 | non-MUST | 6 | `crates/gap-ingest/src/drift/tests.rs:462`, `crates/gap-ingest/src/drift/tests.rs:541`, `crates/gap-snapshot/src/diff/tests.rs:108`, `tests/security/injection/injection_corpus.rs:223` (+2 more) |
| `SEC-083` | 97 | non-MUST | 8 | `adapters/gap-mcp/tests/inbound/conformance.rs:145`, `adapters/gap-mcp/tests/inbound/spotlight.rs:15`, `adapters/gap-mcp/tests/inbound/spotlight.rs:70`, `adapters/gap-mcp/tests/inbound/spotlight.rs:150` (+4 more) |
| `SEC-084` | 98 | non-MUST | 3 | `crates/gap-sec/src/session_risk/tests.rs:19`, `crates/gap-sec/src/session_risk/tests.rs:99`, `tests/security/injection/injection_corpus.rs:306` |
| `SEC-085` | 99 | non-MUST | 2 | `crates/gap-ingest/src/lint/tests.rs:132`, `tests/security/injection/injection_corpus.rs:306` |
| `SEC-090` | 102 | non-MUST | 4 | `crates/gap-ingest/src/proposals/tests.rs:102`, `crates/gap-store/src/audit/tests.rs:23`, `crates/gap-store/src/audit/tests.rs:34`, `crates/gap-store/src/audit/tests.rs:232` |
| `SEC-091` | 103 | non-MUST | 2 | `crates/gap-sec/src/audit/tests.rs:33`, `crates/gap-store/src/audit/tests.rs:23` |
| `SEC-092` | 104 | non-MUST | 5 | `crates/gap-cli/src/commands/tests.rs:344`, `crates/gap-sec/src/audit/tests.rs:132`, `crates/gap-store/src/audit/tests.rs:23`, `crates/gap-store/src/audit/tests.rs:112` (+1 more) |
| `SEC-093` | 105 | non-MUST | 2 | `crates/gap-cli/src/commands/tests.rs:344`, `crates/gap-sec/src/audit/tests.rs:132` |
| `SEC-100` | 108 | non-MUST | 0 | _none_ |
| `SEC-101` | 109 | non-MUST | 0 | _none_ |
| `SEC-102` | 110 | non-MUST | 4 | `crates/gap-cli/src/commands/tests.rs:90`, `crates/gap-cli/src/commands/tests.rs:173`, `crates/gap-models/src/lib.rs:1126`, `crates/gap-models/src/lib.rs:1152` |
| `SEC-103` | 111 | non-MUST | 0 | _none_ |
| `SEC-110` | 114 | non-MUST | 2 | `crates/gap-route/src/feedback/tests.rs:115`, `crates/gap-store/src/feedback/tests.rs:204` |
| `SEC-111` | 115 | non-MUST | 0 | _none_ |

### `spec/08-operations.md`

| ID | Spec Line | Obligation | Markers | Primary Verification Locations |
|---|---:|---|---:|---|
| `OPS-001` | 6 | non-MUST | 30 | `crates/gap-cli/tests/cli.rs:81`, `crates/gap-cli/tests/cli.rs:99`, `crates/gap-cli/tests/cli.rs:159`, `crates/gap-cli/tests/cli.rs:262` (+26 more) |
| `OPS-002` | 7 | non-MUST | 18 | `crates/gap-cli/tests/cli.rs:345`, `crates/gap-cli/tests/cli.rs:369`, `crates/gap-server/tests/e2e.rs:219`, `crates/gap-server/tests/e2e.rs:267` (+14 more) |
| `OPS-005` | 8 | `MUST` | 6 | `crates/gap-core/src/config.rs:1068`, `crates/gap-server/tests/http3_dual_stack.rs:205`, `crates/gap-server/tests/http3_dual_stack.rs:378`, `crates/gap-transport/tests/http3.rs:25` (+2 more) |
| `OPS-003` | 9 | non-MUST | 0 | _none_ |
| `OPS-004` | 10 | non-MUST | 0 | _none_ |
| `OPS-010` | 30 | non-MUST | 9 | `crates/gap-core/src/config.rs:1004`, `crates/gap-core/src/context.rs:471`, `crates/gap-core/src/telemetry.rs:419`, `crates/gap-core/src/telemetry.rs:455` (+5 more) |
| `OPS-011` | 31 | `MUST` | 4 | `crates/gap-core/src/telemetry.rs:369`, `crates/gap-core/src/telemetry.rs:455`, `crates/gap-server/tests/shutdown_and_stdio.rs:336`, `crates/gap-transport/tests/health_metrics_and_spans.rs:275` |
| `OPS-020` | 54 | non-MUST | 7 | `crates/gap-balance/src/breaker.rs:1326`, `crates/gap-balance/src/breaker.rs:2978`, `crates/gap-balance/src/registry.rs:388`, `crates/gap-core/src/telemetry.rs:369` (+3 more) |
| `OPS-030` | 85 | non-MUST | 5 | `tests/chaos/chaos.rs:386`, `tests/soak/soak.rs:269`, `tests/soak/soak.rs:346`, `tests/soak/soak.rs:465` (+1 more) |
| `OPS-040` | 88 | non-MUST | 0 | _none_ |
| `OPS-041` | 89 | non-MUST | 2 | `tests/upgrade/upgrade.rs:159`, `tests/upgrade/upgrade.rs:241` |
| `OPS-042` | 90 | non-MUST | 2 | `crates/gap-store/src/surreal/mod.rs:49`, `crates/gap-store/src/surreal/tests.rs:179` |
| `OPS-043` | 91 | non-MUST | 1 | `tests/upgrade/upgrade.rs:284` |
| `OPS-044` | 92 | non-MUST | 20 | `crates/gap-cli/src/commands/tests.rs:43`, `crates/gap-cli/src/commands/tests.rs:90`, `crates/gap-cli/src/commands/tests.rs:173`, `crates/gap-cli/src/commands/tests.rs:249` (+16 more) |

### `spec/09-compat.md`

| ID | Spec Line | Obligation | Markers | Primary Verification Locations |
|---|---:|---|---:|---|
| `CMP-001` | 7 | `MUST` | 28 | `adapters/gap-mcp/tests/inbound/conformance.rs:145`, `adapters/gap-mcp/tests/inbound/tools.rs:36`, `adapters/gap-mcp/tests/inbound/wire.rs:28`, `adapters/gap-mcp/tests/inbound/wire.rs:86` (+24 more) |
| `CMP-002` | 8 | non-MUST | 20 | `adapters/gap-mcp/tests/inbound/conformance.rs:145`, `adapters/gap-mcp/tests/inbound/discovery.rs:19`, `adapters/gap-mcp/tests/inbound/discovery.rs:84`, `adapters/gap-mcp/tests/inbound/discovery.rs:147` (+16 more) |
| `CMP-003` | 17 | non-MUST | 5 | `adapters/gap-mcp/tests/inbound/conformance.rs:145`, `adapters/gap-mcp/tests/inbound/size_gate.rs:37`, `adapters/gap-mcp/tests/inbound/size_gate.rs:77`, `adapters/gap-mcp/tests/inbound/tools.rs:15` (+1 more) |
| `CMP-004` | 18 | `MUST` | 18 | `adapters/gap-mcp/src/inbound/result.rs:207`, `adapters/gap-mcp/tests/inbound/conformance.rs:145`, `adapters/gap-mcp/tests/inbound/tools.rs:36`, `adapters/gap-mcp/tests/inbound/tools.rs:141` (+14 more) |
| `CMP-005` | 19 | `MUST` | 3 | `adapters/gap-mcp/tests/inbound/conformance.rs:145`, `adapters/gap-mcp/tests/inbound/discovery.rs:392`, `conformance/mcp/15-resources-list-templates-read.json:4` |
| `CMP-006` | 20 | non-MUST | 0 | _none_ |
| `CMP-007` | 21 | non-MUST | 6 | `adapters/gap-mcp/tests/inbound/conformance.rs:145`, `adapters/gap-mcp/tests/inbound/spotlight.rs:15`, `adapters/gap-mcp/tests/inbound/spotlight.rs:70`, `adapters/gap-mcp/tests/inbound/spotlight.rs:150` (+2 more) |
| `CMP-008` | 22 | non-MUST | 5 | `adapters/gap-mcp/tests/inbound/conformance.rs:145`, `adapters/gap-mcp/tests/inbound/size_gate.rs:37`, `adapters/gap-mcp/tests/inbound/size_gate.rs:77`, `conformance/mcp/07-tools-list-static-and-size.json:4` (+1 more) |
| `CMP-009` | 23 | non-MUST | 0 | _none_ |
| `CMP-010` | 26 | non-MUST | 36 | `adapters/gap-mcp/src/outbound/config.rs:127`, `adapters/gap-mcp/src/outbound/jsonrpc.rs:130`, `adapters/gap-mcp/src/outbound/jsonrpc.rs:181`, `adapters/gap-mcp/src/outbound/jsonrpc.rs:212` (+32 more) |
| `CMP-011` | 27 | non-MUST | 12 | `adapters/gap-mcp/src/outbound/map.rs:634`, `adapters/gap-mcp/src/outbound/map.rs:664`, `adapters/gap-mcp/src/outbound/map.rs:697`, `adapters/gap-mcp/src/outbound/map.rs:787` (+8 more) |
| `CMP-012` | 28 | non-MUST | 16 | `adapters/gap-mcp/src/outbound/map.rs:740`, `adapters/gap-mcp/src/outbound/map.rs:771`, `adapters/gap-mcp/src/outbound/map.rs:803`, `adapters/gap-mcp/src/outbound/map.rs:846` (+12 more) |
| `CMP-013` | 29 | non-MUST | 4 | `adapters/gap-mcp/src/outbound/map.rs:771`, `adapters/gap-mcp/src/outbound/session.rs:330`, `adapters/gap-mcp/src/outbound/transport/stdio.rs:523`, `adapters/gap-mcp/tests/outbound/wire.rs:168` |
| `CMP-014` | 30 | non-MUST | 2 | `adapters/gap-mcp/tests/outbound/connector.rs:602`, `crates/gap-ingest/src/extractors/mcp/mod.rs:1570` |
| `CMP-015` | 31 | non-MUST | 2 | `adapters/gap-mcp/tests/outbound/connector.rs:373`, `crates/gap-conformance/tests/ngc_kit_mcp.rs:88` |
| `CMP-020` | 34 | non-MUST | 13 | `adapters/gap-a2a/src/tests.rs:155`, `adapters/gap-a2a/src/tests.rs:201`, `conformance/a2a/001_agent_card_metagraph.json:7`, `crates/gap-ingest/src/extractors/a2a/mod.rs:1177` (+9 more) |
| `CMP-021` | 35 | non-MUST | 9 | `adapters/gap-a2a/src/tests.rs:231`, `adapters/gap-a2a/src/tests.rs:290`, `adapters/gap-a2a/tests/loop_and_wire.rs:381`, `conformance/a2a/002_text_resolve_typename.json:7` (+5 more) |
| `CMP-022` | 36 | non-MUST | 8 | `adapters/gap-a2a/src/tests.rs:548`, `adapters/gap-a2a/tests/loop_and_wire.rs:291`, `adapters/gap-a2a/tests/loop_and_wire.rs:381`, `conformance/a2a/004_task_state_surface.json:7` (+4 more) |
| `CMP-023` | 37 | non-MUST | 2 | `adapters/gap-a2a/tests/loop_and_wire.rs:291`, `conformance/a2a/005_trace_context_surface.json:7` |
| `CMP-025` | 40 | `MUST` | 4 | `adapters/gap-agui/tests/agui_wire.rs:141`, `adapters/gap-agui/tests/agui_wire.rs:390`, `conformance/agui/01_lifecycle_and_interrupt.json:7`, `crates/gap-server/tests/assemble.rs:767` |
| `CMP-026` | 41 | `MUST` | 2 | `adapters/gap-agui/tests/agui_wire.rs:203`, `conformance/agui/01_lifecycle_and_interrupt.json:8` |
| `CMP-027` | 42 | `MUST` | 2 | `adapters/gap-agui/tests/agui_wire.rs:243`, `conformance/agui/01_lifecycle_and_interrupt.json:9` |
| `CMP-028` | 43 | `MUST` | 2 | `adapters/gap-agui/tests/agui_wire.rs:346`, `conformance/agui/01_lifecycle_and_interrupt.json:10` |
| `CMP-030` | 50 | non-MUST | 24 | `adapters/gap-ngc-openapi/src/auth.rs:568`, `adapters/gap-ngc-openapi/src/connector.rs:1753`, `adapters/gap-ngc-openapi/src/connector.rs:1896`, `adapters/gap-ngc-openapi/src/connector.rs:2056` (+20 more) |
| `CMP-031` | 51 | non-MUST | 7 | `adapters/gap-ngc-openapi/src/auth.rs:467`, `adapters/gap-ngc-openapi/src/auth.rs:568`, `adapters/gap-ngc-openapi/src/connector.rs:1753`, `adapters/gap-ngc-openapi/src/connector.rs:1896` (+3 more) |
| `CMP-032` | 52 | non-MUST | 15 | `adapters/gap-ngc-openapi/src/connector.rs:2720`, `adapters/gap-ngc-openapi/src/connector.rs:3162`, `adapters/gap-ngc-openapi/src/connector.rs:3241`, `adapters/gap-ngc-openapi/src/connector.rs:3419` (+11 more) |
| `CMP-033` | 53 | non-MUST | 10 | `adapters/gap-ngc-openapi/src/connector.rs:2182`, `adapters/gap-ngc-openapi/src/connector.rs:2409`, `adapters/gap-ngc-openapi/src/connector.rs:3927`, `adapters/gap-ngc-openapi/src/connector.rs:3990` (+6 more) |
| `CMP-050` | 55 | non-MUST | 17 | `adapters/gap-ngc-graphql/tests/graphql_connector.rs:433`, `adapters/gap-ngc-graphql/tests/graphql_connector.rs:548`, `adapters/gap-ngc-graphql/tests/graphql_connector.rs:591`, `adapters/gap-ngc-graphql/tests/graphql_connector.rs:758` (+13 more) |
| `CMP-051` | 56 | non-MUST | 2 | `adapters/gap-ngc-sql/tests/sql_connector.rs:617`, `crates/gap-conformance/tests/ngc_kit_sql.rs:121` |
| `CMP-052` | 57 | non-MUST | 4 | `adapters/gap-ngc-sql/src/template.rs:727`, `adapters/gap-ngc-sql/tests/sql_connector.rs:263`, `adapters/gap-ngc-sql/tests/sql_connector.rs:368`, `crates/gap-conformance/tests/ngc_kit_sql.rs:121` |
| `CMP-060` | 63 | non-MUST | 3 | `adapters/gap-ngc-cli/src/tests.rs:51`, `crates/gap-sec/src/manifest/tests.rs:185`, `tests/security/sandbox/sandbox_smoke.rs:282` |
