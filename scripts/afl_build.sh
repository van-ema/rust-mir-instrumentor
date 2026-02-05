#!/usr/bin/env bash
set -euo pipefail

# Enable shell tracing with TRACE=1 to see executed commands and env.
if [[ "${TRACE:-}" == "1" ]]; then
  set -x
fi

# AFL++ location:
# - If you have an AFLplusplus *checkout*, set `AFL_PATH` to that directory.
# - If you have an AFL++ *system install* (e.g. `/usr/local/bin/afl-fuzz`), you can leave
#   `AFL_PATH` unset and we will auto-detect `afl-compiler-rt.o`.
#
# We always need `afl-compiler-rt.o` to link coverage runtime into Rust binaries.
AFL_PATH="${AFL_PATH:-}"
AFL_COMPILER_RT="${AFL_COMPILER_RT:-}"

PROFILE="${PROFILE:-release}"
TARGET="${TARGET:-bytes}" # bytes | smallvec

case "$TARGET" in
  bytes) BIN="afl_bytes_driver" ;;
  smallvec) BIN="afl_smallvec_driver" ;;
  *) echo "unknown TARGET=$TARGET (expected bytes|smallvec)" >&2; exit 2 ;;
esac

HARNESS_TARGET_DIR="${HARNESS_TARGET_DIR:-./target/afl-${PROFILE}}"
# Use absolute paths so TMPDIR/RUSTC_TMPDIR are stable even if Cargo changes CWD.
HARNESS_TARGET_DIR="$(realpath -m "$HARNESS_TARGET_DIR")"
# `cargo build -p runtime` places `libruntime.rlib` in `${target_dir}/${profile}/deps/`.
# If we point `--runtime-path` at `${profile}/`, a stale `${profile}/libruntime.rlib` can be
# picked up and cause E0460 "found possibly newer version of crate `runtime`".
RUNTIME_PATH="${HARNESS_TARGET_DIR}/${PROFILE}/deps"

export CARGO_INCREMENTAL=0
export RZ_INSTRUMENT_ALL_DEPS=1
export CARGO_TARGET_DIR="$HARNESS_TARGET_DIR"
# Put temp files in the same directory that rustc writes metadata (`deps/`) to avoid EXDEV.
export RUSTC_TMPDIR="${RUSTC_TMPDIR:-$(realpath -m "${RUNTIME_PATH}")}"
export TMPDIR="${TMPDIR:-$(realpath -m "${RUNTIME_PATH}")}"
mkdir -p "$RUSTC_TMPDIR"

if [[ -z "$AFL_COMPILER_RT" ]]; then
  if [[ -n "$AFL_PATH" && -f "${AFL_PATH}/afl-compiler-rt.o" ]]; then
    AFL_COMPILER_RT="${AFL_PATH}/afl-compiler-rt.o"
  elif [[ -f "/usr/local/lib/afl/afl-compiler-rt.o" ]]; then
    AFL_COMPILER_RT="/usr/local/lib/afl/afl-compiler-rt.o"
  elif [[ -f "/usr/lib/afl/afl-compiler-rt.o" ]]; then
    AFL_COMPILER_RT="/usr/lib/afl/afl-compiler-rt.o"
  elif [[ -f "/opt/aflpp/afl-compiler-rt.o" ]]; then
    # Docker image builds AFL++ from source into /opt/aflpp.
    AFL_COMPILER_RT="/opt/aflpp/afl-compiler-rt.o"
  fi
fi

if [[ -z "$AFL_COMPILER_RT" || ! -f "$AFL_COMPILER_RT" ]]; then
  echo "missing AFL++ runtime (afl-compiler-rt.o)." >&2
  echo "set AFL_COMPILER_RT to the full path, or set AFL_PATH to your AFLplusplus checkout." >&2
  exit 2
fi

# Build rusteze toolchain + runtime in the chosen profile.
RUNTIME_FEATURES="${RUNTIME_FEATURES:-}"
if [[ "$PROFILE" == "release" ]]; then
  cargo build -p instrument-mir --release
  # Build runtime *after* instrument-mir so the final rlib in deps reflects RUNTIME_FEATURES.
  cargo build -p runtime --release ${RUNTIME_FEATURES}
else
  cargo build -p instrument-mir
  cargo build -p runtime ${RUNTIME_FEATURES}
fi

# Avoid accidental linking against a stale top-level `libruntime.rlib` if one exists.
rm -f "${HARNESS_TARGET_DIR}/${PROFILE}/libruntime.rlib" 2>/dev/null || true

TOOL="${HARNESS_TARGET_DIR}/${PROFILE}/cargo-instrument-mir"
if [[ ! -x "$TOOL" ]]; then
  TOOL="$(command -v cargo-instrument-mir || true)"
fi
if [[ -z "$TOOL" || ! -x "$TOOL" ]]; then
  echo "missing cargo-instrument-mir (instrument-mir build/install failed?)" >&2
  exit 2
fi

# AFL++ coverage for Rust via LLVM SanitizerCoverage (PCGUARD).
# This mirrors the approach used by rust-fuzz/afl.rs and AFL++ Rust guidance.
#
# NOTE: These are LLVM-internal flags; they may need adjustment across LLVM versions.
AFL_RUSTFLAGS=(
  "-Ztemps-dir=${RUSTC_TMPDIR}"
  "-Cpasses=sancov-module"
  "-Cllvm-args=-sanitizer-coverage-level=3"
  "-Cllvm-args=-sanitizer-coverage-trace-pc-guard"
  "-Cllvm-args=-sanitizer-coverage-prune-blocks=0"
  "-Clink-arg=${AFL_COMPILER_RT}"
)

export RUSTFLAGS="${RUSTFLAGS:-} ${AFL_RUSTFLAGS[*]}"

"$TOOL" instrument-mir --runtime-path="$RUNTIME_PATH" ${PROFILE/release/--release} -p afl_harness --bin "$BIN"

echo "built: ${HARNESS_TARGET_DIR}/${PROFILE}/${BIN}"
