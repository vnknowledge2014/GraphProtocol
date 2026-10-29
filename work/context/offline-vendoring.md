# Offline vendoring recipe (reviewer-run, local only)

Written 2026-10-27 by the reviewer (Claude Code), revised the same day for WP-M0-09, WP-M2-01 and
WP-M3-01. It records how the third-party crates that the implementing agent may use are made
available **offline**, because the agent's sandbox has no network and nothing may be installed in it.

## What exists on the reviewer's machine (never committed)

| Path | Purpose |
|---|---|
| `.local/vendor/` | Output of `cargo vendor`: 328 crates, about 468 MB (`.gitignore`: `/.local/`) |
| `.cargo/config.toml` | Source replacement pointing at the vendor directory plus `net.offline = true` (`.gitignore`: `/.cargo/`) |

`.cargo/config.toml` (do not commit; it would break online builds elsewhere):

```toml
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = ".local/vendor"

[net]
offline = true
```

## Recipe to (re)build the vendor directory

The method changed on 2026-10-27: vendor from a **scratch copy of the real workspace** instead of a
hand-made `vendorset` crate. This way cargo keeps every version already recorded in `Cargo.lock`
(a separately resolved crate can pick newer transitive versions, and the repository lock would then
miss them offline) and only adds what the new dependencies need.

1. Copy the repository to a scratch directory outside it, without `.git`, `.local`, `target`,
   `.cargo` (`rsync -a --exclude ...`).
2. In the copy, edit the root `[workspace.dependencies]` (pins, `default-features = false` where members
   must be able to choose features) and add the **intended dependency set of each upcoming WP** to
   the manifests of its crates (the sets are listed below; they are exactly what the agent is later
   allowed to declare).
