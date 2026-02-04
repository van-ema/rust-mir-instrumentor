#!/usr/bin/env bash
set -euo pipefail

IMAGE="${IMAGE:-rusteze-afl}"
SHM_SIZE="${SHM_SIZE:-1g}"

# For fuzzing, some hosts/environments require relaxed sandboxing for forkserver
# / ptrace-like behaviors. Keep it opt-in.
FUZZ="${FUZZ:-0}"
DOCKER_CPUS="${DOCKER_CPUS:-}"
DOCKER_MEMORY="${DOCKER_MEMORY:-}"

extra=()
if [[ "$FUZZ" == "1" || "$FUZZ" == "true" ]]; then
  extra+=(--security-opt seccomp=unconfined --cap-add SYS_PTRACE)
fi
if [[ -n "$DOCKER_CPUS" ]]; then
  extra+=(--cpus "$DOCKER_CPUS")
fi
if [[ -n "$DOCKER_MEMORY" ]]; then
  extra+=(-m "$DOCKER_MEMORY")
fi

exec docker run --rm -it \
  --shm-size="${SHM_SIZE}" \
  -v "${PWD}:/work" \
  -w /work \
  "${extra[@]}" \
  "${IMAGE}" "$@"
