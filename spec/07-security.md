# GAP Specification — 07 Security

Normative IDs `SEC-*`. Rationale: ADR-013, 014, 015, 018. Security posture: **default deny, deterministic authorization, untrusted-by-default content, defense in depth**. No model output participates in an allow/deny decision (INV-04).

## 1. Assets and trust boundaries
Assets: tenant data reachable through branches; branch credentials; policies; approval authority; audit log; snapshot/registry integrity; model artifacts; availability.
Boundaries: (B1) client ↔ gateway; (B2) gateway ↔ connectors/branches; (B3) ingestion workers ↔ untrusted specs/servers/binaries; (B4) control-plane admins ↔ data plane; (B5) GAP node ↔ peer GAP/A2A nodes; (B6) host ↔ model weights/external model providers.

## 2. Threat model (STRIDE + agent/MCP-specific)
Every row has an owner requirement and ≥ 1 automated test (§15).
| ID | Threat | Mitigations (requirements) |
|---|---|---|
| TM-01 | **Tool poisoning**: hidden instructions in tool/operation descriptions or schemas | sanitize + lint (SEC-080/081), descriptions are data (never in trusted prompts), pinned hash (SEC-082), S1 sees only option text without ids (RTE-042), quarantine T2 (ADR-013) |
| TM-02 | **Rug pull**: server changes tool description/behavior after approval | `desc_hash` pinning, DRIFT state, re-approval (ING-052/053), alert; test ING-054 |
| TM-03 | **Cross-server shadowing / name collision** | per-branch namespaces; composition rejects duplicate fully-qualified names; groups only via verified replica evidence (ADR-006) |
| TM-04 | **Confused deputy / token passthrough** | no client token forwarding (SEC-011); RFC 8693 exchange with audience binding (NGC-081) |
| TM-05 | **Prompt injection via tool results/documents** | provenance + trust labels on results, optional spotlighting (SEC-083), "Rule of Two" session policy (SEC-084), approval for side effects |
| TM-06 | **SSRF** (spec URLs, `servers[]`, redirects, DNS rebinding, metadata IPs) | egress guard (SEC-075) |
| TM-07 | **RCE** via launching MCP servers/CLIs, parser bugs | allow-list + hash (ADR-013), sandbox (SEC-070), worker isolation (ING-070), fuzzing (ING-012) |
| TM-08 | **Injection into backends** (SQL, shell, URL path) | parameterized access only; SQL connector read-only role + statement allow-list (CMP-050); CLI args passed as argv array, never via shell (CMP-060) |
| TM-09 | **Cross-tenant leakage** | tenant in every key (INV-09), `TenantScope` type (INV-19), cross-tenant property tests |
| TM-10 | **Existence oracle via discovery/validation errors** | visibility filtering (GQL-062, RTE-010) |
| TM-11 | **DoS by expensive queries** | depth/cost limits (GQL-041), quotas (SEC-050), shedding (RUN-040), deadlines (RUN-030) |
| TM-12 | **DoS via ingestion** (huge/hostile specs) | limits + isolation (ING-012/070/071) |
| TM-13 | **Adversarial queries steering routing** | discovery is advisory; authz is independent; sanitizer; conformal abstention; skeleton validated (GQL-012) |
| TM-14 | **Poisoned feedback/calibration data** | feedback derived server-side from observed executions only (RTE-110); admin labels audited; robust fitting (winsorized NLL), minimum counts |
| TM-15 | **Unapproved irreversible action** | approval gate + four-eyes (RUN-070/071), signed ApprovalProof (NGC-032) |
| TM-16 | **Approval replay / forgery** | proofs bound to `(operation, args_hash, idempotency_key)`, single use, expiry; signature verified by high-risk connectors |
| TM-17 | **Audit tampering** | hash chain + external anchor (SEC-090..092) |
| TM-18 | **Secrets in logs/traces/errors/model inputs** | redaction layer (SEC-041), canary-secret tests (INV-15) |
| TM-19 | **Supply chain** (crates, models, containers) | SEC-100..103 |
| TM-20 | **Peer loops / amplification** | hop limit and trace path (INV-20) |
| TM-21 | **Authorization-by-join** (RHS data exposed to a principal who may only see LHS) | field-level authz applies to RHS fields after join planning; planner requires `Execute` permit on every touched field/branch (SEC-021) |
| TM-22 | **Stale policy** | `policy_version` in plan-cache key and in every decision log; hot reload atomic |

