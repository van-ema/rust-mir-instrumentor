#!/usr/bin/env bash
set -euo pipefail

CARGO="${CARGO:-cargo}"
BUILD_PROFILE="${BUILD_PROFILE:-debug}"
BUILD_STD="${BUILD_STD:-0}"
BUILD_STD_CRATES="${BUILD_STD_CRATES:-alloc,std,core}"
BUILD_STD_FEATURES="${BUILD_STD_FEATURES:-}"
TARGET_ROOT="${TARGET_ROOT:-${CARGO_TARGET_DIR:-}}"
TARGETS="${TARGETS:-bytes smallvec}"
REPORT_DIR="${REPORT_DIR:-reports/fuzz_smoke}"

if [[ "${BUILD_PROFILE}" != "debug" && "${BUILD_PROFILE}" != "release" ]]; then
  echo "Unknown BUILD_PROFILE=${BUILD_PROFILE}. Use debug or release." >&2
  exit 1
fi

if [[ "${BUILD_STD}" != "0" && "${BUILD_STD}" != "1" ]]; then
  echo "Unknown BUILD_STD=${BUILD_STD}. Use 0 or 1." >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
cd "${repo_root}"

timestamp="$(date +%Y%m%d_%H%M%S)"
run_dir="${REPORT_DIR}/${timestamp}"
mkdir -p "${run_dir}"

export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export RZ_INSTRUMENT_ALL_DEPS="${RZ_INSTRUMENT_ALL_DEPS:-1}"
export RZ_LOG="${RZ_LOG:-warn}"
export RUSTEZE_FAILFAST="${RUSTEZE_FAILFAST:-1}"
export RZ_ABORT_ON_VIOLATION="${RZ_ABORT_ON_VIOLATION:-1}"
export RZ_ALIAS_MODEL="${RZ_ALIAS_MODEL:-tb_lite}"

profile_args=()
profile_dir="debug"
runtime_features=(--features rz_log)
if [[ "${BUILD_PROFILE}" == "release" ]]; then
  profile_args=(--release)
  profile_dir="release"
  runtime_features=()
fi

build_std_args=()
if [[ "${BUILD_STD}" == "1" ]]; then
  build_std_args=(-Z "build-std=${BUILD_STD_CRATES}")
  if [[ -n "${BUILD_STD_FEATURES}" ]]; then
    build_std_args+=(-Z "build-std-features=${BUILD_STD_FEATURES}")
  fi
fi

if [[ -z "${TARGET_ROOT}" ]]; then
  TARGET_ROOT="${repo_root}/target/fuzz-smoke"
  if [[ "${BUILD_STD}" == "1" ]]; then
    TARGET_ROOT="${TARGET_ROOT}/build-std-${BUILD_PROFILE}"
  fi
fi

export CARGO_TARGET_DIR="${TARGET_ROOT}"
runtime_path="${TARGET_ROOT}/${profile_dir}/deps"
bin_dir="${TARGET_ROOT}/${profile_dir}"

