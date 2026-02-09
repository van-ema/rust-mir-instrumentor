# AFL++ Docker container (Linux)

This is a convenience container for running AFL++ tools and building/running
rusteze harnesses on Linux even if the host is macOS.

## Build

From the repo root:

```bash
docker build -t rusteze-afl -f docker/afl/Dockerfile .
```

The image preinstalls the repo-pinned Rust toolchain (`nightly-2025-08-01`)
with components required by this project (`rust-src`, `rustc-dev`,
`llvm-tools-preview`).

## Run an interactive shell

```bash
docker run --rm -it \
  --shm-size=1g \
  -v "$PWD:/work" \
  -w /work \
  rusteze-afl
```

Or, using the repo helper script:

```bash
./scripts/docker_afl.sh
```

The helper mounts persistent Docker volumes for rustup/cargo caches by default:
- `rusteze-afl-rustup` -> `/opt/rustup`
- `rusteze-afl-cargo` -> `/opt/cargo`

This avoids repeated toolchain downloads across `--rm` container runs.
Disable with `USE_RUST_CACHE=0`.

## Reproduce a crash (no fuzzing)

If you already have a crash input in `crashes/id:000000,...`, just run the
target binary with that input file (AFL uses `@@` to pass the file name):

```bash
RUSTEZE_FAILFAST=1 RZ_LOG=trace RZ_LOG_LOC=1 ./target/afl-release/release/afl_bytes_driver crashes/id:000000,*
```

Or use AFL tools to minimize:

```bash
afl-tmin -i crashes/id:000000,* -o minimized \
  -- ./target/afl-release/release/afl_bytes_driver @@
```

## Build harnesses

Inside the container:

```bash
RUNTIME_FEATURES="--features rz_log" TARGET=bytes PROFILE=release ./scripts/afl_build.sh
TARGET=serde PROFILE=release ./scripts/afl_build.sh
```

Then you can run/minimize with `afl-tmin` / `afl-cmin` without doing any fuzzing.

## Fuzzing (optional)

You can fuzz inside the container (slower than native Linux, but good for quick checks):

```bash
./scripts/docker_afl.sh
TARGET=bytes PROFILE=release ./scripts/afl_fuzz.sh
```

If the host requires relaxed sandboxing for AFL++ forkserver, start the container with:

```bash
FUZZ=1 ./scripts/docker_afl.sh
```

## Notes

- This container installs AFL++ system-wide, so `afl-fuzz` is on `PATH`.
- The AFL++ runtime object is expected at `/usr/local/lib/afl/afl-compiler-rt.o`.
  If it is missing for any reason, you can point the build script at the in-tree
  runtime object:

  ```bash
  export AFL_COMPILER_RT=/opt/aflpp/afl-compiler-rt.o
  TARGET=bytes PROFILE=release ./scripts/afl_build.sh
  ```
