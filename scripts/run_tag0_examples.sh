#!/usr/bin/env bash
set -euo pipefail

CARGO="${CARGO:-cargo}"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
cd "${repo_root}"

if ! command -v cargo-instrument-mir >/dev/null 2>&1 || ! command -v instrument-mir >/dev/null 2>&1; then
  echo "cargo-instrument-mir or instrument-mir not found; run 'make tools' first." >&2
  exit 1
fi

has_release=false
next_is_profile=false
for arg in "$@"; do
  if ${next_is_profile}; then
    if [[ "${arg}" == "release" ]]; then
      has_release=true
    fi
    next_is_profile=false
    continue
  fi
  if [[ "${arg}" == "--profile" ]]; then
    next_is_profile=true
    continue
  fi
  if [[ "${arg}" == "--release" ]]; then
    has_release=true
  fi
done

release=false
if [[ "${PROFILE:-}" == "release" || "${has_release}" == "true" ]]; then
  release=true
fi

profile_args=()
if [[ "${PROFILE:-}" == "release" && "${has_release}" == "false" ]]; then
  profile_args=(--release)
fi

runtime_path="target/debug"
bin_dir="target/debug"
if [[ "${release}" == "true" ]]; then
  runtime_path="target/release"
  bin_dir="target/release"
fi

if (( ${#profile_args[@]} )); then
  "${CARGO}" build -p runtime "${profile_args[@]}"
else
  "${CARGO}" build -p runtime
fi

panic_re='internal compiler error|thread .* panicked|panicked at'
failures=()

run_checked() {
  local label="$1"
  shift
  local log_file
  log_file="$(mktemp -t run_tag0_examples.XXXXXX)"

  set +e
  "$@" 2>&1 | tee "${log_file}"
  local status="${PIPESTATUS[0]}"
  set -e

  local bad=0
  if (( status != 0 )); then
    bad=1
  fi
  if grep -E -q "${panic_re}" "${log_file}"; then
    bad=1
  fi

  if (( bad != 0 )); then
    echo "==> ${label} failed (exit ${status})" >&2
    rm -f "${log_file}"
    return 1
  fi

  rm -f "${log_file}"
  return 0
}

shopt -s nullglob
tag0_manifests=(examples/tag0_*/Cargo.toml)

if (( ${#tag0_manifests[@]} == 0 )); then
  echo "No tag0 manifests found under examples/tag0_*" >&2
  exit 1
fi

for manifest in "${tag0_manifests[@]}"; do
  pkg_dir="$(dirname "${manifest}")"
  pkg_name="$(basename "${pkg_dir}")"
  if [[ -n "${MIR_OUT_DIR:-}" ]]; then
    mkdir -p "${MIR_OUT_DIR}"
    mir_out="${MIR_OUT_DIR}/${pkg_name}.mir"
  elif [[ -n "${MIR_OUT:-}" ]]; then
    mir_out="${MIR_OUT}"
  else
    mir_out="out.${pkg_name}.mir"
  fi

  echo "==> instrumenting ${pkg_name}"
  instr_cmd=("${CARGO}" instrument-mir --runtime-path="${runtime_path}" --mir-out="${mir_out}" \
    -p "${pkg_name}" --bin "${pkg_name}")
  if (( ${#profile_args[@]} )); then
    instr_cmd+=("${profile_args[@]}")
  fi
  if (( $# )); then
    instr_cmd+=("$@")
  fi
  if ! run_checked "instrument ${pkg_name}" "${instr_cmd[@]}"; then
    failures+=("${pkg_name}:instrument")
    continue
  fi
  echo "==> running ${pkg_name}"
  if ! run_checked "run ${pkg_name}" "./${bin_dir}/${pkg_name}"; then
    failures+=("${pkg_name}:run")
  fi
done

if (( ${#failures[@]} )); then
  printf '%s\n' "Failures:" >&2
  for failure in "${failures[@]}"; do
    printf ' - %s\n' "${failure}" >&2
  done
  exit 1
fi
