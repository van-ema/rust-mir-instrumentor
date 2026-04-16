#!/usr/bin/env bash
set -euo pipefail

# AFL++ location:
# - If you have an AFLplusplus checkout, set AFL_PATH to that directory.
# - If afl-fuzz is installed system-wide, AFL_PATH may be left unset.
AFL_PATH="${AFL_PATH:-}"
AFL_FUZZ="${AFL_FUZZ:-}"
AFL_COMPILER_RT="${AFL_COMPILER_RT:-}"

PROFILE="${PROFILE:-release}"
TARGET="${TARGET:-bytes}" # bytes | smallvec | serde_json | toml | base64 | uuid | itoa | quick_xml | simd_json | zip | rkyv | hyper | image | hashbrown | bumpalo | indexmap | bootc_kcmdline | abacus_apportionment | kvm_bindings
TIMEOUT_MS="${TIMEOUT_MS:-}"
IN_DIR="${IN_DIR:-fuzz/corpus/${TARGET}}"
OUT_DIR="${OUT_DIR:-fuzz/out-asan/${TARGET}}"
HARNESS_TARGET_DIR="${HARNESS_TARGET_DIR:-./target/afl-asan-${PROFILE}-${TARGET}}"
BUILD_TARGET="${BUILD_TARGET:-}"
ASAN_RUSTFLAGS="${ASAN_RUSTFLAGS:--Zsanitizer=address}"
ASAN_OPTIONS_DEFAULT="detect_leaks=0:abort_on_error=1:symbolize=0"
RUSTUP_TOOLCHAIN_OVERRIDE="${RUSTUP_TOOLCHAIN_OVERRIDE:-}"

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
  hashbrown) BIN="afl_hashbrown_driver"; FEATURE="hashbrown_driver" ;;
  bumpalo) BIN="afl_bumpalo_driver"; FEATURE="bumpalo_driver" ;;
  indexmap) BIN="afl_indexmap_driver"; FEATURE="indexmap_driver" ;;
  bootc_kcmdline|bootc-kcmdline|bootc_kernel_cmdline) BIN="afl_bootc_kcmdline_driver"; FEATURE="bootc_kcmdline_driver" ;;
  abacus_apportionment|abacus-apportionment) BIN="afl_abacus_apportionment_driver"; FEATURE="abacus_apportionment_driver" ;;
  kvm_bindings|kvm-bindings) BIN="afl_kvm_bindings_driver"; FEATURE="kvm_bindings_driver" ;;
  *) echo "unknown TARGET=$TARGET (expected bytes|smallvec|serde_json|serde|toml|base64|uuid|itoa|quick_xml|simd_json|zip|rkyv|hyper|image|hashbrown|bumpalo|indexmap|bootc_kcmdline|abacus_apportionment|kvm_bindings)" >&2; exit 2 ;;
esac

canonical_path() {
  local p="$1"
  if realpath -m "$p" >/dev/null 2>&1; then
    realpath -m "$p"
    return 0
  fi
  python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$p"
}

detect_build_target() {
  if [[ -n "$RUSTUP_TOOLCHAIN_OVERRIDE" ]]; then
    rustc "+${RUSTUP_TOOLCHAIN_OVERRIDE}" -vV 2>/dev/null | sed -n 's/^host: //p' | head -n1
  else
    rustc -vV 2>/dev/null | sed -n 's/^host: //p' | head -n1
  fi
}

HARNESS_TARGET_DIR="$(canonical_path "$HARNESS_TARGET_DIR")"
IN_DIR="$(canonical_path "$IN_DIR")"
OUT_DIR="$(canonical_path "$OUT_DIR")"
if [[ -z "$BUILD_TARGET" ]]; then
  BUILD_TARGET="$(detect_build_target)"
fi
if [[ -z "$BUILD_TARGET" ]]; then
  echo "error: failed to detect cargo/rustc host target; set BUILD_TARGET explicitly." >&2
  exit 2
fi
BIN_PATH="${HARNESS_TARGET_DIR}/${BUILD_TARGET}/${PROFILE}/${BIN}"

mkdir -p "$IN_DIR" "$OUT_DIR"

