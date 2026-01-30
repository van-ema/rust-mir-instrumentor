#!/usr/bin/env bash
set -euo pipefail

: "${AFL_PATH:?Set AFL_PATH to your AFLplusplus checkout (must contain afl-fuzz and afl-compiler-rt.o)}"

PROFILE="${PROFILE:-release}"
TARGET="${TARGET:-bytes}" # bytes | smallvec

case "$TARGET" in
  bytes) BIN="afl_bytes_driver" ;;
  smallvec) BIN="afl_smallvec_driver" ;;
  *) echo "unknown TARGET=$TARGET (expected bytes|smallvec)" >&2; exit 2 ;;
esac

RUNTIME_PATH="./target/${PROFILE}"
TOOL="./target/${PROFILE}/cargo-instrument-mir"
HARNESS_TARGET_DIR="./target/afl-${PROFILE}"

export CARGO_INCREMENTAL=0
export RZ_INSTRUMENT_ALL_DEPS=1
export CARGO_TARGET_DIR="$HARNESS_TARGET_DIR"

if [[ ! -f "${AFL_PATH}/afl-compiler-rt.o" ]]; then
  echo "missing ${AFL_PATH}/afl-compiler-rt.o (did you run 'make distrib' in AFLplusplus?)" >&2
  exit 2
fi

# Build rusteze toolchain + runtime in the chosen profile.
if [[ "$PROFILE" == "release" ]]; then
  cargo build -p runtime --release
  cargo build -p instrument-mir --release
else
  cargo build -p runtime
  cargo build -p instrument-mir
fi

if [[ ! -x "$TOOL" ]]; then
  echo "missing $TOOL (instrument-mir build failed?)" >&2
  exit 2
fi

# AFL++ coverage for Rust via LLVM SanitizerCoverage (PCGUARD).
# This mirrors the approach used by rust-fuzz/afl.rs and AFL++ Rust guidance.
#
# NOTE: These are LLVM-internal flags; they may need adjustment across LLVM versions.
AFL_RUSTFLAGS=(
  "-Cpasses=sancov-module"
  "-Cllvm-args=-sanitizer-coverage-level=3"
  "-Cllvm-args=-sanitizer-coverage-trace-pc-guard"
  "-Cllvm-args=-sanitizer-coverage-prune-blocks=0"
  "-Clink-arg=${AFL_PATH}/afl-compiler-rt.o"
)

export RUSTFLAGS="${RUSTFLAGS:-} ${AFL_RUSTFLAGS[*]}"

"$TOOL" instrument-mir --runtime-path="$RUNTIME_PATH" ${PROFILE/release/--release} -p afl_harness --bin "$BIN"

echo "built: ${HARNESS_TARGET_DIR}/${PROFILE}/${BIN}"
