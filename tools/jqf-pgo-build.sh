#!/usr/bin/env bash
#
# Build the `jqf` binary with profile-guided optimization, end to end.
#
# One job, four phases: build an INSTRUMENTED jqf, run the committed profile
# WORKLOAD through it, MERGE the raw counters into one profile, and build the
# OPTIMIZED jqf against that profile. It is worth the machinery — PGO is the
# single largest build-time win on this tree by a wide margin over `fat` LTO
# alone.
#
# THE PROFILE IS NOT COMMITTED; THIS RECIPE IS. A `.profdata` blob is specific
# to one architecture AND one rustc's LLVM version, and it goes stale silently
# as hot paths move — it would be the one artifact in this tree that can rot
# without any gate noticing. So the workload below is the durable artifact and
# the profile is regenerated. The consequence is that this script is safe to
# run as often as needed: every benchmark is taken from the freshly built
# target/pgo/jqf. Plain `cargo build --release` stays PGO-free, and nobody has
# to run this script to get a working jqf.
#
# The binary says which it is. `jqf --diagnostics` prints
# `build=pgo profile=<id>` (or `build=plain profile=none`), where `<id>` is the
# stamp this script computes below: workload version, host triple, the commit
# the profile was trained at, and a digest of the profile itself.
#
# Usage: tools/jqf-pgo-build.sh
#   JQF_PGO_OUT      where to write the optimized binary
#                    (default: target/pgo/jqf — never target/release/jqf, which
#                    the next ordinary `cargo build` would silently overwrite)
#   JQF_PGO_DIR      working directory for raw counters and the merged profile
#                    (default: target/pgo)
#   JQF_PGO_TARGET   target triple to build for (default: the rustc host triple)
#                    — a cross build, e.g. x86_64-unknown-linux-musl, lands under
#                    target/<triple>/ and stamps its OWN triple in the profile id
#   LLVM_PROFDATA    override the discovered llvm-profdata
#   JQF_E2E_FIXDIR   reuse/persist the generated fixtures at this path
#                    (default: a fresh mktemp directory, removed on exit)
#   JQF_PGO_KEEP_RAW keep the raw .profraw files instead of removing them
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Bump when a WORKLOAD line below changes. It rides in the profile identity, so
# a binary always names the workload version it was trained on.
WORKLOAD_VERSION="2"

PGO_DIR="${JQF_PGO_DIR:-$ROOT/target/pgo}"
RAW_DIR="$PGO_DIR/raw"
PROFDATA="$PGO_DIR/jqf.profdata"
OUT="${JQF_PGO_OUT:-$PGO_DIR/jqf}"

# THE PROFILE WORKLOAD: one line per training run, `fixture<TAB>flags<TAB>program`,
# where the flags field is `-` when a lane takes none (tab is IFS whitespace, so
# an empty middle field would collapse into its neighbour).
#
# It trains the ROUTES, not the programs. A profile that never exercised a
# route hands that route the cold-path layout. JSON: whole-document identity
# (plain and escape-heavy), locate, element-stream, count, fold, descent,
# slices, record serial and parallel. Then one identity/path lane each for
# YAML, TOML, XML, CSV, and one `--edit` splice. Same-route extra programs
# add nothing — this is a profile, not a benchmark.
#
# `--input-format ndjson` lines with no `--no-parallel` train the worker path,
# which is the only way the morsel drive's code ever sees a counter.
WORKLOAD=$(cat <<'LANES'
catalog-10mb.json	-	.
escape-10mb.json	-	.
small-29kb.json	-	.
catalog-10mb.json	-	.meta.count
catalog-10mb.json	-	.catalog[500].name
catalog-10mb.json	-	.catalog[0].id
catalog-10mb.json	-	.catalog[] | .name
catalog-10mb.json	-	[.catalog[].name] | length
catalog-10mb.json	-	[.catalog[]] | length
catalog-10mb.json	-	reduce .catalog[].stock as $s (0; . + $s)
catalog-10mb.json	-	reduce .. as $x (0; . + 1)
catalog-10mb.json	-	[..] | length
catalog-10mb.json	-	.catalog[100:110]
catalog-10mb.json	-	.catalog[1000:20000] | length
ndjson-200k.ndjson	--input-format ndjson --no-parallel	.v
ndjson-200k.ndjson	--input-format ndjson	.v
ndjson-200k.ndjson	--input-format ndjson --output-format ndjson	.v
yaml-catalog-10mb.yaml	--input-format yaml	.
yaml-catalog-10mb.yaml	--input-format yaml	.catalog[0].name
toml-catalog-10mb.toml	--input-format toml	.
toml-catalog-10mb.toml	--input-format toml	.catalog[0].name
xml-catalog-10mb.xml	--input-format xml	.
csv-wide-40col.csv	--input-format csv	.[0]
catalog-10mb.json	--edit	.meta.count = 0
LANES
)

