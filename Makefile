# jqf build and gate shortcuts. `make help` lists every `##`-annotated target.

SHELL := /bin/bash
# Recipes are STRICT shells. `-e` stops a multi-command recipe line at its
# first failure
.SHELLFLAGS := -e -o pipefail -c
CARGO ?= cargo
CARGO_FLAGS ?=
JQF ?= target/release/jqf
PGO_BIN := target/pgo/jqf
# The measurement binary: the PGO build when it exists, the plain release
# build otherwise. A number from anything else is not a number.
MEASURE_JQF := $(shell test -x $(PGO_BIN) && echo $(PGO_BIN) || echo $(JQF))

.DEFAULT_GOAL := check

SMOKE_CRATES := jqf-sdk-smoke jqf-codec-receipts jqf-xpath-conformance
FUZZ_CODEC := $(CARGO) build --release --manifest-path tools/jqf-codec-fuzz/Cargo.toml --bin smoke $(CARGO_FLAGS)
FUZZ_SYNTAX := $(CARGO) build --release --manifest-path tools/jqf-syntax-fuzz/Cargo.toml --bin smoke $(CARGO_FLAGS)
FUZZ_PROGRAM := $(CARGO) build --release --manifest-path tools/jqf-program-fuzz/Cargo.toml --bin smoke $(CARGO_FLAGS)
FUZZ_CODEC_BIN := tools/jqf-codec-fuzz/target/release/smoke
FUZZ_SYNTAX_BIN := tools/jqf-syntax-fuzz/target/release/smoke
FUZZ_PROGRAM_BIN := tools/jqf-program-fuzz/target/release/smoke
FUZZ_SYNTAX_CORPUS := tools/jqf-syntax-fuzz/corpus/lifecycle
MAKEFILE_LINT_TARGET ?=

.PHONY: check fmt fmt-check lint test gate clean help ffi-header-lint diag-codes \
	bindings-python bindings-wasm smoke-build sdk-smoke codec-smoke codec-differential \
	codec-toml-smoke codec-flat-smoke codec-csv-smoke codec-cbor-smoke \
	codec-messagepack-smoke codec-xml-smoke codec-json-seq-smoke codec-jsonc-smoke \
	codec-json5-smoke codec-html-smoke codec-yaml-smoke codec-jqft-smoke codec-render-smoke \
	xpath-conformance fuzz fuzz-build fuzz-codec fuzz-syntax fuzz-program \
	source-bench resource-bench data-bench syntax-bench engine-bench \
	core-bench json-bench toml-bench csv-bench syntax-compat \
	jq-suite jsonpath-conformance engine-oracle render-differential \
	compat follow-e2e edit-differential parallel-diff lazy-diff toml-lazy-diff \
	mismatch-diff serve-soak stack-depth rss pgo pgo-fresh capability-gate colour-gate \
	bench cross-format-ladder csv-ladder ttfb broad-bench broad-bench-delta fixtures \
	facade-framing-matrix codec-contracts-check output-durability manpage reference \
	gates gates-merge gates-branch gates-commit gates-teeth makefile-lint licence-audit

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

clean: ## cargo clean
	$(CARGO) clean

diag-codes: ## diagnostic-code tables fresh vs codes.toml
	python3 tools/jqf-diag-codes-gen.py --check

ffi-header-lint: ## C header arity vs Rust signatures
	python3 tools/jqf-ffi-header-lint.py

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

$(JQF):
	$(CARGO) build --release -p jqf $(CARGO_FLAGS)

smoke-build: ## release build of sdk-smoke, codec-receipts, xpath-conformance
	$(CARGO) build --release $(addprefix -p ,$(SMOKE_CRATES)) $(CARGO_FLAGS)

sdk-smoke: smoke-build ## jqf-sdk-smoke receipts
	target/release/jqf-sdk-smoke

codec-smoke: smoke-build ## JSON codec receipts
	target/release/jqf-codec-receipts smoke json

codec-differential: smoke-build ## codec decode differentials vs reference decoders
	target/release/jqf-codec-receipts differential json
	target/release/jqf-codec-receipts differential yaml
	target/release/jqf-codec-receipts differential toml
	target/release/jqf-codec-receipts differential csv
	target/release/jqf-codec-receipts differential cbor
	target/release/jqf-codec-receipts differential xml
	target/release/jqf-codec-receipts differential html

codec-toml-smoke: smoke-build ## TOML codec receipts
	target/release/jqf-codec-receipts smoke toml

