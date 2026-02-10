#!/usr/bin/env bash
set -euo pipefail

# AFL++ location:
# - If you have an AFLplusplus *checkout*, set `AFL_PATH` to that directory.
# - If you have an AFL++ *system install* (afl-fuzz in PATH), you can leave `AFL_PATH` unset.
AFL_PATH="${AFL_PATH:-}"
AFL_FUZZ="${AFL_FUZZ:-}"

PROFILE="${PROFILE:-release}"
TARGET="${TARGET:-bytes}" # bytes | smallvec | serde_json | toml | base64 | uuid | itoa | quick_xml
TIMEOUT_MS="${TIMEOUT_MS:-}" # optional, forwarded to AFL++ via -t

case "$TARGET" in
  bytes) BIN="afl_bytes_driver" ;;
  smallvec) BIN="afl_smallvec_driver" ;;
  serde_json|serde) BIN="afl_serde_json_driver" ;;
  toml) BIN="afl_toml_driver" ;;
  base64) BIN="afl_base64_driver" ;;
  uuid) BIN="afl_uuid_driver" ;;
  itoa) BIN="afl_itoa_driver" ;;
  quick_xml|quick-xml) BIN="afl_quick_xml_driver" ;;
  *) echo "unknown TARGET=$TARGET (expected bytes|smallvec|serde_json|serde|toml|base64|uuid|itoa|quick_xml)" >&2; exit 2 ;;
esac

HARNESS_TARGET_DIR="${HARNESS_TARGET_DIR:-./target/afl-${PROFILE}-${TARGET}}"
BIN_PATH="${HARNESS_TARGET_DIR}/${PROFILE}/${BIN}"
IN_DIR="fuzz/corpus/${TARGET}"
OUT_DIR="fuzz/out/${TARGET}"

mkdir -p "$IN_DIR"
mkdir -p "$OUT_DIR"

if [[ ! -x "$BIN_PATH" ]]; then
  PROFILE="$PROFILE" TARGET="$TARGET" ./scripts/afl_build.sh
fi

# AFL++ requires at least one seed file.
if ! find "$IN_DIR" -maxdepth 1 -type f -print -quit | grep -q .; then
  printf '\x00' > "${IN_DIR}/seed0"
fi

export RUSTEZE_FAILFAST=1
export RZ_INSTRUMENT_ALL_DEPS=1
# Keep SB-lite enabled by default so aliasing checks are always active.
export RZ_SB_LITE="${RZ_SB_LITE:-1}"

# We do not fuzz with ASan in this pipeline; avoid inheriting host/container
# ASAN_* environment variables that make AFL++ enforce ASan-specific checks.
unset ASAN_OPTIONS
unset ASAN_SYMBOLIZER_PATH

# macOS friendliness
export AFL_SKIP_CPUFREQ=1
export AFL_NO_AFFINITY=1

# macOS: shared-memory limits are often tighter and large maps can make
# `shmget()` fail with ENOMEM. Use a conservative default unless the user
# already set `AFL_MAP_SIZE`/`AFL_MAPSIZE`.
if [[ "$(uname -s)" == "Darwin" ]] && [[ -z "${AFL_MAP_SIZE:-}" && -z "${AFL_MAPSIZE:-}" ]]; then
  export AFL_MAP_SIZE=131072
fi

# Bash treats empty arrays as "unset" under `set -u` when expanded as `${arr[@]}`.
# Keep it initialized and only expand when non-empty.
extra=()
if [[ -n "$TIMEOUT_MS" ]]; then
  extra+=("-t" "$TIMEOUT_MS")
fi

if [[ ${#extra[@]} -gt 0 ]]; then
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
  "$AFL_FUZZ" -i "$IN_DIR" -o "$OUT_DIR" "${extra[@]}" -- "$BIN_PATH" @@
else
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
  "$AFL_FUZZ" -i "$IN_DIR" -o "$OUT_DIR" -- "$BIN_PATH" @@
fi
