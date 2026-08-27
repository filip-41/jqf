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
# without any gate noticing. So train.py + cases.json pgo flags are the durable
# artifact and the profile is regenerated. `make bench` builds this binary
# first. Plain `cargo build --release` stays PGO-free, and nobody has to run
# this script to get a working jqf.
#
# The binary says which it is. `jqf --diagnostics` prints
# `build=pgo profile=<id>` (or `build=plain profile=none`), where `<id>` is
# train_hash.code_hash.triple.profdata — hashes of the pgo case set and the
# product crates, the target triple, and a digest of the merged profile.
#
# Usage: tools/pgo/jqf-pgo-build.sh
#   JQF_PGO_OUT      where to write the optimized binary
#                    (default: target/pgo/jqf — never target/release/jqf, which
#                    the next ordinary `cargo build` would silently overwrite)
#   JQF_PGO_DIR      working directory for raw counters and the merged profile
#                    (default: target/pgo)
#   JQF_PGO_TARGET   natively runnable target triple (default: rustc host)
#   LLVM_PROFDATA    override the discovered llvm-profdata
#   JQF_E2E_FIXDIR   reuse/persist the generated fixtures (legacy name)
#                    (default: a fresh mktemp directory, removed on exit)
#   JQF_PGO_KEEP_RAW keep the raw .profraw files instead of removing them
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO_CMD="${CARGO:-cargo}"
RUSTC_CMD="${RUSTC:-rustc}"

PGO_DIR="${JQF_PGO_DIR:-$ROOT/target/pgo}"
RAW_DIR="$PGO_DIR/raw"
PROFDATA="$PGO_DIR/jqf.profdata"
OUT="${JQF_PGO_OUT:-$PGO_DIR/jqf}"
CARGO_TARGET_ROOT="${CARGO_TARGET_DIR:-$ROOT/target}"

# Workload is tools/pgo/train.py: n=25000, cases.json pgo=true, pgo_width.
WORKLOAD_REPEATS=2

fail() {
    echo "error: $*" >&2
    exit 2
}

sha256_file() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        return 1
    fi
}

case "$RAW_DIR:$PROFDATA" in
    *[[:space:]]*) fail "PGO paths cannot contain whitespace: $PGO_DIR" ;;
esac

# --- llvm-profdata discovery ---------------------------------------------
#
# Order: LLVM_PROFDATA override, rustc sysroot (exact LLVM match), PATH,
# then xcrun. The raw counter format is an LLVM version contract: the merger
# must be at least as new as the rustc that wrote the counters. rustup's
# llvm-tools is the exact match; PATH and xcrun can be older and fail as a
# version mismatch, not an unreadable file.
discover_llvm_profdata() {
    if [ -n "${LLVM_PROFDATA:-}" ]; then
        [ -x "$LLVM_PROFDATA" ] || fail "LLVM_PROFDATA=$LLVM_PROFDATA is not executable"
        echo "$LLVM_PROFDATA"
        return 0
    fi

    local sysroot host candidate
    sysroot="$("$RUSTC_CMD" --print sysroot 2>/dev/null || true)"
    host="$("$RUSTC_CMD" -vV 2>/dev/null | sed -n 's/^host: //p')"
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
      LLVM_PROFDATA=/path/to/llvm-profdata tools/pgo/jqf-pgo-build.sh
MISSING
    exit 2
}

LLVM_PROFDATA="$(discover_llvm_profdata)" || exit 2

HOST_TRIPLE="$("$RUSTC_CMD" -vV | sed -n 's/^host: //p')"
[ -n "$HOST_TRIPLE" ] || fail "cannot determine the host target triple from rustc -vV"

# `--target` is not decoration: without it, RUSTFLAGS also apply to build
# scripts, so `-Cprofile-generate` would instrument mimalloc's build script and
# scatter its counters through the profile. With it, host artifacts are built
# clean and only the jqf binary is instrumented.
TARGET_TRIPLE="${JQF_PGO_TARGET:-$HOST_TRIPLE}"
[ "$TARGET_TRIPLE" = "$HOST_TRIPLE" ] \
    || fail "cross-target PGO is unsupported: workload execution requires $HOST_TRIPLE, got $TARGET_TRIPLE"