## 3. Authentication
- **SEC-001** The gateway is an OAuth 2.1 resource server: validates JWT access tokens (`iss`, `aud` equal to the configured resource identifier, `exp`, `nbf`, signature via cached JWKS with rotation, `alg` allow-list excluding `none`/HS* for public issuers) or opaque tokens via introspection (RFC 7662). It publishes Protected Resource Metadata (RFC 9728) at `/.well-known/oauth-protected-resource` and honors resource indicators (RFC 8707). Clock skew tolerance ≤ 60 s.
- **SEC-002** Other credentials: mTLS client certs mapped to service principals (SPIFFE ID in SAN), and API keys for dev/CI (stored as Argon2id hashes, scoped, expiring). Anonymous access is disabled unless `auth.allow_anonymous=true` AND the profile is `dev`.
- **SEC-003** Failed authentication MUST NOT reveal which factor failed; responses are constant-shape; rate-limited per source IP.
- **SEC-004** MCP facade authorization follows the MCP authorization spec (OAuth 2.1 + 9728 discovery); the stdio MCP facade inherits the OS user as a `Service` principal with scopes from config.

## 4. Principals, tenants, delegation
```rust
pub struct Principal { pub id: PrincipalId, pub kind: Human|Agent|Service, pub tenant: TenantId,
                       pub scopes: BTreeSet<Scope>, pub attrs: BTreeMap<String, AttrValue>,
                       pub delegation: Vec<ActorRef> /* outermost first: agent acting for user */ }
```
- **SEC-010** `tenant` and `principal` are derived exclusively from authentication (token claims / cert); values in request bodies or headers (`X-Tenant`) MUST be ignored unless the authenticated principal has `admin:impersonate` (audited).
- **SEC-011** *No token passthrough:* the client's bearer token MUST NOT be sent to any branch, peer or model provider. Delegated access uses RFC 8693 token exchange or connector credentials.
- **SEC-012** *Delegation = intersection:* when an agent acts for a human, an action is allowed iff policy permits it for the actor **and** for the subject; the decision log records both.

## 5. Authorization (Cedar)
- **SEC-020** Policies are Cedar (`cedar-policy`), schema-validated at load; store in `policies/*.cedar` + `policies/schema.cedarschema`; evaluated in-process; default deny; `policy_version` = blake3 of the policy set; publish is atomic. Load failure keeps the previous set.
- **SEC-021** *Evaluation points and actions:* `Discover` (per element; discovery filter and introspection), `Execute` (per field/operation touched by the plan — **all** fields, including joined RHS fields), `Traverse` (graph node/edge types and tenant scopes), `Mutate`, `SubmitTask`, `DecideApproval`, `CancelTask`, `ProposeSource`, `DecideProposal`, `Admin*` (`schema`, `ingest`, `impersonate`, `policy`).
- **SEC-022** Resource attributes available to policies: `tenant`, `trust` (T0–T2), `sideEffect`, `group`, `branch`, `classification` (from `@pii`/`@auth`), `status`. Context: `time`, `hop`, `riskScore` (deterministic function of session flags, see SEC-084), `approved: bool`, `approverIsDifferent: bool`.
- **SEC-023** On deny: for queries the denied field is `null` with error `GAP_FORBIDDEN` (path) when the field is nullable and `policy.on_denied = NULL_WITH_ERROR` (default for queries); otherwise the document fails. Mutations always fail whole. Error text is generic; the precise rule id is only in the audit log.
- **SEC-024** Decision log: every deny, every `Mutate`/`Admin*` allow, and a sampled (1%) subset of read allows records `(request_id, principal chain, action, resource, decision, policy_version, determining policy ids)`.
```cedar
// default deny. Read-only agents may discover and run side-effect-free operations of trusted branches.
permit (principal in Group::"agents-readonly", action in [Action::"Discover", Action::"Execute"], resource)
when { resource.sideEffect == "NONE" && resource.trust != "T2" && principal.tenant == resource.tenant };

// Nobody mutates through T2 branches.
forbid (principal, action == Action::"Mutate", resource) when { resource.trust == "T2" };

// Irreversible/unknown mutations need an approved context from a different person.
forbid (principal, action == Action::"Mutate", resource)
when { (resource.sideEffect == "IRREVERSIBLE" || resource.sideEffect == "UNKNOWN") && !(context.approved && context.approverIsDifferent) };
```

## 6. Secrets and credentials
- **SEC-040** `Vault` trait with providers: `env`, `file` (0600 enforced), OS keychain (macOS Keychain / Secret Service), external (HashiCorp Vault, cloud KMS) behind features. Secrets are fetched at connection time, held as `secrecy::SecretBox` with `zeroize`, never serialized, rotated without restart (watch + TTL).
- **SEC-041** *Redaction:* log/trace/error/metric emission passes a redactor with (a) typed `Secret<T>` that formats as `***`, (b) pattern rules (bearer tokens, JWTs, private keys, cloud key prefixes) applied to free text, (c) tests that inject canary secrets through every input path and assert they never appear in any sink (INV-15).
- **SEC-042** Model providers for the Enricher receive only sanitized, size-capped source text (ING-032/033) and never branch credentials or end-user tokens.