# Repeats per lane. Two, not one: the instrumented binary is slow (~2-3x), and
# a second pass costs a minute while making a lane whose counters were skewed
# by a one-off page fault less likely to mislead the optimizer.
WORKLOAD_REPEATS=2

fail() {
    echo "error: $*" >&2
    exit 2
}

# --- llvm-profdata discovery ---------------------------------------------
#
# Order matters, and it is not "whatever is on PATH". The raw counter format is
# an LLVM version contract: the profile must be merged by a tool whose LLVM is
# at least as new as the rustc that wrote the counters. rustup's own
# llvm-tools is by construction the exact match, so it is tried FIRST; the
# Command Line Tools copy (which `xcrun` finds, and which is NOT on PATH on a
# stock macOS) is the fallback and usually works, but it can be older than the
# toolchain. A version mismatch is reported as itself, with the fix, rather
# than as an unreadable-file error.
discover_llvm_profdata() {
    if [ -n "${LLVM_PROFDATA:-}" ]; then
        [ -x "$LLVM_PROFDATA" ] || fail "LLVM_PROFDATA=$LLVM_PROFDATA is not executable"
        echo "$LLVM_PROFDATA"
        return 0
    fi

    local sysroot host candidate
    sysroot="$(rustc --print sysroot 2>/dev/null || true)"
    host="$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')"
    if [ -n "$sysroot" ] && [ -n "$host" ]; then
        candidate="$sysroot/lib/rustlib/$host/bin/llvm-profdata"
        if [ -x "$candidate" ]; then
            echo "$candidate"
            return 0
        fi
    fi

    candidate="$(command -v llvm-profdata 2>/dev/null || true)"
    if [ -n "$candidate" ]; then
        echo "$candidate"
        return 0
    fi

    candidate="$(xcrun -f llvm-profdata 2>/dev/null || true)"
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then
        echo "$candidate"
        return 0
    fi

    cat >&2 <<'MISSING'
error: llvm-profdata not found, and PGO cannot merge raw counters without it.

  Preferred (exact LLVM version match with your rustc):
      rustup component add llvm-tools-preview

  On macOS it also ships with the Command Line Tools but is NOT on PATH:
      xcrun -f llvm-profdata          # prints the path if present
      xcode-select --install          # if that prints nothing

  Or point this script at one:
      LLVM_PROFDATA=/path/to/llvm-profdata tools/jqf-pgo-build.sh
MISSING
    exit 2
}

LLVM_PROFDATA="$(discover_llvm_profdata)" || exit 2

HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
[ -n "$HOST_TRIPLE" ] || fail "cannot determine the host target triple from rustc -vV"

# `--target` is not decoration: without it, RUSTFLAGS also apply to build
# scripts, so `-Cprofile-generate` would instrument mimalloc's build script and
# scatter its counters through the profile. With it, host artifacts are built
# clean and only the jqf binary is instrumented.
TARGET_TRIPLE="${JQF_PGO_TARGET:-$HOST_TRIPLE}"
TARGET_BIN="$ROOT/target/$TARGET_TRIPLE/release/jqf"

