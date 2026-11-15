# GAP developer and CI verification commands (WP-M0-04; OPS-040, SEC-100, ADR-022)

set shell := ["bash", "-euo", "pipefail", "-c"]

# List available recipes by default
default:
    @just --list

# Run all local CI checks before pushing
verify: fmt lint docs deny tools-test layers wp-validate refs spec-trace test conformance

# Format check across the Rust workspace
fmt:
    cargo fmt --all -- --check

# Lint workspace with warnings denied
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Build workspace API documentation with warnings denied
docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Verify supply-chain policies (advisories, bans, licenses, sources; SEC-100)
deny:
    cargo deny check advisories bans licenses sources

# Run workspace tests via cargo-nextest (with release swap_stress per M2/M3 gates)
test:
    cargo nextest run --workspace --exclude gap-snapshot
    cargo nextest run -p gap-snapshot --lib --test compose --test handle
    cargo test --release -p gap-snapshot --test swap_stress

# Run offline protocol conformance fixtures
conformance:
    cargo run -p gap-conformance -- --class Core --offline conformance/gql

# Verify crate layering and ARC-002 (no ML crates in hot path)
layers:
    python3 tools/check_layers.py

# Validate work-package plan integrity (ADR-022)
wp-validate:
    python3 tools/wp.py validate

# Verify documentation/spec/ADR cross-references
refs:
    python3 tools/check_refs.py

# Verify requirement-to-test traceability matrix
spec-trace:
    python3 tools/spec_trace.py --warn

# Run Python tooling unit tests; fail if any test is skipped
tools-test:
    python3 -c "import sys, unittest; s = unittest.defaultTestLoader.discover('tools', pattern='test_*.py'); r = unittest.TextTestRunner(verbosity=2).run(s); sys.exit(0 if (r.wasSuccessful() and len(r.skipped) == 0 and r.testsRun > 0) else 1)"