## 7. Quotas
- **SEC-050** Quotas: per `(tenant, principal)` rate (RUN-050), per `(tenant, branch)` concurrency share (≤ 50% of branch capacity by default), per-tenant ingestion concurrency, per-tenant storage (store rows, blobs), per-tenant model-inference budget (S1 calls/min). Over-quota ⇒ `GAP_RATE_LIMITED`.

## 8. Trust tiers and manifests
- **SEC-060** *Signed manifest (T0):* `branch.manifest.json` (branch id, connector endpoint(s), allowed operations pattern, `spec_hash`, `desc_hash` list root, side-effect declarations, issuer, `not_before/not_after`) signed with Ed25519 (`ed25519-dalek`); trust anchors configured in `trust.org_keys[]`. Verification failure ⇒ source treated as T2.
- **SEC-061** T1 = admin-approved: approval record stores the pinned hashes; T2 = everything else (discovered/unsigned/unknown). Tier can only be raised by `decideProposal` with `admin:ingest`; lowering is automatic on drift or signature failure.
- **SEC-062** Executable branch launch (MCP stdio, CLI) only from `trust.exec_allowlist[]` entries `{path (absolute), sha256, args_template, env_allowlist}`; the hash is re-checked at every launch.

## 9. Sandbox
- **SEC-070** All process-based connectors, CLI extraction, and optional code execution run inside a sandbox: macOS `sandbox-exec` (seatbelt) profile / Linux `bwrap` (or `nsjail`) with: read-only root, writable tmpfs work dir, no network (unless the branch is declared network-capable and the egress guard proxy is the only route), `RLIMIT_CPU/AS/NOFILE/NPROC`, wall-clock timeout, stdout/stderr size caps (default 1 MiB), dropped capabilities, `no_new_privs`, and a scrubbed environment (`env_allowlist`).
- **SEC-071** If no supported sandbox is available, T2 process branches are refused (fail closed), T1 requires explicit `sandbox.allow_unsandboxed = true` (logged as a prominent warning).
- **SEC-072** `gap doctor` verifies sandbox availability and egress guard self-tests.

## 10. Egress control (SSRF guard)
- **SEC-075** All outbound HTTP(S)/gRPC from connectors, extractors, enrichers and the fetcher goes through `EgressGuard`: resolve DNS → reject loopback, link-local, private (RFC 1918/4193), CGNAT, multicast, cloud metadata (169.254.169.254, fd00:ec2::254, `metadata.google.internal`), and IPv4-mapped IPv6 forms of those; connect to the **resolved IP** (DNS pinning, no re-resolution between check and connect); re-apply on every redirect (max 3, no scheme downgrade); only `https` (and `http` for explicitly allow-listed local dev hosts); deny userinfo in URLs; allow-list overrides per branch (`egress.allow[]` as host or CIDR).
- **SEC-076** `EgressGuard` has table-driven tests incl. rebinding (first lookup public, second private), decimal/octal/hex IP encodings, IPv6 zone IDs, `localhost.` trailing dots, and redirect-to-metadata.

## 11. Prompt-injection posture
- **SEC-080** All text originating outside the trust boundary (branch descriptions, docs, tool results, error strings from upstream) is `Untrusted`: it is sanitized (RTE-020), length-capped, and never concatenated into system/instruction text of any model or into MCP tool *descriptions* generated by GAP (those are static, authored in this repo).
- **SEC-081** *Injection linter:* heuristic + small-classifier detector over untrusted text: imperative addressing the model/agent ("ignore", "disregard", "you must", "do not tell the user"), tool-invocation lures, exfiltration patterns (URLs with query templates containing data placeholders, base64 blobs > 256 chars), role markers (`system:`, `assistant:`), hidden Unicode. Output `lint: Clean | Flagged(reasons)`. Flagged text is excluded from S1/Enricher inputs and shown only in admin review; its element is searchable lexically only after review.
- **SEC-082** *Pinning:* `desc_hash` of every element is stored at approval; served text is generated from the pinned content; mismatches trigger DRIFT (ING-053).
- **SEC-083** *Provenance in results:* node/field results from T1/T2 branches carry `provenance.trust`. The MCP facade wraps results from T2 branches in explicit delimiters (`<gap:untrusted source="…">…</gap:untrusted>`) when `facade.spotlight = true` (default on) and instructs, in its static tool descriptions, that delimited content is data.
- **SEC-084** *Rule of Two (session risk):* the gateway tracks per-session flags `A = ingested untrusted content`, `B = accessed sensitive data (classification ≥ confidential)`, `C = performed or requested external side effect`. If `A ∧ B ∧ C` would hold after the requested action, the action requires human approval regardless of other policy (`riskScore` input to Cedar). Sessions are keyed by `(principal, context_id)`; flags decay after `session.ttl` (default 1 h).
- **SEC-085** Server-generated approval summaries (RUN-071) are template-based from structured data; any model text shown to an approver is labelled as model-generated.