if [[ -z "$AFL_COMPILER_RT" ]]; then
  if [[ -n "$AFL_PATH" && -f "${AFL_PATH}/afl-compiler-rt.o" ]]; then
    AFL_COMPILER_RT="${AFL_PATH}/afl-compiler-rt.o"
  elif [[ -f "/usr/local/lib/afl/afl-compiler-rt.o" ]]; then
    AFL_COMPILER_RT="/usr/local/lib/afl/afl-compiler-rt.o"
  elif [[ -f "/usr/lib/afl/afl-compiler-rt.o" ]]; then
    AFL_COMPILER_RT="/usr/lib/afl/afl-compiler-rt.o"
  elif [[ -f "/opt/aflpp/afl-compiler-rt.o" ]]; then
    AFL_COMPILER_RT="/opt/aflpp/afl-compiler-rt.o"
  fi
fi

if [[ -z "$AFL_COMPILER_RT" || ! -f "$AFL_COMPILER_RT" ]]; then
  echo "missing AFL++ runtime (afl-compiler-rt.o)." >&2
  echo "set AFL_COMPILER_RT to the full path, or set AFL_PATH to your AFLplusplus checkout." >&2
  exit 2
fi

build_asan_target() {
  export CARGO_TARGET_DIR="$HARNESS_TARGET_DIR"
  export CARGO_INCREMENTAL=0
  unset RZ_INSTRUMENT_ALL_DEPS
  unset RZ_UNSAFE_DATAFLOW
  unset RZ_ANALYZE_UNSAFE_SUMMARIES
  unset RZ_USE_UNSAFE_SUMMARIES
  unset RZ_INTERPROC_UNSAFE_SUMMARIES
  unset RZ_ALIAS_MODEL
  unset RZ_SB_LITE
  local afl_cov_flags=(
    "-Cpasses=sancov-module"
    "-Cllvm-args=-sanitizer-coverage-level=3"
    "-Cllvm-args=-sanitizer-coverage-trace-pc-guard"
    "-Cllvm-args=-sanitizer-coverage-prune-blocks=0"
    "-Clink-arg=${AFL_COMPILER_RT}"
  )
  local cov_flags="${afl_cov_flags[*]}"
  local target_var="CARGO_TARGET_${BUILD_TARGET^^}_RUSTFLAGS"
  target_var="${target_var//-/_}"
  if [[ -n "${!target_var:-}" ]]; then
    export "${target_var}=${!target_var} ${ASAN_RUSTFLAGS} ${cov_flags}"
  else
    export "${target_var}=${ASAN_RUSTFLAGS} ${cov_flags}"
  fi

  local cmd=(cargo)
  if [[ -n "$RUSTUP_TOOLCHAIN_OVERRIDE" ]]; then
    cmd+=("+${RUSTUP_TOOLCHAIN_OVERRIDE}")
  fi
  cmd+=(build -p afl_harness --features "$FEATURE" --bin "$BIN")
  cmd+=(--target "$BUILD_TARGET")
  if [[ "$PROFILE" == "release" ]]; then
    cmd+=(--release)
  fi
  "${cmd[@]}"
}

if [[ ! -x "$BIN_PATH" ]]; then
  build_asan_target
fi

# AFL++ requires at least one seed file.
if ! find "$IN_DIR" -maxdepth 1 -type f -print -quit | grep -q .; then
  case "$TARGET" in
    hyper)
      printf 'GET / HTTP/1.1\r\nHost: fuzz.local\r\nConnection: close\r\n\r\n' > "${IN_DIR}/seed0"
      ;;
    image)
      cp benchmarks/eco_bench/data/eco_image_input.png "${IN_DIR}/seed0"
      ;;
    *)
      printf '\x00' > "${IN_DIR}/seed0"
      ;;
  esac
fi

if [[ -z "$AFL_FUZZ" ]]; then
  if [[ -n "$AFL_PATH" && -x "${AFL_PATH}/afl-fuzz" ]]; then
    AFL_FUZZ="${AFL_PATH}/afl-fuzz"
  else
    AFL_FUZZ="$(command -v afl-fuzz || true)"
  fi
fi
if [[ -z "$AFL_FUZZ" ]]; then
  echo "error: afl-fuzz not found (set AFL_FUZZ or AFL_PATH, or install AFL++)." >&2
  exit 2
fi

export AFL_SKIP_CPUFREQ=1
export AFL_NO_AFFINITY=1
export ASAN_OPTIONS="${ASAN_OPTIONS:-$ASAN_OPTIONS_DEFAULT}"
unset RUSTEZE_FAILFAST
unset RZ_ABORT_ON_VIOLATION

extra=()
if [[ -n "$TIMEOUT_MS" ]]; then
  extra+=("-t" "$TIMEOUT_MS")
fi

"$AFL_FUZZ" -i "$IN_DIR" -o "$OUT_DIR" "${extra[@]}" -- "$BIN_PATH" @@
