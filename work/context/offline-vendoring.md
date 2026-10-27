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

Result: 328 crates (199 before; 129 added, none removed), `Cargo.lock` of the scratch copy 359 packages.

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
  `hnsw_rs`, `im`, `jsonschema`, `jsonwebtoken`, `llguidance`, `moka`, `ort`, `petgraph`, `protox`, `redb`, `rmcp`,
  `rustls`, `safetensors`, `sha2`, `sqlparser`, `tokenizers`, `tower-http` (as a direct dependency), `tree-sitter`,
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
