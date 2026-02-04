#!/usr/bin/env bash
set -euo pipefail

CARGO="${CARGO:-cargo}"
PROFILE="${PROFILE:-FAST}"
BUILD_PROFILE="${BUILD_PROFILE:-debug}"
FLAKY_EXAMPLES="${FLAKY_EXAMPLES:-memset_u8_dynamic}"
FLAKY_RUNS="${FLAKY_RUNS:-200}"
REPORT_DIR="${REPORT_DIR:-reports/harness}"
MEDIUM_COMMANDS_FILE="${MEDIUM_COMMANDS_FILE:-}"

if [[ "${PROFILE}" != "FAST" && "${PROFILE}" != "DEBUG" ]]; then
  echo "Unknown PROFILE=${PROFILE}. Use FAST or DEBUG." >&2
  exit 1
fi

if [[ "${BUILD_PROFILE}" != "debug" && "${BUILD_PROFILE}" != "release" ]]; then
  echo "Unknown BUILD_PROFILE=${BUILD_PROFILE}. Use debug or release." >&2
  exit 1
fi

if ! command -v cargo-instrument-mir >/dev/null 2>&1 || ! command -v instrument-mir >/dev/null 2>&1; then
  echo "cargo-instrument-mir or instrument-mir not found; run 'make tools' first." >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
cd "${repo_root}"

timestamp="$(date +%Y%m%d_%H%M%S)"
run_dir="${REPORT_DIR}/${timestamp}"
mkdir -p "${run_dir}"

if [[ -z "${CARGO_INCREMENTAL:-}" ]]; then
  # Force full rebuilds so the harness picks up updated instrumentor binaries.
  export CARGO_INCREMENTAL=0
fi

summary_file="${run_dir}/summary.tsv"
printf "example\trun\tstatus\tsignature\n" > "${summary_file}"

stop_on_violation="${STOP_ON_VIOLATION:-}"
if [[ -z "${stop_on_violation}" ]]; then
  if [[ "${PROFILE}" == "FAST" ]]; then
    stop_on_violation=1
  else
    stop_on_violation=0
  fi
fi

reinstrument="${REINSTRUMENT:-}"
if [[ -z "${reinstrument}" ]]; then
  if [[ "${PROFILE}" == "DEBUG" ]]; then
    reinstrument=1
  else
    reinstrument=0
  fi
fi

if [[ -z "${RZ_LOG:-}" ]]; then
  if [[ "${PROFILE}" == "DEBUG" ]]; then
    export RZ_LOG=Trace
  else
    export RZ_LOG=warn
  fi
fi

# Default to instrumenting all deps for stable pointer metadata unless overridden.
export RZ_INSTRUMENT_ALL_DEPS="${RZ_INSTRUMENT_ALL_DEPS:-1}"

profile_flag=()
runtime_features=()
runtime_path="${repo_root}/target/debug"
bin_dir="${repo_root}/target/debug"
if [[ "${BUILD_PROFILE}" == "release" ]]; then
  profile_flag=(--release)
  runtime_path="${repo_root}/target/release"
  bin_dir="${repo_root}/target/release"
else
  runtime_features=(--features rz_log)
fi

