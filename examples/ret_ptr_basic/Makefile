ROOT_DIR := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/../..)
EXAMPLE ?= ret_ptr_basic

.PHONY: clean clean-mir runtime tools instrument run rebuild

clean:
	$(MAKE) -C $(ROOT_DIR) clean

clean-mir:
	$(MAKE) -C $(ROOT_DIR) clean-mir

runtime:
	$(MAKE) -C $(ROOT_DIR) runtime

tools:
	$(MAKE) -C $(ROOT_DIR) tools

instrument:
	$(MAKE) -C $(ROOT_DIR) instrument EXAMPLE=$(EXAMPLE)

run:
	$(MAKE) -C $(ROOT_DIR) run EXAMPLE=$(EXAMPLE)

rebuild:
	$(MAKE) -C $(ROOT_DIR) rebuild EXAMPLE=$(EXAMPLE)
