# GAP Threat Model Review & Security Matrix Verification (`WP-M6-13`)

- **Gate / Work Package:** `WP-M6-13` (`SEC-100`, `SEC-101`, `SEC-103`, `TM-22`)
- **Review Date:** 2026-11-15
- **Reviewed Baseline Commit:** `main@83b9466` (`WP-M6-01`..`WP-M6-12` + `INV-02` lock-free fix on `gap-exec`)
- **Status:** **`PASS` (with documented offline / host-environment notes)**

---

## 1. Provenance (`VER-030`)

| Field | Measured Value |
|---|---|
| `Cargo.lock` SHA-256 | `96537407ab8e0eeb958d5659ea72997140da28bd01eb3bd3fb74fc4b1005a531` |
| Rust / Cargo | `rustc 1.98.1 (48a229cea 2026-09-01)` / `cargo 1.98.1 (797e8a9bc 2026-08-05)` |
| `cargo-nextest` / `cargo-deny` | `cargo-nextest 0.9.149` / `cargo-deny 0.20.2` |
| Host OS / Architecture | `macOS 26.7.3` (`BuildVersion 25G342`) / `Intel(R) Core(TM) i9-9980HK CPU @ 2.40GHz` (`16` logical cores, `64 GiB` RAM) |
| Workspace Cargo Profile | `--locked --offline` using vendored crates (`.local/vendor`) |

---

## 2. Mandatory 17-Scenario Adversarial Test Matrix (`spec/07-security.md` §15)

Every scenario in `spec/07-security.md` §15 was executed on `main@83b9466` (offline workspace suite) with `0` failures and `0` relaxed assertions:

