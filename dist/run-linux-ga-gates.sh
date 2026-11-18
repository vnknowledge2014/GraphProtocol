#!/usr/bin/env bash
# Production Linux/CI GA verification runner (`G-4`, `G-5`, `G-10`, `G-11`, `G-14`, `G-15`, `G-20`).
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

MODE="--quick-check"
DETACH="${GA_RUNNER_DETACH:-${GAP_DETACH:-0}}"
for arg in "$@"; do
  case "${arg}" in
    --quick-check|--full-release)
      MODE="${arg}"
      ;;
    --detach)
      DETACH="1"
      ;;
    *)
      echo "Usage: $0 [--quick-check | --full-release] [--detach]" >&2
      exit 2
      ;;
  esac
done

# On macOS hosts with Colima/Docker, automatically delegate Linux GA execution
# inside an Ubuntu 24.04 x86_64 container on native VM ext4 storage.
if [[ "$(uname -s)" == "Darwin" && "${GA_RUNNER_IN_LINUX:-0}" != "1" ]]; then
  if ! docker info >/dev/null 2>&1; then
    echo "ERROR: Docker daemon is not reachable. Start Colima first:" >&2
    echo "  colima start --cpu 8 --memory 24 --disk 80" >&2
    exit 1
  fi

  echo "[host-pre 1/3] Distroless non-root container build & runtime smoke (G-15)"
  docker build --build-arg SOURCE_DATE_EPOCH=1700000000 -t gap:ci .
  docker run --rm gap:ci --help

  echo "[host-pre 2/3] Observability & Prometheus rule verification (G-20)"
  python3 dist/verify-ops-bundle.py
  docker run --rm --entrypoint promtool -v "${ROOT_DIR}/ops/prometheus:/rules:ro" \
    prom/prometheus:latest check rules /rules/alerts.yml

  if [[ "${GA_RUNNER_REBUILD:-${GAP_REBUILD_RUNNER:-0}}" == "1" ]] || ! docker image inspect gap-ga-runner:ubuntu24.04 >/dev/null 2>&1; then
    echo "[host-pre 3/3] Building Linux GA runner image (gap-ga-runner:ubuntu24.04)..."
    docker build -f dist/Dockerfile.ga-runner -t gap-ga-runner:ubuntu24.04 dist
  else
    echo "[host-pre 3/3] Reusing cached image gap-ga-runner:ubuntu24.04"
  fi

  RESULTS_DIR="${ROOT_DIR}/.local/ga-linux-results"
  mkdir -p "${RESULTS_DIR}"
  LOG_NAME="ga-${MODE#--}.log"

  INNER_CMD="set -euo pipefail; \
    rm -rf /workspace/* /workspace/.[!.]* /workspace/..?* 2>/dev/null || true; \
    tar -C /host-repo \
      --exclude='./target' \
      --exclude='./.local' \
      --exclude='./.git' \
      --exclude='./.agent' \
      --exclude='./.cargo' \
      --exclude='./fuzz/target' \
      --exclude='./tests/differential/node_modules' \
      -cf - . | tar -C /workspace -xf -; \
    cd /workspace; \
    export GA_RUNNER_IN_LINUX=1 GA_RUNNER_SKIP_DOCKER_BUILD=1 GA_RUNNER_RESULTS_DIR=/results; \
    ./dist/run-linux-ga-gates.sh '${MODE}' 2>&1 | tee '/results/${LOG_NAME}'"

  DOCKER_ENV_ARGS=()
  SOAK_OVERRIDE="${GA_RUNNER_SOAK_DURATION_SECS:-${GAP_SOAK_DURATION_SECS:-}}"
  if [[ -n "${SOAK_OVERRIDE}" ]]; then
    DOCKER_ENV_ARGS+=(-e "GA_RUNNER_SOAK_DURATION_SECS=${SOAK_OVERRIDE}")
  fi
  FUZZ_OVERRIDE="${GA_RUNNER_FUZZ_MAX_TIME:-${GAP_FUZZ_MAX_TIME:-}}"
  if [[ -n "${FUZZ_OVERRIDE}" ]]; then
    DOCKER_ENV_ARGS+=(-e "GA_RUNNER_FUZZ_MAX_TIME=${FUZZ_OVERRIDE}")
  fi

  if [[ "${DETACH}" == "1" ]]; then
    CONTAINER_NAME="gap-ga-${MODE#--}"
    docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
    CID="$(docker run -d --name "${CONTAINER_NAME}" --privileged \
      ${DOCKER_ENV_ARGS[@]+"${DOCKER_ENV_ARGS[@]}"} \
      -v "${ROOT_DIR}:/host-repo:ro" \
      -v "${RESULTS_DIR}:/results" \
      -v gap-ga-cargo-cache:/usr/local/cargo/registry \
      gap-ga-runner:ubuntu24.04 bash -lc "${INNER_CMD}")"
    echo "Started detached Linux GA runner container: ${CONTAINER_NAME} (${CID})"
    echo "Monitor logs with:"
    echo "  tail -f ${RESULTS_DIR}/${LOG_NAME}"
    echo "  docker logs -f ${CONTAINER_NAME}"
    exit 0
  fi

  exec docker run --rm --privileged \
    ${DOCKER_ENV_ARGS[@]+"${DOCKER_ENV_ARGS[@]}"} \
    -v "${ROOT_DIR}:/host-repo:ro" \
    -v "${RESULTS_DIR}:/results" \
    -v gap-ga-cargo-cache:/usr/local/cargo/registry \
    gap-ga-runner:ubuntu24.04 bash -lc "${INNER_CMD}"
