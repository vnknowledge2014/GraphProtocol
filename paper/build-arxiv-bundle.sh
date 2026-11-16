#!/usr/bin/env bash
# Package standalone arXiv source tarball (`target/gap-arxiv-submission.tar.gz`).
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

python3 paper/scripts/validate_tex.py

OUT_TAR="${ROOT_DIR}/target/gap-arxiv-submission.tar.gz"
mkdir -p "${ROOT_DIR}/target"

STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/gap_arxiv_stage.XXXXXX")"
trap 'rm -rf "${STAGE_DIR}"' EXIT

mkdir -p "${STAGE_DIR}/figures"
cp paper/main.tex paper/author_config.tex paper/references.bib "${STAGE_DIR}/"
cp paper/figures/*.tex "${STAGE_DIR}/figures/"

(cd "${STAGE_DIR}" && COPYFILE_DISABLE=1 tar -czf "${OUT_TAR}" .)
echo "Created arXiv submission archive: ${OUT_TAR}"
tar -ztvf "${OUT_TAR}"
