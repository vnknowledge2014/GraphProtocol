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

TARGET_TRIPLE="${CARGO_BUILD_TARGET:-}"
EXTRA_CARGO_ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --offline)
      EXTRA_CARGO_ARGS+=("--offline")
      shift
      ;;
    --target)
      if [[ $# -lt 2 ]]; then
        echo "Missing triple after --target" >&2
        exit 2
      fi
      TARGET_TRIPLE="$2"
      shift 2
      ;;
    *)
      echo "Unknown argument: $1" >&2
      echo "Usage: $0 [--offline] [--target <triple>]" >&2
      exit 2
      ;;
  esac
done

export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=${ROOT_DIR}=/gap-src"

if [[ -n "${TARGET_TRIPLE}" ]]; then
  EXTRA_CARGO_ARGS+=("--target" "${TARGET_TRIPLE}")
  BIN_PATH="${ROOT_DIR}/target/${TARGET_TRIPLE}/release/gap"
else
  BIN_PATH="${ROOT_DIR}/target/release/gap"
fi

if [[ ${#EXTRA_CARGO_ARGS[@]} -gt 0 ]]; then
  cargo build --release --locked "${EXTRA_CARGO_ARGS[@]}" -p gap-cli --bin gap
else
  cargo build --release --locked -p gap-cli --bin gap
fi
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