## 12. Audit
- **SEC-090** *Hash-chained log:* append-only table `audit(seq, ts, tenant, actor_chain, action, resource, decision, details_json, prev_hash, hash)` with `hash = blake3(prev_hash ‖ canonical_json(record_without_hash))`. One chain per tenant plus one system chain. Writes are synchronous for mutations/approvals/admin/policy/ingestion-activation, batched ≤ 100 ms for others.
- **SEC-091** *Anchoring:* every `audit.anchor_interval` (default 10 min) the head hash is signed (Ed25519, gateway key) and exported to a configured sink (file, S3-compatible, syslog) to detect truncation.
- **SEC-092** `gap audit verify [--tenant]` recomputes the chain and reports the first inconsistency; runs at startup (system chain, last 10k records) and nightly (full).
- **SEC-093** PROV-O export: `gap audit export --format prov-jsonld` maps records to `prov:Activity` (operation), `prov:Agent` (principals incl. delegation via `prov:actedOnBehalfOf`), `prov:Entity` (nodes/results by hash).

## 13. Supply chain
- **SEC-100** CI runs `cargo deny check` (advisories, licenses, bans, sources), `cargo audit`; weekly scheduled run; any RUSTSEC `critical/high` advisory with a reachable crate blocks release unless a documented, time-boxed exception exists.
- **SEC-101** Releases: reproducible builds (`--locked`, pinned toolchain, `SOURCE_DATE_EPOCH`), CycloneDX SBOM, binary + container signatures (sigstore/cosign), SLSA provenance attestation.
- **SEC-102** Model artifacts: `models/manifest.toml` lists `{id, url, sha256, size, license, source_commit, training_data_note}`; `gap models pull` verifies sha256 before use; `gap models verify` runs at startup when S1 is enabled; failure ⇒ router baseline (RTE-044), not crash.
- **SEC-103** Third-party code in the repo (vendored protos e.g. A2A `a2a.proto`) is recorded in `third_party/MANIFEST.toml` with checksum and license.

## 14. Privacy and retention
- **SEC-110** Defaults: query text retention 7 days (redacted) or hash-only; audit retention ≥ 1 year (configurable); task payloads 30 days; no raw branch data persisted by ingestion (only schema, sketches, and operator-approved samples ≤ 50 rows flagged `sample=true`).
- **SEC-111** `@pii` fields are masked in logs/traces/metrics always; in responses per policy; deletion API `gap admin purge --tenant --principal` removes tasks, feedback, idempotency and query-log rows (audit records keep hashes only).

## 15. Mandatory adversarial test matrix
`tests/security/` (each tagged with `spec:` ids and TM ids):
1. **Poisoned description corpus** (≥ 50 crafted payloads incl. unicode tricks) through ingest → discovery → MCP facade: none alters routing outside lexical match, none appears unsanitized in model inputs. (TM-01)
2. **Rug-pull** mock server (ING-054). (TM-02)
3. **Shadowing**: two branches define `search` tools; composition rejects ambiguity / namespaces them. (TM-03)
4. **Token passthrough canary**: connector mock asserts `authorization` ≠ client token. (TM-04)
5. **Injection-in-results**: result containing "ignore previous instructions…" is delimited/provenance-tagged in facade; Rule-of-Two forces approval for the follow-up mutation. (TM-05, SEC-084)
6. **SSRF table** (SEC-076). (TM-06)
7. **Sandbox escape smoke**: sandboxed CLI cannot read `$HOME`, open sockets, or fork-bomb. (TM-07)
8. **SQL/argv injection**: hostile arguments never alter statement structure / shell parsing. (TM-08)
9. **Cross-tenant fuzz**: random tenant pairs across store/cache/telemetry/plan-cache/audit APIs. (TM-09)
10. **Existence oracle**: hidden vs. unknown field responses byte-identical aside from `correlationId`. (TM-10)
11. **Resource exhaustion**: deep/wide queries, alias bombs, giant variables, slow-loris, zip-bomb specs. (TM-11/12)
12. **Authorization-by-join**: principal with LHS-only permission cannot read RHS fields via join. (TM-21)
13. **Approval replay/forgery**. (TM-15/16)
14. **Audit tamper**: modify/delete a record ⇒ `verify` pinpoints it. (TM-17)
15. **Secret canary** across logs/traces/errors/metrics/model inputs. (TM-18)
16. **Peer loop**: A→B→A forwarding terminates with `GAP_LOOP_DETECTED`. (TM-20)
17. **Policy staleness**: stale cached plan is not used after policy bump. (TM-22)
A third-party review checklist and pen-test scope are produced in WP-M7-06 before 1.0-rc.
