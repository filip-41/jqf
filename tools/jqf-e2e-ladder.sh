#!/usr/bin/env bash
#
# Standing end-to-end benchmark ladder: jqf CLI vs jq / jaq / gojq across a
# fixed set of tool-level lanes (whole-process decode+filter+encode over
# stdin/stdout, the same way a user invokes any of these tools).
#
# This is a *correctness-gated* benchmark, not a pure timing script: several
# lanes carry assertions (byte-identity, uniformity across extraction
# programs, corruption rejection) and the script exits nonzero if any of
# them fail, independent of --json. Timing numbers are directional — pinned,
# discipline-controlled numbers come from quiet-machine PGO runs recorded
# out-of-tree; runs from this script on a loaded machine are not comparable to
# those.
#
# Lanes (L1-L12), each identified by the LANE column of the printed table:
#   L1  identity            `.`                         on catalog-10mb.json
#   L2  extraction quartet  4 programs (see below)       on catalog-10mb.json
#   L3  escape identity+extraction (2 programs)          on escape-10mb.json
#   L4  NDJSON throughput   `.v`                          on ndjson-200k.ndjson
#   L5  NDJSON extraction   `.name`                       on ndjson-200k.ndjson
#   L6  deep identity       `.`                          on deep-500.json
#   L7  cold-start identity `.`                          on small-29kb.json
#   L8  corruption rejection (no timing; exit-code assertion only)
#   L9  NDJSON collect      `[.[] | .v] | length`         on ndjson-tiny-400k
#       (the widened morsel class, DEFAULT parallel-auto invocation)
#   L10 NDJSON collect floor  same program, --no-parallel  on ndjson-tiny-400k
#       (the materializing serial floor; the widening's own receipt)
#   L11 error-heavy NDJSON  `.[] | .missing`              on ndjson-200k.ndjson
#       (the buffered-stderr receipt: exit-5 parity with jq, no stdout)
#   L12 tiny-record decode floor `empty` --no-parallel     on ndjson-tiny-400k
#       (the per-record decode fixed cost; jqf wins per-byte, loses per-record)
#
# ---------------------------------------------------------------------------
# OUTPUT AGREEMENT IS THE DEFAULT POSTURE, NOT A PER-LANE FEATURE.
# ---------------------------------------------------------------------------
# No cell is timed before its output has been validated, because a benchmark
# that times the wrong answer is worse than no benchmark. jq is the oracle.
# Every timed lane asserts jqf's bytes against `jq -c`'s, and the exemptions
# are a CLOSED, PRINTED registry rather than an accident of which lane
# somebody remembered to check:
#
#   L6 (deep-500)  — EXEMPT WHEN jq FAILS. Depth 500 is exactly the lane
#                    where competitors are permitted to reject; jqf must
#                    succeed. Byte-identity IS asserted whenever jq exits 0.
#   L8 (corrupt)   — EXEMPT, no output. The contract is the exit class, and
#                    both jqf and jq must reject.
#   jaq / gojq     — NEVER assertion-bearing. A valid re-serialization that
#                    differs from jq's is not a jqf defect; a divergence is
#                    an informational note on the row.
#   duckdb / chl   — VALUE-LEVEL, not byte-level (see below).
#
# L2 additionally carries a uniformity bound: jqf's four extraction programs
# must land within 15% of each other.
#
# ---------------------------------------------------------------------------
# A BLANK CELL IS NOT "THE TOOL WAS SLOWER".
# ---------------------------------------------------------------------------
# A cell is blank for one of four reasons, and the ladder always says which:
#   absent    — the binary is not installed on this host
#   n/a       — the tool has no fair-equivalent expression for this lane
#   disagreed — its output did not agree with the oracle, so it was NOT timed
#   excluded  — a declared (tool, lane) exclusion, via JQF_E2E_EXCLUDE
# The exclusion vocabulary exists so a pathological (tool, lane) combination
# can be kept out of a routine run WITHOUT the resulting hole reading as a
# competitor loss. Every exclusion is reported by name in the footer.
#
# ---------------------------------------------------------------------------
# SQL-ON-FILES COLUMNS (L4/L5 only).
# ---------------------------------------------------------------------------
# `duckdb` and `clickhouse local` join the two NDJSON lanes, where a fair
# equivalent genuinely exists: one column projected out of 200k records. They
# are absent everywhere else BY CONSTRUCTION, not by omission — no fair
# SQL equivalent exists for identity over a nested document, for a depth-500
# spine, or for a cold-start lane.
#
# Their agreement rule is VALUE-LEVEL, because the framings legitimately
# differ: the jq family prints the JSON TEXT of each value (`"n0"`), an SQL
# engine prints the VALUE (`n0`). The ladder compares line by line after
# mapping each oracle line through "a JSON string becomes its contents,
# anything else keeps its JSON text". Row ORDER is still required to match —
# an engine that reorders records is not computing this lane's answer.
#
# Two fairness caveats travel with these columns and are not corrected for:
# the SQL engines read the fixture BY PATH (they may mmap) while the jq
# family reads it on stdin, and both SQL engines are multi-threaded by
# default, as is jqf, while jq/jaq/gojq are not.
#
# Usage: tools/jqf-e2e-ladder.sh [--json] [path-to-jqf]
#   JQF_BIN          overrides the jqf binary path (skips the build step)
#   JQF_E2E_FIXDIR   reuses/persists the generated fixtures at this path
#                    (default: a fresh mktemp directory, removed on exit)
#   JQF_E2E_EXCLUDE  comma-separated `tool:lane` exclusions, `*` wildcards
#                    both halves (e.g. "gojq:L4,duckdb:*")
#   JQ / JAQ / GOJQ / DUCKDB / CLICKHOUSE / HYPERFINE
#                    override the detected binary paths
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
if [ ! -x "$JQF" ]; then
    echo "jqf-e2e-ladder: $JQF not found or not executable; building release jqf..." >&2
    if ! (cd "$ROOT" && cargo build --release -p jqf) >&2; then
        echo "error: failed to build jqf" >&2
        exit 2
    fi
fi
if [ ! -x "$JQF" ]; then
    echo "error: jqf binary still not found at $JQF after build" >&2
    exit 2
fi

JQ="${JQ:-$(command -v jq || true)}"
JAQ="${JAQ:-$(command -v jaq || true)}"
GOJQ="${GOJQ:-$(command -v gojq || true)}"
DUCKDB="${DUCKDB:-$(command -v duckdb || true)}"
CLICKHOUSE="${CLICKHOUSE:-$(command -v clickhouse || true)}"
HYPERFINE="${HYPERFINE:-$(command -v hyperfine || true)}"

