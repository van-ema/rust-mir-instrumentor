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
TARGET=toml PROFILE=debug ./scripts/afl_build.sh
TARGET=base64 PROFILE=debug ./scripts/afl_build.sh
TARGET=uuid PROFILE=debug ./scripts/afl_build.sh
TARGET=quick_xml PROFILE=debug ./scripts/afl_build.sh
TARGET=itoa PROFILE=debug ./scripts/afl_build.sh
TARGET=simd_json PROFILE=debug ./scripts/afl_build.sh
TARGET=zip PROFILE=debug ./scripts/afl_build.sh
TARGET=rkyv PROFILE=debug ./scripts/afl_build.sh
TARGET=hyper PROFILE=debug ./scripts/afl_build.sh
```

Then you can run/minimize with `afl-tmin` / `afl-cmin` without doing any fuzzing.

Available `TARGET` values:

```text
bytes | smallvec | serde_json | toml | base64 | uuid | itoa | quick_xml | simd_json | zip | rkyv | hyper
```

## Fuzzing (optional)

You can fuzz inside the container (slower than native Linux, but good for quick checks):

```bash
FUZZ=1 ./scripts/docker_afl.sh TARGET=toml PROFILE=debug ./scripts/afl_fuzz.sh
```

If the host requires relaxed sandboxing for AFL++ forkserver, start the container with:

```bash
FUZZ=1 ./scripts/docker_afl.sh
```

Reproduce crashes from host-generated outputs:

```bash
./scripts/docker_afl.sh TARGET=toml PROFILE=debug ./scripts/afl_repro.sh
```

Build-only (no fuzzing) from host:

```bash
./scripts/docker_afl.sh TARGET=toml PROFILE=debug ./scripts/afl_build.sh
```

Show instrumentation diagnostics during build:

```bash
RZ_PRINT_CRATES=1 RZ_TRACE_PASS=1 TRACE=1 ./scripts/docker_afl.sh TARGET=itoa PROFILE=debug ./scripts/afl_build.sh
```

Strict hook verification (fails build if `__rz_*` hooks are not found in the final binary):

```bash
RZ_VERIFY_HOOKS=1 RZ_VERIFY_HOOKS_STRICT=1 ./scripts/docker_afl.sh TARGET=toml PROFILE=debug ./scripts/afl_build.sh
```

## Notes

- This container installs AFL++ system-wide, so `afl-fuzz` is on `PATH`.
- The AFL++ runtime object is expected at `/usr/local/lib/afl/afl-compiler-rt.o`.
  If it is missing for any reason, you can point the build script at the in-tree
  runtime object:

  ```bash
  export AFL_COMPILER_RT=/opt/aflpp/afl-compiler-rt.o
  TARGET=toml PROFILE=debug ./scripts/afl_build.sh
  ```
- `scripts/docker_afl.sh` forwards host `RZ_*` / `RUSTEZE_*` / `TRACE` / `RUST_BACKTRACE`
  variables into the container.