codec-flat-smoke: smoke-build ## properties/ini/dotenv codec receipts
	target/release/jqf-codec-receipts smoke ini

codec-csv-smoke: smoke-build ## CSV codec receipts
	target/release/jqf-codec-receipts smoke csv

codec-cbor-smoke: smoke-build ## CBOR codec receipts
	target/release/jqf-codec-receipts smoke cbor

codec-messagepack-smoke: smoke-build ## MessagePack codec receipts
	target/release/jqf-codec-receipts smoke messagepack

codec-xml-smoke: smoke-build ## XML codec receipts
	target/release/jqf-codec-receipts smoke xml

codec-json-seq-smoke: smoke-build ## json-seq codec receipts
	target/release/jqf-codec-receipts smoke json-seq

codec-jsonc-smoke: smoke-build ## JSONC codec receipts
	target/release/jqf-codec-receipts smoke jsonc

codec-json5-smoke: smoke-build ## JSON5 codec receipts
	target/release/jqf-codec-receipts smoke json5

codec-html-smoke: smoke-build ## HTML codec receipts + html5lib conformance
	target/release/jqf-codec-receipts smoke html
	target/release/jqf-tokenizer-conformance jqf-codec/html/corpus/tokenizer
	target/release/jqf-tree-conformance jqf-codec/html/corpus/tree-construction

codec-yaml-smoke: smoke-build ## YAML codec receipts
	target/release/jqf-codec-receipts smoke yaml

codec-jqft-smoke: smoke-build ## jqft family codec receipts
	target/release/jqf-codec-receipts smoke jqft

codec-render-smoke: smoke-build ## render codec receipts
	target/release/jqf-codec-receipts smoke render

xpath-conformance: smoke-build ## vendored libxml2 XPath suite
	target/release/jqf-xpath-conformance tools/jqf-xpath-conformance/corpus

fuzz-build: ## build the three standalone fuzz smoke binaries
	$(FUZZ_CODEC)
	$(FUZZ_SYNTAX)
	$(FUZZ_PROGRAM)

fuzz: fuzz-build ## every fuzz crate test + every pinned receipt
	$(CARGO) test --release --manifest-path tools/jqf-codec-fuzz/Cargo.toml $(CARGO_FLAGS)
	$(CARGO) test --release --manifest-path tools/jqf-syntax-fuzz/Cargo.toml $(CARGO_FLAGS)
	$(CARGO) test --release --manifest-path tools/jqf-program-fuzz/Cargo.toml $(CARGO_FLAGS)
	$(FUZZ_CODEC_BIN) --codec json --seed 0x4a51464a534f4e31 --executions 256 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec json --scoped --seed 0x53c09ed000000001 --executions 4096 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec json --stream --seed 0x4a51454c454d3031 --executions 4096 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec json --projected --seed 0x4a5150524f4a3031 --executions 4096 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec jsonc --seed 0x4a51464a534f4e31 --executions 256 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec core --target erased_lifecycle --seed 0x4a5146434f524531 --executions 256 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec streams --seed 0x4a51464e444a3031 --executions 4096 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec core --target demand --seed 0x4a5146434f524531 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec core --target access --seed 0x4a5146434f524531 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec core --target encoder_ack --seed 0x4a5146434f524531 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec yaml --seed 0x004a5159514d4c31 --executions 10000 --verify-receipt
	$(FUZZ_SYNTAX_BIN) --seed 0x4a514653594e3031 --executions 10000 --corpus $(FUZZ_SYNTAX_CORPUS) --verify-receipt
	$(FUZZ_CODEC_BIN) --codec cbor --seed 0x4a514346424f5231 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec cbor_seq --seed 0x4a514342535131 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec xml --seed 0x4a5158504d4c3031 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec json_seq --seed 0x4a51535153455131 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec html --seed 0x4a5148544d4c31 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec toml --seed 0x4a51544f4d4c31 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec delimited --seed 0x4a514353562031 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec cbor --target bytes --seed 0x4a51425954455331 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec yaml --target bytes --seed 0x4a51425954455331 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec xml --target bytes --seed 0x4a51425954455331 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec toml --target bytes --seed 0x4a51425954455331 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec delimited --target bytes --seed 0x4a51425954455331 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec messagepack --target bytes --seed 0x4a51425954455331 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec html --target bytes --seed 0x4a51425954455331 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec render --seed 0x4a5152454e4431 --executions 10000 --verify-receipt
	$(FUZZ_PROGRAM_BIN) --seed 0x4a5150524f473031 --executions 1500 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec jqft --seed 0x4a514a51465431 --executions 10000 --verify-receipt
	$(FUZZ_CODEC_BIN) --codec messagepack --seed 0x4a51464d504b3031 --executions 10000 --verify-receipt