[ -n "$JQ" ] || echo "notice: jq not found on PATH; jq column skipped, byte-identity checks against it skipped" >&2
[ -n "$JAQ" ] || echo "notice: jaq not found on PATH; jaq column skipped" >&2
[ -n "$GOJQ" ] || echo "notice: gojq not found on PATH; gojq column skipped" >&2
[ -n "$DUCKDB" ] || echo "notice: duckdb not found on PATH; duckdb column absent on L4/L5" >&2
[ -n "$CLICKHOUSE" ] || echo "notice: clickhouse not found on PATH; chl column absent on L4/L5" >&2
[ -n "$HYPERFINE" ] || echo "notice: hyperfine not found on PATH; timing columns skipped (correctness/assertion lanes still run)" >&2

# Every row is only as good as the binary it measures, so the binary's own
# --diagnostics provenance line (build= profile= allocator=
# platform=) is captured once and rides every row. A plain-release number can
# never be mistaken for a PGO one.
PROVENANCE_LINE="$("$JQF" --diagnostics . </dev/null 2>&1 >/dev/null | sed -n 's/^jqf: build=.*/&/p' | head -1 || true)"
if [ -z "$PROVENANCE_LINE" ]; then
    echo "notice: could not read a --diagnostics provenance line from $JQF" >&2
fi
echo "jqf-e2e-ladder: provenance: $PROVENANCE_LINE" >&2

# label|binary|compact-flag, one entry per detected competitor.
COMPETITORS=()
[ -n "$JQ" ] && COMPETITORS+=("jq|$JQ|-c")
[ -n "$JAQ" ] && COMPETITORS+=("jaq|$JAQ|-c")
[ -n "$GOJQ" ] && COMPETITORS+=("gojq|$GOJQ|-c")

FIXDIR="${JQF_E2E_FIXDIR:-}"
CLEANUP_FIXDIR=0
if [ -z "$FIXDIR" ]; then
    FIXDIR="$(mktemp -d "${TMPDIR:-/tmp}/jqf-e2e-fixtures.XXXXXX")"
    CLEANUP_FIXDIR=1
fi
mkdir -p "$FIXDIR"
if ! python3 "$ROOT/tools/jqf-e2e-fixtures.py" "$FIXDIR"; then
    echo "error: fixture generation failed" >&2
    exit 2
fi

CATALOG="$FIXDIR/catalog-10mb.json"
ESCAPE="$FIXDIR/escape-10mb.json"
DEEP="$FIXDIR/deep-500.json"
NDJSON="$FIXDIR/ndjson-200k.ndjson"
TINY_NDJSON="$FIXDIR/ndjson-tiny-400k.ndjson"
SMALL="$FIXDIR/small-29kb.json"
CORRUPT="$FIXDIR/corrupt-late.json"

OUTDIR="$(mktemp -d "${TMPDIR:-/tmp}/jqf-e2e-ladder.XXXXXX")"
cleanup() {
    rm -rf "$OUTDIR"
    [ "$CLEANUP_FIXDIR" -eq 1 ] && rm -rf "$FIXDIR"
}
trap cleanup EXIT

ROW_COUNT=0

# Failures are logged to a file, not an in-memory array: several callers
# (assert_identity_vs_jq, check_uniformity) run inside `$(...)` command
# substitution to capture their return note, which forks a subshell — a
# plain variable/array mutation from note_fail there would vanish with the
# subshell and silently swallow the assertion. A file append survives it.
FAILLOG="$OUTDIR/failures.log"
: >"$FAILLOG"

note_fail() {
    printf '%s\n' "$1" >>"$FAILLOG"
    echo "ASSERT FAIL: $1" >&2
}

# Excluded cells are logged for the same subshell-survival reason as
# failures, and for one more: the footer must be able to name every hole in
# the table, since an unexplained blank is indistinguishable from a loss.
EXCLUSIONLOG="$OUTDIR/exclusions.log"
: >"$EXCLUSIONLOG"

note_exclusion() {
    printf '%s\n' "$1" >>"$EXCLUSIONLOG"
}

# is_excluded <tool> <lane> -> 0 when a declared JQF_E2E_EXCLUDE entry covers
# this cell. Entries are `tool:lane`; `*` wildcards either half.
EXCLUDE_SPEC="${JQF_E2E_EXCLUDE:-}"
is_excluded() {
    local tool="$1" lane="$2" entry etool elane
    [ -n "$EXCLUDE_SPEC" ] || return 1
    local IFS=','
    for entry in $EXCLUDE_SPEC; do
        etool="${entry%%:*}"
        elane="${entry#*:}"
        [ "$etool" = "*" ] || [ "$etool" = "$tool" ] || continue
        [ "$elane" = "*" ] || [ "$elane" = "$lane" ] || continue
        return 0
    done
    return 1
}

shq() {
    printf '%q' "$1"
}

# combine_note <a> <b> -> "a; b", or whichever of the two is non-empty
combine_note() {
    if [ -n "$1" ] && [ -n "$2" ]; then
        printf '%s; %s' "$1" "$2"
    else
        printf '%s%s' "$1" "$2"
    fi
}

# Every tool in this file, jqf included, runs COMPACT. jqf's default output is
# jq's two-space pretty print, so a bare jqf would both break the byte-identity
# assertions against `jq -c` and time its pretty writer against everyone else's
# compact one. Formatting is the compat gate's dimension, not the ladder's.
#
# tool_cmd <bin> <flag-or-empty> <program> <infile> -> shell command string
tool_cmd() {
    local bin="$1" flag="$2" program="$3" infile="$4"
    local -a flag_words
    if [ -n "$flag" ]; then
        read -r -a flag_words <<<"$flag"
    fi
    local out
    out="$(shq "$bin")"
    if [ "${#flag_words[@]}" -gt 0 ]; then
        for word in "${flag_words[@]}"; do
            out="$out $(shq "$word")"
        done
    fi
    printf '%s %s < %s > /dev/null' "$out" "$(shq "$program")" "$(shq "$infile")"
}

# capture_output <outfile> <bin> <flag-or-empty> <program> <infile> -> prints exit code
capture_output() {
    local outfile="$1" bin="$2" flag="$3" program="$4" infile="$5"
    local -a flag_words
    if [ -n "$flag" ]; then
        read -r -a flag_words <<<"$flag"
    fi
    if [ "${#flag_words[@]}" -gt 0 ]; then
        "$bin" "${flag_words[@]}" "$program" <"$infile" >"$outfile" 2>"$outfile.err"
    else
        "$bin" "$program" <"$infile" >"$outfile" 2>"$outfile.err"
    fi
    echo $?
}

