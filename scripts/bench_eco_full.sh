#!/usr/bin/env bash
set -euo pipefail

PROFILE="${PROFILE:-release}"
RUNS="${RUNS:-2}"
WARMUP="${WARMUP:-1}"
TIMEOUT_S="${TIMEOUT_S:-600}"
REPORT_DIR="${REPORT_DIR:-reports/overhead/eco_full}"

TARGETS=(
  eco_bench::eco_base64
  eco_bench::eco_bytes
  eco_bench::eco_hyper_uri
  eco_bench::eco_image
  eco_bench::eco_image_resize
  eco_bench::eco_itoa
  eco_bench::eco_quick_xml
  eco_bench::eco_serde_json
  eco_bench::eco_smallvec
  eco_bench::eco_toml
  eco_bench::eco_uuid_parse
)

mkdir -p target/tmp

exec env \
  CARGO_INCREMENTAL=0 \
  RZ_INSTRUMENT_ALL_DEPS=1 \
  TMPDIR="${TMPDIR:-$PWD/target/tmp}" \
  RUSTC_TMPDIR="${RUSTC_TMPDIR:-$PWD/target/tmp}" \
  python3 scripts/bench_overhead.py \
    --profile "${PROFILE}" \
    --targets "${TARGETS[@]}" \
    --runs "${RUNS}" \
    --warmup "${WARMUP}" \
    --timeout-s "${TIMEOUT_S}" \
    --include-asan \
    --report-dir "${REPORT_DIR}" \
    "$@"
