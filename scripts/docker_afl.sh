#!/usr/bin/env bash
set -euo pipefail

IMAGE="${IMAGE:-rusteze-afl}"
SHM_SIZE="${SHM_SIZE:-1g}"
USE_RUST_CACHE="${USE_RUST_CACHE:-1}"

# For fuzzing, some hosts/environments require relaxed sandboxing for forkserver
# / ptrace-like behaviors. Keep it opt-in.
FUZZ="${FUZZ:-0}"
DOCKER_CPUS="${DOCKER_CPUS:-}"
DOCKER_MEMORY="${DOCKER_MEMORY:-}"

extra=()
forward_env=()

# Forward commonly used build/fuzz environment variables when set.
for var in \
  TARGET PROFILE AFL_COMPILER_RT RUNTIME_FEATURES \
  HARNESS_TARGET_DIR CARGO_TARGET_DIR RUSTFLAGS AFL_FUZZ \
  TIMEOUT_MS OUT_DIR
do
  if [[ -n "${!var:-}" ]]; then
    forward_env+=(-e "${var}=${!var}")
  fi
done

# Forward rusteze debug/tuning knobs when exported on the host so
# instrumentation tracing is visible inside the container as well.
for var in $(compgen -e); do
  case "$var" in
    RZ_*|RUSTEZE_*|TRACE|RUST_BACKTRACE)
      if [[ -n "${!var:-}" ]]; then
        forward_env+=(-e "${var}=${!var}")
      fi
      ;;
  esac
done

# Also support inline env assignments passed as leading args, e.g.:
#   ./scripts/docker_afl.sh TARGET=serde PROFILE=release ./scripts/afl_build.sh
while [[ $# -gt 0 ]]; do
  if [[ "$1" =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; then
    forward_env+=(-e "$1")
    shift
  else
    break
  fi
done
if [[ "$FUZZ" == "1" || "$FUZZ" == "true" ]]; then
  extra+=(--security-opt seccomp=unconfined --cap-add SYS_PTRACE)
fi
if [[ -n "$DOCKER_CPUS" ]]; then
  extra+=(--cpus "$DOCKER_CPUS")
fi
if [[ -n "$DOCKER_MEMORY" ]]; then
  extra+=(-m "$DOCKER_MEMORY")
fi

cmd=(
  docker run --rm -it
  --shm-size="${SHM_SIZE}"
  -v "${PWD}:/work"
  -w /work
)

# Persist rustup/cargo state across ephemeral container runs to avoid
# re-downloading the pinned toolchain/components each time.
if [[ "$USE_RUST_CACHE" == "1" || "$USE_RUST_CACHE" == "true" ]]; then
  cmd+=(-v rusteze-afl-rustup:/opt/rustup)
  cmd+=(-v rusteze-afl-cargo:/opt/cargo)
fi

# Avoid expanding empty arrays with nounset on older bash versions.
if [[ ${#extra[@]} -gt 0 ]]; then
  cmd+=("${extra[@]}")
fi
if [[ ${#forward_env[@]} -gt 0 ]]; then
  cmd+=("${forward_env[@]}")
fi

cmd+=("${IMAGE}")
if [[ $# -gt 0 ]]; then
  cmd+=("$@")
fi

exec "${cmd[@]}"
