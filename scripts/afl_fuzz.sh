#!/usr/bin/env bash
set -euo pipefail

: "${AFL_PATH:?Set AFL_PATH to your AFLplusplus checkout (must contain afl-fuzz)}"

PROFILE="${PROFILE:-release}"
TARGET="${TARGET:-bytes}" # bytes | smallvec
TIMEOUT_MS="${TIMEOUT_MS:-}" # optional, forwarded to AFL++ via -t

case "$TARGET" in
  bytes) BIN="afl_bytes_driver" ;;
  smallvec) BIN="afl_smallvec_driver" ;;
  *) echo "unknown TARGET=$TARGET (expected bytes|smallvec)" >&2; exit 2 ;;
esac

HARNESS_TARGET_DIR="./target/afl-${PROFILE}"
BIN_PATH="${HARNESS_TARGET_DIR}/${PROFILE}/${BIN}"
IN_DIR="fuzz/corpus/${TARGET}"
OUT_DIR="fuzz/out/${TARGET}"

mkdir -p "$OUT_DIR"

if [[ ! -x "$BIN_PATH" ]]; then
  PROFILE="$PROFILE" TARGET="$TARGET" ./scripts/afl_build.sh
fi

export RUSTEZE_FAILFAST=1
export RZ_INSTRUMENT_ALL_DEPS=1

# macOS friendliness
export AFL_SKIP_CPUFREQ=1
export AFL_NO_AFFINITY=1

# macOS: System V shmget() appears to require page-aligned sizes.
# AFL++ uses `MAP_SIZE + 8` when the map is exactly the default MAP_SIZE, which
# breaks alignment on 16K-page systems. Avoid this by picking a different
# page-aligned size unless the user already set `AFL_MAP_SIZE`.
if [[ "$(uname -s)" == "Darwin" ]] && [[ -z "${AFL_MAP_SIZE:-}" && -z "${AFL_MAPSIZE:-}" ]]; then
  pagesize="$(sysctl -n hw.pagesize 2>/dev/null || echo 16384)"
  export AFL_MAP_SIZE="$((1048576 + pagesize))"
fi

# Bash treats empty arrays as "unset" under `set -u` when expanded as `${arr[@]}`.
# Keep it initialized and only expand when non-empty.
extra=()
if [[ -n "$TIMEOUT_MS" ]]; then
  extra+=("-t" "$TIMEOUT_MS")
fi

if [[ ${#extra[@]} -gt 0 ]]; then
  "${AFL_PATH}/afl-fuzz" -i "$IN_DIR" -o "$OUT_DIR" "${extra[@]}" -- "$BIN_PATH" @@
else
  "${AFL_PATH}/afl-fuzz" -i "$IN_DIR" -o "$OUT_DIR" -- "$BIN_PATH" @@
fi
