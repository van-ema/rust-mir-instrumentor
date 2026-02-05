#!/usr/bin/env bash
set -euo pipefail

# Usage:
#   scripts/run_with_trace.sh EXAMPLE=tag0_return_raw_ptr_from_arg [PROFILE=debug|release]
#
# Runs an example with runtime trace logging enabled and captures output.

EXAMPLE="${EXAMPLE:-}"
PROFILE="${PROFILE:-debug}"
OUT="${OUT:-trace.out}"

if [[ -z "$EXAMPLE" ]]; then
  echo "missing EXAMPLE=... (e.g., EXAMPLE=tag0_return_raw_ptr_from_arg)" >&2
  exit 2
fi

PROFILE_FLAG=()
RUNTIME_PATH="target/debug"
if [[ "$PROFILE" == "release" ]]; then
  PROFILE_FLAG=(--release)
  RUNTIME_PATH="target/release"
fi

export CARGO_INCREMENTAL=0
export RZ_INSTRUMENT_ALL_DEPS=1

# Build runtime with logging enabled.
cargo build -p runtime "${PROFILE_FLAG[@]}" --features rz_log

# Instrument + build the example.
cargo instrument-mir --runtime-path="${RUNTIME_PATH}" \
  -p "$EXAMPLE" --bin "$EXAMPLE" "${PROFILE_FLAG[@]}"

# Run with tracing enabled.
RZ_LOG=trace RZ_LOG_LOC=1 "${RUNTIME_PATH}/${EXAMPLE}" > "$OUT" 2>&1

echo "trace written to: $OUT"
