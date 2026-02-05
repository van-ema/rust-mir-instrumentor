#!/usr/bin/env bash
set -euo pipefail

TARGET="${TARGET:-bytes}" # bytes | smallvec | serde_json
PROFILE="${PROFILE:-release}"
OUT_DIR="${OUT_DIR:-fuzz/out/${TARGET}}"
ONLY=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --input)
      ONLY="${2:-}"
      if [[ -z "$ONLY" ]]; then
        echo "--input requires a path or filename" >&2
        exit 2
      fi
      shift 2
      ;;
    --input=*)
      ONLY="${1#--input=}"
      if [[ -z "$ONLY" ]]; then
        echo "--input requires a path or filename" >&2
        exit 2
      fi
      shift
      ;;
    -h|--help)
      cat <<EOF
Usage: scripts/afl_repro.sh [--input <file>]

Options:
  --input <file>   Repro only this crash file (path or filename under crashes/)
EOF
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

case "$TARGET" in
  bytes) BIN="afl_bytes_driver" ;;
  smallvec) BIN="afl_smallvec_driver" ;;
  serde_json|serde) BIN="afl_serde_json_driver" ;;
  *) echo "unknown TARGET=$TARGET (expected bytes|smallvec|serde_json)" >&2; exit 2 ;;
esac

CRASH_DIR="${OUT_DIR}/default/crashes"
if [[ ! -d "$CRASH_DIR" ]]; then
  echo "missing crash dir: $CRASH_DIR" >&2
  exit 2
fi

HARNESS_TARGET_DIR="${HARNESS_TARGET_DIR:-./target/afl-${PROFILE}}"
BIN_PATH="${HARNESS_TARGET_DIR}/${PROFILE}/${BIN}"

if [[ ! -x "$BIN_PATH" ]]; then
  echo "missing $BIN_PATH; build first with scripts/afl_build.sh" >&2
  exit 2
fi

echo "crash dir: $CRASH_DIR"
echo "binary: $BIN_PATH"
echo

shopt -s nullglob

if [[ -n "$ONLY" ]]; then
  if [[ -f "$ONLY" ]]; then
    files=("$ONLY")
  else
    files=("${CRASH_DIR}/${ONLY}")
  fi
else
  files=("$CRASH_DIR"/*)
fi

for f in "${files[@]}"; do
  if [[ "$(basename "$f")" == "README.txt" ]]; then
    continue
  fi
  if [[ ! -f "$f" ]]; then
    echo "missing crash file: $f" >&2
    exit 2
  fi
  echo "=== repro: $f ==="
  echo "+ RUSTEZE_FAILFAST=1 RZ_ABORT_ON_VIOLATION=1 RZ_INSTRUMENT_ALL_DEPS=1 \"$BIN_PATH\" \"$f\""
  RUSTEZE_FAILFAST=1 RZ_ABORT_ON_VIOLATION=1 RZ_INSTRUMENT_ALL_DEPS=1 "$BIN_PATH" "$f" || true
  echo
done