# hyperfine_time <minruns (0 = default)> <out_json> <label1> <cmd1> [<label2> <cmd2> ...]
hyperfine_time() {
    local minruns="$1" outjson="$2"
    shift 2
    local hf_args=(--warmup 3 --export-json "$outjson" --ignore-failure)
    [ "$minruns" -gt 0 ] && hf_args+=(--min-runs "$minruns")
    while [ "$#" -gt 0 ]; do
        hf_args+=(--command-name "$1" "$2")
        shift 2
    done
    "$HYPERFINE" "${hf_args[@]}" >"$outjson.log" 2>&1
}

# hyperfine_get <out_json> <label> -> "mean_ms stddev_ms" (or "n/a n/a")
hyperfine_get() {
    python3 -c '
import json, sys
try:
    data = json.load(open(sys.argv[1]))
except (OSError, json.JSONDecodeError):
    print("n/a n/a")
    sys.exit(0)
for r in data.get("results", []):
    if r["command"] == sys.argv[2]:
        mean_ms = r["mean"] * 1000
        stddev_ms = r["stddev"] * 1000
        print(f"{mean_ms:.1f} {stddev_ms:.1f}")
        sys.exit(0)
print("n/a n/a")
' "$1" "$2"
}

# measure_rss <cmd_string> <receipt_label>
# Sets RSS_BYTES, RSS_MB, RECLAIMS, USER_S, SYS_S, and MAJFLT or exits 2
# without fabricating evidence. The BSD /usr/bin/time -l log already carries
# user/sys/minflt/majflt beside RSS, so the same never-
# timed run feeds every resource column. Page RECLAIMS are the minor faults
# (minflt); page FAULTS are the major faults (majflt) — two different lines.
measure_rss() {
    local cmd="$1" receipt_label="$2"
    local timelog="$OUTDIR/${receipt_label}.time.log"
    /usr/bin/time -l bash -c "$cmd" >/dev/null 2>"$timelog"
    local time_status=$?
    local parsed
    if ! parsed="$(python3 "$ROOT/tools/jqf-rss-parse.py" --full --status "$time_status" "$timelog")"; then
        local retained_log
        retained_log="$(mktemp "${TMPDIR:-/tmp}/jqf-e2e-rss-failure.XXXXXX")"
        cp "$timelog" "$retained_log"
        echo "error: RSS measurement failed; raw /usr/bin/time log retained at $retained_log" >&2
        exit 2
    fi
    IFS=$'\t' read -r RSS_BYTES RSS_MB RECLAIMS USER_S SYS_S MAJFLT <<<"$parsed"
}

TABLE_HEADER_PRINTED=0
print_table_header() {
    [ "$TABLE_HEADER_PRINTED" -eq 1 ] && return
    printf '%-4s %-26s %-16s %-9s %-9s %-9s %-9s %-9s %-13s %-9s %-9s %-8s %-8s %-9s %s\n' \
        "LANE" "PROGRAM" "JQF(ms+-sd)" "JQ(ms)" "JAQ(ms)" "GOJQ(ms)" "DUCKDB" "CHL(ms)" "RSS(bytes)" "RSS(MiB)" "RECLAIM" "USER(s)" "SYS(s)" "MINFLT" "NOTE"
    printf '%s\n' "-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------"
    TABLE_HEADER_PRINTED=1
}

# emit_row lane program jqf_mean jqf_std jq_mean jaq_mean gojq_mean duckdb_mean chl_mean rss_bytes rss_mib reclaims user_s sys_s minflt majflt note
emit_row() {
    local lane="$1" program="$2" jqf_mean="$3" jqf_std="$4" jq_mean="$5" jaq_mean="$6" gojq_mean="$7" duckdb_mean="$8" chl_mean="$9" rss_bytes="${10}" rss_mib="${11}" reclaims="${12}" user_s="${13}" sys_s="${14}" minflt="${15}" majflt="${16}" note="${17}"
    ROW_COUNT=$((ROW_COUNT + 1))
    if [ "$JSON_MODE" -eq 1 ]; then
        python3 -c '
import json, sys

def num(x):
    if x in ("", "n/a"):
        return None
    try:
        return float(x) if "." in x else int(x)
    except ValueError:
        return x

(lane, program, jqf_mean, jqf_std, jq_mean, jaq_mean, gojq_mean, duckdb_mean,
 chl_mean, rss_bytes, rss_mib, reclaims, user_s, sys_s, minflt, majflt, note, provenance) = sys.argv[1:19]
print(json.dumps({
    "lane": lane,
    "program": program,
    "jqf_mean_ms": num(jqf_mean),
    "jqf_stddev_ms": num(jqf_std),
    "jq_mean_ms": num(jq_mean),
    "jaq_mean_ms": num(jaq_mean),
    "gojq_mean_ms": num(gojq_mean),
    "duckdb_mean_ms": num(duckdb_mean),
    "clickhouse_mean_ms": num(chl_mean),
    "jqf_rss_bytes": num(rss_bytes),
    "jqf_rss_mb": num(rss_mib),
    "jqf_page_reclaims": num(reclaims),
    "jqf_user_s": num(user_s),
    "jqf_sys_s": num(sys_s),
    "jqf_minflt": num(minflt),
    "jqf_majflt": num(majflt),
    "jqf_provenance": provenance,
    "note": note,
}))
' "$lane" "$program" "$jqf_mean" "$jqf_std" "$jq_mean" "$jaq_mean" "$gojq_mean" "$duckdb_mean" "$chl_mean" "$rss_bytes" "$rss_mib" "$reclaims" "$user_s" "$sys_s" "$minflt" "$majflt" "$note" "$PROVENANCE_LINE"
    else
        print_table_header
        local jqf_disp="n/a"
        [ "$jqf_mean" != "n/a" ] && jqf_disp="${jqf_mean}+-${jqf_std}"
        printf '%-4s %-26s %-16s %-9s %-9s %-9s %-9s %-9s %-13s %-9s %-9s %-8s %-8s %-9s %s\n' \
            "$lane" "$program" "$jqf_disp" "$jq_mean" "$jaq_mean" "$gojq_mean" "$duckdb_mean" "$chl_mean" "$rss_bytes" "$rss_mib" "$reclaims" "$user_s" "$sys_s" "$minflt" "$note"
    fi
}