| # | Scenario | Threat / Spec IDs | Automated Test Path & Function | Observed Result |
|---:|---|---|---|---|
| 1 | **Poisoned description corpus** (>= 50 crafted payloads + unicode tricks) | `TM-01`, `SEC-080`, `SEC-081`, `RTE-042` | [`tests/security/injection/injection_corpus.rs`](../../tests/security/injection/injection_corpus.rs) (`poisoned_description_corpus_blocked_from_models_and_benign_fp_rate_within_budget`) & [`crates/gap-ingest/src/lint/tests.rs`](../../crates/gap-ingest/src/lint/tests.rs) (`lint_detects_exfil_urls_long_base64_unicode_tags_and_filters_s1_enricher`) | **PASS** |
| 2 | **Rug-pull mock server** (`desc_hash` & signature mutation) | `TM-02`, `ING-052`, `ING-053`, `ING-054`, `SEC-082` | [`tests/security/rugpull/rug_pull.rs`](../../tests/security/rugpull/rug_pull.rs) (`rug_pull_mcp_description_and_signature_mutation_keeps_pinned_baseline_and_triggers_drift`) & [`crates/gap-ingest/src/drift/tests.rs`](../../crates/gap-ingest/src/drift/tests.rs) (`drift_description_change_sets_drift_serves_pinned_and_prevents_poisoning`) | **PASS** |
| 3 | **Cross-server shadowing / collision** | `TM-03`, `ADR-006` | [`crates/gap-snapshot/tests/compose.rs`](../../crates/gap-snapshot/tests/compose.rs) (`compose_rejects_ambiguous_root_field_and_colliding_namespaced_types_naming_both_branches`) | **PASS** |
| 4 | **Confused deputy / token passthrough canary** | `TM-04`, `SEC-011`, `NGC-081`, `INV-15` | [`crates/gap-sec/src/authn/tests.rs`](../../crates/gap-sec/src/authn/tests.rs) (`authn_delegation_chain_tenant_isolation_and_rfc8693_token_exchange`), [`adapters/gap-a2a/src/tests.rs`](../../adapters/gap-a2a/src/tests.rs) (`outbound_connector_exposes_tasks_strips_bearer_echoes_tenant_and_backs_off`), [`adapters/gap-ngc-graphql/tests/graphql_connector.rs`](../../adapters/gap-ngc-graphql/tests/graphql_connector.rs) (`inv_15_graphql_credentials_redaction_and_no_bearer_passthrough`) | **PASS** |
| 5 | **Injection-in-results + Rule-of-Two** | `TM-05`, `SEC-083`, `SEC-084` | [`tests/security/injection/injection_corpus.rs`](../../tests/security/injection/injection_corpus.rs) (`injection_in_results_spotlighting_and_rule_of_two_approval_end_to_end`) & [`crates/gap-sec/src/session_risk/tests.rs`](../../crates/gap-sec/src/session_risk/tests.rs) (`rule_of_two_triggers_risk_100_and_forces_cedar_four_eyes_approval`) | **PASS** |
| 6 | **SSRF table & DNS rebinding** | `TM-06`, `SEC-075`, `SEC-076` | [`tests/security/ssrf/ssrf_egress.rs`](../../tests/security/ssrf/ssrf_egress.rs) (`ssrf_table_blocks_all_hostile_vectors_across_formats`, `dns_rebinding_first_lookup_pins_socket_and_second_lookup_rejects_private_ip`) | **PASS** |
| 7 | **Sandbox escape smoke & CLI containment** | `TM-07`, `SEC-070`, `SEC-071`, `SEC-062` | [`tests/security/sandbox/sandbox_smoke.rs`](../../tests/security/sandbox/sandbox_smoke.rs) (`sandbox_doctor_fail_closed_and_escape_smoke`) & [`tests/security/rugpull/rug_pull.rs`](../../tests/security/rugpull/rug_pull.rs) (`malicious_cli_binary_swap_and_stdout_flood_and_hang_are_contained`) | **PASS** |
| 8 | **SQL & argv injection** | `TM-08`, `CMP-050`, `CMP-060` | [`adapters/gap-ngc-sql/tests/sql_connector.rs`](../../adapters/gap-ngc-sql/tests/sql_connector.rs) (`adversarial_sql_injection_and_read_only_guards_hold`) & [`adapters/gap-ngc-cli/src/tests.rs`](../../adapters/gap-ngc-cli/src/tests.rs) (`cli_argv_template_blocks_option_injection_and_executes_sandboxed_connector`) | **PASS** |
| 9 | **Cross-tenant fuzz & `TenantScope`** | `TM-09`, `INV-09`, `INV-19`, `SEC-010` | [`tests/security/tenant/tenant_isolation.rs`](../../tests/security/tenant/tenant_isolation.rs) (`fuzz_cross_tenant_isolation_across_stores_caches_telemetry_and_authz`, `sec_010_untrusted_x_tenant_and_body_hints_ignored_unless_impersonate`, `inv_19_compile_fail_proves_store_apis_require_non_forgeable_tenant_scope`) | **PASS** |
| 10 | **Existence oracle** (byte-identical errors/discovery) | `TM-10`, `GQL-062`, `RTE-010`, `INV-05` | [`crates/gap-ql/src/visibility.rs`](../../crates/gap-ql/src/visibility.rs) (`hidden_field_and_unknown_field_produce_byte_identical_errors_and_suggestions`, `prop_hidden_field_errors_are_byte_identical_to_nonexistent_fields_and_leak_nothing`) & [`adapters/gap-mcp/tests/inbound/discovery.rs`](../../adapters/gap-mcp/tests/inbound/discovery.rs) (`inv_05_hide_t2_makes_describe_and_search_byte_identical_to_snapshot_without_t2_branch`) | **PASS** |
| 11 | **Resource exhaustion & hostile specs** | `TM-11`, `TM-12`, `GQL-041`, `SEC-050`, `RUN-040`, `RUN-050`, `ING-012`, `ING-070`, `ING-071` | [`crates/gap-ql/src/cost.rs`](../../crates/gap-ql/src/cost.rs) (`request_limits_reject_adversarial_inputs_with_computed_vs_max_messages`), [`crates/gap-sec/src/quota/tests.rs`](../../crates/gap-sec/src/quota/tests.rs) (`quota_token_bucket_rate_and_cost_unit_budgets_enforce_and_reconcile`), [`crates/gap-ngc-sdk/tests/transports.rs`](../../crates/gap-ngc-sdk/tests/transports.rs) (`http_enforces_405_404_415_413_slow_loris_ndjson_sse_retry_after_and_metadata`), [`crates/gap-ingest/src/extractors/openapi/mod.rs`](../../crates/gap-ingest/src/extractors/openapi/mod.rs) (`extractor_openapi_handles_hostile_specs_and_rejects_external_refs`), [`crates/gap-ingest/src/worker/mod.rs`](../../crates/gap-ingest/src/worker/mod.rs) (`worker_crash_and_panic_yield_failed_report_without_crashing_pool`, `worker_timeout_terminates_and_records_timeout_failure`) | **PASS** |
| 12 | **Authorization-by-join** | `TM-21`, `SEC-021`, `SEC-023` | [`crates/gap-sec/src/authz/tests.rs`](../../crates/gap-sec/src/authz/tests.rs) (`authz_layer_joined_rhs_denial_nulls_without_leaking_policy_ids_and_redacts_pii`) | **PASS** |
| 13 | **Approval replay / forgery** | `TM-15`, `TM-16`, `RUN-070`, `RUN-071`, `NGC-032` | [`crates/gap-exec/src/mutate/tests.rs`](../../crates/gap-exec/src/mutate/tests.rs) (`mutate_approval_gate_four_eyes_expiry_and_signed_proof_verification`) | **PASS** |
| 14 | **Audit tamper & anchor truncation** | `TM-17`, `SEC-090`, `SEC-091`, `SEC-092` | [`crates/gap-store/src/audit/tests.rs`](../../crates/gap-store/src/audit/tests.rs) (`audit_tamper_modify_delete_reorder_and_corrupt_prev_pinpoint_exact_seq`) & [`crates/gap-sec/src/audit/tests.rs`](../../crates/gap-sec/src/audit/tests.rs) (`audit_anchor_ed25519_sinks_cadence_and_truncation_detection`) | **PASS** |
| 15 | **Secret canary** across logs/traces/errors/metrics/model inputs | `TM-18`, `SEC-041`, `SEC-042`, `INV-15` | [`tests/security/secrets/secret_canary.rs`](../../tests/security/secrets/secret_canary.rs) (`secret_canary_never_appears_in_logs_traces_errors_metrics_or_enricher_inputs`) & [`crates/gap-conformance/tests/canary_principals.rs`](../../crates/gap-conformance/tests/canary_principals.rs) (`inv_15_principal_bearer_tokens_never_appear_in_logs_errors_debug_or_report`) | **PASS** |
| 16 | **Peer loop / amplification** | `TM-20`, `INV-20`, `RUN-083` | [`adapters/gap-a2a/tests/loop_and_wire.rs`](../../adapters/gap-a2a/tests/loop_and_wire.rs) (`inv_20_two_gap_nodes_mutual_delegation_terminates_with_loop_detected`) & [`crates/gap-exec/src/tasks/tests.rs`](../../crates/gap-exec/src/tasks/tests.rs) (`task_deadline_hop_limit_and_trace_loop_detection`) | **PASS** |
| 17 | **Policy staleness & plan-cache key invalidation** | `TM-22`, `SEC-020`, `INV-01`, `INV-09` | [`crates/gap-sec/src/authz/tests.rs`](../../crates/gap-sec/src/authz/tests.rs) (`policy_store_atomic_reload_failure_retains_previous_and_version_invalidates_plan`) & [`crates/gap-plan/tests/cache_and_hostile.rs`](../../crates/gap-plan/tests/cache_and_hostile.rs) (`run_014_inv_01_inv_09_plan_cache_hit_miss_and_key_invalidation_matrix`) | **PASS** |

