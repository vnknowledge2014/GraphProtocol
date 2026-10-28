# WP-M0-06 brief — protobuf pipeline and the `gap-proto` crate

Status: binding for WP-M0-06 (reviewer, 2026-10-28). The deliverables and acceptance commands are those of `work/work-packages.toml`; this file makes
them concrete. If this file and the work package disagree, the work package wins; say so in the handoff. Normative inputs: `proto/gap/ngc/v1/ngc.proto`,
`proto/gap/gateway/v1/gateway.proto`, `spec/03-ngc.md` (NGC-001, NGC-004), ADR-002, and the pinned A2A proto `third_party/specs/a2a-v1.0.1/specification/a2a.proto`.

## 1. Scope

In: `crates/gap-proto/**` (manifest, `build.rs`, `src/`, `tests/`, `golden/`, `CHANGELOG.md`) and, only if a `.proto` defect makes compilation impossible, `proto/**`
(stop and ask before changing any field number or name there).
Out: any consumer of the generated types (`gap-ngc-sdk`, `gap-server`, `gap-testkit`, `gap-conformance` keep compiling untouched), a reflection service, `buf`, a
`protoc` binary, the `adapters/gap-a2a` crate, and edits to `third_party/`, `spec/`, the root `Cargo.toml` (the catalogue is already complete) or `.local/`.

## 2. Offline dependencies (already vendored; anything else is a stop-and-ask)

The reviewer vendored the build tooling on 2026-10-28 (see `work/context/offline-vendoring.md`). `crates/gap-proto/Cargo.toml` declares exactly:

```toml
[features]
a2a = []

[dependencies]
gap-core.workspace = true
prost.workspace = true
prost-types.workspace = true
tonic.workspace = true
tonic-prost.workspace = true

[build-dependencies]
protox.workspace = true
tonic-prost-build.workspace = true

[dev-dependencies]
proptest.workspace = true
prost-reflect.workspace = true
sha2.workspace = true
```

All as `name.workspace = true`, no feature overrides, no `hex` crate (format hex by hand). `Cargo.lock` grows by about 28 packages when cargo resolves this offline (protox, protox-parse,
prost-build, prost-reflect, tonic-build, tonic-prost-build, sha2 and its `digest` family, miette, logos, petgraph, ...); that is expected and the only change to it. The sandbox cannot read `~/.cargo/config.toml`:
use `CARGO_HOME=.git/tmp-cargo-home` for cargo commands and delete that directory before the handoff (as earlier work packages did; ask before any `rm -rf`).
**No system `protoc` is available or allowed**: every `.proto` is compiled in `build.rs` by `protox` (a pure-Rust compiler).
Probe already run by the reviewer on this exact vendor set: `protox::compile` + `tonic_prost_build::configure().compile_fds(fds)` builds `ngc.proto`, `gateway.proto` and the A2A proto (with the googleapis includes) offline.

## 3. Design (binding)

1. **Module layout** (public API): `gap_proto::ngc::v1` (package `gap.ngc.v1`, including the tonic `connector_server` / `connector_client` modules), `gap_proto::gateway::v1` (`gap.gateway.v1`, `gateway_server` / `gateway_client`),
   and, only with `feature = "a2a"`, `gap_proto::a2a::v1` (package `lf.a2a.v1`, the `A2AService` server/client). Generated code is included with `include!(concat!(env!("OUT_DIR"), "/<package>.rs"))` inside those modules; nothing generated is committed.
   Generated modules carry `#[allow(...)]` for the lints they trigger (`clippy::all`, `clippy::pedantic`, `missing_docs`, `rustdoc` lints) so that the crate-level `deny(clippy::unwrap_used, ...)` header and `cargo clippy -p gap-proto --all-targets -D warnings`
   and `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps` stay green; hand-written code stays under the strict header. `#![forbid(unsafe_code)]` stays.
2. **`build.rs`**: compile with `protox::Compiler` (not the `protox::compile` shortcut, which turns source info and imports on): `Compiler::new([..includes])?` with `include_source_info(false)` for the descriptor output (so a comment edit never changes a digest) and `include_imports(false)`.
   Own files: `gap/ngc/v1/ngc.proto` and `gap/gateway/v1/gateway.proto` with include root `../../proto` (relative to the crate). Feature `a2a` (`CARGO_FEATURE_A2A`): `a2a.proto` with the include roots
   `../../third_party/specs/a2a-v1.0.1/specification` and `../../third_party/specs/googleapis-646596e286b4` (the five `google/api/*.proto` files it imports; protox bundles the `google/protobuf/*` well-known types).
   Code generation: `tonic_prost_build::configure()` with server **and** client code, `compile_fds(..)` on the descriptor set (well-known types map to `prost_types` automatically; the A2A code must not require generated `google.api` Rust types).
   Emit `cargo:rerun-if-changed` for every input directory/file and for `build.rs`; write the encoded descriptor sets to `OUT_DIR` and expose them as `pub const DESCRIPTOR_SET: &[u8]` (own files only: ngc + gateway, files sorted by name, no source info, no imports) and,
   with the feature, `pub const A2A_DESCRIPTOR_SET: &[u8]` (a2a.proto only). The build must fail with a readable message (not a panic backtrace) when a proto does not compile.
