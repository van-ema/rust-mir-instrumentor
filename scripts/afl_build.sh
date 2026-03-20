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
TARGET="${TARGET:-bytes}" # bytes | smallvec | serde_json | toml | base64 | uuid | itoa | quick_xml | simd_json | zip | rkyv | hyper | image
RZ_VERIFY_HOOKS="${RZ_VERIFY_HOOKS:-1}"
RZ_VERIFY_HOOKS_STRICT="${RZ_VERIFY_HOOKS_STRICT:-0}"
RZ_INTERPROC_UNSAFE_SUMMARIES="${RZ_INTERPROC_UNSAFE_SUMMARIES:-0}"

case "$TARGET" in
  bytes) BIN="afl_bytes_driver"; FEATURE="bytes_driver" ;;
  smallvec) BIN="afl_smallvec_driver"; FEATURE="smallvec_driver" ;;
  serde_json|serde) BIN="afl_serde_json_driver"; FEATURE="serde_json_driver" ;;
  toml) BIN="afl_toml_driver"; FEATURE="toml_driver" ;;
  base64) BIN="afl_base64_driver"; FEATURE="base64_driver" ;;
  uuid) BIN="afl_uuid_driver"; FEATURE="uuid_driver" ;;
  itoa) BIN="afl_itoa_driver"; FEATURE="itoa_driver" ;;
  quick_xml|quick-xml) BIN="afl_quick_xml_driver"; FEATURE="quick_xml_driver" ;;
  simd_json|simd-json) BIN="afl_simd_json_driver"; FEATURE="simd_json_driver" ;;
  zip) BIN="afl_zip_driver"; FEATURE="zip_driver" ;;
  rkyv) BIN="afl_rkyv_driver"; FEATURE="rkyv_driver" ;;
  hyper) BIN="afl_hyper_driver"; FEATURE="hyper_driver" ;;
  image) BIN="afl_image_driver"; FEATURE="image_driver" ;;
  *) echo "unknown TARGET=$TARGET (expected bytes|smallvec|serde_json|serde|toml|base64|uuid|itoa|quick_xml|simd_json|zip|rkyv|hyper|image)" >&2; exit 2 ;;
esac

if [[ "${RZ_INTERPROC_UNSAFE_SUMMARIES}" == "1" && "${RZ_INTERPROC_STAGE:-0}" != "1" ]]; then
  exec env RZ_INTERPROC_STAGE=1 ./scripts/afl_build_interproc.sh
fi

if [[ "$TARGET" == "serde" || "$TARGET" == "serde_json" ]]; then
  if [[ ! -f "third_party/serde/serde/Cargo.toml" ]]; then
    echo "missing third_party/serde checkout (needed by third_party/serde_json path dependency)." >&2
    echo "run: git clone https://github.com/serde-rs/serde.git third_party/serde" >&2
    exit 2
  fi
fi
if [[ "$TARGET" == "hyper" ]]; then
  if [[ ! -f "third_party/hyper/Cargo.toml" ]]; then
    echo "missing third_party/hyper checkout." >&2
    echo "run: git clone https://github.com/hyperium/hyper.git third_party/hyper" >&2
    exit 2
  fi
fi

HARNESS_TARGET_DIR="${HARNESS_TARGET_DIR:-./target/afl-${PROFILE}-${TARGET}}"
# Use absolute paths so TMPDIR/RUSTC_TMPDIR are stable even if Cargo changes CWD.
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

HARNESS_TARGET_DIR="$(canonical_path "$HARNESS_TARGET_DIR")"
# `cargo build -p runtime` places `libruntime.rlib` in `${target_dir}/${profile}/deps/`.
# If we point `--runtime-path` at `${profile}/`, a stale `${profile}/libruntime.rlib` can be
# picked up and cause E0460 "found possibly newer version of crate `runtime`".
RUNTIME_PATH="${HARNESS_TARGET_DIR}/${PROFILE}/deps"

