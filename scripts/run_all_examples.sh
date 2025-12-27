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

shopt -s nullglob
example_manifests=(examples/*/Cargo.toml)

if (( ${#example_manifests[@]} == 0 )); then
  echo "No example manifests found under examples/*" >&2
  exit 1
fi

for manifest in "${example_manifests[@]}"; do
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
  "${instr_cmd[@]}"
  echo "==> running ${pkg_name}"
  "./${bin_dir}/${pkg_name}"
done
