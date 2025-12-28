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

reports_dir="${REPORTS_DIR:-reports}"
mir_dir="${reports_dir}/mir"
mir_report="${reports_dir}/examples.mir.txt"
run_report="${reports_dir}/examples.run.txt"
src_report="${reports_dir}/examples.src.txt"

mkdir -p "${reports_dir}" "${mir_dir}"
: > "${mir_report}"
: > "${run_report}"
: > "${src_report}"

shopt -s nullglob
example_manifests=(examples/*/Cargo.toml)

if (( ${#example_manifests[@]} == 0 )); then
  echo "No example manifests found under examples/*" >&2
  exit 1
fi

for manifest in "${example_manifests[@]}"; do
  pkg_dir="$(dirname "${manifest}")"
  pkg_name="$(basename "${pkg_dir}")"

  echo "==> instrumenting ${pkg_name}"
  mir_out="${mir_dir}/${pkg_name}.mir"
  before_out="${mir_dir}/before.${pkg_name}.mir"
  after_out="${mir_dir}/after.${pkg_name}.mir"

  instr_cmd=("${CARGO}" instrument-mir --runtime-path="${runtime_path}" --mir-out="${mir_out}" \
    -p "${pkg_name}" --bin "${pkg_name}")
  if (( ${#profile_args[@]} )); then
    instr_cmd+=("${profile_args[@]}")
  fi

  if ! "${instr_cmd[@]}"; then
    printf -- "===== %s =====\n" "${pkg_name}" >> "${mir_report}"
    printf -- "instrument failed\n\n" >> "${mir_report}"
    printf -- "===== %s =====\n" "${pkg_name}" >> "${run_report}"
    printf -- "instrument failed\n\n" >> "${run_report}"
    continue
  fi

  printf -- "===== %s =====\n" "${pkg_name}" >> "${mir_report}"
  if [[ -f "${before_out}" ]]; then
    printf -- "--- before ---\n" >> "${mir_report}"
    cat "${before_out}" >> "${mir_report}"
    printf -- "\n" >> "${mir_report}"
  else
    printf -- "--- before ---\n(missing %s)\n\n" "${before_out}" >> "${mir_report}"
  fi
  if [[ -f "${after_out}" ]]; then
    printf -- "--- after ---\n" >> "${mir_report}"
    cat "${after_out}" >> "${mir_report}"
    printf -- "\n" >> "${mir_report}"
  else
    printf -- "--- after ---\n(missing %s)\n\n" "${after_out}" >> "${mir_report}"
  fi

  printf -- "===== %s =====\n" "${pkg_name}" >> "${run_report}"
  if [[ -x "./${bin_dir}/${pkg_name}" ]]; then
    if run_output=$("./${bin_dir}/${pkg_name}" 2>&1); then
      printf -- "%s\n\n" "${run_output}" >> "${run_report}"
    else
      status=$?
      printf -- "%s\n\n" "${run_output}" >> "${run_report}"
      printf -- "exit status: %s\n\n" "${status}" >> "${run_report}"
    fi
  else
    printf -- "missing binary: %s\n\n" "./${bin_dir}/${pkg_name}" >> "${run_report}"
  fi

  printf -- "===== %s =====\n" "${pkg_name}" >> "${src_report}"
  if [[ -d "${pkg_dir}/src" ]]; then
    while IFS= read -r path; do
      printf -- "--- %s ---\n" "${path#${repo_root}/}" >> "${src_report}"
      cat "${path}" >> "${src_report}"
      printf -- "\n" >> "${src_report}"
    done < <(find "${pkg_dir}/src" -type f -name '*.rs' | LC_ALL=C sort)
  else
    printf -- "missing src dir\n\n" >> "${src_report}"
  fi
  printf -- "\n" >> "${src_report}"
done

echo "Reports written to ${reports_dir}"