export CARGO_INCREMENTAL=0
export RZ_INSTRUMENT_ALL_DEPS=1
# Keep aliasing checks enabled by default with tb_lite as the active model.
export RZ_ALIAS_MODEL="${RZ_ALIAS_MODEL:-tb_lite}"
# SB-lite knob is still honored when explicitly selecting RZ_ALIAS_MODEL=sb_lite.
export RZ_SB_LITE="${RZ_SB_LITE:-1}"
export CARGO_TARGET_DIR="$HARNESS_TARGET_DIR"
# Put temp files in the same directory that rustc writes metadata (`deps/`) to avoid EXDEV.
export RUSTC_TMPDIR="${RUSTC_TMPDIR:-$(canonical_path "${RUNTIME_PATH}")}"
export TMPDIR="${TMPDIR:-$(canonical_path "${RUNTIME_PATH}")}"
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

hash_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
    return 0
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
    return 0
  fi
  cksum "$file" | awk '{print $1":"$2}'
}

find_runtime_rlib() {
  local deps_dir="$1"
  local candidate=""
  candidate="$(ls -t "${deps_dir}"/libruntime-*.rlib 2>/dev/null | head -n 1 || true)"
  if [[ -z "${candidate}" || ! -f "${candidate}" ]]; then
    candidate="${deps_dir}/libruntime.rlib"
    if [[ ! -f "${candidate}" ]]; then
      candidate=""
    fi
  fi
  echo "${candidate}"
}

build_tools_and_runtime() {
  if [[ "$PROFILE" == "release" ]]; then
    cargo build -p instrument-mir --release --bins
    # Build runtime *after* instrument-mir so the final rlib in deps reflects RUNTIME_FEATURES.
    cargo build -p runtime --release ${RUNTIME_FEATURES}
    PROFILE_FLAG="--release"
  else
    cargo build -p instrument-mir --bins
    cargo build -p runtime ${RUNTIME_FEATURES}
    PROFILE_FLAG=""
  fi
}

# Build rusteze toolchain + runtime in the chosen profile.
RUNTIME_FEATURES="${RUNTIME_FEATURES:-}"
build_tools_and_runtime

# Avoid accidental linking against a stale top-level `libruntime.rlib` if one exists.
rm -f "${HARNESS_TARGET_DIR}/${PROFILE}/libruntime.rlib" 2>/dev/null || true

TOOL="${HARNESS_TARGET_DIR}/${PROFILE}/cargo-instrument-mir"
if [[ ! -x "$TOOL" ]]; then
  echo "missing ${TOOL} after local instrument-mir build; refusing PATH fallback to avoid stale toolchain use" >&2
  exit 2
fi
INSTRUMENT_BIN="${HARNESS_TARGET_DIR}/${PROFILE}/instrument-mir"
if [[ ! -x "$INSTRUMENT_BIN" ]]; then
  echo "missing ${INSTRUMENT_BIN} after local instrument-mir build" >&2
  exit 2
fi

STAMP_FILE="${HARNESS_TARGET_DIR}/.rusteze-toolchain-stamp"
runtime_rlib="$(find_runtime_rlib "${RUNTIME_PATH}")"
if [[ -z "${runtime_rlib}" || ! -f "${runtime_rlib}" ]]; then
  echo "missing runtime rlib under ${RUNTIME_PATH} (runtime build failed?)" >&2
  exit 2
fi

current_stamp="$(
  {
    echo "tool=$(hash_file "${TOOL}")"
    echo "instrumentor=$(hash_file "${INSTRUMENT_BIN}")"
    echo "runtime=$(hash_file "${runtime_rlib}")"
    echo "profile=${PROFILE}"
    echo "runtime_features=${RUNTIME_FEATURES}"
    echo "instrument_all_deps=${RZ_INSTRUMENT_ALL_DEPS:-}"
    echo "instrumented_crates=${RZ_INSTRUMENTED_CRATES:-}"
  } | tr '\n' ';'
)"
clean_needed=0

# Legacy target dirs created before stamp support can keep stale instrumented artifacts.
if [[ ! -f "${STAMP_FILE}" && -f "${HARNESS_TARGET_DIR}/${PROFILE}/${BIN}" ]]; then
  clean_needed=1
fi

if [[ -f "${STAMP_FILE}" ]]; then
  old_stamp="$(cat "${STAMP_FILE}" || true)"
  if [[ "${old_stamp}" != "${current_stamp}" ]]; then
    clean_needed=1
  fi
