EXAMPLE ?= hello
PROFILE ?= debug
MIR_OUT ?= ./out.mir
EXTRA_ARGS ?=
CARGO ?= cargo
CARGO_INCREMENTAL ?= 0
RZ_INSTRUMENT_ALL_DEPS ?= 1
RZ_INSTRUMENT_STDLIB ?= none
BATCH_BUILD_STD ?= 1

BUILD_STD ?= 0
BUILD_STD_CRATES ?= alloc,std,core
BUILD_STD_FEATURES ?=
SUITE_TARGET_ROOT ?= $(abspath target/example-tests-stdlib-all)

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
COMMON_ENV := CARGO_INCREMENTAL=$(CARGO_INCREMENTAL) RZ_INSTRUMENT_ALL_DEPS=$(RZ_INSTRUMENT_ALL_DEPS)
INSTRUMENT_ENV := $(COMMON_ENV) RZ_INSTRUMENT_STDLIB=$(RZ_INSTRUMENT_STDLIB)
RUNTIME_PATH := $(TARGET_ROOT)/$(PROFILE_DIR)/deps
BIN_PATH := $(TARGET_ROOT)/$(PROFILE_DIR)/$(EXAMPLE)

.PHONY: clean clean-mir runtime tools instrument run rebuild instrument-stdlib-core instrument-stdlib-core-alloc instrument-stdlib-all test-stdlib-core test-stdlib-core-alloc test-stdlib-all test-medium-stdlib-all test-fuzz-stdlib-all-smoke

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
instrument-stdlib-core:
	$(MAKE) instrument BUILD_STD=1 BUILD_STD_CRATES=core,alloc,std RZ_INSTRUMENT_STDLIB=core

instrument-stdlib-core-alloc:
	$(MAKE) instrument BUILD_STD=1 BUILD_STD_CRATES=core,alloc,std RZ_INSTRUMENT_STDLIB=core_alloc

instrument-stdlib-all:
	$(MAKE) instrument BUILD_STD=1 BUILD_STD_CRATES=core,alloc,std RZ_INSTRUMENT_STDLIB=all

test-stdlib-core:
	CARGO_TARGET_DIR=$(SUITE_TARGET_ROOT)-core BATCH_BUILD_STD=$(BATCH_BUILD_STD) $(COMMON_ENV) BUILD_STD=1 BUILD_STD_CRATES=core,alloc,std RZ_INSTRUMENT_STDLIB=core python3 scripts/run_example_tests.py

test-stdlib-core-alloc:
	CARGO_TARGET_DIR=$(SUITE_TARGET_ROOT)-core-alloc BATCH_BUILD_STD=$(BATCH_BUILD_STD) $(COMMON_ENV) BUILD_STD=1 BUILD_STD_CRATES=core,alloc,std RZ_INSTRUMENT_STDLIB=core_alloc python3 scripts/run_example_tests.py

test-stdlib-all:
	CARGO_TARGET_DIR=$(SUITE_TARGET_ROOT)-all BATCH_BUILD_STD=$(BATCH_BUILD_STD) $(COMMON_ENV) BUILD_STD=1 BUILD_STD_CRATES=core,alloc,std RZ_INSTRUMENT_STDLIB=all python3 scripts/run_example_tests.py

test-medium-stdlib-all:
	CARGO_TARGET_DIR=$(SUITE_TARGET_ROOT)-medium-all $(COMMON_ENV) BUILD_STD=1 BUILD_STD_CRATES=core,alloc,std RZ_INSTRUMENT_STDLIB=all FLAKY_EXAMPLES= MEDIUM_COMMANDS_FILE=scripts/medium_commands.txt scripts/run_harness.sh

test-fuzz-stdlib-all-smoke:
	CARGO_TARGET_DIR=$(SUITE_TARGET_ROOT)-fuzz-all $(COMMON_ENV) BUILD_STD=1 BUILD_STD_CRATES=core,alloc,std RZ_INSTRUMENT_STDLIB=all scripts/run_fuzz_smoke.sh

run:
	$(BIN_PATH)

rebuild: instrument