Additional threat rows outside §15 (`TM-13`, `TM-14`, `TM-19`) remain continuously covered by M4/M5/M6 verification:
- **`TM-13` (Adversarial queries steering routing):** Verified by M5 RouteBench & calibration coverage (`RTE-042`, `GQL-012`, `INV-04`: routing never participates in authz allow/deny decisions).
- **`TM-14` (Poisoned feedback / calibration data):** Verified in `crates/gap-store/src/feedback/` and `crates/gap-route/src/calibration_job/` (`RTE-110` server-observed outcomes, audited `USER_LABEL`, winsorized NLL).
- **`TM-19` (Supply chain — crates, models, containers):** Audited in §4 below (`SEC-100`, `SEC-101`, `SEC-102`, `SEC-103`).

---

## 3. Threat Model & Boundary Review Summary (`TM-01`..`TM-22`)

| Trust Boundary | Core Controls Verified in M6 | Verification Evidence |
|---|---|---|
| **B1: Client <-> Gateway** | OAuth 2.1 JWT (`RS256`/`EdDSA`, `alg=none` rejected) + RFC 7662 introspection + RFC 9728 PRM (`SEC-001`), mTLS SPIFFE SAN + Argon2id/BLAKE3 API keys (`SEC-002`), constant-shape auth failures (`SEC-003`), untrusted `X-Tenant` / body header stripping without `admin:impersonate` (`SEC-010`), quota & token buckets (`SEC-050`), request depth/cost caps (`GQL-041`). | `gap-sec::authn`, `gap-transport::gateway_and_security`, `gap-sec::quota` |
| **B2: Gateway <-> Branches** | Zero client bearer forwarding (`SEC-011`, `INV-15`), RFC 8693 token exchange (`NGC-081`), DNS-pinning `EgressGuard` (`SEC-075`, `SEC-076`), zeroizing `Vault` secrets (`SEC-040`), fail-closed OS sandbox + `exec_allowlist` SHA-256 verification (`SEC-062`, `SEC-070`, `SEC-071`), Ed25519 `ApprovalProof` bound to `(operation, args_hash, idempotency_key)` + four-eyes gate (`RUN-071`, `NGC-032`). | `ssrf_egress`, `sandbox_smoke`, `mutate::tests`, `canary_principals` |
| **B3: Ingestion <-> Untrusted Specs** | Worker timeout/crash isolation (`ING-070`, `ING-071`), hostile OpenAPI/GraphQL/Proto/SQL bounds (`ING-012`), injection linter + S1 exclusion (`SEC-080`, `SEC-081`), `desc_hash` pinning & drift detection (`ING-052`..`ING-054`, `SEC-082`). | `rug_pull`, `injection_corpus`, `gap-ingest` worker & lint suites |
| **B4: Admin <-> Control Plane** | Separate `/admin/*` listener scopes (`SEC-021`), atomic Cedar schema-validated policy hot-reload with rollback-on-parse-error (`SEC-020`, `TM-22`), BLAKE3 tamper-evident per-tenant/system audit hash chains + Ed25519 external anchor sinks + PROV-O export (`SEC-090`..`SEC-093`). | `gap-sec::authz`, `gap-store::audit`, `gap-sec::audit` |
| **B5: GAP Node <-> Peer GAP/A2A Nodes** | Hop limit + trace-path cycle rejection (`INV-20`, `TM-20`), JWS agent card signature verification, header bearer stripping (`SEC-011`), SSRF egress pinning on peer URLs. | `gap-a2a` loop & outbound connector suites, `gap-exec::tasks` |
| **B6: Host <-> Model Weights & Enrichers** | `models/manifest.toml` digest schema (`SEC-102`), enricher input scrubbing (`SEC-042`), optional `INV-17` S1 baseline fallback (`routing.model = "baseline"`). | `gap-models`, `gap-ingest::lint`, `secret_canary` |

