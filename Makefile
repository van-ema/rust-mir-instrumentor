EXAMPLE ?= hello
PROFILE ?= debug
MIR_OUT ?= ./out.mir
EXTRA_ARGS ?=
CARGO ?= cargo

ifeq ($(PROFILE),release)
RUNTIME_PATH := target/release
PROFILE_FLAG := --release
BIN_PATH := target/release/$(EXAMPLE)
else
RUNTIME_PATH := target/debug
PROFILE_FLAG :=
BIN_PATH := target/debug/$(EXAMPLE)
endif

.PHONY: clean clean-mir runtime tools instrument run rebuild

clean:
	$(CARGO) clean

clean-mir:
	rm -f *.mir

runtime:
	$(CARGO) build -p runtime $(PROFILE_FLAG)

tools:
	$(CARGO) install --path instrument-mir --bin instrument-mir
	$(CARGO) install --path instrument-mir --bin cargo-instrument-mir

instrument: clean-mir clean tools runtime
	$(CARGO) instrument-mir --runtime-path=$(RUNTIME_PATH) --mir-out=$(MIR_OUT) -p examples --bin $(EXAMPLE) $(PROFILE_FLAG) $(EXTRA_ARGS)

run:
	$(BIN_PATH)