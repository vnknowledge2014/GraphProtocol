#!/usr/bin/env bash
set -euo pipefail

# Reproduction script for GAP-Bench and RouteBench (WP-M7-05, ADR-021, CMP-008, RTE-069).
# Default is air-gapped deterministic scripted execution (--offline). Set GAP_OFFLINE=0 to allow
# Cargo network resolution, or pass GAPBENCH_EXTRA_ARGS for live OpenAI-compatible LLM endpoints.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT_DIR"

OFFLINE_FLAG="--offline"
if [[ "${GAP_OFFLINE:-1}" == "0" ]]; then
  OFFLINE_FLAG=""
fi

echo "==> Running GAP-Bench (release, scripted N-sweep 10,50,200,1000 across all 3 suites)..."
# shellcheck disable=SC2086
cargo run --release -p gapbench ${OFFLINE_FLAG} -- \
  --scripted \
  --suite all \
  --n 10,50,200,1000 \
  --out bench/results/gapbench \
  ${GAPBENCH_EXTRA_ARGS:-}

echo "==> Verifying GAP-Bench artifact bundle..."
# shellcheck disable=SC2086
cargo run -p gapbench ${OFFLINE_FLAG} -- --verify-results bench/results/gapbench

echo "==> Running RouteBench (release, final selective-risk & calibration suite)..."
# shellcheck disable=SC2086
cargo run --release -p routebench ${OFFLINE_FLAG} -- --out bench/results/routebench/final

echo "==> Benchmark reproduction complete:"
echo "    GAP-Bench:  bench/results/gapbench/{baseline.json,README.md}"
echo "    RouteBench: bench/results/routebench/final/"