fuzz-codec: ## codec-fuzz tests + receipts
	$(FUZZ_CODEC)
	$(CARGO) test --release --manifest-path tools/jqf-codec-fuzz/Cargo.toml $(CARGO_FLAGS)

fuzz-syntax: ## syntax-fuzz tests + lifecycle receipt
	$(FUZZ_SYNTAX)
	$(CARGO) test --release --manifest-path tools/jqf-syntax-fuzz/Cargo.toml $(CARGO_FLAGS)
	$(FUZZ_SYNTAX_BIN) --seed 0x4a514653594e3031 --executions 10000 --corpus $(FUZZ_SYNTAX_CORPUS) --verify-receipt

fuzz-program: ## program-fuzz tests + receipt
	$(FUZZ_PROGRAM)
	$(CARGO) test --release --manifest-path tools/jqf-program-fuzz/Cargo.toml $(CARGO_FLAGS)
	$(FUZZ_PROGRAM_BIN) --seed 0x4a5150524f473031 --executions 1500 --verify-receipt

source-bench: ## jqf-source timing worker
	$(CARGO) run --release --locked -p jqf-source-bench $(CARGO_FLAGS)

resource-bench: ## jqf-resource timing worker
	$(CARGO) run --release --locked -p jqf-resource-bench $(CARGO_FLAGS)

data-bench: ## jqf-data timing worker
	$(CARGO) run --release --locked -p jqf-data-bench $(CARGO_FLAGS)

syntax-bench: ## jqf-syntax timing worker
	$(CARGO) run --release --locked -p jqf-syntax-bench $(CARGO_FLAGS)

engine-bench: ## jqf-engine timing worker
	$(CARGO) run --release --locked -p jqf-engine-bench $(CARGO_FLAGS)

core-bench: ## codec-core timing worker
	$(CARGO) run --release -p jqf-codec-core-bench $(CARGO_FLAGS)

json-bench: ## codec-json timing worker
	$(CARGO) run --release -p jqf-codec-json-bench --bin jqf-codec-json-bench $(CARGO_FLAGS)

toml-bench: ## codec-toml timing worker
	$(CARGO) run --release -p jqf-codec-toml-bench $(CARGO_FLAGS)

csv-bench: ## codec-csv timing worker
	$(CARGO) run --release -p jqf-codec-csv-bench $(CARGO_FLAGS)

syntax-compat: ## jq 1.8.2 grammar-compat harness (manual; needs jq-1.8.2)
	$(CARGO) run -p jqf-syntax-compat $(CARGO_FLAGS)

jq-suite: $(JQF) ## jq 1.8.2 official jq.test and onig.test
	python3 tools/jqf-jq-test-suite.py $(JQF)
	python3 tools/jqf-jq-test-suite.py $(JQF) --suite onig

jsonpath-conformance: $(JQF) ## RFC 9535 JSONPath CTS
	python3 tools/jqf-jsonpath-cts.py $(JQF)

engine-oracle: $(JQF) ## ~ engine-oracle self-consistency laws
	python3 tools/jqf-engine-oracle.py $(JQF)

render-differential: $(JQF) ## engine renderer vs codec
	python3 tools/jqf-render-differential.py $(JQF)

compat: $(JQF) ## jq CLI flag/compat corpus
	tools/jqf-cli-jq-compat.sh $(JQF)

follow-e2e: $(JQF) ## --follow live-tail lanes
	python3 tools/jqf-follow-e2e.py $(JQF)

edit-differential: $(JQF) ## identity-edit + survival receipts
	python3 tools/jqf-edit-differential.py $(JQF)

parallel-diff: $(JQF) ## serial vs parallel byte identity
	python3 tools/jqf-parallel-differential.py $(JQF)

lazy-diff: $(JQF) ## eager vs on-demand spans
	python3 tools/jqf-lazy-differential.py $(JQF)

toml-lazy-diff: $(JQF) ## TOML on-demand differential
	python3 tools/jqf-toml-lazy-differential.py $(JQF)

mismatch-diff: $(JQF) ## mismatch-policy strict vs reference route
	python3 tools/jqf-mismatch-differential.py $(JQF)