# Extra (label, shell-command) pairs a lane wants timed alongside the jq
# family — the SQL-on-files columns. One `label<TAB>command` per line rather
# than a pair of arrays: this host's /bin/bash is 3.2, where expanding an
# EMPTY array under `set -u` is an unbound-variable error, and these are
# empty on every lane but L4/L5.
EXTRA_PAIRS=""

# time_and_measure <fileprefix> <program> <infile> <minruns> <include_competitors 0|1> [<lane> [<jqf_flags> [<skip_rss 0|1>]]]
# Sets T_JQF_MEAN T_JQF_STD T_JQ_MEAN T_JAQ_MEAN T_GOJQ_MEAN T_DUCKDB_MEAN
# T_CHL_MEAN T_RSS_BYTES T_RSS T_RECLAIMS T_USER_S T_SYS_S T_MINFLT T_NOTE. `jqf_flags` (default `-c`)
# is the whole jqf flag word, so a floor lane can time `-c --no-parallel`.
# `skip_rss` exempts a lane whose jqf invocation exits nonzero BY DESIGN (the
# error lane) — the RSS receipt measures exit-0 runs only.
time_and_measure() {
    local fileprefix="$1" program="$2" infile="$3" minruns="$4" include_competitors="$5" lane="${6:-}" jqf_flags="${7:--c}" skip_rss="${8:-0}"
    T_JQF_MEAN="n/a"
    T_JQF_STD="n/a"
    T_JQ_MEAN="n/a"
    T_JAQ_MEAN="n/a"
    T_GOJQ_MEAN="n/a"
    T_DUCKDB_MEAN="n/a"
    T_CHL_MEAN="n/a"
    T_RSS_BYTES="n/a"
    T_USER_S="n/a"
    T_SYS_S="n/a"
    T_MINFLT="n/a"
    T_MAJFLT="n/a"
    T_NOTE=""

    local jqf_cmd
    jqf_cmd="$(tool_cmd "$JQF" "$jqf_flags" "$program" "$infile")"

    # A declared exclusion removes the cell from the hyperfine run itself, so
    # an excluded tool costs nothing and can never be half-measured.
    local timed_labels=()
    if [ "$include_competitors" -eq 1 ]; then
        local entry label bin flag
        for entry in "${COMPETITORS[@]}"; do
            IFS='|' read -r label bin flag <<<"$entry"
            if [ -n "$lane" ] && is_excluded "$label" "$lane"; then
                note_exclusion "$lane/$label: declared in JQF_E2E_EXCLUDE (excluded, NOT slower)"
                continue
            fi
            timed_labels+=("$label")
        done
    fi

    if [ -z "$HYPERFINE" ]; then
        T_NOTE="hyperfine absent; timing skipped"
    else
        local outjson="$OUTDIR/${fileprefix}.hf.json"
        local hf_pairs=("jqf" "$jqf_cmd")
        local entry name label bin flag extra_label extra_cmd
        for label in "${timed_labels[@]:-}"; do
            [ -n "$label" ] || continue
            for entry in "${COMPETITORS[@]}"; do
                IFS='|' read -r name bin flag <<<"$entry"
                [ "$name" = "$label" ] || continue
                hf_pairs+=("$label" "$(tool_cmd "$bin" "$flag" "$program" "$infile")")
            done
        done
        while IFS=$'\t' read -r extra_label extra_cmd; do
            [ -n "$extra_label" ] || continue
            hf_pairs+=("$extra_label" "$extra_cmd")
        done <<<"$EXTRA_PAIRS"
        hyperfine_time "$minruns" "$outjson" "${hf_pairs[@]}"

        local result
        result="$(hyperfine_get "$outjson" "jqf")"
        T_JQF_MEAN="$(printf '%s' "$result" | awk '{print $1}')"
        T_JQF_STD="$(printf '%s' "$result" | awk '{print $2}')"
        for label in "${timed_labels[@]:-}"; do
            case "$label" in
                jq) T_JQ_MEAN="$(hyperfine_get "$outjson" "jq" | awk '{print $1}')" ;;
                jaq) T_JAQ_MEAN="$(hyperfine_get "$outjson" "jaq" | awk '{print $1}')" ;;
                gojq) T_GOJQ_MEAN="$(hyperfine_get "$outjson" "gojq" | awk '{print $1}')" ;;
            esac
        done
        while IFS=$'\t' read -r extra_label extra_cmd; do
            case "$extra_label" in
                duckdb) T_DUCKDB_MEAN="$(hyperfine_get "$outjson" "duckdb" | awk '{print $1}')" ;;
                chl) T_CHL_MEAN="$(hyperfine_get "$outjson" "chl" | awk '{print $1}')" ;;
            esac
        done <<<"$EXTRA_PAIRS"
    fi

    if [ "$skip_rss" -eq 0 ]; then
        measure_rss "$jqf_cmd" "$fileprefix"
        T_RSS_BYTES="$RSS_BYTES"
        T_RSS="$RSS_MB"
        T_RECLAIMS="$RECLAIMS"
        T_USER_S="$USER_S"
        T_SYS_S="$SYS_S"
        T_MINFLT="$RECLAIMS"  # minor faults == page reclaims
        T_MAJFLT="$MAJFLT"
    else
        T_RSS_BYTES="n/a"
        T_RSS="n/a"
        T_RECLAIMS="n/a"
        T_USER_S="n/a"
        T_SYS_S="n/a"
        T_MINFLT="n/a"
        T_MAJFLT="n/a"
    fi
}

# assert_identity_vs_jq <lane> <program> <infile> <fileprefix>
# Runs jqf and (if present) jq/jaq/gojq directly, asserts jqf byte-matches
# jq (required), and notes jaq/gojq divergence informationally only.
# Prints a note string on stdout for the caller to fold into the row.
assert_identity_vs_jq() {
    local lane="$1" program="$2" infile="$3" fileprefix="$4"
    local jqf_out="$OUTDIR/${fileprefix}_jqf.out"
    local jqf_exit
    jqf_exit="$(capture_output "$jqf_out" "$JQF" "-c" "$program" "$infile")"

    local note=""
    if [ -n "$JQ" ]; then
        local jq_out="$OUTDIR/${fileprefix}_jq.out"
        local jq_exit
        jq_exit="$(capture_output "$jq_out" "$JQ" "-c" "$program" "$infile")"
        if [ "$jqf_exit" -ne 0 ] || [ "$jq_exit" -ne 0 ]; then
            note_fail "$lane $program: expected both jqf and jq to exit 0 (jqf=$jqf_exit jq=$jq_exit)"
            note="nonzero exit"
        elif ! cmp -s "$jqf_out" "$jq_out"; then
            note_fail "$lane $program: jqf output not byte-identical to jq -c"
            note="jqf!=jq (FAIL)"
        else
            note="jqf==jq"
        fi
    else
        note="jq absent; byte-identity skipped"
    fi

    local entry label bin flag other_out
    for entry in "${COMPETITORS[@]}"; do
        IFS='|' read -r label bin flag <<<"$entry"
        [ "$label" = "jq" ] && continue
        other_out="$OUTDIR/${fileprefix}_${label}.out"
        capture_output "$other_out" "$bin" "$flag" "$program" "$infile" >/dev/null
        if [ -n "$JQ" ] && [ -f "$OUTDIR/${fileprefix}_jq.out" ] && ! cmp -s "$other_out" "$OUTDIR/${fileprefix}_jq.out"; then
            note="$note; $label differs from jq (not asserted)"
        fi
    done

    printf '%s' "$note"
}