TARGET_BIN="$CARGO_TARGET_ROOT/$TARGET_TRIPLE/release/jqf"

# Linux release is the system allocator; macOS keeps mimalloc. The flag is a
# whole-program allocator property, so it rides the same guarded build lines
# below; the host guard leaves the macOS build on mimalloc.
EXTRA_FEATURES=""
[ "$(uname -s)" = "Linux" ] && EXTRA_FEATURES="--no-default-features"

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

# --- phase 1: instrumented build -----------------------------------------
rm -rf "$RAW_DIR"
mkdir -p "$RAW_DIR"
echo "jqf-pgo-build: [1/4] instrumented build ($TARGET_TRIPLE)" >&2
phase_start="$(date +%s)"
RUSTFLAGS="-Cprofile-generate=$RAW_DIR" \
    "$CARGO_CMD" build --release -p jqf $EXTRA_FEATURES ${CARGO_FLAGS:-} --target "$TARGET_TRIPLE" --manifest-path "$ROOT/Cargo.toml" \
    || fail "instrumented build failed"
[ -x "$TARGET_BIN" ] || fail "instrumented binary missing at $TARGET_BIN"
instrumented_seconds=$(( $(date +%s) - phase_start ))

# --- phase 2: the workload ------------------------------------------------
echo "jqf-pgo-build: [2/4] profile workload (${WORKLOAD_REPEATS} passes, n=25000 pgo cases)" >&2
phase_start="$(date +%s)"
export LLVM_PROFILE_FILE="$RAW_DIR/jqf-%p.profraw"
train_out="$(python3 "$ROOT/tools/pgo/train.py" --jqf "$TARGET_BIN" --fixtures "$FIXDIR" --repeats "$WORKLOAD_REPEATS")" \
    || fail "pgo workload failed"
read -r lane_runs TRAIN_HASH CODE_HASH <<EOF
$train_out
EOF
[ -n "$TRAIN_HASH" ] && [ -n "$CODE_HASH" ] || fail "train.py printed no hashes: [$train_out]"
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
  counters (rustc $("$RUSTC_CMD" --version | cut -d' ' -f2), LLVM $("$RUSTC_CMD" -vV | sed -n 's/^LLVM version: //p')).
  Install the exact match and re-run:
      rustup component add llvm-tools-preview
MISSING
    exit 2
fi
[ -s "$PROFDATA" ] || fail "merged profile $PROFDATA is empty"

# --- the profile identity -------------------------------------------------
# Four facts, because staleness has four causes: the training cases moved, the
# product crates moved, the machine changed, or the profile itself is not the
# one you think it is.
profile_sha="$(sha256_file "$PROFDATA")" || fail "no SHA-256 tool (shasum or sha256sum)"
[ -n "$profile_sha" ] || fail "SHA-256 tool returned an empty digest for $PROFDATA"
profile_digest="$(printf '%.8s' "$profile_sha")"
PROFILE_ID="$TRAIN_HASH.$CODE_HASH.$TARGET_TRIPLE.$profile_digest"

# --- phase 4: optimized build --------------------------------------------
echo "jqf-pgo-build: [4/4] optimized build (profile $PROFILE_ID)" >&2
phase_start="$(date +%s)"
JQF_PGO_PROFILE="$PROFILE_ID" \
    RUSTFLAGS="-Cprofile-use=$PROFDATA" \
    "$CARGO_CMD" build --release -p jqf $EXTRA_FEATURES ${CARGO_FLAGS:-} --target "$TARGET_TRIPLE" --manifest-path "$ROOT/Cargo.toml" \
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
printf 'jqf-pgo-build: profile=%s train=%s code=%s lanes=%s raw=%s profdata_bytes=%s binary=%s instrumented_s=%s workload_s=%s optimized_s=%s\n' \
    "$PROFILE_ID" "$TRAIN_HASH" "$CODE_HASH" "$lane_runs" "$raw_count" "$profdata_bytes" \
    "$OUT" "$instrumented_seconds" "$workload_seconds" "$optimized_seconds"
