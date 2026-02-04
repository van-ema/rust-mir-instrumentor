FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive

# ---- Base tooling ----
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    git \
    python3 \
    python3-pip \
    build-essential \
    clang \
    llvm \
    lld \
    cmake \
    ninja-build \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# ---- Rust toolchain (needed to build rusteze + harnesses in-container) ----
ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:$PATH

RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
    && rustup toolchain install nightly \
    && rustup default nightly

# ---- AFL++ ----
# We build from source to get LLVM mode and consistent paths.
WORKDIR /opt
RUN git clone --depth 1 https://github.com/AFLplusplus/AFLplusplus.git aflpp \
    && make -C /opt/aflpp distrib \
    && make -C /opt/aflpp install \
    && mkdir -p /usr/local/lib/afl \
    && cp /opt/aflpp/afl-compiler-rt.o /usr/local/lib/afl/afl-compiler-rt.o

# AFL++ install convention:
# - binaries: /usr/local/bin/afl-*
# - runtime:  /usr/local/lib/afl/afl-compiler-rt.o
ENV AFL_PATH=/usr/local/lib/afl

# Helpful defaults for fuzzing/repro in containers.
ENV CARGO_INCREMENTAL=0 \
    RZ_INSTRUMENT_ALL_DEPS=1 \
    RZ_LOG=warn \
    ASAN_OPTIONS=detect_leaks=0

WORKDIR /work
