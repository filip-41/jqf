# jqf build and gate shortcuts. `make help` lists every `##`-annotated target.

SHELL := /bin/bash
# Recipes are STRICT shells. `-e` stops a multi-command recipe line at its
# first failure
.SHELLFLAGS := -e -o pipefail -c
CARGO ?= cargo
RUSTC ?= rustc
CARGO_FLAGS ?=
JQF ?= target/release/jqf

.DEFAULT_GOAL := check

SMOKE_CRATES := jqf-sdk-smoke jqf-codec-smoke

.PHONY: check fmt fmt-check lint test gate clean help ffi-header-lint diag-codes \
	bindings-python bindings-wasm smoke-build sdk-smoke codec-json-smoke codec-smokes codec-differential \
	codec-toml-smoke codec-flat-smoke codec-csv-smoke codec-cbor-smoke \
	codec-messagepack-smoke codec-xml-smoke codec-json-seq-smoke codec-jsonc-smoke \
	codec-json5-smoke codec-html-smoke codec-yaml-smoke codec-jqft-smoke codec-render-smoke \
	stack-depth capability-gate colour-gate \
	codec-contracts-check manpage ci ci-gates bench pgo pgo-fresh pgo-test

check: ## cargo check --workspace --all-targets
	$(CARGO) check --workspace --all-targets $(CARGO_FLAGS)

fmt: ## rustfmt, writing
	$(CARGO) fmt --all

fmt-check: ## rustfmt, checking
	$(CARGO) fmt --all --check

lint: ## cargo clippy --workspace --all-targets (zero warnings)
	$(CARGO) clippy --workspace --all-targets $(CARGO_FLAGS) -- -D warnings

test: ## cargo test --workspace < /dev/null
	$(CARGO) test --workspace $(CARGO_FLAGS) < /dev/null

# One recipe so `make -j gate` cannot run clippy and test on the same
# `target/` directory.
gate: ## fmt-check, lint, and test
	$(MAKE) --no-print-directory -j1 fmt-check lint test

# GHA `gates` job. Add tree-checks here; each stays callable alone.
ci-gates: ## CI gates: tree checks + capability/colour/stack-depth
	$(MAKE) --no-print-directory -j1 codec-contracts-check ffi-header-lint diag-codes \
		capability-gate colour-gate stack-depth

# Local stand-in for the GHA DAG: fmt + cargo, then test and gates together.
ci: ## fmt-check, lint, then test + ci-gates (--locked)
	$(MAKE) --no-print-directory -j1 CARGO_FLAGS=--locked fmt-check lint
	$(MAKE) --no-print-directory -j2 CARGO_FLAGS=--locked test ci-gates

clean: ## cargo clean
	$(CARGO) clean

diag-codes: ## diagnostic-code tables fresh vs codes.toml
	python3 tools/gates/jqf-diag-codes-gen.py --check

ffi-header-lint: ## C header vs Rust signatures
	python3 tools/gates/jqf-ffi-header-lint.py

bindings-python: ## build the FFI cdylib and run the ctypes tests
	$(CARGO) build --release -p jqf-sdk-ffi $(CARGO_FLAGS)
	python3 -m unittest discover -s bindings/python/tests

bindings-wasm: ## build the wasm bundle and run the Node smoke test
	@command -v node >/dev/null || { echo 'bindings-wasm: node is required' >&2; exit 1; }
	@command -v wasm-bindgen >/dev/null || { echo 'bindings-wasm: wasm-bindgen CLI is required' >&2; exit 1; }
	$(CARGO) build --release -p jqf-wasm --target wasm32-unknown-unknown $(CARGO_FLAGS)
	wasm-bindgen --target web --out-dir bindings/wasm --out-name jqf_wasm \
	  target/wasm32-unknown-unknown/release/jqf_wasm.wasm
	node bindings/wasm/tests/smoke.mjs

.PHONY: FORCE
FORCE:
$(JQF): FORCE
	$(CARGO) build --release -p jqf $(CARGO_FLAGS)

smoke-build: ## release build of sdk-smoke and codec-smoke
	$(CARGO) build --release $(addprefix -p ,$(SMOKE_CRATES)) $(CARGO_FLAGS)

sdk-smoke: smoke-build ## jqf-sdk-smoke receipts
	target/release/jqf-sdk-smoke

