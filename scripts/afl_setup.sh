#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
THIRD_PARTY_DIR="${REPO_ROOT}/third_party"

# Popular + actively maintained fuzz targets.
DEFAULT_TARGETS=(bytes smallvec serde serde_json toml uuid quick_xml base64 itoa)

usage() {
  cat <<'EOF'
Usage:
  ./scripts/afl_setup.sh [target ...]

Targets:
  bytes smallvec serde serde_json toml uuid quick_xml base64 itoa

Behavior:
  1) clones missing third_party repos for selected targets
  2) prints pinned revisions currently checked out
  3) detects AFL++ tools/runtime and prints export commands
  4) creates fuzz seed files when missing

Examples:
  ./scripts/afl_setup.sh
  ./scripts/afl_setup.sh bytes smallvec serde_json

Optional per-repo ref pinning (branch/tag/commit):
  REF_BYTES=master REF_SMALLVEC=v1.13.2 ./scripts/afl_setup.sh bytes smallvec
EOF
}

norm_target() {
  case "$1" in
    quick-xml) echo "quick_xml" ;;
    serde) echo "serde_json" ;;
    *) echo "$1" ;;
  esac
}

repo_url_for_target() {
  case "$1" in
    bytes) echo "https://github.com/tokio-rs/bytes.git" ;;
    smallvec) echo "https://github.com/servo/rust-smallvec.git" ;;
    serde) echo "https://github.com/serde-rs/serde.git" ;;
    serde_json) echo "https://github.com/serde-rs/json.git" ;;
    toml) echo "https://github.com/toml-rs/toml.git" ;;
    uuid) echo "https://github.com/uuid-rs/uuid.git" ;;
    quick_xml) echo "https://github.com/tafia/quick-xml.git" ;;
    base64) echo "https://github.com/marshallpierce/rust-base64.git" ;;
    itoa) echo "https://github.com/dtolnay/itoa.git" ;;
    *) return 1 ;;
  esac
}

repo_dir_for_target() {
  case "$1" in
    quick_xml) echo "${THIRD_PARTY_DIR}/quick-xml" ;;
    *) echo "${THIRD_PARTY_DIR}/$1" ;;
  esac
}

ref_var_name_for_target() {
  case "$1" in
    bytes) echo "REF_BYTES" ;;
    smallvec) echo "REF_SMALLVEC" ;;
    serde) echo "REF_SERDE" ;;
    serde_json) echo "REF_SERDE_JSON" ;;
    toml) echo "REF_TOML" ;;
    uuid) echo "REF_UUID" ;;
    quick_xml) echo "REF_QUICK_XML" ;;
    base64) echo "REF_BASE64" ;;
    itoa) echo "REF_ITOA" ;;
    *) return 1 ;;
  esac
}

validate_target() {
  case "$1" in
    bytes|smallvec|serde|serde_json|toml|uuid|quick_xml|base64|itoa) ;;
    *) echo "error: unknown target '$1'" >&2; usage; exit 2 ;;
  esac
}

find_afl_fuzz() {
  if [[ -n "${AFL_FUZZ:-}" && -x "${AFL_FUZZ}" ]]; then
    echo "${AFL_FUZZ}"
    return 0
  fi
  if [[ -n "${AFL_PATH:-}" && -x "${AFL_PATH}/afl-fuzz" ]]; then
    echo "${AFL_PATH}/afl-fuzz"
    return 0
  fi
  command -v afl-fuzz || true
}

find_afl_cc() {
  if [[ -n "${AFL_PATH:-}" && -x "${AFL_PATH}/afl-clang-fast" ]]; then
    echo "${AFL_PATH}/afl-clang-fast"
    return 0
  fi
  if command -v afl-clang-fast >/dev/null 2>&1; then
    command -v afl-clang-fast
    return 0
  fi
  if command -v afl-cc >/dev/null 2>&1; then
    command -v afl-cc
    return 0
  fi
  true
}

find_afl_compiler_rt() {
  local cc_path="${1:-}"
  if [[ -n "${AFL_COMPILER_RT:-}" && -f "${AFL_COMPILER_RT}" ]]; then
    echo "${AFL_COMPILER_RT}"
    return 0
  fi
  if [[ -n "${AFL_PATH:-}" && -f "${AFL_PATH}/afl-compiler-rt.o" ]]; then
    echo "${AFL_PATH}/afl-compiler-rt.o"
    return 0
  fi
  if [[ -f "/usr/local/lib/afl/afl-compiler-rt.o" ]]; then
    echo "/usr/local/lib/afl/afl-compiler-rt.o"
    return 0
  fi
  if [[ -f "/usr/lib/afl/afl-compiler-rt.o" ]]; then
    echo "/usr/lib/afl/afl-compiler-rt.o"
    return 0
  fi
  if [[ -f "/opt/aflpp/afl-compiler-rt.o" ]]; then
    echo "/opt/aflpp/afl-compiler-rt.o"
    return 0
  fi
  if [[ -n "${cc_path}" ]]; then
    local cc_dir
    cc_dir="$(cd "$(dirname "${cc_path}")" && pwd)"
    if [[ -f "${cc_dir}/afl-compiler-rt.o" ]]; then
      echo "${cc_dir}/afl-compiler-rt.o"
      return 0
    fi
  fi
  true
}