build_runtime_cmd=("${CARGO}" build -p runtime)
if (( ${#profile_flag[@]} )); then
  build_runtime_cmd+=("${profile_flag[@]}")
fi
if (( ${#runtime_features[@]} )); then
  build_runtime_cmd+=("${runtime_features[@]}")
fi
"${build_runtime_cmd[@]}"

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

instrument_example() {
  local example="$1"
  local mir_out="$2"
  local log_file="$3"

  local cmd=(
    "${CARGO}" instrument-mir
    --runtime-path="${runtime_path}"
    --mir-out="${mir_out}"
    -p "${example}"
    --bin "${example}"
  )
  if (( ${#profile_flag[@]} )); then
    cmd+=("${profile_flag[@]}")
  fi

  if ! "${cmd[@]}" > "${log_file}" 2>&1; then
    return 1
  fi
  return 0
}

run_example() {
  local example="$1"
  local runs="$2"
  local example_dir="${run_dir}/${example}"
  local mir_dir="${example_dir}/mir"
  local logs_dir="${example_dir}/logs"
  local first_fail_mir=""

  mkdir -p "${mir_dir}" "${logs_dir}"

  for ((i=1; i<=runs; i++)); do
    local instr_log="${logs_dir}/instrument.${i}.log"
    local run_log="${logs_dir}/run.${i}.log"
    local mir_tmp="${mir_dir}/current.mir"

    if [[ "${reinstrument}" -eq 1 || "${i}" -eq 1 ]]; then
      if ! instrument_example "${example}" "${mir_tmp}" "${instr_log}"; then
        printf "%s\t%s\tinstrumentor_fail\t-\n" "${example}" "${i}" >> "${summary_file}"
        return 1
      fi
    fi

    local bin_path="${bin_dir}/${example}"
    if [[ ! -x "${bin_path}" ]]; then
      printf "%s\t%s\tmissing_binary\t-\n" "${example}" "${i}" >> "${summary_file}"
      return 1
    fi

    local status=0
    if ! "${bin_path}" > "${run_log}" 2>&1; then
      status=$?
    fi

    local signature=""
    if signature="$(extract_signature "${run_log}" 2>/dev/null)"; then
      printf "%s\t%s\tviolation\t%s\n" "${example}" "${i}" "${signature}" >> "${summary_file}"
      if [[ "${PROFILE}" == "DEBUG" ]]; then
        if [[ -z "${first_fail_mir}" && -f "${mir_tmp}" ]]; then
          first_fail_mir="${mir_dir}/first_fail.mir"
          mv "${mir_tmp}" "${first_fail_mir}"
        else
          rm -f "${mir_tmp}"
        fi
      fi
      if [[ "${stop_on_violation}" -eq 1 ]]; then
        return 2
      fi
    else
      printf "%s\t%s\tok\t-\n" "${example}" "${i}" >> "${summary_file}"
      if [[ "${PROFILE}" == "DEBUG" ]]; then
        rm -f "${mir_tmp}"
      fi
    fi

    if [[ "${status}" -ne 0 && "${stop_on_violation}" -eq 1 ]]; then
      return 2
    fi
  done

  return 0
}

run_medium_commands() {
  local commands_file="$1"
  if [[ ! -f "${commands_file}" ]]; then
    echo "Medium commands file not found: ${commands_file}" >&2
    return 0
  fi

  local idx=0
  while IFS= read -r line || [[ -n "${line}" ]]; do
    line="${line#"${line%%[![:space:]]*}"}"
    if [[ -z "${line}" || "${line}" == \#* ]]; then
      continue
    fi

    idx=$((idx + 1))
    local name="medium_${idx}"
    local cmd="${line}"
    if [[ "${line}" == *"|"* ]]; then
      name="${line%%|*}"
      cmd="${line#*|}"
    fi

    local log_file="${run_dir}/${name}.log"
    local status=0
    if ! bash -lc "${cmd}" > "${log_file}" 2>&1; then
      status=$?
    fi

    local signature=""
    if signature="$(extract_signature "${log_file}" 2>/dev/null)"; then
      printf "%s\t%s\tviolation\t%s\n" "${name}" "1" "${signature}" >> "${summary_file}"
    elif [[ "${status}" -ne 0 ]]; then
      printf "%s\t%s\tmedium_fail\t-\n" "${name}" "1" >> "${summary_file}"
    else
      printf "%s\t%s\tok\t-\n" "${name}" "1" >> "${summary_file}"
    fi
  done < "${commands_file}"
}

stop_all=0
for example in ${FLAKY_EXAMPLES}; do
  echo "==> example ${example} (${FLAKY_RUNS} runs)"
  if ! run_example "${example}" "${FLAKY_RUNS}"; then
    rc=$?
    if [[ "${rc}" -eq 2 ]]; then
      stop_all=1
      break
    fi
    stop_all=1
    break
  fi
done

if [[ "${stop_all}" -eq 0 ]]; then
  if [[ -n "${MEDIUM_COMMANDS_FILE}" ]]; then
    echo "==> medium crates"
    run_medium_commands "${MEDIUM_COMMANDS_FILE}"
  else
    echo "==> medium crates skipped (set MEDIUM_COMMANDS_FILE to enable)"
  fi
fi

echo "==> summary: ${summary_file}"
