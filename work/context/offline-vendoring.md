# Offline vendoring recipe (reviewer-run, local only)

Written 2026-10-27 by the reviewer (Claude Code). It records how the third-party crates that the
implementing agent may use are made available **offline**, because the agent's sandbox has no
network and nothing may be installed in it.

## What exists on the reviewer's machine (never committed)

| Path | Purpose |
|---|---|
| `.local/vendor/` | Output of `cargo vendor`: 199 crates, about 210 MB (`.gitignore`: `/.local/`) |
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

1. Create a scratch crate **outside the repository** whose `[dependencies]` is the union of every
   crate the next work packages may use, copying the versions and features from the root
   `[workspace.dependencies]`. The set vendored on 2026-10-27 (for WP-M0-08, reused by WP-M1-01):

```toml
[package]
name = "vendorset"
version = "0.0.0"
edition = "2024"
publish = false

[features]
otel = ["dep:opentelemetry", "dep:opentelemetry_sdk", "dep:opentelemetry-otlp", "dep:tracing-opentelemetry"]

[dependencies]
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.154"
thiserror = "2.0.21"
zeroize = { version = "1.9.1", features = ["derive"] }
toml = "1.1.8"
schemars = { version = "1.2.3", features = ["derive"] }
apollo-compiler = "1.33.1"
tracing = "0.1.44"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json", "fmt"] }
uuid = { version = "1.29.0", features = ["v4", "v7", "serde"] }
opentelemetry = { version = "0.33.1", optional = true }
opentelemetry_sdk = { version = "0.33", optional = true }
opentelemetry-otlp = { version = "0.33", optional = true, default-features = false, features = ["grpc-tonic", "trace"] }
tracing-opentelemetry = { version = "0.34.0", optional = true }

[dev-dependencies]
proptest = "1.12.0"
```

2. Resolve (index only), then vendor into the repository, using a throw-away `CARGO_HOME`:
   `CARGO_HOME=<scratch>/cargo-home cargo generate-lockfile` then
   `CARGO_HOME=<scratch>/cargo-home cargo vendor <repo>/.local/vendor`.
3. Prove completeness: build the scratch crate offline with `--all-features --all-targets`, an
   **empty** `CARGO_HOME`, and a `.cargo/config.toml` that points at the vendor directory.
4. Create the repository's `.cargo/config.toml` (above). Check `git status` shows neither `.local` nor `.cargo`.

## Rules learned

- A member crate cannot turn default features off when the root catalogue entry leaves them on:
  put `default-features = false` in the root `[workspace.dependencies]` (the `opentelemetry-otlp` incident).
- A crate that is not vendored cannot be resolved offline; the agent must stop and ask. Not vendored
  today (pinned in the root catalogue): `arc-swap`, `axum`, `blake3`, `candle-*`, `cedar-policy`,
  `clap`, `criterion`, `dashmap`, `ed25519-dalek`, `fastembed`, `governor`, `hnsw_rs`, `im`,
  `jsonschema`, `jsonwebtoken`, `llguidance`, `moka`, `ort`, `petgraph`, `protox`, `redb`, `reqwest`,
  `rmcp`, `rusqlite`, `rustls`, `safetensors`, `sha2`, `sqlparser`, `tokenizers`, `tower-http`,
  `tree-sitter`, `usearch` and others. Re-vendor with the union of the old and new sets.
- Reviewer tools live outside the repository: `cargo install cargo-deny --version 0.20.2 --locked --root <scratch>/tools`
  and `cargo install cargo-nextest --version 0.9.149 --locked --root <scratch>/tools`, each with a
  throw-away `CARGO_HOME` and `--target-dir`; run them with `PATH=<scratch>/tools/bin:$PATH`.
  Each network action needs the human's approval.