3. **Wire version**: `pub const NGC_WIRE_VERSION: &str = "gap/0.1";` (NGC-004: `ConnectorDescription.ngc_version` carries it). A unit test asserts that `ConnectorDescription` has a string field `ngc_version` (by descriptor) and that the constant is `gap/` + `major.minor` of the workspace package version (`0.1`).
4. **Golden descriptor test** (`tests/descriptor_golden.rs`): the SHA-256 of `DESCRIPTOR_SET` (and, with the feature, of `A2A_DESCRIPTOR_SET`) is compared with `crates/gap-proto/golden/descriptor.sha256` (and `golden/descriptor-a2a.sha256`): one line, lower-case hex, nothing else.
   Coupling to the changelog: the test also reads `crates/gap-proto/CHANGELOG.md` and requires that it contains the first 16 hex characters of the current digest (so a golden can only change together with a changelog entry that names it). Create `CHANGELOG.md` (Keep a Changelog style) with the initial entry for `0.1.0`.
   The failure message must say what to do (update the golden **and** add a CHANGELOG entry; breaking wire changes need a minor bump before 1.0, `ADR-024`).
   Rules for the golden itself: it is computed by running the test pipeline, never typed by hand; a comment-only edit of a `.proto` must NOT change it; renaming/renumbering/removing a field, enum value, message, service or method MUST.
5. **Wire-compat test** (`tests/wire_roundtrip.rs`): for **every** message in the descriptor set (top level and nested; map-entry messages are covered through their owner), proptest builds random `prost_reflect::DynamicMessage` values from the `MessageDescriptor`
   (all scalar kinds with the full value ranges, `bytes`, unicode `string`, enums using declared values, `repeated`, `map` with string and integer keys, nested messages with a recursion depth limit of 3, one arm of each `oneof`, and the well-known types `Struct`/`Value`/`ListValue`/`Timestamp` with valid values; finite floats only).
   Each value is encoded by the dynamic message and then (a) decoded by the **static generated type**, whose re-encoding must decode (dynamically) to the same value as the original, and (b) the static type's `encode_to_vec` -> `decode` must be equal (`PartialEq`).
   A registry (a macro listing every generated message type) maps full names to static round-trip functions; a test asserts that the registry covers exactly the set of message full names of the descriptor set, so a new message without a registration fails. At least 256 cases per message in the default run; the whole file must stay under 30 s in a debug build.
   Add two negative tests: (1) bytes with an unknown field appended still decode with every static type; (2) decoding a truncated buffer returns an error, never a panic.
6. **Service shape test**: the descriptor's services and methods (names, client/server streaming flags) equal an explicit table in the test (`Connector`: 11 methods, `Gateway`: 2 methods) so that the golden failure is not the only signal.
7. Tests carry `// spec: NGC-001`, `// spec: NGC-004` (and `// spec: CMP-020` .. `CMP-022` for the A2A parts); keep the traceability tool green.

## 4. Process rules

Follow `docs/AGENT-GUIDE.md` (step expansion, TDD, handoff format). Branch `wp/WP-M0-06` from `main`. Claim the WP first (`python3 tools/wp.py set WP-M0-06 doing`), write `work/steps/WP-M0-06.md`, commit small Conventional Commits (`feat(gap-proto): ...`). Edit only `crates/gap-proto/**`, `proto/**` (see section 1), `Cargo.lock` (cargo only), `work/steps/`, `work/handoffs/`, `work/status.toml`.
Run before the handoff: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`, `cargo clippy -p gap-proto --all-targets --all-features --locked --offline -- -D warnings`, `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`, `cargo build --workspace --locked --offline`,
`cargo test -p gap-proto --locked --offline` and `... --all-features` (3 runs each), `python3 tools/check_layers.py`, `tools/wp.py validate`, `tools/check_refs.py`, `tools/spec_trace.py --warn`, the tools unit tests. `cargo nextest` is not installed in your sandbox: report it NOT RUN (the reviewer runs it).
Mutation check (required in the handoff): for each of the following, edit a copy of the proto text or the build script, show the named test failing, and restore: rename a field; change a field number; delete an enum value; rename a method; flip a method to streaming; remove a message; a comment-only edit (must NOT fail the golden).
Do not merge; do not mark the work package done. Finish with exactly one line `WP-M0-06: READY`, `WP-M0-06: READY-WITH-NOT-RUN` or `WP-M0-06: BLOCKED - <reason>`, then a short summary in Vietnamese.

## 5. What the reviewer will check

An independent Python decoder of the compiled descriptor against `proto/` text (field numbers/names/enum values/methods equal), `cargo nextest` and `cargo deny` on the head, a clean-room rebuild offline with an empty `CARGO_HOME`, my own mutation set on the golden and the round-trip registry (including edits you did not list), the feature matrix (`--no-default-features`, `--all-features`), the doc and clippy gates, test run time and flakiness under load.
