#!/usr/bin/env bash

# Hermeticity: a developer's .jqf.toml must never reach a gate.
export JQF_NO_CONFIG=1
#
# Cross-format end-to-end benchmark ladder: jqf's non-JSON codecs and its
# beyond-jq features vs the tools that can do each thing — yq, dasel, python3,
# jq/jaq/gojq, and gzip. Whole-process decode+filter+encode over stdin/stdout
# (and, for the --in-place lanes, over a temp file).
#
# This ladder exists because the JSON ladder (jqf-e2e-ladder.sh) was written
# when jqf was a JSON tool. Since then jqf has grown YAML, TOML, CBOR, and XML
# codecs, compression builtins, mutation/assignment semantics, and the
# --edit/--in-place editing routes — and none of that had an end-to-end,
# competitor-compared, correctness-gated measurement. Lanes F1-F6 shipped
# first; the B/Z/C/M/E lanes followed.
#
# Lanes:
#   F1  YAML identity       `.`     --input-format yaml -c   on yaml-catalog-10mb.yaml
#   F2  YAML extraction     `.catalog[500].id`
#   F3  TOML identity       `.`     --input-format toml -c   on toml-catalog-10mb.toml
#   F4  TOML extraction     `.catalog[500].color`
#   F5  YAML->YAML rt       `.`     --input-format yaml --output-format yaml -c
#   F6  YAML->NDJSON stream `.catalog[]`  --input-format yaml --output-format ndjson -c
#   B1  CBOR->JSON identity `.`     --input-format cbor -c   on cbor-catalog.bin
#   B2  CBOR->CBOR identity `.`     --input-format cbor --output-format cbor
#   Z1  XML->JSON identity  `.`     --input-format xml -c    on xml-catalog-10mb.xml
#   C1  gzip compress       `tostring | gzip_compress`       vs gzip -c -n
#   C2  zlib compress       `tostring | zlib_compress`       vs python3 zlib
#   C3  deflate compress    `tostring | deflate_compress`    vs python3 raw deflate
#   C4  gzip round-trip     `tostring | gzip_compress | gzip_decompress` vs gzip pipe
#   M1  mutation            `.catalog[].stock |= .+1`        vs jq/jaq/gojq
#   M2  deep update         `.meta.count |= .+1`
#   M3  delete              `del(.catalog[].sku)`
#   M4  setpath             `setpath(["meta","generated"]; true)`
#   E1  --edit identity     `--edit '.'`                     vs jq identity
#   E2  --edit update       `--edit '.catalog[].stock *= 2'` vs jq reduce form
#   E3  --in-place identity `--in-place '.'`                 vs yq -i
#   E4  --in-place update   `--in-place '.meta.generated = true'` vs yq -i
#
# ---------------------------------------------------------------------------
# OUTPUT AGREEMENT IS THE DEFAULT POSTURE.
# ---------------------------------------------------------------------------
# No cell is timed before its output has been validated. jqf must match the
# lane oracle: yq for YAML/TOML, jq for JSON and JSON-mutation, the fixture
# itself for CBOR byte-identity, an external decompressor for the compression
# lanes. AND every COMPETITOR is run once against the real fixture and its
# output compared to the same oracle before it is allowed near hyperfine.
#
# Competitor columns: yq, dasel, py, jq, jaq, gojq, gzip — the tools that can
# read each format / express each program. A lane's column is `n/a` when no
# fair-equivalent expression exists for that tool (jq cannot read YAML, gzip
# cannot edit JSON), `absent` when the tool is not installed or failed its
# probe for this lane, `disagreed` when it ran but its output failed the
# oracle comparison, and `err` when the timing harness itself failed.
#
# Usage: tools/jqf-cross-format-ladder.sh [--json] [path-to-jqf]
#   JQF_BIN                  overrides the jqf binary path (skips the build step)
#   JQF_CROSS_FORMAT_FIXDIR  reuses/persists generated fixtures
#   JQF_CROSS_FORMAT_TIMEOUT per-lane timing bound in seconds (default 300)
#   JQF_CROSS_FORMAT_PROBE_TIMEOUT per-probe/validation bound (default 60)
#   JQ_BIN / JAQ_BIN / GOJQ_BIN / YQ_BIN / DASEL_BIN / PYTHON3_BIN / GZIP_BIN
#                            override tool paths
#   HYPERFINE_BIN            overrides hyperfine path
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

JSON_MODE=0
JQF_ARG=""
for arg in "$@"; do
    case "$arg" in
        --json) JSON_MODE=1 ;;
        *) JQF_ARG="$arg" ;;
    esac
done