# sql_value_agreement <oracle_out> <sql_out> -> prints "ok" or a reason.
#
# The comparison rule, in one sentence: an SQL engine prints the VALUE where
# the jq family prints the JSON TEXT of the value, so each oracle line is
# mapped through "a JSON string becomes its contents, anything else keeps its
# text" and the two sequences must then match line for line, in order.
# Order is part of the answer: an engine that reorders records has not
# computed this lane.
sql_value_agreement() {
    python3 - "$1" "$2" <<'PY'
import json, sys

def unwrap(line):
    try:
        value = json.loads(line)
    except json.JSONDecodeError:
        return line
    return value if isinstance(value, str) else line

with open(sys.argv[1], "r", encoding="utf-8", errors="replace") as handle:
    oracle = [unwrap(l.rstrip("\n")) for l in handle]
with open(sys.argv[2], "r", encoding="utf-8", errors="replace") as handle:
    actual = [l.rstrip("\n") for l in handle]

if len(oracle) != len(actual):
    print(f"row count {len(actual)} != oracle {len(oracle)}")
    raise SystemExit(0)
for index, (want, got) in enumerate(zip(oracle, actual)):
    if want != got:
        print(f"row {index}: {got!r} != oracle {want!r}")
        raise SystemExit(0)
print("ok")
PY
}

# run_sql_columns <lane> <fileprefix> <column> <oracle_out>
# Validates each available SQL engine's value sequence against the oracle
# output and, only on agreement, appends its command to EXTRA_PAIRS so
# time_and_measure times it. A disagreeing or failing engine is EXCLUDED —
# never timed, never reported as a loss.
run_sql_columns() {
    local lane="$1" fileprefix="$2" column="$3" oracle_out="$4"
    EXTRA_PAIRS=""
    local label bin out verdict status sql cmd
    for label in duckdb chl; do
        case "$label" in
            duckdb)
                bin="$DUCKDB"
                sql="SELECT $column FROM read_ndjson_auto('$NDJSON')"
                ;;
            chl)
                bin="$CLICKHOUSE"
                sql="SELECT $column FROM file('$NDJSON', JSONEachRow) FORMAT TSVRaw"
                ;;
        esac
        [ -n "$bin" ] || continue
        if is_excluded "$label" "$lane"; then
            note_exclusion "$lane/$label: declared in JQF_E2E_EXCLUDE (excluded, NOT slower)"
            continue
        fi
        out="$OUTDIR/${fileprefix}_${label}.out"
        if [ "$label" = "duckdb" ]; then
            "$bin" -noheader -list -c "$sql" >"$out" 2>"$out.err"
        else
            "$bin" local --query "$sql" >"$out" 2>"$out.err"
        fi
        status=$?
        if [ "$status" -ne 0 ]; then
            note_exclusion "$lane/$label: exited $status (excluded, NOT slower)"
            continue
        fi
        verdict="$(sql_value_agreement "$oracle_out" "$out")"
        if [ "$verdict" != "ok" ]; then
            note_exclusion "$lane/$label: value-level disagreement — $verdict (excluded, NOT slower)"
            continue
        fi
        if [ "$label" = "duckdb" ]; then
            cmd="$(shq "$bin") -noheader -list -c $(shq "$sql") > /dev/null"
        else
            cmd="$(shq "$bin") local --query $(shq "$sql") > /dev/null"
        fi
        EXTRA_PAIRS="${EXTRA_PAIRS}${label}	${cmd}
"
    done
}

# run_identity_lane <lane> <program> <infile> <fileprefix> <minruns>
# Shared shape for lanes that assert jqf's output byte-matches jq on one
# program, then time it. Used by L1, L3 (twice), and L7.
run_identity_lane() {
    local lane="$1" program="$2" infile="$3" fileprefix="$4" minruns="$5"
    local note
    note="$(assert_identity_vs_jq "$lane" "$program" "$infile" "$fileprefix")"
    EXTRA_PAIRS=""
    time_and_measure "$fileprefix" "$program" "$infile" "$minruns" 1 "$lane"
    emit_row "$lane" "$program" "$T_JQF_MEAN" "$T_JQF_STD" "$T_JQ_MEAN" "$T_JAQ_MEAN" "$T_GOJQ_MEAN" "$T_DUCKDB_MEAN" "$T_CHL_MEAN" "$T_RSS_BYTES" "$T_RSS" "$T_RECLAIMS" "$T_USER_S" "$T_SYS_S" "$T_MINFLT" "$T_MAJFLT" "$(combine_note "$note" "$T_NOTE")"
}

echo "jqf-e2e-ladder: jqf=$JQF jq=${JQ:-none} jaq=${JAQ:-none} gojq=${GOJQ:-none} hyperfine=${HYPERFINE:-none}" >&2
echo "jqf-e2e-ladder: fixtures in $FIXDIR" >&2

### L1: identity on catalog-10mb.json ##########################################
run_identity_lane "L1" "." "$CATALOG" "l1" 10

