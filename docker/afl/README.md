# AFL++ Docker container (Linux)

This is a convenience container for running AFL++ tools and building/running
rusteze harnesses on Linux even if the host is macOS.

## Build

From the repo root:

```bash
docker build -t rusteze-afl -f docker/afl/Dockerfile .
```

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

## Reproduce a crash (no fuzzing)

If you already have a crash input in `crashes/id:000000,...`, just run the
target binary with that input file (AFL uses `@@` to pass the file name):

```bash
./target/afl-release/release/afl_bytes_driver crashes/id:000000,*
```

Or use AFL tools to minimize:

```bash
afl-tmin -i crashes/id:000000,* -o minimized \
  -- ./target/afl-release/release/afl_bytes_driver @@
```

## Build harnesses inside the container

Inside the container:

```bash
TARGET=bytes PROFILE=release ./scripts/afl_build.sh
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