select_targets=()
if [[ $# -eq 0 ]]; then
  select_targets=("${DEFAULT_TARGETS[@]}")
else
  for t in "$@"; do
    case "$t" in
      -h|--help) usage; exit 0 ;;
      *)
        t="$(norm_target "$t")"
        validate_target "$t"
        select_targets+=("$t")
        ;;
    esac
  done
fi

# serde_json fuzzing requires serde checkout due path dependency in third_party/serde_json.
if printf '%s\n' "${select_targets[@]}" | grep -q '^serde_json$'; then
  if ! printf '%s\n' "${select_targets[@]}" | grep -q '^serde$'; then
    select_targets=(serde "${select_targets[@]}")
  fi
fi

mkdir -p "${THIRD_PARTY_DIR}"
mkdir -p "${REPO_ROOT}/fuzz/corpus"

echo "== cloning/updating third_party targets =="
for target in "${select_targets[@]}"; do
  url="$(repo_url_for_target "$target")"
  dir="$(repo_dir_for_target "$target")"
  ref_var="$(ref_var_name_for_target "$target")"
  ref="${!ref_var:-}"

  if [[ ! -d "${dir}/.git" ]]; then
    echo "clone ${target} -> ${dir}"
    git clone "${url}" "${dir}"
  else
    echo "reuse ${target} -> ${dir}"
  fi

  if [[ -n "${ref}" ]]; then
    echo "pin ${target} -> ${ref}"
    git -C "${dir}" fetch --tags origin
    git -C "${dir}" checkout "${ref}"
  fi

  rev="$(git -C "${dir}" rev-parse --short HEAD)"
  branch="$(git -C "${dir}" rev-parse --abbrev-ref HEAD || true)"
  echo "  ${target}: ${rev} (${branch})"
done

echo
echo "== preparing fuzz corpus =="
for target in "${select_targets[@]}"; do
  # There is no dedicated "serde" harness target; serde_json covers it.
  if [[ "${target}" == "serde" ]]; then
    continue
  fi
  in_dir="${REPO_ROOT}/fuzz/corpus/${target}"
  mkdir -p "${in_dir}"
  if ! find "${in_dir}" -maxdepth 1 -type f -print -quit | grep -q .; then
    printf '\x00' > "${in_dir}/seed0"
    echo "seed created: ${in_dir}/seed0"
  else
    echo "seed exists: ${in_dir}"
  fi
done

echo
echo "== AFL++ toolchain check =="
afl_fuzz_bin="$(find_afl_fuzz)"
afl_cc_bin="$(find_afl_cc)"
afl_rt="$(find_afl_compiler_rt "${afl_cc_bin}")"

if [[ -n "${afl_fuzz_bin}" ]]; then
  echo "afl-fuzz: ${afl_fuzz_bin}"
else
  echo "afl-fuzz: MISSING (install AFL++ or set AFL_FUZZ/AFL_PATH)"
fi
if [[ -n "${afl_cc_bin}" ]]; then
  echo "afl-cc/afl-clang-fast: ${afl_cc_bin}"
else
  echo "afl-cc/afl-clang-fast: MISSING"
fi
if [[ -n "${afl_rt}" ]]; then
  echo "afl-compiler-rt.o: ${afl_rt}"
else
  echo "afl-compiler-rt.o: MISSING (set AFL_COMPILER_RT or AFL_PATH)"
fi

echo
echo "== recommended environment =="
echo "export RZ_INSTRUMENT_ALL_DEPS=1"
echo "export CARGO_INCREMENTAL=0"
if [[ -n "${afl_rt}" ]]; then
  echo "export AFL_COMPILER_RT='${afl_rt}'"
fi
if [[ -n "${afl_fuzz_bin}" ]]; then
  afl_dir="$(cd "$(dirname "${afl_fuzz_bin}")" && pwd)"
  echo "export AFL_PATH='${afl_dir}'"
fi
if [[ "$(uname -s)" == "Darwin" ]]; then
  echo "export AFL_MAP_SIZE=131072"
fi

echo
echo "== next commands =="
first_target=""
for target in "${select_targets[@]}"; do
  if [[ "${target}" != "serde" ]]; then
    first_target="${target}"
    break
  fi
done
if [[ -z "${first_target}" ]]; then
  first_target="bytes"
fi

echo "# native build/fuzz"
echo "TARGET=${first_target} PROFILE=debug ./scripts/afl_build.sh"
echo "TARGET=${first_target} PROFILE=debug ./scripts/afl_fuzz.sh"
echo
echo "# docker build/fuzz (recommended on macOS)"
echo "./scripts/docker_afl.sh TARGET=${first_target} PROFILE=debug ./scripts/afl_build.sh"
echo "FUZZ=1 ./scripts/docker_afl.sh TARGET=${first_target} PROFILE=debug ./scripts/afl_fuzz.sh"