### L2: extraction quartet on catalog-10mb.json, uniformity assertion ##########
run_extraction_quartet_lane() {
    local lane="L2"
    local programs=(".catalog[0].id" ".catalog[500].name" ".meta.count" ".missing_key")
    local mid_index=1 # ".catalog[500].name" is the mid case timed against competitors
    local means=()
    local i program fileprefix include_competitors l2_note agreement
    for i in 0 1 2 3; do
        program="${programs[$i]}"
        fileprefix="l2_$i"
        # Agreement before timing on ALL FOUR programs, not only the one
        # that carries competitor timings: the quartet's uniformity bound is
        # meaningless if one of the four is computing a different answer.
        agreement="$(assert_identity_vs_jq "$lane" "$program" "$CATALOG" "$fileprefix")"

        include_competitors=0
        [ "$i" -eq "$mid_index" ] && include_competitors=1
        EXTRA_PAIRS=""
        time_and_measure "$fileprefix" "$program" "$CATALOG" 10 "$include_competitors" "$lane"
        means+=("$T_JQF_MEAN")

        l2_note="$agreement"
        [ "$include_competitors" -eq 0 ] && l2_note="$(combine_note "$l2_note" "competitor timing not run (mid case only)")"
        if [ "$i" -eq 3 ]; then
            l2_note="$(combine_note "$l2_note" "$(check_uniformity "$lane" "${means[@]}")")"
        fi
        emit_row "$lane" "$program" "$T_JQF_MEAN" "$T_JQF_STD" "$T_JQ_MEAN" "$T_JAQ_MEAN" "$T_GOJQ_MEAN" "$T_DUCKDB_MEAN" "$T_CHL_MEAN" "$T_RSS_BYTES" "$T_RSS" "$T_RECLAIMS" "$T_USER_S" "$T_SYS_S" "$T_MINFLT" "$T_MAJFLT" "$(combine_note "$l2_note" "$T_NOTE")"
    done
}

# check_uniformity <lane> <mean1> <mean2> ... -> prints a note describing
# whether jqf's timings across the given programs land within 15% of each
# other; calls note_fail if the bound is violated.
check_uniformity() {
    local lane="$1"
    shift
    local ratio
    ratio="$(python3 -c '
import sys
try:
    nums = [float(v) for v in sys.argv[1:]]
except ValueError:
    print("skip")
    sys.exit(0)
mn, mx = min(nums), max(nums)
print(f"{mx / mn:.3f}" if mn > 0 else "skip")
' "$@")"
    if [ "$ratio" = "skip" ]; then
        printf 'uniformity: skipped (no timing data)'
        return
    fi
    local within
    within="$(python3 -c "print('1' if float('$ratio') <= 1.15 else '0')")"
    if [ "$within" = "1" ]; then
        printf 'uniformity OK: max/min=%sx (<=1.15x)' "$ratio"
    else
        note_fail "$lane uniformity: jqf max/min=${ratio}x across the extraction quartet exceeds 1.15x"
        printf 'uniformity FAIL: max/min=%sx (>1.15x)' "$ratio"
    fi
}

run_extraction_quartet_lane

### L3: escape-heavy identity + extraction on escape-10mb.json #################
run_identity_lane "L3" "." "$ESCAPE" "l3_0" 10
run_identity_lane "L3" ".catalog[500].description" "$ESCAPE" "l3_1" 10

### L4/L5: NDJSON lanes, all-tool byte-identity required ########################
run_ndjson_lane() {
    local lane="$1" program="$2" fileprefix="$3" sql_column="$4"
    local jqf_out="$OUTDIR/${fileprefix}_jqf.out"
    local jqf_exit
    jqf_exit="$(capture_output "$jqf_out" "$JQF" "-c" "$program" "$NDJSON")"

    local reference="" reference_label="" note="" mismatch=0
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane $program: jqf exited $jqf_exit"
    else
        reference="$jqf_out"
        reference_label="jqf"
    fi

    local entry label bin flag out exit_code
    for entry in "${COMPETITORS[@]}"; do
        IFS='|' read -r label bin flag <<<"$entry"
        out="$OUTDIR/${fileprefix}_${label}.out"
        exit_code="$(capture_output "$out" "$bin" "$flag" "$program" "$NDJSON")"
        if [ "$exit_code" -ne 0 ]; then
            note_fail "$lane $program: $label exited $exit_code"
            mismatch=1
            continue
        fi
        if [ -n "$reference" ]; then
            if ! cmp -s "$out" "$reference"; then
                note_fail "$lane $program: $label output not byte-identical to $reference_label"
                mismatch=1
            fi
        else
            reference="$out"
            reference_label="$label"
        fi
    done

    if [ "$mismatch" -eq 0 ]; then
        note="all present tools byte-identical"
    else
        note="byte-identity FAIL (see stderr)"
    fi

    # SQL-on-files columns, validated at value level against jqf's own
    # output. They join only if they agree.
    EXTRA_PAIRS=""
    if [ -n "$reference" ]; then
        run_sql_columns "$lane" "$fileprefix" "$sql_column" "$reference"
        [ -n "$EXTRA_PAIRS" ] && note="$(combine_note "$note" "sql value-level agreement OK")"
    fi

    time_and_measure "$fileprefix" "$program" "$NDJSON" 0 1 "$lane"
    emit_row "$lane" "$program" "$T_JQF_MEAN" "$T_JQF_STD" "$T_JQ_MEAN" "$T_JAQ_MEAN" "$T_GOJQ_MEAN" "$T_DUCKDB_MEAN" "$T_CHL_MEAN" "$T_RSS_BYTES" "$T_RSS" "$T_RECLAIMS" "$T_USER_S" "$T_SYS_S" "$T_MINFLT" "$T_MAJFLT" "$(combine_note "$note" "$T_NOTE")"
}

run_ndjson_lane "L4" ".v" "l4" "v"
run_ndjson_lane "L5" ".name" "l5" "name"

### L6: deep-500 identity — jqf must succeed, competitors may fail #############
run_deep_lane() {
    local lane="L6" program="." fileprefix="l6"
    local jqf_out jqf_exit
    jqf_out="$OUTDIR/${fileprefix}_jqf.out"
    jqf_exit="$(capture_output "$jqf_out" "$JQF" "-c" "$program" "$DEEP")"
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane $program: jqf must succeed at depth 500 but exited $jqf_exit"
    fi

    local status_note="jqf=$jqf_exit"
    local agreement="agreement exempt: depth 500 is where competitors may reject"
    local entry label bin flag out exit_code
    for entry in "${COMPETITORS[@]}"; do
        IFS='|' read -r label bin flag <<<"$entry"
        out="$OUTDIR/${fileprefix}_${label}.out"
        exit_code="$(capture_output "$out" "$bin" "$flag" "$program" "$DEEP")"
        status_note="$status_note $label=$exit_code"
        # The exemption is conditional, not blanket: when jq DOES accept the
        # spine there is an oracle, so the ladder uses it.
        if [ "$label" = "jq" ] && [ "$exit_code" -eq 0 ] && [ "$jqf_exit" -eq 0 ]; then
            if cmp -s "$OUTDIR/${fileprefix}_jqf.out" "$out"; then
                agreement="jqf==jq"
            else
                note_fail "$lane $program: jqf output not byte-identical to jq -c (jq accepted depth 500)"
                agreement="jqf!=jq (FAIL)"
            fi
        fi
    done

    EXTRA_PAIRS=""
    time_and_measure "$fileprefix" "$program" "$DEEP" 0 1 "$lane"
    emit_row "$lane" "$program" "$T_JQF_MEAN" "$T_JQF_STD" "$T_JQ_MEAN" "$T_JAQ_MEAN" "$T_GOJQ_MEAN" "$T_DUCKDB_MEAN" "$T_CHL_MEAN" "$T_RSS_BYTES" "$T_RSS" "$T_RECLAIMS" "$T_USER_S" "$T_SYS_S" "$T_MINFLT" "$T_MAJFLT" "$(combine_note "$agreement" "$(combine_note "exit codes: $status_note" "$T_NOTE")")"
}
run_deep_lane