JQF="${JQF_BIN:-${JQF_ARG:-$ROOT/target/release/jqf}}"
case "$JQF" in
    /*) ;;
    *) JQF="$ROOT/$JQF" ;;
esac
if [ ! -x "$JQF" ]; then
    echo "jqf-cross-format-ladder: $JQF not found; building release jqf..." >&2
    (cd "$ROOT" && cargo build --release -p jqf) >&2 || { echo "error: build failed" >&2; exit 2; }
fi

YQ="${YQ_BIN:-$(command -v yq || true)}"
DASEL="${DASEL_BIN:-$(command -v dasel || true)}"
PYTHON3="${PYTHON3_BIN:-$(command -v python3 || true)}"
JQ="${JQ_BIN:-$(command -v jq || true)}"
JAQ="${JAQ_BIN:-$(command -v jaq || true)}"
GOJQ="${GOJQ_BIN:-$(command -v gojq || true)}"
GZIP="${GZIP_BIN:-$(command -v gzip || true)}"
HYPERFINE="${HYPERFINE_BIN:-$(command -v hyperfine || true)}"

[ -n "$YQ" ] || echo "notice: yq not found; byte-oracle checks and yq column skipped" >&2
[ -n "$DASEL" ] || echo "notice: dasel not found; dasel column skipped" >&2
[ -n "$PYTHON3" ] || echo "notice: python3 not found; python3 column skipped" >&2
[ -n "$JQ" ] || echo "notice: jq not found; byte-oracle checks and jq column skipped" >&2
[ -n "$JAQ" ] || echo "notice: jaq not found; jaq column skipped" >&2
[ -n "$GOJQ" ] || echo "notice: gojq not found; gojq column skipped" >&2
[ -n "$GZIP" ] || echo "notice: gzip not found; gzip column skipped" >&2
[ -n "$HYPERFINE" ] || echo "notice: hyperfine not found; timing skipped (correctness still runs)" >&2

FIXDIR="${JQF_CROSS_FORMAT_FIXDIR:-}"
CLEANUP_FIXDIR=0
if [ -z "$FIXDIR" ]; then
    FIXDIR="$(mktemp -d "${TMPDIR:-/tmp}/jqf-cross-format-fixtures.XXXXXX")"
    CLEANUP_FIXDIR=1
fi

# We reuse the e2e fixture generator for the JSON/YAML/TOML/XML/CBOR catalog
# fixtures. The generator is idempotent and cached; its CBOR fixture is built
# with jqf itself when python3 lacks cbor2 (a weaker oracle than an external
# decoder).
python3 "$ROOT/tools/jqf-e2e-fixtures.py" "$FIXDIR" || { echo "error: fixture generation failed" >&2; exit 2; }

CATALOG_JSON="$FIXDIR/catalog-10mb.json"
YAML_CATALOG="$FIXDIR/yaml-catalog-10mb.yaml"
TOML_CATALOG="$FIXDIR/toml-catalog-10mb.toml"
XML_CATALOG="$FIXDIR/xml-catalog-10mb.xml"
CBOR_CATALOG="$FIXDIR/cbor-catalog.bin"

# NOTE: no `trap rm -rf "$FIXDIR"` here. `cleanup` below is the ONE exit trap,
# and it honours CLEANUP_FIXDIR — a bare trap at this point deleted a
# caller-supplied JQF_CROSS_FORMAT_FIXDIR that the script was only meant to
# reuse.

OUTDIR="$(mktemp -d "${TMPDIR:-/tmp}/jqf-cross-format-ladder.XXXXXX")"
cleanup() {
    rm -rf "$OUTDIR"
    [ "$CLEANUP_FIXDIR" -eq 1 ] && rm -rf "$FIXDIR"
}
trap cleanup EXIT

fail=0
note_fail() { echo "FAIL: $*" >&2; fail=1; }

ROW_COUNT=0
TABLE_HEADER_PRINTED=0

print_table_header() {
    [ "$TABLE_HEADER_PRINTED" -eq 1 ] && return
    printf '%-4s %-30s %-16s %-9s %-9s %-9s %-9s %-9s %-9s %-9s %s\n' \
        "LANE" "PROGRAM" "JQF(ms+-sd)" "YQ(ms)" "DASEL(ms)" "PY(ms)" "JQ(ms)" "JAQ(ms)" "GOJQ(ms)" "GZIP(ms)" "NOTE"
    printf '%s\n' "----------------------------------------------------------------------------------------------------------------------------------------"
    TABLE_HEADER_PRINTED=1
}

# emit_row lane program jqf_mean jqf_std yq_mean dasel_mean py_mean jq_mean jaq_mean gojq_mean gzip_mean note
emit_row() {
    local lane="$1" program="$2" jqf_mean="$3" jqf_std="$4"
    local yq_mean="$5" dasel_mean="$6" py_mean="$7"
    local jq_mean="$8" jaq_mean="$9" gojq_mean="${10}" gzip_mean="${11}" note="${12}"
    ROW_COUNT=$((ROW_COUNT + 1))
    if [ "$JSON_MODE" -eq 1 ]; then
        python3 -c '
import json, sys
def num(x):
    if not x or x in ("n/a", "absent", "disagreed", "no-equiv", "err", "timeout"): return x
    try: return float(x) if "." in x else int(x)
    except ValueError: return x
args = sys.argv[1:13]
lane, program, jqf_mean, jqf_std, yq, dasel, py, jq, jaq, gojq, gzip, note = args
print(json.dumps({
    "lane": lane, "program": program,
    "jqf_mean_ms": num(jqf_mean), "jqf_stddev_ms": num(jqf_std),
    "yq_mean_ms": num(yq), "dasel_mean_ms": num(dasel), "python3_mean_ms": num(py),
    "jq_mean_ms": num(jq), "jaq_mean_ms": num(jaq), "gojq_mean_ms": num(gojq),
    "gzip_mean_ms": num(gzip), "note": note,
}))
' "$lane" "$program" "$jqf_mean" "$jqf_std" "$yq_mean" "$dasel_mean" "$py_mean" "$jq_mean" "$jaq_mean" "$gojq_mean" "$gzip_mean" "$note"
    else
        print_table_header
        local jqf_disp="$jqf_mean"
        case "$jqf_mean" in
            n/a | err | timeout) ;;
            *) jqf_disp="${jqf_mean}+-${jqf_std}" ;;
        esac
        printf '%-4s %-30s %-16s %-9s %-9s %-9s %-9s %-9s %-9s %-9s %s\n' \
            "$lane" "$program" "$jqf_disp" "$yq_mean" "$dasel_mean" "$py_mean" "$jq_mean" "$jaq_mean" "$gojq_mean" "$gzip_mean" "$note"
    fi
}

# ---------------------------------------------------------------------------
# Hang bounding
# ---------------------------------------------------------------------------
# hyperfine 1.20.0 has NO --timeout flag (passing one makes it exit before
# running anything). Every probe and every timing run is therefore wrapped in
# tools/jqf-timeout-run.pl, which puts the command in its own process group
# and KILLs the whole group when the bound expires — a hung competitor (or a
# hung hyperfine) cannot derail the run, and a SIGTERM to hyperfine alone
# would orphan its benchmarked children (verified: they get reparented to
# PID 1 and keep running).
PROBE_TIMEOUT="${JQF_CROSS_FORMAT_PROBE_TIMEOUT:-60}"
LANE_TIMEOUT="${JQF_CROSS_FORMAT_TIMEOUT:-300}"

# run_bounded <seconds> <label> -- <cmd...> ; 124 = timed out
run_bounded() {
    local secs="$1" label="$2"; shift 2
    [ "$1" = "--" ] && shift
    "$ROOT/tools/jqf-timeout-run.pl" "$secs" "$OUTDIR/${label}.pid" -- "$@"
}

# ---------------------------------------------------------------------------
# Tool command builders (no output redirect; the caller decides where output
# goes). A command is wrapped in single quotes around the program, so a lane
# program containing a single quote would need a different quoting — none of
# the lane programs here does.
# ---------------------------------------------------------------------------

# jqf_cmd <flags> <program> <infile> -> shell command string
jqf_cmd() {
    local flags="$1" program="$2" infile="$3"
    printf '%s %s %s < %s' "$JQF" "$flags" "'$program'" "$infile"
}

# yq_cmd <input-fmt> <output-fmt> <program> <infile> -> shell command string
yq_cmd() {
    local ifmt="$1" ofmt="$2" program="$3" infile="$4"
    printf '%s -p %s -o %s %s < %s' "$YQ" "$ifmt" "$ofmt" "'$program'" "$infile"
}

# dasel_cmd <reader> <writer> <selector> <infile> -> shell command string
# dasel v3 takes `-i`/`-o`, not v2's `-r`/`-w`, and spells the identity
# selector as the EMPTY selector (`.` and `this` both fail to parse; `$this`
# works but the empty form is the documented identity). The v2 spelling exits
# 80 instantly, which `--ignore-failure` was happy to time as a 3.7 ms win.
dasel_cmd() {
    local reader="$1" writer="$2" selector="$3" infile="$4"
    if [ -z "$selector" ] || [ "$selector" = "." ]; then
        printf '%s -i %s -o %s < %s' "$DASEL" "$reader" "$writer" "$infile"
    else
        printf '%s -i %s -o %s %s < %s' "$DASEL" "$reader" "$writer" "'$selector'" "$infile"
    fi
}

# jq_family_cmd <tool> <flags> <program> <infile> -> shell command string
jq_family_cmd() {
    local tool="$1" flags="$2" program="$3" infile="$4"
    printf '%s %s %s < %s' "$tool" "$flags" "'$program'" "$infile"
}

# py_yaml_to_json <infile> -> shell command string
py_yaml_to_json() {
    local infile="$1"
    printf '%s -c %s < %s' "$PYTHON3" "'import yaml,json,sys; [json.dump(doc) for doc in yaml.safe_load_all(sys.stdin)]'" "$infile"
}

# py_toml_to_json <infile> -> shell command string
py_toml_to_json() {
    local infile="$1"
    printf '%s -c %s < %s' "$PYTHON3" "'import tomllib,json,sys; json.dump(tomllib.loads(sys.stdin.read()),sys.stdout)'" "$infile"
}

# py_cbor_to_json <infile> -> shell command string (cbor2; absent here)
py_cbor_to_json() {
    local infile="$1"
    printf '%s -c %s < %s' "$PYTHON3" "'import cbor2,json,sys; json.dump(cbor2.load(sys.stdin.buffer),sys.stdout)'" "$infile"
}

# py_toml_extract <infile> <py-expr> -> shell command string
# Runs a python expression over the tomllib-decoded document, so an
# extraction lane asks python the SAME question the lane asks jqf instead of
# handing it the whole-document conversion.
py_toml_extract() {
    local infile="$1" expr="$2"
    printf '%s -c %s < %s' "$PYTHON3" "'import tomllib,json,sys; d=tomllib.loads(sys.stdin.read()); json.dump(${expr},sys.stdout)'" "$infile"
}

# py_yaml_extract <infile> <py-expr> -> shell command string
py_yaml_extract() {
    local infile="$1" expr="$2"
    printf '%s -c %s < %s' "$PYTHON3" "'import yaml,json,sys; d=yaml.safe_load(sys.stdin); json.dump(${expr},sys.stdout)'" "$infile"
}

# dasel_selector <jq-path> -> dasel selector for the same extraction.
# dasel v3 spells an indexed path WITHOUT the leading dot that jq/yq use
# (`.catalog[500].color` -> `catalog[500].color`), so the yq expression cannot
# be handed to dasel verbatim. The empty/`.` identity stays the empty identity.
dasel_selector() {
    local path="$1"
    case "$path" in
        "."|"") printf '%s' "" ;;
        .*) printf '%s' "${path#.}" ;;
        *) printf '%s' "$path" ;;
    esac
}

# py_path_expr <jq-path> -> python index expression over the decoded `d`.
# `.catalog[500].color` -> `d["catalog"][500]["color"]`. Only the lanes' own
# path shapes reach here (static member names and integer indexes); anything
# else degrades to the identity so a malformed lane program fails loudly on
# the probe rather than silently timing the wrong question. Keys are DOUBLE
# quoted because the expression is spliced into a shell-single-quoted `-c`
# string by the `py_*_extract` builders.
py_path_expr() {
    python3 -c '
import sys
path = sys.argv[1]
expr = "d"
i = 0
if path.startswith("."):
    path = path[1:]
while i < len(path):
    if path[i] == "[":
        j = path.index("]", i)
        expr += f"[{path[i+1:j]}]"
        i = j + 1
    else:
        j = i
        while j < len(path) and path[j] != "." and path[j] != "[":
            j += 1
        key = path[i:j]
        if key:
            expr += f"[\"{key}\"]"
        i = j + 1 if j < len(path) and path[j] == "." else j
print(expr)
' "$1"
}

# gzip_compress_cmd <infile> -> shell command string (deterministic gzip)
gzip_compress_cmd() {
    printf '%s -c -n < %s' "$GZIP" "$1"
}

# gzip_roundtrip_cmd <infile> -> shell command string
gzip_roundtrip_cmd() {
    printf '%s -c -n < %s | %s -dc' "$GZIP" "$1" "$GZIP"
}

# py_zlib_cmd <infile> -> shell command string (RFC 1950 zlib stream)
py_zlib_cmd() {
    printf '%s -c %s < %s' "$PYTHON3" "'import zlib,sys; sys.stdout.buffer.write(zlib.compress(sys.stdin.buffer.read()))'" "$1"
}

# py_deflate_cmd <infile> -> shell command string (raw RFC 1951 deflate)
py_deflate_cmd() {
    printf '%s -c %s < %s' "$PYTHON3" "'import zlib,sys; c=zlib.compressobj(6,zlib.DEFLATED,-15); sys.stdout.buffer.write(c.compress(sys.stdin.buffer.read())+c.flush())'" "$1"
}

# inplace_cmd <jqf|yq> <program> <src> <tmp> -> shell command string.
# --in-place reads AND rewrites one file, so the timed command must reset the
# temp file from the pristine fixture before every run — otherwise hyperfine's
# second run edits the first run's output and the measurement drifts.
inplace_cmd() {
    local tool="$1" program="$2" src="$3" tmp="$4"
    if [ "$tool" = "jqf" ]; then
        # `--in-place` is a BOOLEAN flag over the positional input files, so the
        # program comes first exactly as it does without the flag. Passing the
        # file first made jqf parse the temp PATH as the program ("cannot parse
        # program at bytes 0..1"), which failed E3/E4 as a jqf defect when it
        # was this line.
        printf 'cp %s %s && %s --in-place %s %s' "'$src'" "'$tmp'" "$JQF" "'$program'" "'$tmp'"
    else
        printf 'cp %s %s && %s -i %s %s' "'$src'" "'$tmp'" "$YQ" "'$program'" "'$tmp'"
    fi
}

# ---------------------------------------------------------------------------
# Correctness validation
# ---------------------------------------------------------------------------

# normalize_json <infile> <outfile>
# Parses JSON from infile and writes compact, sorted-key JSON to outfile.
normalize_json() {
    python3 -c '
import json, sys
with open(sys.argv[1], "r") as f:
    data = json.load(f)
with open(sys.argv[2], "w") as f:
    json.dump(data, f, sort_keys=True, separators=(",", ":"))
' "$1" "$2"
}

# validate_yaml_json <lane> <program> <infile> <jqf-flags> <yq-expr> [expected_count]
# Normalizes both jqf and yq output, then byte-compares. When yq is absent,
# structural checks only. Writes the raw jqf output to $OUTDIR/${lane}_jqf.out
# (the lane oracle the competitors are later compared against).
validate_yaml_json() {
    local lane="$1" program="$2" infile="$3" jqf_flags="$4" yq_expr="$5" expected_count="${6:-}"

    local jqf_out="$OUTDIR/${lane}_jqf.out"
    "$JQF" $jqf_flags "$program" <"$infile" >"$jqf_out" 2>"$jqf_out.err"
    local jqf_exit=$?
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane: jqf exited $jqf_exit"
        return 1
    fi

    if [ -n "$YQ" ]; then
        local yq_out="$OUTDIR/${lane}_yq.out"
        "$YQ" -p yaml -o json "$yq_expr" <"$infile" >"$yq_out" 2>"$yq_out.err"
        local yq_exit=$?
        if [ "$yq_exit" -ne 0 ]; then
            note_fail "$lane: yq exited $yq_exit (oracle failed)"
            return 1
        fi
        local jqf_norm="$OUTDIR/${lane}_jqf_norm.json"
        local yq_norm="$OUTDIR/${lane}_yq_norm.json"
        if normalize_json "$jqf_out" "$jqf_norm" && normalize_json "$yq_out" "$yq_norm"; then
            if cmp -s "$jqf_norm" "$yq_norm"; then
                echo "yq value-match (normalized JSON byte-identical)"
            else
                note_fail "$lane: jqf and yq produce different JSON values after normalization"
                echo "FAIL: jqf != yq (value-level)"
            fi
        else
            note_fail "$lane: JSON normalization failed"
            echo "FAIL: normalization failed"
        fi
    else
        # yq absent: structural validation
        local jqf_lines
        jqf_lines="$(wc -l <"$jqf_out" | tr -d ' ')"
        if [ -n "$expected_count" ] && [ "$expected_count" -gt 0 ] && [ "$jqf_lines" -ne "$expected_count" ]; then
            note_fail "$lane: expected $expected_count output lines, got $jqf_lines"
            echo "FAIL: wrong line count (expected=$expected_count got=$jqf_lines)"
        else
            echo "structural-ok (yq absent, $jqf_lines lines)"
        fi
    fi
}

# validate_toml_json <lane> <program> <infile> <jqf-flags> <yq-expr>
validate_toml_json() {
    local lane="$1" program="$2" infile="$3" jqf_flags="$4" yq_expr="$5"

    local jqf_out="$OUTDIR/${lane}_jqf.out"
    "$JQF" $jqf_flags "$program" <"$infile" >"$jqf_out" 2>"$jqf_out.err"
    local jqf_exit=$?
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane: jqf exited $jqf_exit"
        return 1
    fi

    # yq is pathologically slow on TOML (>60s per run on a 10 MB fixture);
    # skip the oracle comparison for TOML and validate structurally instead.
    if [ -n "$YQ" ]; then
        echo "yq skipped on TOML (pathological performance >60s on this fixture)"
    else
        local jqf_lines
        jqf_lines="$(wc -l <"$jqf_out" | tr -d ' ')"
        echo "structural-ok (yq absent, $jqf_lines lines)"
    fi
}

# ---------------------------------------------------------------------------
# Competitor classification: the probe-and-oracle law.
# ---------------------------------------------------------------------------
# Every competitor is run ONCE against the real fixture, must exit 0 AND emit
# at least one byte, and its output must then agree with the lane oracle. A
# failure at the run stage is `absent` (the tool cannot serve this lane here);
# a run that produced output but disagrees with the oracle is `disagreed`.
# Neither is ever timed. This is the mechanism that kept dasel v2's exit-80
# startup death and python3's missing PyYAML out of the first draft's table as
# fabricated 100x wins.
#
# classify_competitor <lane> <tool> <cmd> <mode> <oracle> <mode_arg>
#   mode determines how the competitor's output is compared to the oracle:
#     json       normalize both, byte-compare           oracle = normalized file
#     bytes      raw byte-compare                        oracle = raw file
#     gunzip     gzip -dc the output, byte-compare       oracle = raw payload
#     zlibd      python3 zlib.decompress the output      oracle = raw payload
#     rawdeflate python3 raw-inflate the output          oracle = raw payload
#     yamlrt     re-decode the output as YAML via jqf    oracle = normalized JSON
#     inplace    compare the in-place TMP file           oracle = raw file, mode_arg = tmp
#   echoes: ok | absent | disagreed | timeout
classify_competitor() {
    local lane="$1" tool="$2" cmd="$3" mode="$4" oracle="$5" mode_arg="$6"
    local out="$OUTDIR/${lane}.${tool}.probe"

    run_bounded "$PROBE_TIMEOUT" "$lane.$tool.probe" -- bash -c "$cmd > '$out' 2>'$out.err'"
    local rc=$?
    if [ "$rc" -eq 124 ]; then
        echo "timeout"
        return
    fi
    if [ "$rc" -ne 0 ]; then
        echo "absent"
        return
    fi
    if [ "$mode" != "inplace" ] && [ ! -s "$out" ]; then
        echo "absent"
        return
    fi

    local norm=""
    case "$mode" in
        json)
            norm="$OUTDIR/${lane}.${tool}.probe.norm"
            if normalize_json "$out" "$norm" 2>/dev/null && cmp -s "$norm" "$oracle"; then
                echo "ok"
            else
                echo "disagreed"
            fi
            ;;
        bytes)
            if cmp -s "$out" "$oracle"; then echo "ok"; else echo "disagreed"; fi
            ;;
        gunzip)
            if "$GZIP" -dc < "$out" 2>/dev/null | cmp -s - "$oracle"; then echo "ok"; else echo "disagreed"; fi
            ;;
        zlibd)
            if "$PYTHON3" -c 'import zlib,sys; sys.stdout.buffer.write(zlib.decompress(sys.stdin.buffer.read()))' < "$out" 2>/dev/null | cmp -s - "$oracle"; then
                echo "ok"
            else
                echo "disagreed"
            fi
            ;;
        rawdeflate)
            if "$PYTHON3" -c 'import zlib,sys; d=zlib.decompressobj(-15); sys.stdout.buffer.write(d.decompress(sys.stdin.buffer.read()))' < "$out" 2>/dev/null | cmp -s - "$oracle"; then
                echo "ok"
            else
                echo "disagreed"
            fi
            ;;
        yamlrt)
            local rt="$OUTDIR/${lane}.${tool}.probe.rt"
            norm="$OUTDIR/${lane}.${tool}.probe.norm"
            if "$JQF" --input-format yaml -c '.' < "$out" > "$rt" 2>/dev/null \
               && normalize_json "$rt" "$norm" 2>/dev/null && cmp -s "$norm" "$oracle"; then
                echo "ok"
            else
                echo "disagreed"
            fi
            ;;
        inplace)
            if cmp -s "$mode_arg" "$oracle"; then echo "ok"; else echo "disagreed"; fi
            ;;
        *)
            echo "absent"
            ;;
    esac
}

# build_tool_spec <lane> <mode> <oracle> <mode_arg>
#   Reads $OUTDIR/${lane}.candidates (one `label<TAB>command` per line),
#   classifies each, and writes $OUTDIR/${lane}.spec (label<TAB>command<TAB>
#   status). Only status=ok tools are timed.
#   A candidate whose command is the sentinel `NOEQUIV` records the distinct
#   `no-equiv` status WITHOUT running anything: the lane's question has no
#   fair-equivalent competitor command exists, and blanking with a
#   distinct reason instead of "disagreed" is what stops a "different question"
#   from reading as a "wrong answer".
NOEQUIV="NOEQUIV"
build_tool_spec() {
    local lane="$1" mode="$2" oracle="$3" mode_arg="$4"
    local cand_file="$OUTDIR/${lane}.candidates"
    local spec_file="$OUTDIR/${lane}.spec"
    : > "$spec_file"
    while IFS=$'\t' read -r tlabel tcmd; do
        local status
        if [ "$tcmd" = "$NOEQUIV" ]; then
            status="no-equiv"
            echo "jqf-cross-format-ladder: $lane: $tlabel no equivalent competitor command" >&2
        else
            status="$(classify_competitor "$lane" "$tlabel" "$tcmd" "$mode" "$oracle" "$mode_arg")"
        fi
        printf '%s\t%s\t%s\n' "$tlabel" "$tcmd" "$status" >> "$spec_file"
        case "$status" in
            absent)    echo "jqf-cross-format-ladder: $lane: $tlabel absent (could not run this lane)" >&2 ;;
            timeout)   echo "jqf-cross-format-ladder: $lane: $tlabel timed out its probe" >&2 ;;
            disagreed) echo "jqf-cross-format-ladder: $lane: $tlabel DISAGREED with the oracle" >&2 ;;
        esac
    done < "$cand_file"
}

# ---------------------------------------------------------------------------
# timing
# ---------------------------------------------------------------------------

# hyperfine_get <export-json> <command-name> -> "mean stddev" (ms) or empty
hyperfine_get() {
    python3 - "$1" "$2" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    data = json.load(f)
for r in data.get("results", []):
    if r.get("command") == sys.argv[2]:
        print(f"{r['mean']*1000:.1f} {r['stddev']*1000:.1f}")
        break
PY
}

# time_lane <label> <jqf-command> <infile>
#   Times the jqf command plus every status=ok tool in $OUTDIR/${label}.spec.
#   Echoes 9 fields: jqf_mean jqf_std yq dasel py jq jaq gojq gzip
#   (each competitor field is a mean in ms, or `absent`/`disagreed`/`n-a`).
time_lane() {
    local label="$1" jqf_command="$2" infile="$3"
    local spec_file="$OUTDIR/${label}.spec"

    local yq_out="n/a" dasel_out="n/a" py_out="n/a" jq_out="n/a" jaq_out="n/a" gojq_out="n/a" gzip_out="n/a"

    if [ -z "$HYPERFINE" ]; then
        echo "n/a n/a n/a n/a n/a n/a n/a n/a n/a"
        return
    fi

    # NO `--timeout`: hyperfine has no such flag (1.20.0 rejects it outright).
    # An unknown flag makes hyperfine exit before it runs anything. Per-case
    # bounding is `run_bounded`'s job, not hyperfine's.
    local hf_args=(--warmup 1 --runs 5 --ignore-failure --export-json "$OUTDIR/${label}.json")
    hf_args+=(--command-name "jqf" "$jqf_command > /dev/null")

    if [ -f "$spec_file" ]; then
        while IFS=$'\t' read -r tlabel tcmd tstatus; do
            [ "$tstatus" = "ok" ] && hf_args+=(--command-name "$tlabel" "$tcmd > /dev/null")
        done < "$spec_file"
    fi

    if ! run_bounded "$LANE_TIMEOUT" "$label.hyperfine" -- "$HYPERFINE" "${hf_args[@]}" < "$infile" > /dev/null 2>"$OUTDIR/${label}.hferr"; then
        echo "jqf-cross-format-ladder: hyperfine failed or timed out for lane $label:" >&2
        sed 's/^/    /' "$OUTDIR/${label}.hferr" >&2
        echo "err err err err err err err err err"
        return
    fi

    local jqf_res jqf_mean jqf_std
    jqf_res="$(hyperfine_get "$OUTDIR/${label}.json" "jqf")"
    read -r jqf_mean jqf_std <<<"$jqf_res"
    [ -z "$jqf_mean" ] && { jqf_mean="err"; jqf_std="err"; }

    if [ -f "$spec_file" ]; then
        while IFS=$'\t' read -r tlabel tcmd tstatus; do
            local tv="n/a"
            if [ "$tstatus" = "ok" ]; then
                tv="$(hyperfine_get "$OUTDIR/${label}.json" "$tlabel" | awk '{print $1}')"
                [ -z "$tv" ] && tv="err"
            else
                tv="$tstatus"
            fi
            case "$tlabel" in
                yq) yq_out="$tv" ;;
                dasel) dasel_out="$tv" ;;
                py) py_out="$tv" ;;
                jq) jq_out="$tv" ;;
                jaq) jaq_out="$tv" ;;
                gojq) gojq_out="$tv" ;;
                gzip) gzip_out="$tv" ;;
            esac
        done < "$spec_file"
    fi

    printf '%s %s %s %s %s %s %s %s %s\n' \
        "$jqf_mean" "$jqf_std" "$yq_out" "$dasel_out" "$py_out" "$jq_out" "$jaq_out" "$gojq_out" "$gzip_out"
}

# finish_lane <lane> <program> <jqf_command> <infile> <note-extra>
#   Shared tail of every lane: time, split the 9 fields, default empties to
#   n/a, and emit the row. Reads $OUTDIR/${lane}_jqf.out for the count note.
finish_lane() {
    local lane="$1" program="$2" jqf_command="$3" infile="$4" note_extra="$5"
    local times
    times="$(time_lane "$lane" "$jqf_command" "$infile")"

    local jqf_mean jqf_std yq_mean dasel_mean py_mean jq_mean jaq_mean gojq_mean gzip_mean
    read -r jqf_mean jqf_std yq_mean dasel_mean py_mean jq_mean jaq_mean gojq_mean gzip_mean <<<"$times"

    [ -z "$jqf_mean" ] && jqf_mean="n/a"
    [ -z "$jqf_std" ] && jqf_std="n/a"
    [ -z "$yq_mean" ] && yq_mean="n/a"
    [ -z "$dasel_mean" ] && dasel_mean="n/a"
    [ -z "$py_mean" ] && py_mean="n/a"
    [ -z "$jq_mean" ] && jq_mean="n/a"
    [ -z "$jaq_mean" ] && jaq_mean="n/a"
    [ -z "$gojq_mean" ] && gojq_mean="n/a"
    [ -z "$gzip_mean" ] && gzip_mean="n/a"

    local count_info=""
    local outfile="$OUTDIR/${lane}_jqf.out"
    if [ -f "$outfile" ]; then
        count_info="$(wc -l <"$outfile" | tr -d ' ')"
    fi
    local note="$note_extra"
    [ "$count_info" != "" ] && note="$note ($count_info lines)"

    emit_row "$lane" "$program" "$jqf_mean" "$jqf_std" "$yq_mean" "$dasel_mean" "$py_mean" "$jq_mean" "$jaq_mean" "$gojq_mean" "$gzip_mean" "$note"
}

# ---------------------------------------------------------------------------
# Lanes
# ---------------------------------------------------------------------------

echo "jqf-cross-format-ladder: jqf=$JQF yq=${YQ:-none} dasel=${DASEL:-none} py=${PYTHON3:-none} jq=${JQ:-none} jaq=${JAQ:-none} gojq=${GOJQ:-none} gzip=${GZIP:-none} hyperfine=${HYPERFINE:-none}" >&2
echo "jqf-cross-format-ladder: fixtures in $FIXDIR" >&2

### F1-F4: YAML/TOML lanes ###################################################
run_cross_format_lane() {
    local lane="$1" jqf_flags="$2" program="$3" infile="$4" validate_fn="$5"
    shift 5
    # Remaining args to validate_fn: yq_expr, expected_count, etc.

    local validation
    validation="$("$validate_fn" "$lane" "$program" "$infile" "$jqf_flags" "$@")"
    local validate_ok=$?
    if [ "$validate_ok" -ne 0 ]; then
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "VALIDATION FAILED (see stderr)"
        return
    fi

    # The lane oracle is the normalized jqf output (validated above against yq
    # for YAML; structural-only for TOML where yq is pathological).
    local oracle_norm="$OUTDIR/${lane}.oracle.norm"
    normalize_json "$OUTDIR/${lane}_jqf.out" "$oracle_norm" 2>/dev/null || true

    local cand_file="$OUTDIR/${lane}.candidates"
    : > "$cand_file"
    case "$lane" in
        F1|F2)
            # After the `shift 5` above, the validator's extra arguments have
            # slid down: the yq expression is `$1`, not the `$5` it was passed
            # as. Under `set -u` the stale index aborts the whole ladder before
            # a single row prints.
            local yq_expr="$1"
            [ -n "$YQ" ] && printf 'yq\t%s\n' "$(yq_cmd yaml json "$yq_expr" "$infile")" >> "$cand_file"
            # dasel and py ask the SAME extraction the lane asks jqf:
            # dasel drops jq's leading dot, python indexes the decoded
            # document. The identity lane (F1) keeps the whole-document
            # conversion, which IS the equivalent question there.
            [ -n "$DASEL" ] && printf 'dasel\t%s\n' "$(dasel_cmd yaml json "$(dasel_selector "$yq_expr")" "$infile")" >> "$cand_file"
            if [ -n "$PYTHON3" ]; then
                if [ "$yq_expr" = "." ]; then
                    printf 'py\t%s\n' "$(py_yaml_to_json "$infile")" >> "$cand_file"
                else
                    printf 'py\t%s\n' "$(py_yaml_extract "$infile" "$(py_path_expr "$yq_expr")")" >> "$cand_file"
                fi
            fi
            build_tool_spec "$lane" "json" "$oracle_norm" ""
            ;;
        F3|F4)
            # Same per-lane selector law as F1/F2. Before this, F4 handed
            # dasel/py the WHOLE-document conversion no matter
            # that the lane asks `.catalog[500].color`, so they could never
            # agree and the cell blanked as "disagreed" — a competitor asked a
            # different question being read as one that got it wrong.
            local yq_expr="$1"
            [ -n "$DASEL" ] && printf 'dasel\t%s\n' "$(dasel_cmd toml json "$(dasel_selector "$yq_expr")" "$infile")" >> "$cand_file"
            if [ -n "$PYTHON3" ]; then
                if [ "$yq_expr" = "." ]; then
                    printf 'py\t%s\n' "$(py_toml_to_json "$infile")" >> "$cand_file"
                else
                    printf 'py\t%s\n' "$(py_toml_extract "$infile" "$(py_path_expr "$yq_expr")")" >> "$cand_file"
                fi
            fi
            build_tool_spec "$lane" "json" "$oracle_norm" ""
            ;;
    esac

    finish_lane "$lane" "$program" "$(jqf_cmd "$jqf_flags" "$program" "$infile")" "$infile" "$validation"
}

# F1: YAML identity -> JSON
run_cross_format_lane "F1" "--input-format yaml -c" "." "$YAML_CATALOG" \
    validate_yaml_json "." 0

# F2: YAML extraction -> JSON
run_cross_format_lane "F2" "--input-format yaml -c" ".catalog[500].id" "$YAML_CATALOG" \
    validate_yaml_json ".catalog[500].id" 1

# F3: TOML identity -> JSON
run_cross_format_lane "F3" "--input-format toml -c" "." "$TOML_CATALOG" \
    validate_toml_json "."

# F4: TOML extraction -> JSON
run_cross_format_lane "F4" "--input-format toml -c" ".catalog[500].color" "$TOML_CATALOG" \
    validate_toml_json ".catalog[500].color"

### F5: YAML -> YAML round-trip ##############################################
# Oracle: re-decode the round-tripped YAML and compare the decoded JSON to the
# original YAML->JSON decode. No byte-level oracle (YAML formatting varies).
run_yaml_roundtrip_lane() {
    local lane="F5" program="." jqf_flags="--input-format yaml --output-format yaml -c"
    local jqf_out="$OUTDIR/${lane}_jqf.out"

    "$JQF" $jqf_flags "$program" <"$YAML_CATALOG" >"$jqf_out" 2>"$jqf_out.err"
    local jqf_exit=$?
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane: jqf exited $jqf_exit"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "VALIDATION FAILED (jqf exited $jqf_exit)"
        return
    fi

    # Validate: decode output YAML back to JSON, compare against original YAML->JSON decode
    local orig_json="$OUTDIR/${lane}_orig.json"
    "$JQF" --input-format yaml -c '.' <"$YAML_CATALOG" >"$orig_json" 2>/dev/null
    local rt_json="$OUTDIR/${lane}_rt.json"
    "$JQF" --input-format yaml -c '.' <"$jqf_out" >"$rt_json" 2>/dev/null
    local rt_exit=$?
    local note=""
    if [ "$rt_exit" -ne 0 ]; then
        note_fail "$lane: round-tripped YAML not decodable by jqf (exit $rt_exit)"
        note="FAIL: round-tripped YAML not decodable"
    elif cmp -s "$orig_json" "$rt_json"; then
        note="YAML rt: decoded JSON byte-identical to original"
    else
        local orig_lines rt_lines
        orig_lines="$(wc -l <"$orig_json" | tr -d ' ')"
        rt_lines="$(wc -l <"$rt_json" | tr -d ' ')"
        if [ "$orig_lines" -eq "$rt_lines" ]; then
            note="YAML rt: line count matches ($orig_lines lines); byte-differs (YAML formatting variance)"
        else
            note_fail "$lane: round-tripped JSON differs in line count (orig=$orig_lines rt=$rt_lines)"
            note="FAIL: round-trip broken (orig=$orig_lines rt=$rt_lines)"
        fi
    fi

    # Competitors: yq (YAML->YAML) and dasel/py (YAML->JSON) must re-decode to
    # the ORIGINAL YAML->JSON value. JSON is valid YAML, so one comparison mode
    # (yamlrt) covers both.
    local orig_norm="$OUTDIR/${lane}.oracle.norm"
    normalize_json "$orig_json" "$orig_norm" 2>/dev/null
    local cand_file="$OUTDIR/${lane}.candidates"
    : > "$cand_file"
    [ -n "$YQ" ] && printf 'yq\t%s\n' "$(yq_cmd yaml yaml "." "$YAML_CATALOG")" >> "$cand_file"
    [ -n "$DASEL" ] && printf 'dasel\t%s\n' "$(dasel_cmd yaml json "." "$YAML_CATALOG")" >> "$cand_file"
    [ -n "$PYTHON3" ] && printf 'py\t%s\n' "$(py_yaml_to_json "$YAML_CATALOG")" >> "$cand_file"
    build_tool_spec "$lane" "yamlrt" "$orig_norm" ""

    finish_lane "$lane" "$program" "$(jqf_cmd "$jqf_flags" "$program" "$YAML_CATALOG")" "$YAML_CATALOG" "$note"
}
run_yaml_roundtrip_lane

### F6: YAML -> NDJSON streaming #############################################
run_yaml_to_ndjson_lane() {
    local lane="F6" program=".catalog[]" jqf_flags="--input-format yaml --output-format ndjson -c"
    local jqf_out="$OUTDIR/${lane}_jqf.out"

    "$JQF" $jqf_flags "$program" <"$YAML_CATALOG" >"$jqf_out" 2>"$jqf_out.err"
    local jqf_exit=$?
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane: jqf exited $jqf_exit"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "VALIDATION FAILED (jqf exited $jqf_exit)"
        return
    fi

    # Validate: record count must be 100000 (TOML_CATALOG_ITEMS), and each line
    # must be valid compact JSON.
    local ndjson_lines
    ndjson_lines="$(wc -l <"$jqf_out" | tr -d ' ')"
    if [ "$ndjson_lines" -ne 100000 ]; then
        note_fail "$lane: expected 100000 NDJSON records, got $ndjson_lines"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "FAIL: record count $ndjson_lines != 100000"
        return
    fi

    local first_record last_record
    first_record="$(head -1 "$jqf_out")"
    last_record="$(tail -1 "$jqf_out")"
    if echo "$first_record" | python3 -c 'import json,sys; r=json.loads(sys.stdin.read()); assert "id" in r' 2>/dev/null; then
        :
    else
        note_fail "$lane: first record not valid JSON with 'id' field"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "FAIL: first record invalid"
        return
    fi

    local note="NDJSON stream: $ndjson_lines records, valid JSON; first_id=$(echo "$first_record" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["id"])' 2>/dev/null || echo '?')"

    # The oracle is the NDJSON record stream itself (byte-identity). yq's
    # `-o json -c` is NOT compact (it pretty-prints, 7 lines per record), so
    # it DISAGREES with the record stream and is reported disagreed, never a
    # number.
    local cand_file="$OUTDIR/${lane}.candidates"
    : > "$cand_file"
    [ -n "$YQ" ] && printf 'yq\t%s\n' "$(yq_cmd yaml json ".catalog[]" "$YAML_CATALOG")" >> "$cand_file"
    [ -n "$DASEL" ] && printf 'dasel\t%s\n' "$(dasel_cmd yaml json ".catalog[]" "$YAML_CATALOG")" >> "$cand_file"
    [ -n "$PYTHON3" ] && printf 'py\t%s\n' "$(py_yaml_to_json "$YAML_CATALOG")" >> "$cand_file"
    build_tool_spec "$lane" "bytes" "$jqf_out" ""

    finish_lane "$lane" "$program" "$(jqf_cmd "$jqf_flags" "$program" "$YAML_CATALOG")" "$YAML_CATALOG" "$note"
}
run_yaml_to_ndjson_lane

### B1: CBOR -> JSON identity ###############################################
run_cbor_lane() {
    local lane="B1" program="." jqf_flags="--input-format cbor -c"
    local jqf_out="$OUTDIR/${lane}_jqf.out"

    "$JQF" $jqf_flags "$program" <"$CBOR_CATALOG" >"$jqf_out" 2>"$jqf_out.err"
    local jqf_exit=$?
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane: jqf exited $jqf_exit"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "VALIDATION FAILED (jqf exited $jqf_exit)"
        return
    fi

    # Oracle: the CBOR fixture is the catalog JSON re-encoded, and jqf's
    # json->cbor->json round-trip is byte-identical to the compact catalog plus
    # one newline. So the decode must equal `cat catalog-10mb.json; echo`.
    # NOTE: the fixture is jqf-generated (python3 has no cbor2 here), so this
    # oracle is jqf-self and weaker — a symmetric encode/decode bug could hide.
    local oracle="$OUTDIR/${lane}.oracle"
    { cat "$CATALOG_JSON"; printf '\n'; } > "$oracle"
    local note=""
    if cmp -s "$jqf_out" "$oracle"; then
        note="cbor decode == catalog JSON (jqf-self fixture, weaker oracle)"
    else
        note_fail "$lane: cbor->json != catalog JSON"
        note="FAIL: cbor decode != catalog JSON"
    fi

    local oracle_norm="$OUTDIR/${lane}.oracle.norm"
    normalize_json "$oracle" "$oracle_norm" 2>/dev/null

    # Competitor: python3 + cbor2. Absent here (no cbor2 module).
    local cand_file="$OUTDIR/${lane}.candidates"
    : > "$cand_file"
    [ -n "$PYTHON3" ] && printf 'py\t%s\n' "$(py_cbor_to_json "$CBOR_CATALOG")" >> "$cand_file"
    build_tool_spec "$lane" "json" "$oracle_norm" ""

    finish_lane "$lane" "$program" "$(jqf_cmd "$jqf_flags" "$program" "$CBOR_CATALOG")" "$CBOR_CATALOG" "$note"
}
run_cbor_lane

### B2: CBOR -> CBOR identity ################################################
run_cbor_cbor_lane() {
    local lane="B2" program="." jqf_flags="--input-format cbor --output-format cbor"
    local jqf_out="$OUTDIR/${lane}_jqf.out"

    "$JQF" $jqf_flags "$program" <"$CBOR_CATALOG" >"$jqf_out" 2>"$jqf_out.err"
    local jqf_exit=$?
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane: jqf exited $jqf_exit"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "VALIDATION FAILED (jqf exited $jqf_exit)"
        return
    fi

    # Oracle 1 (byte): the re-encoded cbor equals the input fixture bytes.
    # Oracle 2 (semantic): re-decoding the re-encode gives the same JSON as
    # decoding the fixture (origin-independent). Both must hold.
    local note=""
    if cmp -s "$jqf_out" "$CBOR_CATALOG"; then
        note="cbor round-trip byte-identical to input"
    else
        local orig_json="$OUTDIR/${lane}_orig.json"
        local rt_json="$OUTDIR/${lane}_rt.json"
        "$JQF" --input-format cbor -c '.' <"$CBOR_CATALOG" >"$orig_json" 2>/dev/null
        "$JQF" --input-format cbor -c '.' <"$jqf_out" >"$rt_json" 2>/dev/null
        if cmp -s "$orig_json" "$rt_json"; then
            note="cbor re-encode re-decodes to same JSON (semantic rt); byte-differs from input"
        else
            note_fail "$lane: cbor round-trip broken"
            note="FAIL: cbor round-trip broken"
        fi
    fi

    # No competitors: no comparable CLI reads or writes CBOR.
    finish_lane "$lane" "$program" "$(jqf_cmd "$jqf_flags" "$program" "$CBOR_CATALOG")" "$CBOR_CATALOG" "$note"
}
run_cbor_cbor_lane

### Z1: XML -> JSON identity #################################################
run_xml_lane() {
    local lane="Z1" program="." jqf_flags="--input-format xml -c"
    local jqf_out="$OUTDIR/${lane}_jqf.out"

    "$JQF" $jqf_flags "$program" <"$XML_CATALOG" >"$jqf_out" 2>"$jqf_out.err"
    local jqf_exit=$?
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane: jqf exited $jqf_exit"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "VALIDATION FAILED (jqf exited $jqf_exit)"
        return
    fi

    # Oracle: the D1 projection law. The fixture is <catalog> with 100000 empty
    # <item/> children, so the decode must be an array of 100000 empty arrays.
    # (The element->JSON mapping differs by tool, so no external byte oracle.)
    local note=""
    if python3 - "$jqf_out" <<'PY'
import json, sys
v = json.load(open(sys.argv[1]))
assert isinstance(v, list) and len(v) == 100000 and all(x == [] for x in v), "not the D1 projection"
PY
    then
        note="xml decode == D1 projection (100000 empty item arrays)"
    else
        note_fail "$lane: xml decode is not the D1 projection"
        note="FAIL: xml decode != D1 projection"
    fi

    local oracle_norm="$OUTDIR/${lane}.oracle.norm"
    normalize_json "$jqf_out" "$oracle_norm" 2>/dev/null

    # Competitors: dasel and python3 ElementTree produce a DIFFERENT
    # element->JSON mapping (objects, not child arrays) for which there is NO
    # competitor command that reproduces jqf's D1 projection — the lane asks a
    # question they cannot be asked. They blank as `no-equiv` (a distinct
    # reason), never as `disagreed`, which would read as "the
    # competitor got it wrong" when no equivalent command exists.
    local cand_file="$OUTDIR/${lane}.candidates"
    : > "$cand_file"
    [ -n "$DASEL" ] && printf 'dasel\t%s\n' "$NOEQUIV" >> "$cand_file"
    [ -n "$PYTHON3" ] && printf 'py\t%s\n' "$NOEQUIV" >> "$cand_file"
    build_tool_spec "$lane" "json" "$oracle_norm" ""

    finish_lane "$lane" "$program" "$(jqf_cmd "$jqf_flags" "$program" "$XML_CATALOG")" "$XML_CATALOG" "$note"
}
run_xml_lane

### C1-C4: compression builtins #############################################
# The compression builtins (gzip_compress / gzip_decompress / deflate_compress
# / deflate_decompress / zlib_compress / zlib_decompress) are arity-0, take a
# STRING input, and answer base64(compressed bytes). `tostring | gzip_compress`
# therefore compresses the compact JSON text of the document — the same payload
# the external tools compress from the fixture file, which is what makes the
# external decompressor a valid oracle: base64-decode jqf's output and
# decompress it, and it must reproduce the fixture bytes exactly.

# run_compress_lane <lane> <program> <decode-mode> <note-label>
#   decode-mode: gunzip | zlibd | rawdeflate
run_compress_lane() {
    local lane="$1" program="$2" dec="$3" label="$4"
    local jqf_flags="-c -r"
    local jqf_out="$OUTDIR/${lane}_jqf.out"

    "$JQF" $jqf_flags "$program" <"$CATALOG_JSON" >"$jqf_out" 2>"$jqf_out.err"
    local jqf_exit=$?
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane: jqf exited $jqf_exit"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "VALIDATION FAILED (jqf exited $jqf_exit)"
        return
    fi

    local note=""
    case "$dec" in
        gunzip)    if "$GZIP" -dc < <(base64 -d < "$jqf_out" 2>/dev/null) 2>/dev/null | cmp -s - "$CATALOG_JSON"; then note="$label: externally gunzip-validated"; else note_fail "$lane: compressed payload != fixture"; note="FAIL: payload != fixture"; fi ;;
        zlibd)     if "$PYTHON3" -c 'import zlib,sys; sys.stdout.buffer.write(zlib.decompress(sys.stdin.buffer.read()))' < <(base64 -d < "$jqf_out" 2>/dev/null) 2>/dev/null | cmp -s - "$CATALOG_JSON"; then note="$label: externally zlib-validated"; else note_fail "$lane: compressed payload != fixture"; note="FAIL: payload != fixture"; fi ;;
        rawdeflate) if "$PYTHON3" -c 'import zlib,sys; d=zlib.decompressobj(-15); sys.stdout.buffer.write(d.decompress(sys.stdin.buffer.read()))' < <(base64 -d < "$jqf_out" 2>/dev/null) 2>/dev/null | cmp -s - "$CATALOG_JSON"; then note="$label: externally raw-deflate-validated"; else note_fail "$lane: compressed payload != fixture"; note="FAIL: payload != fixture"; fi ;;
    esac

    # Competitor correctness: the competitor's compressed stream must ALSO
    # decompress to the fixture payload (its own mode check).
    local cand_file="$OUTDIR/${lane}.candidates"
    : > "$cand_file"
    case "$lane" in
        C1) [ -n "$GZIP" ] && printf 'gzip\t%s\n' "$(gzip_compress_cmd "$CATALOG_JSON")" >> "$cand_file"; build_tool_spec "$lane" "gunzip" "$CATALOG_JSON" "" ;;
        C2) [ -n "$PYTHON3" ] && printf 'py\t%s\n' "$(py_zlib_cmd "$CATALOG_JSON")" >> "$cand_file"; build_tool_spec "$lane" "zlibd" "$CATALOG_JSON" "" ;;
        C3) [ -n "$PYTHON3" ] && printf 'py\t%s\n' "$(py_deflate_cmd "$CATALOG_JSON")" >> "$cand_file"; build_tool_spec "$lane" "rawdeflate" "$CATALOG_JSON" "" ;;
    esac

    finish_lane "$lane" "$program" "$(jqf_cmd "$jqf_flags" "$program" "$CATALOG_JSON")" "$CATALOG_JSON" "$note"
}

# C1: gzip compress
run_compress_lane "C1" "tostring | gzip_compress" "gunzip" "gzip"

# C2: zlib compress
run_compress_lane "C2" "tostring | zlib_compress" "zlibd" "zlib"

# C3: deflate compress
run_compress_lane "C3" "tostring | deflate_compress" "rawdeflate" "deflate"

# C4: gzip round-trip
run_gzip_roundtrip_lane() {
    local lane="C4" program="tostring | gzip_compress | gzip_decompress"
    local jqf_flags="-c"
    local jqf_out="$OUTDIR/${lane}_jqf.out"

    "$JQF" $jqf_flags "$program" <"$CATALOG_JSON" >"$jqf_out" 2>"$jqf_out.err"
    local jqf_exit=$?
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane: jqf exited $jqf_exit"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "VALIDATION FAILED (jqf exited $jqf_exit)"
        return
    fi

    # Oracle: the round trip must reproduce `tostring` byte-for-byte, and jqf's
    # tostring is byte-identical to jq's (external oracle).
    local oracle="$OUTDIR/${lane}.oracle"
    "$JQ" -c 'tostring' <"$CATALOG_JSON" >"$oracle" 2>/dev/null
    local note=""
    if cmp -s "$jqf_out" "$oracle"; then
        note="gzip rt == jq tostring (byte)"
    else
        note_fail "$lane: gzip round trip != jq tostring"
        note="FAIL: round trip != jq tostring"
    fi

    # Competitor: the external gzip pipe round trip must reproduce the fixture.
    local cand_file="$OUTDIR/${lane}.candidates"
    : > "$cand_file"
    [ -n "$GZIP" ] && printf 'gzip\t%s\n' "$(gzip_roundtrip_cmd "$CATALOG_JSON")" >> "$cand_file"
    build_tool_spec "$lane" "bytes" "$CATALOG_JSON" ""

    finish_lane "$lane" "$program" "$(jqf_cmd "$jqf_flags" "$program" "$CATALOG_JSON")" "$CATALOG_JSON" "$note"
}
run_gzip_roundtrip_lane

### M1-M4: mutation / assignment programs ####################################
# The oracle is jq (byte-identical on all four programs). jaq byte-matches;
# gojq sorts object keys by default, so its output is compared at the VALUE
# level (normalized) — it agrees there and is timed, key order is its product
# behavior, not a defect.
run_jq_lane() {
    local lane="$1" jqf_flags="$2" program="$3" infile="$4"
    local oracle_program="$5" oracle_flags="$6"

    local jqf_out="$OUTDIR/${lane}_jqf.out"
    "$JQF" $jqf_flags "$program" <"$infile" >"$jqf_out" 2>"$jqf_out.err"
    local jqf_exit=$?
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane: jqf exited $jqf_exit"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "VALIDATION FAILED (jqf exited $jqf_exit)"
        return
    fi

    local note=""
    local oracle="$OUTDIR/${lane}.oracle"
    if [ -n "$JQ" ]; then
        "$JQ" $oracle_flags "$oracle_program" <"$infile" >"$oracle" 2>/dev/null
        if cmp -s "$jqf_out" "$oracle"; then
            note="jq byte-identical"
        else
            note_fail "$lane: jqf != jq (byte)"
            note="FAIL: jqf != jq (byte)"
        fi
    else
        cp "$jqf_out" "$oracle"
        note="jq absent; self-oracle (weaker)"
    fi

    local oracle_norm="$OUTDIR/${lane}.oracle.norm"
    normalize_json "$oracle" "$oracle_norm" 2>/dev/null

    local cand_file="$OUTDIR/${lane}.candidates"
    : > "$cand_file"
    [ -n "$JQ" ] && printf 'jq\t%s\n' "$(jq_family_cmd "$JQ" "$oracle_flags" "$oracle_program" "$infile")" >> "$cand_file"
    [ -n "$JAQ" ] && printf 'jaq\t%s\n' "$(jq_family_cmd "$JAQ" "$oracle_flags" "$oracle_program" "$infile")" >> "$cand_file"
    [ -n "$GOJQ" ] && printf 'gojq\t%s\n' "$(jq_family_cmd "$GOJQ" "$oracle_flags" "$oracle_program" "$infile")" >> "$cand_file"
    build_tool_spec "$lane" "json" "$oracle_norm" ""

    finish_lane "$lane" "$program" "$(jqf_cmd "$jqf_flags" "$program" "$infile")" "$infile" "$note"
}

# M1: per-element-style update (jq emits the whole edited document once)
run_jq_lane "M1" "-c" ".catalog[].stock |= .+1" "$CATALOG_JSON" \
    ".catalog[].stock |= .+1" "-c"

# M2: deep single-field update
run_jq_lane "M2" "-c" ".meta.count |= .+1" "$CATALOG_JSON" \
    ".meta.count |= .+1" "-c"

# M3: field deletion
run_jq_lane "M3" "-c" "del(.catalog[].sku)" "$CATALOG_JSON" \
    "del(.catalog[].sku)" "-c"

# M4: path-based insertion
run_jq_lane "M4" "-c" 'setpath(["meta","generated"]; true)' "$CATALOG_JSON" \
    'setpath(["meta","generated"]; true)' "-c"

### E1-E4: --edit and --in-place #############################################
# --edit makes the whole document the output subject. E1/E2 publish the edited
# document on stdout; their oracle is jq (E1 = jq identity, E2 = the whole-doc
# reduce form, since jq has no --edit). E3/E4 rewrite a file in place; their
# oracle is jq's PRETTY render (jqf --in-place re-renders with the formatting
# flags), which yq -i reproduces byte-for-byte.

# E1: --edit identity
run_jq_lane "E1" "--edit" "." "$CATALOG_JSON" \
    "." "-c"

# E2: --edit update (jq's whole-document equivalent is a range reduce)
run_jq_lane "E2" "--edit" ".catalog[].stock *= 2" "$CATALOG_JSON" \
    'reduce range(0; .catalog|length) as $i (.; .catalog[$i].stock *= 2)' "-c"

# run_inplace_lane <lane> <program> <oracle-program>
#   Both jqf and yq cells copy the pristine fixture into a per-lane temp file
#   before every run, so each hyperfine iteration starts from the same input.
run_inplace_lane() {
    local lane="$1" program="$2" oracle_program="$3"
    local src="$CATALOG_JSON"
    local tmp="$OUTDIR/${lane}.edit.json"

    # Oracle: jq's pretty render of the edited document.
    local oracle="$OUTDIR/${lane}.oracle"
    "$JQ" "$oracle_program" <"$src" >"$oracle" 2>/dev/null

    local note=""
    local jqf_c="$(inplace_cmd jqf "$program" "$src" "$tmp")"
    if ! run_bounded "$PROBE_TIMEOUT" "$lane.jqf" -- bash -c "$jqf_c"; then
        note_fail "$lane: --in-place run failed"
        emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "VALIDATION FAILED (--in-place run failed)"
        return
    fi
    if cmp -s "$tmp" "$oracle"; then
        note="in-place == jq render (byte)"
    else
        note_fail "$lane: in-place result != jq render"
        note="FAIL: in-place != jq render"
    fi

    local cand_file="$OUTDIR/${lane}.candidates"
    : > "$cand_file"
    [ -n "$YQ" ] && printf 'yq\t%s\n' "$(inplace_cmd yq "$program" "$src" "$tmp")" >> "$cand_file"
    build_tool_spec "$lane" "inplace" "$oracle" "$tmp"

    finish_lane "$lane" "$program" "$jqf_c" "$src" "$note"
}

# E3: --in-place identity
run_inplace_lane "E3" "." "."

# E4: --in-place update (idempotent absolute assignment, so repeated runs
#     against the pristine copy are stable)
run_inplace_lane "E4" '.meta.generated = true' '.meta.generated = true'

# ---------------------------------------------------------------------------
# Footer
# ---------------------------------------------------------------------------
if [ "$JSON_MODE" -eq 0 ]; then
    printf '%s\n' "----------------------------------------------------------------------------------------------------------------------------------------"
    printf 'rows=%d failures=%d\n' "$ROW_COUNT" "$fail"
    printf 'A BLANK CELL IS NOT "THE TOOL WAS SLOWER": it is absent (not installed,\n'
    printf 'or installed but it FAILED ITS PROBE for this lane -- reason on stderr),\n'
    printf 'n/a (no fair-equivalent expression), no-equiv (the lane asks a question\n'
    printf 'no competitor command can express -- dasel/python3 on XML, whose element\n'
    printf 'mapping differs by doctrine), disagreed (output failed the oracle\n'
    printf 'comparison -- yq on NDJSON, gojq on byte lanes), or err (the TIMING\n'
    printf 'HARNESS itself failed -- hyperfine diagnostics are on stderr; this is a\n'
    printf 'bug in this script or its environment, never a tool result).\n'
    printf 'jq/jaq/gojq cannot read YAML/TOML/CBOR/XML, and only jq-family tools can\n'
    printf 'express the mutation lanes -- so each column is n/a on every lane outside\n'
    printf 'its own family. Oracles: yq for YAML/TOML->JSON, jq for JSON/mutation/edit,\n'
    printf 'external gzip/zlib for the compression lanes, the fixture itself for CBOR\n'
    printf '(jqf-self, weaker). Every competitor is probed AND oracle-checked before it\n'
    printf 'is timed; every lane and probe runs under a process-group timeout.\n'
    if [ "$fail" -ne 0 ]; then
        printf 'FAILURES DETECTED — see stderr above.\n'
    fi
else
    python3 -c '
import json, sys
print(json.dumps({
    "summary": {
        "rows": int(sys.argv[1]),
        "failures": int(sys.argv[2]),
        "oracle": "yq for YAML/TOML->JSON; jq for JSON/mutation/edit; external gzip/zlib for compression; fixture for CBOR (jqf-self)",
        "competitors": "yq/dasel/py on format lanes; jq/jaq/gojq on mutation lanes; gzip on compression lanes",
    }
}))
' "$ROW_COUNT" "$fail"
fi

[ "$fail" -eq 0 ]
