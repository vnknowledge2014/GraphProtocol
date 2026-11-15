#!/usr/bin/env bash
# Reproducible release build script for `gap` (`WP-M7-08`, `OPS-041`, `SEC-101`).
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-1700000000}"
export TZ="UTC"
export LC_ALL="C"
export CARGO_INCREMENTAL=0

OUT_DIR="${OUT_DIR:-${ROOT_DIR}/target/dist}"
mkdir -p "${OUT_DIR}"

EXTRA_CARGO_ARGS=()
for arg in "$@"; do
  case "${arg}" in
    --offline)
      EXTRA_CARGO_ARGS+=("--offline")
      ;;
    *)
      echo "Unknown argument: ${arg}" >&2
      echo "Usage: $0 [--offline]" >&2
      exit 2
      ;;
  esac
done

export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=${ROOT_DIR}=/gap-src"

if [[ ${#EXTRA_CARGO_ARGS[@]} -gt 0 ]]; then
  cargo build --release --locked "${EXTRA_CARGO_ARGS[@]}" -p gap-cli --bin gap
else
  cargo build --release --locked -p gap-cli --bin gap
fi

BIN_PATH="${ROOT_DIR}/target/release/gap"
if [[ ! -x "${BIN_PATH}" ]]; then
  echo "Expected compiled binary at ${BIN_PATH}" >&2
  exit 1
fi

cp "${BIN_PATH}" "${OUT_DIR}/gap"

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$(dirname "$1")" && sha256sum "$(basename "$1")")
  else
    (cd "$(dirname "$1")" && shasum -a 256 "$(basename "$1")")
  fi
}

sha256_file "${OUT_DIR}/gap" > "${OUT_DIR}/gap.sha256"
echo "Built reproducible binary: ${OUT_DIR}/gap"
cat "${OUT_DIR}/gap.sha256"