codec-json-smoke: smoke-build ## JSON codec receipts
	target/release/jqf-codec-smoke smoke json

codec-smokes: ## every codec smoke + html5lib + differentials
	$(MAKE) --no-print-directory codec-json-smoke codec-differential codec-toml-smoke codec-flat-smoke \
		codec-csv-smoke codec-cbor-smoke codec-messagepack-smoke codec-xml-smoke \
		codec-json-seq-smoke codec-jsonc-smoke codec-json5-smoke codec-html-smoke \
		codec-yaml-smoke codec-jqft-smoke codec-render-smoke

codec-differential: smoke-build ## codec decode differentials vs reference decoders
	target/release/jqf-codec-smoke differential json
	target/release/jqf-codec-smoke differential yaml
	target/release/jqf-codec-smoke differential toml
	target/release/jqf-codec-smoke differential csv
	target/release/jqf-codec-smoke differential cbor
	target/release/jqf-codec-smoke differential xml
	target/release/jqf-codec-smoke differential html

codec-toml-smoke: smoke-build ## TOML codec receipts
	target/release/jqf-codec-smoke smoke toml

codec-flat-smoke: smoke-build ## properties/ini/dotenv codec receipts
	target/release/jqf-codec-smoke smoke ini

codec-csv-smoke: smoke-build ## CSV codec receipts
	target/release/jqf-codec-smoke smoke csv

codec-cbor-smoke: smoke-build ## CBOR codec receipts
	target/release/jqf-codec-smoke smoke cbor

codec-messagepack-smoke: smoke-build ## MessagePack codec receipts
	target/release/jqf-codec-smoke smoke messagepack

codec-xml-smoke: smoke-build ## XML codec receipts + libxml2 xpath suite
	target/release/jqf-codec-smoke smoke xml
	target/release/jqf-xpath-conformance

codec-json-seq-smoke: smoke-build ## json-seq codec receipts
	target/release/jqf-codec-smoke smoke json-seq

codec-jsonc-smoke: smoke-build ## JSONC codec receipts
	target/release/jqf-codec-smoke smoke jsonc

codec-json5-smoke: smoke-build ## JSON5 codec receipts
	target/release/jqf-codec-smoke smoke json5

codec-html-smoke: smoke-build ## HTML codec receipts + html5lib conformance
	target/release/jqf-codec-smoke smoke html
	target/release/jqf-tokenizer-conformance jqf-codec/html/corpus/tokenizer
	target/release/jqf-tree-conformance jqf-codec/html/corpus/tree-construction

codec-yaml-smoke: smoke-build ## YAML codec receipts
	target/release/jqf-codec-smoke smoke yaml

codec-jqft-smoke: smoke-build ## jqft family codec receipts
	target/release/jqf-codec-smoke smoke jqft

codec-render-smoke: smoke-build ## render codec receipts
	target/release/jqf-codec-smoke smoke render

stack-depth: ## recursion-guard lanes against the debug binary
	$(CARGO) build -p jqf $(CARGO_FLAGS)
	python3 tools/gates/jqf-stack-depth-gate.py target/debug/jqf

capability-gate: $(JQF) ## input x output format matrix
	python3 tools/gates/jqf-capability-gate.py $(JQF)

colour-gate: $(JQF) ## colour strip-identity / TTY decision law
	python3 tools/gates/jqf-colour-gate.py $(JQF)

codec-contracts-check: ## backtick identifiers in jqf-codec/CONTRACTS.md resolve
	python3 tools/gates/jqf-codec-contracts-check.py

manpage: $(JQF) ## regenerate docs/jqf.1
	python3 tools/gates/jqf-manpage-gen.py --jqf $(JQF)

pgo: ## profile-guided jqf → target/pgo/jqf
	CARGO="$(CARGO)" RUSTC="$(RUSTC)" tools/pgo/jqf-pgo-build.sh

pgo-fresh: ## verify target/pgo/jqf matches code and training workload
	tools/pgo/jqf-pgo-freshness.sh

pgo-test: ## PGO trainer and freshness regressions
	python3 tools/pgo/test_pgo.py

bench: pgo ## CLI comparison vs pinned competitors (always PGO jqf)
	$(MAKE) -C benchmark

help: ## list targets
	@awk 'BEGIN {FS = ":.*## "; printf "jqf targets (default: check):\n\n"} /^[a-zA-Z0-9_-]+:.*## / {printf "  %-22s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