### L7: cold-start identity on small-29kb.json (all tools) #####################
run_identity_lane "L7" "." "$SMALL" "l7" 0

### L8: corruption rejection — correctness only, no timing #####################
run_corruption_lane() {
    local lane="L8" program=".catalog[500].name"
    local jqf_out jqf_exit
    jqf_out="$OUTDIR/l8_jqf.out"
    jqf_exit="$(capture_output "$jqf_out" "$JQF" "-c" "$program" "$CORRUPT")"
    if [ "$jqf_exit" -eq 0 ]; then
        note_fail "$lane: jqf must reject corrupt-late.json but exited 0"
    fi

    local jq_note="jq absent"
    if [ -n "$JQ" ]; then
        local jq_out jq_exit
        jq_out="$OUTDIR/l8_jq.out"
        jq_exit="$(capture_output "$jq_out" "$JQ" "-c" "$program" "$CORRUPT")"
        if [ "$jq_exit" -eq 0 ]; then
            note_fail "$lane: jq must reject corrupt-late.json but exited 0"
        elif [ "$jq_exit" -ne 5 ]; then
            note_fail "$lane: expected jq's parse-error exit class (5), got $jq_exit"
        fi
        jq_note="jq_exit=$jq_exit"
    fi

    emit_row "$lane" "$program" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "n/a" "agreement exempt: no output, the contract is the exit class; jqf_exit=$jqf_exit (nonzero, OK); $jq_note"
}
run_corruption_lane

### L9/L10: NDJSON collect — default parallel-auto AND the serial floor ########
# The morsel widening made `[.[] | .v] | length` eligible for the record lane
# (every overload pure); L9 times what users get (parallel-auto) and L10 pins
# the materializing serial floor the widening is measured against. Both assert stdout byte identity across all present tools.
run_ndjson_collect_lane() {
    local lane="$1" program="$2" fileprefix="$3" jqf_flags="$4"
    local jqf_out jqf_exit
    jqf_out="$OUTDIR/${fileprefix}_jqf.out"
    jqf_exit="$(capture_output "$jqf_out" "$JQF" "$jqf_flags" "$program" "$TINY_NDJSON")"

    local reference="" reference_label="" note="" mismatch=0
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane $program: jqf exited $jqf_exit"
    else
        reference="$jqf_out"
        reference_label="jqf"
    fi

    local entry label bin flag out exit_code
    for entry in "${COMPETITORS[@]}"; do
        IFS='|' read -r label bin flag <<<"$entry"
        out="$OUTDIR/${fileprefix}_${label}.out"
        exit_code="$(capture_output "$out" "$bin" "$flag" "$program" "$TINY_NDJSON")"
        if [ "$exit_code" -ne 0 ]; then
            note_fail "$lane $program: $label exited $exit_code"
            mismatch=1
            continue
        fi
        if [ -n "$reference" ]; then
            if ! cmp -s "$out" "$reference"; then
                note_fail "$lane $program: $label output not byte-identical to $reference_label"
                mismatch=1
            fi
        else
            reference="$out"
            reference_label="$label"
        fi
    done

    if [ "$mismatch" -eq 0 ]; then
        note="all present tools byte-identical"
    else
        note="byte-identity FAIL (see stderr)"
    fi

    EXTRA_PAIRS=""
    time_and_measure "$fileprefix" "$program" "$TINY_NDJSON" 0 1 "$lane" "$jqf_flags"
    emit_row "$lane" "$program" "$T_JQF_MEAN" "$T_JQF_STD" "$T_JQ_MEAN" "$T_JAQ_MEAN" "$T_GOJQ_MEAN" "$T_DUCKDB_MEAN" "$T_CHL_MEAN" "$T_RSS_BYTES" "$T_RSS" "$T_RECLAIMS" "$T_USER_S" "$T_SYS_S" "$T_MINFLT" "$T_MAJFLT" "$(combine_note "$note" "$T_NOTE")"
}

# The collect lanes run on the TINY nested fixture: every record is
# {"a":{"v":N}}, so `.[] | .v` extracts exactly one value per record and the
# lane carries no per-value errors (the 2-field fixture would error on the
# string field — L11 owns the error lane).
run_ndjson_collect_lane "L9" "[.[] | .v] | length" "l9" "-c --input-format ndjson"
run_ndjson_collect_lane "L10" "[.[] | .v] | length" "l10" "-c --input-format ndjson --no-parallel"

### L11: error-heavy NDJSON — the buffered-stderr receipt ######################
# `.[] | .missing` over two-field records raises per value: the lane where
# the per-error cost of unbuffered write(2) was measured before the buffered
# stderr channel existed. The contract here is exit-class parity with jq (both 5)
# and empty stdout on both; every tool ERRORS, so none is assertion-bearing
# for bytes. Timing is informational and now includes the buffered channel.
run_error_lane() {
    local lane="L11" program=".[] | .missing" fileprefix="l11"
    local jqf_out jqf_exit jq_out jq_exit
    jqf_out="$OUTDIR/${fileprefix}_jqf.out"
    jqf_exit="$(capture_output "$jqf_out" "$JQF" "-c" "$program" "$NDJSON")"
    jq_out="$OUTDIR/${fileprefix}_jq.out"
    jq_exit="$(capture_output "$jq_out" "$JQ" "-c" "$program" "$NDJSON")"

    local note="agreement exempt: the contract is the exit class"
    if [ "$jqf_exit" -eq 0 ] || [ "$jqf_exit" -ne 5 ]; then
        note_fail "$lane: expected jqf exit class 5 (per-value error), got $jqf_exit"
        note="exit-class FAIL (see stderr)"
    elif [ -s "$jqf_out" ]; then
        note_fail "$lane: jqf wrote stdout on a per-value error lane"
        note="stdout FAIL (see stderr)"
    elif [ -n "$JQ" ]; then
        if [ "$jq_exit" -ne 5 ]; then
            note_fail "$lane: jq also must exit 5 here, got $jq_exit"
            note="jq exit-class FAIL (see stderr)"
        else
            note="jqf_exit=$jqf_exit == jq_exit=$jq_exit, stdout empty both"
        fi
    fi

    EXTRA_PAIRS=""
    time_and_measure "$fileprefix" "$program" "$NDJSON" 0 1 "$lane" "-c" 1
    emit_row "$lane" "$program" "$T_JQF_MEAN" "$T_JQF_STD" "$T_JQ_MEAN" "$T_JAQ_MEAN" "$T_GOJQ_MEAN" "$T_DUCKDB_MEAN" "$T_CHL_MEAN" "$T_RSS_BYTES" "$T_RSS" "$T_RECLAIMS" "$T_USER_S" "$T_SYS_S" "$T_MINFLT" "$T_MAJFLT" "$note"
}
run_error_lane