---

## 4. Supply Chain, Manifest & License Audit (`SEC-100`, `SEC-101`, `SEC-102`, `SEC-103`)

- **`cargo deny --offline check licenses bans sources`:** Exit `0` — `bans ok, licenses ok, sources ok` (`deny.toml` enforces Apache-2.0 / MIT / BSD / Unicode-3.0 / Zlib allowlist and explicitly bans OpenSSL, native-tls, and default-build `surrealdb` BSL-1.1).
- **`cargo deny --offline check` (including advisories):** Exited `1` strictly due to offline air-gapped environment lacking cached `~/.cargo/advisory-dbs/advisory-db-3157b0e258782691` (`fatal: cannot change to '...': No such file or directory`). Online RustSec advisory sweep remains a mandatory CI online gate per `SEC-100` and `WP-M7-10`.
- **Vendored Third-Party Manifest (`SEC-103`):** Python SHA-256 and byte-size audit over [`third_party/MANIFEST.toml`](../../third_party/MANIFEST.toml) verified **`8` bundles / `224` files checked: `0` missing, `0` SHA-256 or byte mismatches, `0` extra untracked files on disk**.
- **Model Manifest (`SEC-102`):** Inspected [`models/manifest.toml`](../../models/manifest.toml): `bge-small-en-v1.5` has pinned HuggingFace commit `5c38ec7c405ec4b44b94cc5a9bb96e735b38267a` and concrete SHA-256 (`133,466,304` bytes, MIT); `laya-typed-decisions`, `laya-base`, and `laya-multilingual` carry explicit `sha256_verification_status = "offline-unpinned-requires-online-verify"` and `routing.model = "baseline"` keeps S1 disabled by default (`INV-17`).
- **Release Pipeline (`SEC-101`):** Tracked in `WP-M7-08` (reproducible `--locked` + `SOURCE_DATE_EPOCH` binaries, CycloneDX SBOM, `cosign`, SLSA provenance).