fi

if [[ "${clean_needed}" == "1" ]]; then
  echo "[rusteze] instrumentation toolchain changed; cleaning ${HARNESS_TARGET_DIR} to avoid stale MIR artifacts"
  cargo clean --target-dir "${HARNESS_TARGET_DIR}"
  build_tools_and_runtime
  rm -f "${HARNESS_TARGET_DIR}/${PROFILE}/libruntime.rlib" 2>/dev/null || true

  TOOL="${HARNESS_TARGET_DIR}/${PROFILE}/cargo-instrument-mir"
  if [[ ! -x "$TOOL" ]]; then
    echo "missing ${TOOL} after clean/rebuild; refusing PATH fallback to avoid stale toolchain use" >&2
    exit 2
  fi
  INSTRUMENT_BIN="${HARNESS_TARGET_DIR}/${PROFILE}/instrument-mir"
  if [[ ! -x "$INSTRUMENT_BIN" ]]; then
    echo "missing ${INSTRUMENT_BIN} after clean/rebuild" >&2
    exit 2
  fi

  runtime_rlib="$(find_runtime_rlib "${RUNTIME_PATH}")"
  if [[ -z "${runtime_rlib}" || ! -f "${runtime_rlib}" ]]; then
    echo "missing runtime rlib after clean/rebuild under ${RUNTIME_PATH}" >&2
    exit 2
  fi

  current_stamp="$(
    {
      echo "tool=$(hash_file "${TOOL}")"
      echo "instrumentor=$(hash_file "${INSTRUMENT_BIN}")"
      echo "runtime=$(hash_file "${runtime_rlib}")"
      echo "profile=${PROFILE}"
      echo "runtime_features=${RUNTIME_FEATURES}"
    } | tr '\n' ';'
  )"
fi

echo "${current_stamp}" > "${STAMP_FILE}"

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

"$TOOL" instrument-mir --runtime-path="$RUNTIME_PATH" $PROFILE_FLAG -p afl_harness --features "$FEATURE" --bin "$BIN"

BIN_PATH="${HARNESS_TARGET_DIR}/${PROFILE}/${BIN}"
echo "built: ${BIN_PATH}"

if [[ "${RZ_VERIFY_HOOKS}" == "1" || "${RZ_VERIFY_HOOKS}" == "true" ]]; then
  hooks_found=0
  # Check for known hook symbol names. We inspect multiple symbol sources
  # because some builds only expose dynamic symbols.
  hook_tokens=(
    "__rz_ptr_read"
    "__rz_ptr_write"
    "__record_ref_creation"
    "__record_raw_ptr_creation"
  )
  symbol_dump=""
  if command -v nm >/dev/null 2>&1; then
    symbol_dump+=$'\n'"$(nm -an "${BIN_PATH}" 2>/dev/null || true)"
    symbol_dump+=$'\n'"$(nm -D "${BIN_PATH}" 2>/dev/null || true)"
  fi
  if command -v readelf >/dev/null 2>&1; then
    symbol_dump+=$'\n'"$(readelf -Ws "${BIN_PATH}" 2>/dev/null || true)"
  fi
  if command -v strings >/dev/null 2>&1; then
    symbol_dump+=$'\n'"$(strings "${BIN_PATH}" 2>/dev/null || true)"
  fi

  for tok in "${hook_tokens[@]}"; do
    # Use a here-string instead of a pipe to avoid SIGPIPE/pipefail false negatives.
    if grep -F -q "${tok}" <<<"${symbol_dump}"; then
      hooks_found=1
      break
    fi
  done

  if [[ "${hooks_found}" == "1" ]]; then
    echo "[rusteze] hook check: found rusteze hooks (__rz_*) in ${BIN}"
  else
    msg="[rusteze] hook check: no rusteze hooks (__rz_*) found in ${BIN} (possible non-instrumented build)"
    if [[ "${RZ_VERIFY_HOOKS_STRICT}" == "1" || "${RZ_VERIFY_HOOKS_STRICT}" == "true" ]]; then
      echo "${msg}" >&2
      exit 3
    fi
    echo "${msg}" >&2
  fi
fi