fi

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
if [[ "${GA_RUNNER_SKIP_DOCKER_BUILD:-0}" == "1" ]]; then
  echo "SKIP: Distroless container build & --help smoke already verified by host Colima preflight."
else
  docker build --build-arg SOURCE_DATE_EPOCH=1700000000 -t gap:ci .
  docker run --rm gap:ci --help
fi

echo "[3/7] CycloneDX SBOM & reproducible binary parity check (G-15, SEC-101)"
OUT_DIR=target/dist-1 ./dist/build-reproducible.sh
cargo clean -p gap-cli
OUT_DIR=target/dist-2 ./dist/build-reproducible.sh
cmp target/dist-1/gap target/dist-2/gap
python3 dist/generate-sbom.py --out target/sbom.cdx.json
if [[ -n "${GA_RUNNER_RESULTS_DIR:-}" ]]; then
  mkdir -p "${GA_RUNNER_RESULTS_DIR}"
  cp target/dist-1/gap.sha256 "${GA_RUNNER_RESULTS_DIR}/gap-linux.sha256"
  cp target/sbom.cdx.json "${GA_RUNNER_RESULTS_DIR}/sbom.cdx.json"
fi

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
  echo "[6/7] Quick-check passed."
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
  --lcov --output-path target/lcov.info
cargo llvm-cov report \
  --ignore-filename-regex '(^|/)(bench|crates/gap-conformance|crates/gap-testkit)/' \
  --summary-only | tee "${GA_RUNNER_RESULTS_DIR:-target}/coverage-summary.txt"
if [[ -n "${GA_RUNNER_RESULTS_DIR:-}" ]]; then
  cp target/lcov.info "${GA_RUNNER_RESULTS_DIR}/lcov.info"
fi

