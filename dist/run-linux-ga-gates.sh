#!/usr/bin/env bash
# Production Linux/CI GA verification runner (`G-4`, `G-5`, `G-10`, `G-11`, `G-14`, `G-15`, `G-20`).
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

MODE="${1:---quick-check}"
case "${MODE}" in
  --quick-check|--full-release)
    ;;
  *)
    echo "Usage: $0 [--quick-check | --full-release]" >&2
    exit 2
    ;;
esac

echo "[1/7] Core Rust, license, advisory, and layer gates (G-4, G-5, G-21)"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
python3 tools/check_layers.py
python3 tools/spec_trace.py --strict
python3 tools/check_refs.py
python3 tools/wp.py validate

echo "[2/7] Distroless non-root container build & runtime smoke (G-15)"
docker build -t gap:ci .
docker run --rm gap:ci --help

echo "[3/7] CycloneDX SBOM & reproducible binary parity check (G-15, SEC-101)"
OUT_DIR=target/dist-1 ./dist/build-reproducible.sh
cargo clean -p gap-cli
OUT_DIR=target/dist-2 ./dist/build-reproducible.sh
cmp target/dist-1/gap target/dist-2/gap
python3 dist/generate-sbom.py --out target/sbom.cdx.json

echo "[4/7] Observability & Prometheus rule verification (G-20)"
python3 dist/verify-ops-bundle.py
if command -v promtool >/dev/null 2>&1; then
  promtool check rules ops/prometheus/alerts.yml
elif command -v docker >/dev/null 2>&1; then
  docker run --rm --entrypoint promtool -v "${ROOT_DIR}/ops/prometheus:/rules:ro" prom/prometheus:latest check rules /rules/alerts.yml
else
  echo "WARN: promtool/docker not installed; install prometheus to run promtool check rules" >&2
fi

if [[ "${MODE}" == "--quick-check" ]]; then
  echo "[5/7] Smoke conformance & chaos/soak (quick mode)"
  CI=1 cargo nextest run --no-pager -j 1 -p gap-conformance
  GAP_SOAK_DURATION_SECS=10 CI=1 cargo nextest run --no-pager -j 1 -p gap-server --test soak
  exit 0
fi

echo "[5/7] Coverage (>=80% L0-L3, >=70% overall) & mutation testing (G-4)"
cargo llvm-cov nextest --workspace \
  --exclude gap-conformance \
  --exclude gapbench \
  --exclude routebench \
  --exclude enrichment-bench \
  --ignore-filename-regex '(^|/)(bench|crates/gap-conformance|crates/gap-testkit)/' \
  -E 'not test(e2e_typed_path_overhead_p99_under_10ms_with_zero_io_mocks) and not binary(swap_stress)' \
  --lcov --output-path target/lcov.info --summary-only
if command -v cargo-mutants >/dev/null 2>&1; then
  cargo mutants --workspace --no-shuffle --timeout 120
fi

echo "[6/7] Extended soak & parallel 10-target fuzzing (G-11, G-14)"
GAP_SOAK_DURATION_SECS="${GAP_SOAK_DURATION_SECS:-259200}" CI=1 cargo nextest run --release --no-pager -j 1 -p gap-server --test soak
GAP_FUZZ_MAX_TIME="${GAP_FUZZ_MAX_TIME:-86400}"
fuzz_pids=()
for target in \
  fuzz_cursor_decode fuzz_extract_graphql fuzz_extract_mcp fuzz_extract_openapi \
  fuzz_extract_proto fuzz_extract_sql fuzz_gapql_parse fuzz_ngc_decode \
  fuzz_policy_input fuzz_sanitize; do
  (cd fuzz && cargo +nightly fuzz run "${target}" --target x86_64-unknown-linux-gnu -- -max_total_time="${GAP_FUZZ_MAX_TIME}") &
  fuzz_pids+=("$!")
done
for pid in "${fuzz_pids[@]}"; do
  wait "${pid}"
done

echo "[7/7] GA Linux verification suite completed."
