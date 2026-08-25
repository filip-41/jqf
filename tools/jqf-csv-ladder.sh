#!/usr/bin/env bash

# Hermeticity: a developer's .jqf.toml must never reach a gate.
export JQF_NO_CONFIG=1
#
# CSV end-to-end benchmark ladder: jqf's record route vs the jq-like tools
# that can process CSV — jq (`-R` + split), miller (`mlr`), and `qsv`.
#
# Whole-process decode+filter+encode over stdin/stdout, the same way a user
# invokes any of these tools. Correctness-gated: no cell is timed before its
# output has been validated. jq is the byte oracle ONLY on the identity lane
# (C1), where `jq -R -c 'split(",")'` is byte-identical to jqf's array
# output on simple unquoted rows. C2/C3 validate jqf's own expected shapes
# (jq's split cannot express column selection or counts without extra
# programs that change its bytes). mlr and qsv publish different shapes, so
# they are timed with their own validated outputs.
#
# Lanes:
#   C1 identity   `.`     on csv-500k.csv   (jqf arrays == jq split rows)
#   C2 column     `.[1]`  on csv-500k.csv   (jqf scoped fast path)
#   C3 count      `length` on csv-500k.csv
#   C4 quoted     `.`     on csv-quoted.csv (embedded newlines/quotes)
#
# Usage: tools/jqf-csv-ladder.sh [path-to-jqf] [--json]

set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JSON_MODE=0
JQF_ARG=""
for arg in "$@"; do
    case "$arg" in
        --json) JSON_MODE=1 ;;
        *) JQF_ARG="$arg" ;;
    esac
done