# The linux release binary is the SYSTEM-ALLOCATOR build (measured 12-38 %
# faster in the linux-arm64 container, zero RSS delta; macOS keeps mimalloc,
# where measurement showed noise and `record` favors it). The flag is a
# whole-program allocator property, so it rides the same guarded build lines
# below; the host guard keeps the macOS build on mimalloc untouched.
EXTRA_FEATURES=""
[ "$(uname -s)" = "Linux" ] && EXTRA_FEATURES="--no-default-features"

# Linux-aarch64 release builds enable the LSE atomics
# (`-Ctarget-feature=+lse`, measured +2.5 %; the baseline aarch64-unknown-
# linux-gnu target ships pre-LSE armv8-a and emits a call for every atomic,
# 10 LSE ops vs 1269 with the feature). The guard is the TARGET TRIPLE, never
# the host: macOS builds are aarch64-apple-darwin (which already carries lse)
# and must not be touched. This lives in the RUSTFLAGS here rather than in a
# `.cargo/config.toml` `[target.*]` rustflags entry because the two phases
# below set `RUSTFLAGS` themselves, and cargo ignores config rustflags
# whenever the environment variable is set — a config entry would silently
# not reach the instrumented OR the optimized build.
TARGET_FEATURES=""
if [ "$TARGET_TRIPLE" = "aarch64-unknown-linux-gnu" ]; then
    TARGET_FEATURES=" -Ctarget-feature=+lse"
fi

FIXDIR="${JQF_E2E_FIXDIR:-}"
CLEANUP_FIXDIR=0
if [ -z "$FIXDIR" ]; then
    FIXDIR="$(mktemp -d "${TMPDIR:-/tmp}/jqf-pgo-fixtures.XXXXXX")"
    CLEANUP_FIXDIR=1
fi
cleanup() {
    [ "$CLEANUP_FIXDIR" -eq 1 ] && rm -rf "$FIXDIR"
    if [ -z "${JQF_PGO_KEEP_RAW:-}" ]; then
        rm -rf "$RAW_DIR"
    fi
}
trap cleanup EXIT

mkdir -p "$FIXDIR" "$PGO_DIR"
python3 "$ROOT/tools/jqf-e2e-fixtures.py" "$FIXDIR" || fail "fixture generation failed"

# --- phase 1: instrumented build -----------------------------------------
rm -rf "$RAW_DIR"
mkdir -p "$RAW_DIR"
echo "jqf-pgo-build: [1/4] instrumented build ($TARGET_TRIPLE)" >&2
phase_start="$(date +%s)"
RUSTFLAGS="-Cprofile-generate=$RAW_DIR$TARGET_FEATURES" \
    cargo build --release -p jqf $EXTRA_FEATURES --target "$TARGET_TRIPLE" --manifest-path "$ROOT/Cargo.toml" \
    || fail "instrumented build failed"
[ -x "$TARGET_BIN" ] || fail "instrumented binary missing at $TARGET_BIN"
instrumented_seconds=$(( $(date +%s) - phase_start ))

# --- phase 2: the workload ------------------------------------------------
echo "jqf-pgo-build: [2/4] profile workload (v$WORKLOAD_VERSION, ${WORKLOAD_REPEATS} passes)" >&2
phase_start="$(date +%s)"
export LLVM_PROFILE_FILE="$RAW_DIR/jqf-%p.profraw"
lane_runs=0
pass=1
while [ "$pass" -le "$WORKLOAD_REPEATS" ]; do
    while IFS="$(printf '\t')" read -r fixture flags program; do
        [ -n "$fixture" ] || continue
        [ "$flags" = "-" ] && flags=""
        input="$FIXDIR/$fixture"
        [ -f "$input" ] || fail "workload fixture $fixture missing at $input"
        # shellcheck disable=SC2086 -- flags is a deliberate word-split lane field
        if ! "$TARGET_BIN" $flags "$program" <"$input" >/dev/null; then
            fail "workload lane failed: [$flags] $program < $fixture"
        fi
        lane_runs=$((lane_runs + 1))
    done <<<"$WORKLOAD"
    pass=$((pass + 1))
