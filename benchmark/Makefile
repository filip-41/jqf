# From the repo root: `make bench` (builds PGO first).
# This Makefile always times target/pgo/jqf.

SHELL := /bin/bash
JQF_BIN ?= ../target/pgo/jqf
RUNS ?=
WARMUP ?=
FLAGS := --jqf $(JQF_BIN)
ifneq ($(RUNS),)
FLAGS += --runs $(RUNS)
endif
ifneq ($(WARMUP),)
FLAGS += --warmup $(WARMUP)
endif

.PHONY: all setup pgo-fresh run force jqf-only with-excluded quick self-test clean help

all: run ## fetch pinned tools if needed, then run the panel

setup: ## download pinned tools into .deps/bin
	python3 run.py setup

pgo-fresh: ## require a current PGO jqf
	JQF_PGO_BIN="$(JQF_BIN)" ../tools/pgo/jqf-pgo-freshness.sh

run force jqf-only with-excluded quick: pgo-fresh

run: ## full panel (skips compatible cached cells)
	python3 run.py run $(FLAGS)

force: ## full panel, ignore cache
	python3 run.py run $(FLAGS) --force

jqf-only: ## remeasure jqf; keep compatible cached competitor cells
	python3 run.py run $(FLAGS) --jqf-only

with-excluded: ## full panel including excluded yq cells
	python3 run.py run $(FLAGS) --with-excluded

quick: ## size 100
	python3 run.py run $(FLAGS) --quick

self-test: ## measure + catalog expand
	python3 test_benchmark.py
	python3 run.py self-test
	python3 measure.py

clean: ## drop downloaded tools and generated fixtures
	rm -rf .deps .cache

help: ## list targets
	@awk 'BEGIN {FS = ":.*## "; printf "benchmark targets (default: run):\n\n"} /^[a-zA-Z0-9_-]+:.*## / {printf "  %-12s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
