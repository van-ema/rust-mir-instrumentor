EXAMPLE ?= hello
PROFILE ?= debug
MIR_OUT ?= ./out.mir
EXTRA_ARGS ?=
CARGO ?= cargo

BUILD_STD ?= 0
BUILD_STD_CRATES ?= alloc,std,core
BUILD_STD_FEATURES ?=


CARGO_CMD := $(CARGO)

# Extra cargo flags appended to the `cargo instrument-mir` invocation.
BUILD_STD_ARGS :=
ifeq ($(BUILD_STD),1)
  BUILD_STD_ARGS += -Z build-std=$(BUILD_STD_CRATES)
  ifneq ($(strip $(BUILD_STD_FEATURES)),)
    BUILD_STD_ARGS += -Z build-std-features=$(BUILD_STD_FEATURES)
  endif
endif

ifeq ($(PROFILE),release)
RUNTIME_PATH := $(abspath target/release)
PROFILE_FLAG := --release
BIN_PATH := target/release/$(EXAMPLE)
else
RUNTIME_PATH := $(abspath target/debug)
PROFILE_FLAG :=
BIN_PATH := target/debug/$(EXAMPLE)
endif

.PHONY: clean clean-mir runtime tools instrument run rebuild

clean:
	$(CARGO_CMD) clean

clean-mir:
	rm -f *.mir

runtime:
	$(CARGO_CMD) build -p runtime $(PROFILE_FLAG) $(BUILD_STD_ARGS)

tools:
	$(CARGO_CMD) install --path instrument-mir --bin instrument-mir
	$(CARGO_CMD) install --path instrument-mir --bin cargo-instrument-mir

instrument: clean-mir clean tools runtime
	$(CARGO_CMD) instrument-mir --runtime-path=$(RUNTIME_PATH) --mir-out=$(MIR_OUT) -p $(EXAMPLE) $(PROFILE_FLAG) $(BUILD_STD_ARGS) $(EXTRA_ARGS)

run:
	$(BIN_PATH)