serve-soak: $(JQF) ## jqf serve RSS-flat soak
	python3 tools/jqf-serve-soak.py $(JQF)

stack-depth: ## recursion-guard lanes against the debug binary
	$(CARGO) build -p jqf $(CARGO_FLAGS)
	python3 tools/jqf-stack-depth-gate.py target/debug/jqf

rss: $(JQF) ## peak-RSS ceilings
	python3 tools/jqf-rss-gate.py $(JQF)

pgo: ## PGO build -> target/pgo/jqf (the only binary a measurement may come from)
	tools/jqf-pgo-build.sh

pgo-fresh: pgo ## PGO binary trained at HEAD? (freshness gate)
	tools/jqf-pgo-freshness.sh

bench: ## e2e ladder: jqf vs jq/jaq/gojq, correctness-gated
	JQF_BIN=$(MEASURE_JQF) tools/jqf-e2e-ladder.sh

cross-format-ladder: ## YAML/TOML cross-format ladder vs jq/gojq
	JQF_BIN=$(MEASURE_JQF) tools/jqf-cross-format-ladder.sh

csv-ladder: ## CSV record route vs jq/mlr/qsv ladders
	tools/jqf-csv-ladder.sh $(MEASURE_JQF)

ttfb: ## time-to-first-output lanes
	JQF_BIN=$(MEASURE_JQF) tools/jqf-ttfb-runner.py

broad-bench: ## broad cross-tool comparison (lineage binaries on PATH when present)
	JQF_BIN=$(MEASURE_JQF) tools/jqf-broad-bench.py

broad-bench-delta: ## row-level delta of fresh broad-bench results vs the pinned receipt
	@test -f $(or $(NEW_RESULTS),target/broad-bench-results.jsonl) || \
	  { echo 'run tools/jqf-broad-bench.py first (or set NEW_RESULTS=<path>)' >&2; exit 2; }
	tools/jqf-broad-bench-delta.py $(or $(NEW_RESULTS),target/broad-bench-results.jsonl)

fixtures: ## generate e2e fixtures into $JQF_E2E_FIXDIR
	@test -n "$(JQF_E2E_FIXDIR)" || { echo 'set JQF_E2E_FIXDIR=<dir> first' >&2; exit 2; }
	tools/jqf-e2e-fixtures.py $(JQF_E2E_FIXDIR)

capability-gate: $(JQF) ## input x output format matrix
	python3 tools/jqf-capability-gate.py $(JQF)

colour-gate: $(JQF) ## colour strip-identity / TTY decision law
	python3 tools/jqf-colour-gate.py $(JQF)

facade-framing-matrix: $(JQF) ## per-format facade suffix over output/edit/record
	python3 tools/jqf-facade-framing-matrix.py $(JQF)

codec-contracts-check: ## backtick identifiers in jqf-codec/CONTRACTS.md resolve
	python3 tools/jqf-codec-contracts-check.py

output-durability: ## fsync calls still precede both atomic renames
	python3 tools/jqf-output-durability-check.py

manpage: $(JQF) ## regenerate docs/jqf.1
	python3 tools/jqf-manpage-gen.py --jqf $(JQF)

reference: $(JQF) ## REFERENCE.md freshness vs the release binary
	python3 tools/jqf-reference-gen.py --check --jqf $(JQF)

gates: gates-merge ## full gate battery (merge tier)

gates-merge: $(JQF) ## full gate battery, merge tier
	python3 tools/jqf-gates.py --tier merge --jqf $(JQF)

gates-branch: $(JQF) ## full gate battery, branch tier
	python3 tools/jqf-gates.py --tier branch --jqf $(JQF)

gates-commit: $(JQF) ## fast per-commit gate battery
	python3 tools/jqf-gates.py --tier commit --jqf $(JQF)

gates-teeth: ## known-bad probes: every lane must go red
	python3 tools/jqf-gates-teeth.py

makefile-lint: ## recipe failure-propagation lint + tier-registry cross-check
	python3 tools/jqf-makefile-recipe-lint.py $(MAKEFILE_LINT_TARGET)

licence-audit: ## Cargo.lock licence table, local, no network
	python3 tools/jqf-licence-audit.py

help: ## list targets
	@awk 'BEGIN {FS = ":.*## "; printf "jqf targets (default: check):\n\n"} /^[a-zA-Z0-9_-]+:.*## / {printf "  %-22s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