### L12: tiny-record decode floor — `empty` --no-parallel on ndjson-tiny #######
# The per-record decode fixed cost: 400k two-node records, nothing emitted.
# This is the lane where jqf's indexed Document build trails jq's
# whole-record decode; the per-byte lanes (L1-L5) are where jqf wins.
# Competitors run the same program; duckdb/chl are absent by construction.
run_tiny_decode_lane() {
    local lane="L12" program="empty" fileprefix="l12"
    local jqf_out jqf_exit
    jqf_out="$OUTDIR/${fileprefix}_jqf.out"
    jqf_exit="$(capture_output "$jqf_out" "$JQF" "-c --no-parallel" "$program" "$TINY_NDJSON")"

    local reference="" reference_label="" note="" mismatch=0
    if [ "$jqf_exit" -ne 0 ]; then
        note_fail "$lane $program: jqf exited $jqf_exit"
    else
        reference="$jqf_out"
        reference_label="jqf"
    fi

    local entry label bin flag out exit_code
    for entry in "${COMPETITORS[@]}"; do
        IFS='|' read -r label bin flag <<<"$entry"
        out="$OUTDIR/${fileprefix}_${label}.out"
        exit_code="$(capture_output "$out" "$bin" "$flag" "$program" "$TINY_NDJSON")"
        if [ "$exit_code" -ne 0 ]; then
            note_fail "$lane $program: $label exited $exit_code"
            mismatch=1
            continue
        fi
        if [ -n "$reference" ]; then
            if ! cmp -s "$out" "$reference"; then
                note_fail "$lane $program: $label output not byte-identical to $reference_label"
                mismatch=1
            fi
        else
            reference="$out"
            reference_label="$label"
        fi
    done

    if [ "$mismatch" -eq 0 ]; then
        note="all present tools byte-identical"
    else
        note="byte-identity FAIL (see stderr)"
    fi

    EXTRA_PAIRS=""
    time_and_measure "$fileprefix" "$program" "$TINY_NDJSON" 0 1 "$lane"
    emit_row "$lane" "$program" "$T_JQF_MEAN" "$T_JQF_STD" "$T_JQ_MEAN" "$T_JAQ_MEAN" "$T_GOJQ_MEAN" "$T_DUCKDB_MEAN" "$T_CHL_MEAN" "$T_RSS_BYTES" "$T_RSS" "$T_RECLAIMS" "$T_USER_S" "$T_SYS_S" "$T_MINFLT" "$T_MAJFLT" "$(combine_note "$note" "$T_NOTE")"
}
run_tiny_decode_lane

FAILURE_COUNT="$(wc -l <"$FAILLOG" | tr -d ' ')"
EXCLUSION_COUNT="$(wc -l <"$EXCLUSIONLOG" | tr -d ' ')"

if [ "$JSON_MODE" -eq 0 ]; then
    printf '%s\n' "------------------------------------------------------------------------------------------------------------------------"
    printf 'rows=%d failures=%d exclusions=%d\n' "$ROW_COUNT" "$FAILURE_COUNT" "$EXCLUSION_COUNT"
    printf 'A BLANK CELL IS NOT "THE TOOL WAS SLOWER": it is absent (not installed),\n'
    printf 'n/a (no fair-equivalent expression for the lane), disagreed (output failed\n'
    printf 'validation, so it was never timed), or excluded (declared in JQF_E2E_EXCLUDE).\n'
    printf 'Agreement exemptions in force: L6 when jq rejects depth 500; L8 (no output,\n'
    printf 'exit-class contract); jaq/gojq divergence from jq is informational, never an\n'
    printf 'assertion; duckdb/chl are validated at value level, not byte level.\n'
    printf 'duckdb/chl are absent outside L4/L5 BY CONSTRUCTION — no fair equivalent.\n'
    if [ "$EXCLUSION_COUNT" -gt 0 ]; then
        printf 'excluded cells:\n'
        while IFS= read -r e; do
            printf '  - %s\n' "$e"
        done <"$EXCLUSIONLOG"
    fi
    if [ "$FAILURE_COUNT" -gt 0 ]; then
        printf 'failed assertions:\n'
        while IFS= read -r f; do
            printf '  - %s\n' "$f"
        done <"$FAILLOG"
    fi
else
    # The legend travels with the JSON too. A consumer that reads only the
    # rows would otherwise have to guess what a null column means.
    python3 -c '
import json, sys
rows, failures, exclusions, provenance = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
print(json.dumps({
    "summary": {
        "rows": rows,
        "failures": failures,
        "exclusions": exclusions,
        "jqf_provenance": provenance,
        "blank_cell_law": "a null column is absent / n-a / disagreed / excluded, NOT \"the tool was slower\"",
        "agreement_exemptions": [
            "L6: exempt only when jq rejects depth 500; asserted whenever jq exits 0",
            "L8: no output — the contract is the exit class",
            "jaq/gojq: divergence from jq is informational, never an assertion",
            "duckdb/chl: value-level agreement, not byte-level",
            "duckdb/chl: absent outside L4/L5 by construction — no fair equivalent",
        ],
        "excluded_cells": [l.strip() for l in open(sys.argv[5]) if l.strip()],
        "failed_assertions": [l.strip() for l in open(sys.argv[6]) if l.strip()],
    }
}))
' "$ROW_COUNT" "$FAILURE_COUNT" "$EXCLUSION_COUNT" "$PROVENANCE_LINE" "$EXCLUSIONLOG" "$FAILLOG"
fi

[ "$FAILURE_COUNT" -eq 0 ]