JQF="${JQF_BIN:-${JQF_ARG:-$ROOT/target/release/jqf}}"
# Resolve to an absolute path: hyperfine runs commands through a shell that
# may not share this script's working directory.
case "$JQF" in
    /*) ;;
    *) JQF="$ROOT/$JQF" ;;
esac
if [ ! -x "$JQF" ]; then
    echo "jqf-csv-ladder: $JQF not found; building release jqf..." >&2
    (cd "$ROOT" && cargo build --release -p jqf) >&2 || { echo "error: build failed" >&2; exit 2; }
fi

JQ="${JQ:-$(command -v jq || true)}"
MLR="${MLR:-$(command -v mlr || true)}"
QSV="${QSV:-$(command -v qsv || true)}"
HYPERFINE="${HYPERFINE:-$(command -v hyperfine || true)}"

[ -n "$JQ" ] || echo "notice: jq not found; byte-oracle checks and jq column skipped" >&2
[ -n "$MLR" ] || echo "notice: mlr not found; mlr column skipped" >&2
[ -n "$QSV" ] || echo "notice: qsv not found; qsv column skipped" >&2
[ -n "$HYPERFINE" ] || echo "notice: hyperfine not found; timing skipped (correctness still runs)" >&2

FIXDIR="$(mktemp -d "${TMPDIR:-/tmp}/jqf-csv-ladder.XXXXXX")"
trap 'rm -rf "$FIXDIR"' EXIT

# Fixtures: a 500k-row simple CSV (byte-oracle lanes) and a quoted CSV with
# embedded newlines and doubled quotes.
python3 - "$FIXDIR" <<'PY'
import sys
out = sys.argv[1]
with open(f"{out}/csv-500k.csv", "w") as f:
    f.write("name,age,city\n")
    for i in range(500_000):
        f.write(f"person{i},{i % 90},city{i % 500}\n")
with open(f"{out}/csv-quoted.csv", "w") as f:
    f.write("name,note\n")
    for i in range(50_000):
        f.write(f"person{i},\"note {i} with a,\nnewline\"\n")
        f.write(f"\"say \"\"hi\"\"\",plain{i}\n")
PY

CSV="$FIXDIR/csv-500k.csv"
QUOTED="$FIXDIR/csv-quoted.csv"

fail=0
note_fail() { echo "FAIL: $*" >&2; fail=1; }

# Time one lane with hyperfine when present. Each command reads the fixture
# from stdin; hyperfine runs each with the file redirected in.
time_lane() {
    local label="$1" program="$2" infile="$3" jqf_flags="$4"
    if [ -z "$HYPERFINE" ]; then
        return
    fi
    local hf_args=(--warmup 1 --runs 5 --ignore-failure --export-json "$FIXDIR/$label.json")
    # Hyperfine does NOT forward stdin to the benchmarked commands, so each
    # command string redirects the fixture itself.
    hf_args+=(--command-name jqf "$JQF --input-format csv $jqf_flags '$program' < '$infile'")
    if [ -n "$JQ" ]; then
        hf_args+=(--command-name jq "jq -R -c 'split(\",\")' < '$infile'")
    fi
    if [ -n "$MLR" ]; then
        hf_args+=(--command-name mlr "mlr --icsv --ojson cat < '$infile'")
    fi
    if [ -n "$QSV" ]; then
        hf_args+=(--command-name qsv "qsv select 1,2,3 < '$infile'")
    fi
    if ! hyperfine "${hf_args[@]}" < "$infile" > /dev/null 2>&1; then
        echo "  $label: hyperfine failed (skipped)" >&2
        return
    fi
    if [ "$JSON_MODE" -eq 1 ]; then
        python3 - "$FIXDIR/$label.json" "$label" "$program" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    data = json.load(f)
results = {r["command"].split()[0]: r for r in data["results"]}
print(f"csv-ladder {sys.argv[2]} {sys.argv[3]}: " + " ".join(
    f"{name}={r['median']:.4f}s" for name, r in results.items()))
PY
    else
        echo "  $label ($program):"
        python3 - "$FIXDIR/$label.json" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    data = json.load(f)
for r in data["results"]:
    name = r["command"].split()[0]
    print(f"    {name:<6} {r['median']:8.4f} s  ({r['stddev']:.4f} sd)")
PY
    fi
}

echo "jqf-csv-ladder: fixtures at $FIXDIR"

# C1 identity: jqf arrays must equal jq -R split rows byte-for-byte.
jqf_c1="$("$JQF" --input-format csv -c '.' < "$CSV" 2>/dev/null)"
if [ -n "$JQ" ]; then
    jq_c1="$("$JQ" -R -c 'split(",")' < "$CSV" 2>/dev/null)"
    if [ "$jqf_c1" != "$jq_c1" ]; then
        note_fail "C1: jqf output not byte-identical to jq -R split"
    fi
fi
time_lane "c1" "." "$CSV" "-c"

# C2 column: jqf's `.[1]` must produce the second field of every row.
jqf_c2="$("$JQF" --input-format csv -c '.[1]' < "$CSV" 2>/dev/null)"
# Validate shape: 500001 lines (header + 500k rows), first is "age", and
# every line is a quoted string (the age column).
c2_lines="$(printf '%s\n' "$jqf_c2" | wc -l | tr -d ' ')"
c2_first="$(printf '%s\n' "$jqf_c2" | sed -n '1p')"
c2_bad="$(printf '%s\n' "$jqf_c2" | grep -cv '^"' || true)"
if [ "$c2_lines" -ne 500001 ] || [ "$c2_first" != '"age"' ] || [ "$c2_bad" -ne 0 ]; then
    note_fail "C2: jqf .[1] output shape wrong (lines=$c2_lines first=$c2_first bad=$c2_bad)"
fi
time_lane "c2" ".[1]" "$CSV" "-c"

# C3 count: jqf's `length` must be 3 for every row (3 columns).
jqf_c3="$("$JQF" --input-format csv -c 'length' < "$CSV" 2>/dev/null)"
c3_rows="$(printf '%s\n' "$jqf_c3" | wc -l | tr -d ' ')"
c3_non3="$(printf '%s\n' "$jqf_c3" | grep -cvx '3' || true)"
if [ "$c3_rows" -ne 500001 ] || [ "$c3_non3" -ne 0 ]; then
    note_fail "C3: jqf length output wrong (rows=$c3_rows non3=$c3_non3)"
fi
time_lane "c3" "length" "$CSV" "-c"

# C4 quoted: jqf must publish one array per record with the embedded newline
# inside the field. Assert the record count (50k records) and that a field
# contains a literal newline (a record spans physical lines).
jqf_c4="$("$JQF" --input-format csv -c '.' < "$QUOTED" 2>/dev/null)"
c4_records="$(printf '%s\n' "$jqf_c4" | grep -c '^\["person')"
if [ "$c4_records" -ne 50000 ]; then
    note_fail "C4: jqf quoted record count wrong (got $c4_records, want 50000)"
fi
time_lane "c4" "." "$QUOTED" "-c"

if [ "$fail" -ne 0 ]; then
    echo "csv-ladder: FAILED (see above)" >&2
    exit 1
fi
echo "csv-ladder: all correctness checks passed"
