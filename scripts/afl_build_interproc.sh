#!/usr/bin/env bash
set -euo pipefail

PROFILE="${PROFILE:-release}"
TARGET="${TARGET:-bytes}"

canonical_path() {
  local p="$1"
  if realpath -m "$p" >/dev/null 2>&1; then
    realpath -m "$p"
    return 0
  fi
  if realpath "$p" >/dev/null 2>&1; then
    realpath "$p"
    return 0
  fi
  python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$p"
}

BASE_TARGET_DIR="${HARNESS_TARGET_DIR:-./target/afl-${PROFILE}-${TARGET}}"
BASE_TARGET_DIR="$(canonical_path "$BASE_TARGET_DIR")"
ANALYZE_TARGET_DIR="${RZ_INTERPROC_ANALYZE_TARGET_DIR:-${BASE_TARGET_DIR}-summary-pass}"
ANALYZE_TARGET_DIR="$(canonical_path "$ANALYZE_TARGET_DIR")"
MERGED_SUMMARY_DIR="${RZ_UNSAFE_SUMMARY_INPUT_DIR:-${BASE_TARGET_DIR}-unsafe-summaries-merged}"
MERGED_SUMMARY_DIR="$(canonical_path "$MERGED_SUMMARY_DIR")"

echo "[rusteze] phase 1/3: analyze-only build -> ${ANALYZE_TARGET_DIR}"
env \
  HARNESS_TARGET_DIR="${ANALYZE_TARGET_DIR}" \
  RZ_ANALYZE_UNSAFE_SUMMARIES=1 \
  RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP=1 \
  RZ_USE_UNSAFE_SUMMARIES=0 \
  ./scripts/afl_build.sh

SUMMARY_INPUT_DIR="${ANALYZE_TARGET_DIR}/rusteze-unsafe-summaries"
if [[ ! -d "${SUMMARY_INPUT_DIR}" ]]; then
  echo "missing summary dump dir: ${SUMMARY_INPUT_DIR}" >&2
  exit 2
fi

rm -rf "${MERGED_SUMMARY_DIR}"
mkdir -p "${MERGED_SUMMARY_DIR}"

echo "[rusteze] phase 2/3: merge summaries -> ${MERGED_SUMMARY_DIR}"
python3 ./scripts/merge_unsafe_summaries.py \
  --input-dir "${SUMMARY_INPUT_DIR}" \
  --output-dir "${MERGED_SUMMARY_DIR}" \
  --report "${MERGED_SUMMARY_DIR}/merge.report.txt"

echo "[rusteze] phase 3/3: instrumented build using merged summaries -> ${BASE_TARGET_DIR}"
env \
  HARNESS_TARGET_DIR="${BASE_TARGET_DIR}" \
  RZ_ANALYZE_UNSAFE_SUMMARIES=0 \
  RZ_UNSAFE_DATAFLOW_SUMMARY_DUMP=0 \
  RZ_USE_UNSAFE_SUMMARIES=1 \
  RZ_UNSAFE_SUMMARY_INPUT_DIR="${MERGED_SUMMARY_DIR}" \
  ./scripts/afl_build.sh