done
unset LLVM_PROFILE_FILE
workload_seconds=$(( $(date +%s) - phase_start ))

raw_count="$(find "$RAW_DIR" -name '*.profraw' | wc -l | tr -d ' ')"
[ "$raw_count" -gt 0 ] || fail "the workload produced no .profraw files; the instrumented build did not instrument"

# --- phase 3: merge -------------------------------------------------------
echo "jqf-pgo-build: [3/4] merging $raw_count raw profiles" >&2
rm -f "$PROFDATA"
if ! find "$RAW_DIR" -name '*.profraw' -print0 \
    | xargs -0 "$LLVM_PROFDATA" merge -o "$PROFDATA"; then
    cat >&2 <<MISSING
error: llvm-profdata merge failed ($LLVM_PROFDATA).

  The usual cause is an LLVM version older than the rustc that wrote the raw
  counters (rustc $(rustc --version | cut -d' ' -f2), LLVM $(rustc -vV | sed -n 's/^LLVM version: //p')).
  Install the exact match and re-run:
      rustup component add llvm-tools-preview
MISSING
    exit 2
fi
[ -s "$PROFDATA" ] || fail "merged profile $PROFDATA is empty"

# --- the profile identity -------------------------------------------------
# Four facts, because staleness has four causes: the workload changed, the
# machine changed, the code moved under the profile, or the profile itself is
# not the one you think it is.
profile_digest="$(shasum -a 256 "$PROFDATA" | cut -c1-8)"
commit="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo nogit)"
if ! git -C "$ROOT" diff --quiet HEAD 2>/dev/null; then
    commit="$commit-dirty"
fi
PROFILE_ID="w$WORKLOAD_VERSION.$TARGET_TRIPLE.$commit.$profile_digest"

# --- phase 4: optimized build --------------------------------------------
echo "jqf-pgo-build: [4/4] optimized build (profile $PROFILE_ID)" >&2
phase_start="$(date +%s)"
JQF_PGO_PROFILE="$PROFILE_ID" \
    RUSTFLAGS="-Cprofile-use=$PROFDATA$TARGET_FEATURES" \
    cargo build --release -p jqf $EXTRA_FEATURES --target "$TARGET_TRIPLE" --manifest-path "$ROOT/Cargo.toml" \
    || fail "optimized build failed"
optimized_seconds=$(( $(date +%s) - phase_start ))

mkdir -p "$(dirname "$OUT")"
cp "$TARGET_BIN" "$OUT" || fail "cannot copy $TARGET_BIN to $OUT"

# The receipt asserts the binary agrees, rather than trusting that the flag was
# honoured: a profile rustc silently ignored would otherwise ship as `pgo`.
stamped="$("$OUT" --diagnostics . </dev/null 2>&1 >/dev/null | sed -n 's/^jqf: build=/build=/p' | head -1)"
case "$stamped" in
    "build=pgo profile=$PROFILE_ID"*) ;;
    *) fail "the built binary reports [$stamped], not the profile it was built with ($PROFILE_ID)" ;;
esac

profdata_bytes="$(wc -c <"$PROFDATA" | tr -d ' ')"
printf 'jqf-pgo-build: profile=%s workload_v=%s lanes=%s raw=%s profdata_bytes=%s binary=%s instrumented_s=%s workload_s=%s optimized_s=%s\n' \
    "$PROFILE_ID" "$WORKLOAD_VERSION" "$lane_runs" "$raw_count" "$profdata_bytes" \
    "$OUT" "$instrumented_seconds" "$workload_seconds" "$optimized_seconds"