3. Resolve and vendor with a throw-away `CARGO_HOME` (network, needs the human's approval):
   `CARGO_HOME=<scratch>/cargo-home cargo vendor --locked <scratch>/vendor-new`
   (`cargo metadata` first extends `Cargo.lock`; compare it with the repository lock: **no existing
   (name, version) may disappear**, and axum/hyper/prost/tower must stay single-version).
4. Check that every `(name, version)` of the old vendor directory is still present in the new one.
5. Prove completeness offline: in the scratch copy add a `.cargo/config.toml` that points at
   the new vendor directory with `net.offline = true` and build with an **empty** `CARGO_HOME`:
   `cargo build --workspace --all-targets --all-features --locked --offline`. Then run capability probes
   (see "Probes") and `cargo deny --offline check`.
6. Move the old vendor aside, copy the new one to `<repo>/.local/vendor`, apply the root-catalogue
   changes to the real `Cargo.toml` (own commit), and re-run on the real repository: `cargo build
   --workspace --locked --offline`, `cargo nextest run`, `cargo fmt --check`, `check_layers.py`,
   `wp.py validate`, `cargo deny check`. `Cargo.lock` must be unchanged by this step.

## Intended dependency sets (vendored on 2026-10-27)

Each crate declares these as `name.workspace = true` (features as shown); anything else needs a stop-and-ask.

| WP / crate | `[dependencies]` | `[dev-dependencies]` |
|---|---|---|
| WP-M0-09 `gap-conformance` | gap-core, gap-ngc-sdk, gap-proto, anyhow, clap (`derive`), reqwest (`json`; no TLS), serde (`derive`), serde_json, thiserror, tokio (`io-util`, `macros`, `net`, `process`, `rt-multi-thread`, `time`), tracing | axum, proptest, tempfile |
| WP-M0-09 `gap-testkit` | gap-core, gap-ngc-sdk, gap-proto, serde (`derive`), serde_json, thiserror, tokio (`macros`, `rt`, `sync`, `test-util`, `time`), tracing, tracing-subscriber, uuid (`v4`) | proptest, tempfile |
| WP-M2-01 `gap-store` | gap-core, blake3, rusqlite (`backup`, `blob`, `bundled`, `functions`, `limits`, `serde_json`, `uuid`), serde (`derive`), serde_json, thiserror, tokio (`rt`, `sync`), tracing, uuid (`serde`, `v4`, `v7`) | proptest, tempfile, tokio (`macros`, `rt-multi-thread`) |
| WP-M3-01 `gap-balance` | gap-core, arc-swap, crossbeam-utils, metrics, thiserror, tokio (`rt`, `sync`, `time`), tracing | criterion (`cargo_bench_support`), hdrhistogram, loom, proptest, shuttle, tokio (`macros`, `rt-multi-thread`, `test-util`, `time`) |
| WP-M0-06 `gap-proto` | gap-core, prost, prost-types, tonic, tonic-prost; `[build-dependencies]` protox, tonic-prost-build | proptest, prost-reflect, sha2 |

Result: 328 crates (199 before; 129 added, none removed), `Cargo.lock` of the scratch copy 359 packages.

Revised 2026-10-28 for WP-M0-06 (user approved the crates.io access for it): 356 crates (328 before; 28 added: protox, protox-parse,
prost-build, prost-reflect, tonic-build 0.14.8, tonic-prost-build 0.14.8, sha2 0.11.1 and its `digest` family, miette, logos, petgraph,
fixedbitset, multimap, pulldown-cmark, prettyplease, unicase, unicode-width 0.1.14, foldhash, hashbrown 0.15.5, typenum). Three
crates of the old set (`loom`, `generator`, `windows-result`) are kept in the vendor directory although no manifest uses them any more.
`cargo deny --offline check bans licenses sources` ok, `cargo build --workspace --all-targets --all-features --locked --offline` ok with an empty
`CARGO_HOME`, and a probe `build.rs` compiled `proto/gap/ngc/v1/ngc.proto`, `proto/gap/gateway/v1/gateway.proto` and the vendored `a2a.proto`
(with the five `google/api/*.proto` files of `third_party/specs/googleapis-646596e286b4/`) through `protox::compile` +
`tonic_prost_build::configure().compile_fds(..)` without any system `protoc`. The real `Cargo.lock` is unchanged by the vendoring itself;
the agent extends it offline (28 new packages). Root catalogue gained `prost-reflect = "0.16.5"` and `prost-types = "0.14.4"`.

Revised 2026-10-29 for WP-M1-03 (user approved the crates.io access for it): only 2 packages were missing, `serde-value 0.7.0` and
`ordered-float 2.10.1` (the optional dependencies behind prost-reflect's `serde` feature, which the SDK needs for the protobuf JSON mapping
of NGC-002/003). Vendored from a scratch copy of the workspace with the intended `gap-ngc-sdk` manifest (`cargo metadata`, then `cargo vendor
--locked` with a throw-away `CARGO_HOME`): the scratch lock gained exactly those 2 packages and lost none. The two crate directories were added
to `.local/vendor` (358 crates; the three unused old ones `loom`, `generator`, `windows-result` are still kept). Root catalogue gained
`futures-util 0.3.34`, `http-body-util 0.1.5`, `hyper-util 0.1.21`, `tokio-util 0.7.20` (all already in the lockfile). Not vendored and not needed:
`async-stream`, `tokio-test`, `rustls` (TLS is out of scope for M1-03; the server refuses plaintext off-host instead, NGC-093 g).

| WP / crate | `[dependencies]` | `[dev-dependencies]` |
|---|---|---|
| WP-M1-03 `gap-ngc-sdk` | axum, bytes, futures-util, gap-core, gap-proto, http, hyper-util (`tokio`), prost, prost-reflect (`serde`), prost-types, serde (`derive`), serde_json, thiserror, tokio (`io-std`, `io-util`, `macros`, `net`, `rt`, `sync`, `time`), tokio-stream (`net`), tokio-util, tonic, tonic-prost, tracing | http-body-util, proptest, tokio (`macros`, `rt-multi-thread`, `test-util`, `time`), tower (`util`) |

## Probes that were run offline on the vendored set (scratch copy, 2026-10-27)

- bundled SQLite (rusqlite `bundled`): FTS5 `MATCH`, recursive CTE, `json_extract`, `PRAGMA journal_mode=WAL` on a
  file database, the online backup API (`backup::Backup`), `blake3` and `uuid` v7: pass.
- shuttle `check_random` and loom `model`, hdrhistogram without default features, arc-swap, crossbeam `CachePadded`,
  `metrics` macro, tokio paused clock (`start_paused`): pass.
- clap derive + `--help` (`ErrorKind::DisplayHelp`), axum server + reqwest `json` client round trip on `127.0.0.1`: pass.
- `cargo bench -p gap-balance -- --quick` with criterion (`cargo_bench_support`): works **only** when the crate sets
  `[lib] bench = false`; without it the `--quick` flag reaches the libtest harness of the lib target
  (`Unrecognized option: 'quick'`). WP-M3-01 now says so.
- `cargo deny --offline check` on the new lockfile: advisories ok, bans ok, licenses ok, sources ok.
  Not verified offline: yanked status (`index-failure` warnings; the versions were resolved the same day) and
  advisories newer than the cached database (commit of 2026-10-26).

## Rules learned

- A member crate cannot turn default features off when the root catalogue entry leaves them on:
  put `default-features = false` in the root `[workspace.dependencies]` (the `opentelemetry-otlp` incident;
  done on 2026-10-27 for `reqwest`, `criterion`, `hdrhistogram`).
- `reqwest` has **no TLS backend** in the catalogue on purpose: the default TLS stacks pull `aws-lc`/`ring`, whose
  licences are not in the `deny.toml` allow-list. The first WP that needs HTTPS must pick a backend and settle the licence gate.
- A crate that is not vendored cannot be resolved offline; the agent must stop and ask. Not vendored
  today (pinned in the root catalogue): `candle-*`, `cedar-policy`, `dashmap`, `ed25519-dalek`, `fastembed`, `governor`,
  `hnsw_rs`, `im`, `jsonschema`, `jsonwebtoken`, `llguidance`, `moka`, `ort`, `redb`, `rmcp`,
  `rustls`, `safetensors`, `sqlparser`, `tokenizers`, `tower-http` (as a direct dependency), `tree-sitter`,
  `usearch` and others. Re-vendor with the union of the old and new sets (steps above).
- Seatbelt sandbox: the agent cannot read `~/.cargo/config.toml` or `~/.config/git/config`; it works around this with
  `CARGO_HOME=.git/tmp-cargo-home` and `GIT_CONFIG_GLOBAL=/dev/null`. Whether binding `127.0.0.1` is allowed inside it
  is untested: a WP whose tests open loopback sockets (WP-M0-09) must report those tests NOT RUN if the sandbox refuses,
  and the reviewer runs them.
- Reviewer tools live outside the repository: `cargo install cargo-deny --version 0.20.2 --locked --root <scratch>/tools`
  and `cargo install cargo-nextest --version 0.9.149 --locked --root <scratch>/tools`, each with a
  throw-away `CARGO_HOME` and `--target-dir`; run them with `PATH=<scratch>/tools/bin:$PATH`.
  `cargo deny --offline check` (the flag belongs to `cargo deny`, not to `check`) uses the advisory database cached in that `CARGO_HOME`.
  Each network action needs the human's approval.