tool_target_root="${repo_root}/target"
tool_dir="${tool_target_root}/${profile_dir}"
tool_build_cmd=("${CARGO}" build -p instrument-mir --bins)
if (( ${#profile_args[@]} )); then
  tool_build_cmd+=("${profile_args[@]}")
fi
CARGO_TARGET_DIR="${tool_target_root}" "${tool_build_cmd[@]}"
if [[ ! -x "${tool_dir}/cargo-instrument-mir" || ! -x "${tool_dir}/instrument-mir" ]]; then
  echo "instrument-mir tools were not built successfully" >&2
  exit 1
fi
export PATH="${tool_dir}:${PATH}"

build_runtime_abi_cmd=("${CARGO}" build -p runtime_abi)
build_runtime_cmd=("${CARGO}" build -p runtime)
if (( ${#profile_args[@]} )); then
  build_runtime_abi_cmd+=("${profile_args[@]}")
  build_runtime_cmd+=("${profile_args[@]}")
fi
if (( ${#build_std_args[@]} )); then
  build_runtime_abi_cmd+=("${build_std_args[@]}")
  build_runtime_cmd+=("${build_std_args[@]}")
fi
if (( ${#runtime_features[@]} )); then
  build_runtime_cmd+=("${runtime_features[@]}")
fi

if [[ "${BUILD_STD}" == "1" ]]; then
  env \
    RZ_INSTRUMENT_ALL_DEPS=0 \
    RZ_INSTRUMENT_STDLIB="${RZ_INSTRUMENT_STDLIB:-none}" \
    RZ_SKIP_RUNTIME_HOOKS=1 \
    RUSTC=instrument-mir \
    "${build_runtime_abi_cmd[@]}"
  env \
    RZ_INSTRUMENT_ALL_DEPS=0 \
    RZ_INSTRUMENT_STDLIB="${RZ_INSTRUMENT_STDLIB:-none}" \
    RZ_SKIP_RUNTIME_HOOKS=1 \
    RUSTC=instrument-mir \
    "${build_runtime_cmd[@]}"
else
  "${build_runtime_abi_cmd[@]}"
  "${build_runtime_cmd[@]}"
fi

extract_signature() {
  local log_file="$1"
  awk '
    BEGIN {
      kind="";
      access="";
      size="";
      pkind="";
      in_block=0;
    }
    /RUSTEZE VIOLATION/ { in_block=1; next }
    in_block && kind=="" { kind=$0; next }
    in_block && ($1=="READ" || $1=="WRITE") {
      access=$1;
      for (i=1; i<=NF; i++) {
        if ($i ~ /^size=/) { sub("size=","",$i); size=$i }
      }
    }
    in_block && $0 ~ /kind=/ && pkind=="" {
      for (i=1; i<=NF; i++) {
        if ($i ~ /^kind=/) { sub("kind=","",$i); pkind=$i }
      }
    }
    in_block && $0 ~ /^=+$/ { exit }
    END {
      if (kind == "") { exit 1 }
      if (access == "") { access="UNKNOWN" }
      if (pkind == "") { pkind="unknown" }
      if (size == "") { size="unknown" }
      print kind "|" access "|" pkind "|" size
    }
  ' "${log_file}"
}

target_feature_bin_seed() {
  case "$1" in
    bytes)
      printf '%s|%s|%s\n' "bytes_driver" "afl_bytes_driver" "fuzz/corpus/bytes/seed0"
      ;;
    smallvec)
      printf '%s|%s|%s\n' "smallvec_driver" "afl_smallvec_driver" "fuzz/corpus/smallvec/seed0"
      ;;
    serde_json|serde)
      printf '%s|%s|%s\n' "serde_json_driver" "afl_serde_json_driver" "fuzz/corpus/serde_json/seed0"
      ;;
    *)
      return 1
      ;;
  esac
}

summary_file="${run_dir}/summary.tsv"
printf "target\tstatus\tseed\tsignature\n" > "${summary_file}"

for target in ${TARGETS}; do
  if ! mapping="$(target_feature_bin_seed "${target}")"; then
    printf "%s\tunsupported\t-\t-\n" "${target}" >> "${summary_file}"
    continue
  fi

  IFS='|' read -r feature bin seed <<< "${mapping}"
  if [[ ! -f "${seed}" ]]; then
    printf "%s\tmissing_seed\t%s\t-\n" "${target}" "${seed}" >> "${summary_file}"
    continue
  fi

  target_dir="${run_dir}/${target}"
  mkdir -p "${target_dir}"
  instr_log="${target_dir}/instrument.log"
  run_log="${target_dir}/run.log"

  cmd=(
    "${CARGO}" instrument-mir
    --runtime-path="${runtime_path}"
    -p afl_harness
    --features "${feature}"
    --bin "${bin}"
  )
  if (( ${#profile_args[@]} )); then
    cmd+=("${profile_args[@]}")
  fi
  if (( ${#build_std_args[@]} )); then
    cmd+=("${build_std_args[@]}")
  fi

  if ! "${cmd[@]}" > "${instr_log}" 2>&1; then
    printf "%s\tinstrument_fail\t%s\t-\n" "${target}" "${seed}" >> "${summary_file}"
    continue
  fi

  bin_path="${bin_dir}/${bin}"
  if [[ ! -x "${bin_path}" ]]; then
    printf "%s\tmissing_binary\t%s\t-\n" "${target}" "${seed}" >> "${summary_file}"
    continue
  fi

  status=0
  if ! "${bin_path}" "${seed}" > "${run_log}" 2>&1; then
    status=$?
  fi

  if signature="$(extract_signature "${run_log}" 2>/dev/null)"; then
    printf "%s\tviolation\t%s\t%s\n" "${target}" "${seed}" "${signature}" >> "${summary_file}"
    continue
  fi

  if [[ "${status}" -ne 0 ]]; then
    printf "%s\trun_fail\t%s\t-\n" "${target}" "${seed}" >> "${summary_file}"
    continue
  fi

  printf "%s\tok\t%s\t-\n" "${target}" "${seed}" >> "${summary_file}"
done

echo "==> summary: ${summary_file}"
