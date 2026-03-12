EXAMPLE ?= hello
PROFILE ?= debug
MIR_OUT ?= ./out.mir
EXTRA_ARGS ?=
CARGO ?= cargo
CARGO_INCREMENTAL ?= 0
RZ_INSTRUMENT_ALL_DEPS ?= 1
RZ_INSTRUMENT_STDLIB ?= none

BUILD_STD ?= 0
BUILD_STD_CRATES ?= alloc,std,core
BUILD_STD_FEATURES ?=

CARGO_CMD := $(CARGO)
TARGET_ROOT ?= $(abspath target)
RUNTIME_RUSTC :=
RUNTIME_ENV_EXTRA :=

# Extra cargo flags appended to the `cargo instrument-mir` invocation.
BUILD_STD_ARGS :=
ifeq ($(BUILD_STD),1)
  BUILD_STD_ARGS += -Z build-std=$(BUILD_STD_CRATES)
  ifneq ($(strip $(BUILD_STD_FEATURES)),)
    BUILD_STD_ARGS += -Z build-std-features=$(BUILD_STD_FEATURES)
  endif
endif

ifeq ($(PROFILE),release)
PROFILE_FLAG := --release
PROFILE_DIR := release
RUNTIME_FEATURES :=
else
PROFILE_FLAG :=
PROFILE_DIR := debug
RUNTIME_FEATURES := --features rz_log
endif

ifeq ($(BUILD_STD),1)
TARGET_ROOT := $(abspath $(TARGET_ROOT)/build-std-$(PROFILE_DIR))
RUNTIME_RUSTC := RUSTC=instrument-mir
RUNTIME_ENV_EXTRA := RZ_INSTRUMENT_ALL_DEPS=0 RZ_INSTRUMENT_STDLIB=$(RZ_INSTRUMENT_STDLIB) RZ_SKIP_RUNTIME_HOOKS=1
endif

CARGO_ENV := CARGO_TARGET_DIR=$(TARGET_ROOT)
INSTRUMENT_ENV := CARGO_INCREMENTAL=$(CARGO_INCREMENTAL) RZ_INSTRUMENT_ALL_DEPS=$(RZ_INSTRUMENT_ALL_DEPS) RZ_INSTRUMENT_STDLIB=$(RZ_INSTRUMENT_STDLIB)
RUNTIME_PATH := $(TARGET_ROOT)/$(PROFILE_DIR)/deps
BIN_PATH := $(TARGET_ROOT)/$(PROFILE_DIR)/$(EXAMPLE)

.PHONY: clean clean-mir runtime tools instrument run rebuild instrument-stdlib-all

clean:
	$(CARGO_ENV) $(CARGO_CMD) clean

clean-mir:
	rm -f *.mir

runtime:
	$(CARGO_ENV) $(RUNTIME_ENV_EXTRA) $(RUNTIME_RUSTC) $(CARGO_CMD) build -p runtime_abi $(PROFILE_FLAG) $(BUILD_STD_ARGS)
	$(CARGO_ENV) $(RUNTIME_ENV_EXTRA) $(RUNTIME_RUSTC) $(CARGO_CMD) build -p runtime $(PROFILE_FLAG) $(BUILD_STD_ARGS) $(RUNTIME_FEATURES)

tools:
	$(CARGO_CMD) install --path instrument-mir --bin instrument-mir
	$(CARGO_CMD) install --path instrument-mir --bin cargo-instrument-mir

instrument: clean-mir clean tools runtime
	$(CARGO_ENV) $(INSTRUMENT_ENV) $(CARGO_CMD) instrument-mir --runtime-path=$(RUNTIME_PATH) --mir-out=$(MIR_OUT) -p $(EXAMPLE) $(PROFILE_FLAG) $(BUILD_STD_ARGS) $(EXTRA_ARGS)

# Full stdlib instrumentation (core + alloc + std) via build-std.
instrument-stdlib-all:
	$(MAKE) instrument BUILD_STD=1 BUILD_STD_CRATES=core,alloc,std RZ_INSTRUMENT_STDLIB=all

run:
	$(BIN_PATH)

rebuild: instrument
