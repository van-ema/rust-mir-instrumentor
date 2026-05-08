#!/usr/bin/env bash
set -euo pipefail

TARGET="${TARGET:-bytes}" # bytes | smallvec | serde_json | toml | base64 | uuid | itoa | quick_xml | simd_json | zip | rkyv | hyper | image | hashbrown | bumpalo | indexmap | bootc_kcmdline | abacus_apportionment | kvm_bindings
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
  --input <file>   Repro only this crash file (path or filename under crashes/).
                   Files ending in .hex are decoded as AFL input bytes.

Environment:
  AFL_REPRO_STRICT=1  Exit non-zero if the harness rejects an input.
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
  toml) BIN="afl_toml_driver" ;;
  base64) BIN="afl_base64_driver" ;;
  uuid) BIN="afl_uuid_driver" ;;
  itoa) BIN="afl_itoa_driver" ;;
  quick_xml|quick-xml) BIN="afl_quick_xml_driver" ;;
  simd_json|simd-json) BIN="afl_simd_json_driver" ;;
  zip) BIN="afl_zip_driver" ;;
  rkyv) BIN="afl_rkyv_driver" ;;
  hyper) BIN="afl_hyper_driver" ;;
  image) BIN="afl_image_driver" ;;
  hashbrown) BIN="afl_hashbrown_driver" ;;
  bumpalo) BIN="afl_bumpalo_driver" ;;
  indexmap) BIN="afl_indexmap_driver" ;;
  bootc_kcmdline|bootc-kcmdline|bootc_kernel_cmdline) BIN="afl_bootc_kcmdline_driver" ;;
  abacus_apportionment|abacus-apportionment) BIN="afl_abacus_apportionment_driver" ;;
  kvm_bindings|kvm-bindings) BIN="afl_kvm_bindings_driver" ;;
  *) echo "unknown TARGET=$TARGET (expected bytes|smallvec|serde_json|serde|toml|base64|uuid|itoa|quick_xml|simd_json|zip|rkyv|hyper|image|hashbrown|bumpalo|indexmap|bootc_kcmdline|abacus_apportionment|kvm_bindings)" >&2; exit 2 ;;
esac

CRASH_DIR="${OUT_DIR}/default/crashes"
if [[ -z "$ONLY" && ! -d "$CRASH_DIR" ]]; then
  echo "missing crash dir: $CRASH_DIR" >&2
  exit 2
fi

HARNESS_TARGET_DIR="${HARNESS_TARGET_DIR:-./target/afl-${PROFILE}-${TARGET}}"
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
    if [[ ! -d "$CRASH_DIR" ]]; then
      echo "missing crash dir: $CRASH_DIR" >&2
      exit 2
    fi
    files=("${CRASH_DIR}/${ONLY}")
  fi
else
  files=("$CRASH_DIR"/*)
fi

tmp_inputs=()
cleanup_tmp_inputs() {
  for tmp in "${tmp_inputs[@]:-}"; do
    rm -f "$tmp"
  done
}
trap cleanup_tmp_inputs EXIT

for f in "${files[@]}"; do
  if [[ "$(basename "$f")" == "README.txt" ]]; then
    continue
  fi
  if [[ ! -f "$f" ]]; then
    echo "missing crash file: $f" >&2
    exit 2
  fi
  run_input="$f"
  if [[ "$f" == *.hex ]]; then
    run_input="$(mktemp)"
    tmp_inputs+=("$run_input")
    tr -d '[:space:]' < "$f" | xxd -r -p > "$run_input"
  fi

  echo "=== repro: $f ==="
  alias_model="${RZ_ALIAS_MODEL:-tb_lite}"
  sb_lite="${RZ_SB_LITE:-1}"
  echo "+ RUSTEZE_FAILFAST=1 RZ_ABORT_ON_VIOLATION=1 RZ_INSTRUMENT_ALL_DEPS=1 RZ_ALIAS_MODEL=${alias_model} RZ_SB_LITE=${sb_lite} \"$BIN_PATH\" \"$run_input\""
  if ! RUSTEZE_FAILFAST=1 RZ_ABORT_ON_VIOLATION=1 RZ_INSTRUMENT_ALL_DEPS=1 RZ_ALIAS_MODEL="${alias_model}" RZ_SB_LITE="${sb_lite}" "$BIN_PATH" "$run_input"; then
    if [[ "${AFL_REPRO_STRICT:-0}" == "1" ]]; then
      exit 1
    fi
  fi
  echo
done