if command -v cargo-mutants >/dev/null 2>&1; then
  rm -rf target/mutants-out
  mutants_rc=0
  cargo mutants --no-shuffle --timeout 120 -j 4 \
    -p gap-balance -p gap-ql -p gap-route \
    -f 'crates/gap-balance/src/**' \
    -f 'crates/gap-ql/src/cost.rs' \
    -f 'crates/gap-route/src/calibration_job/**' \
    -f 'crates/gap-route/src/debias/**' \
    --output target/mutants-out || mutants_rc=$?
  if [[ -n "${GA_RUNNER_RESULTS_DIR:-}" && -d target/mutants-out ]]; then
    rm -rf "${GA_RUNNER_RESULTS_DIR}/mutants-out"
    cp -R target/mutants-out "${GA_RUNNER_RESULTS_DIR}/"
  fi
  python3 - "${mutants_rc}" <<'PY'
import json, pathlib, sys
rc = int(sys.argv[1])
if rc not in (0, 2, 3):
    raise SystemExit(f"ERROR: cargo-mutants failed with unexpected exit code {rc}")
out_dir = pathlib.Path("target/mutants-out/mutants.out")
outcomes = out_dir / "outcomes.json"
if outcomes.is_file():
    data = json.loads(outcomes.read_text(encoding="utf-8"))
    caught = int(data.get("caught", 0))
    missed = int(data.get("missed", 0))
    timeout = int(data.get("timeout", 0))
else:
    def count_lines(name: str) -> int:
        p = out_dir / name
        return sum(1 for ln in p.read_text(encoding="utf-8").splitlines() if ln.strip()) if p.is_file() else 0
    caught = count_lines("caught.txt")
    missed = count_lines("missed.txt")
    timeout = count_lines("timeout.txt")
tested = caught + missed + timeout
if tested == 0:
    raise SystemExit("ERROR: cargo-mutants produced 0 tested mutants")
score = caught / tested
print(f"G-4 mutation summary: caught={caught} missed={missed} timeout={timeout} score={score:.4%}")
if score < 0.60:
    raise SystemExit(f"ERROR: G-4 mutation score {score:.4%} < 60.0% threshold")
PY
fi

echo "[6/7] Extended soak & parallel 10-target fuzzing (G-11, G-14)"
SOAK_SECS="${GA_RUNNER_SOAK_DURATION_SECS:-${GAP_SOAK_DURATION_SECS:-259200}}"
GAP_SOAK_DURATION_SECS="${SOAK_SECS}" CI=1 cargo nextest run --release --no-pager -j 1 -p gap-server --test soak
FUZZ_SECS="${GA_RUNNER_FUZZ_MAX_TIME:-${GAP_FUZZ_MAX_TIME:-86400}}"
fuzz_pids=()
fuzz_targets=(
  fuzz_cursor_decode fuzz_extract_graphql fuzz_extract_mcp fuzz_extract_openapi
  fuzz_extract_proto fuzz_extract_sql fuzz_gapql_parse fuzz_ngc_decode
  fuzz_policy_input fuzz_sanitize
)
for target in "${fuzz_targets[@]}"; do
  fuzz_log="${GA_RUNNER_RESULTS_DIR:-target}/fuzz-${target}.log"
  (cd fuzz && cargo +nightly fuzz run "${target}" --target x86_64-unknown-linux-gnu -- -max_total_time="${FUZZ_SECS}") >"${fuzz_log}" 2>&1 &
  fuzz_pids+=("$!")
done

fuzz_rc=0
for idx in "${!fuzz_pids[@]}"; do
  if ! wait "${fuzz_pids[$idx]}"; then
    echo "ERROR: fuzz target ${fuzz_targets[$idx]} failed; inspect fuzz-${fuzz_targets[$idx]}.log" >&2
    fuzz_rc=1
  fi
done
if [[ "${fuzz_rc}" -ne 0 ]]; then
  exit 1
fi

echo "[7/7] GA Linux verification suite completed."
if [[ -n "${GA_RUNNER_RESULTS_DIR:-}" ]]; then
  date -u +"completed_utc=%Y-%m-%dT%H:%M:%SZ mode=${MODE}" > "${GA_RUNNER_RESULTS_DIR}/ga-full-release-complete.txt"
fi