---

## 5. Residual Risks & Owners

| Risk ID | Description | Severity | Mitigation / Current Control | Owner | Target Resolution |
|---|---|---|---|---|---|
| `RR-01` | Air-gapped dev runs lack live RustSec advisory database sync (`cargo deny check advisories` exited `1` locally) | Medium | `licenses`, `bans`, `sources` pass `0`; online CI workflow runs full `cargo deny check` + `cargo audit` before any release tag (`SEC-100`) | Release & Security Engineering | Online CI (`WP-M7-08` / `WP-M7-10`) |
| `RR-02` | External third-party penetration test & 24 h continuous libFuzzer / ASan / TSan / Miri runs not yet executed | Medium | In-repo 17-scenario adversarial suite passes; `WP-M7-06` seeds fuzz corpora, documents sanitizer/miri constraints, and delivers `docs/security/pentest-scope.md` | Security Lead | `WP-M7-06` / `WP-M7-10` |
| `RR-03` | `models/manifest.toml` Laya model revisions carry `offline-unpinned-requires-online-verify` metadata and S1 Gate `G-S1` failed (`routing.model = "baseline"` default) | Low | `INV-17` fail-safe baseline router active; any non-baseline deployment requires `gap models pull` + online weight hash verification (`SEC-102`) | ML / Routing Owner | Before enabling `s1` in prod (`WP-M7-08` / release) |
| `RR-04` | macOS `sandbox-exec` relies on legacy Seatbelt profiles whereas Linux deployments require `bwrap`/`nsjail` (`SEC-070`); host without sandbox fails closed on `T2` (`SEC-071`) | Low | `SEC-071` fail-closed behavior tested in `sandbox_smoke`; `gap doctor` checks sandbox readiness (`SEC-072`) | Runtime Security Owner | Verified on Linux CI in `WP-M7-07` / `WP-M7-08` |
| `RR-05` | Multi-node active-active deployment without external DB relies on SQLite active/standby snapshot shipping (`OPS-003`); `store-surreal` is optional feature only | Low | Single-node `SQLite` store enforces tenant scoping & WAL durability; `WP-M7-11` adds optional `store-surreal` feature (`ADR-020`) | Storage & Platform Owner | Post-1.0 Postgres backend (`OPS-003`) |
