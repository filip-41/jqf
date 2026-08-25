#!/usr/bin/env bash
#
# The PGO freshness gate: the measurement binary must PROVE it is not older
# than the code being measured.
#
# `jqf --diagnostics` prints one provenance line on every request:
#     jqf: build=pgo profile=w<workload>.<triple>.<commit>.<digest> allocator=...
# The profile id carries the commit the profile was TRAINED at — the commit
# whose code the instrumentation ran. This gate fails when that commit is not
# HEAD: a binary trained before the last code-touching commit answers every
# query correctly and is merely slower (the one artifact in this tree that can
# rot with no gate noticing), and every benchmark claim is supposed to ride on
# this binary.
#
# The comparison is the freshness law, exactly: the profile's commit against the
# newest code-touching commit. A `-dirty` suffix means the profile was trained
# on an uncommitted tree — that marker stays in the receipt (it is part of the
# provenance a measurement pastes next to its numbers) but only the commit is
# judged, because the dirty state of a build cannot be compared to anything.
#
# Usage: tools/jqf-pgo-freshness.sh
#   JQF_PGO_BIN   the binary to judge (default: target/pgo/jqf)
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${JQF_PGO_BIN:-$ROOT/target/pgo/jqf}"

[ -x "$BIN" ] || {
    echo "error: no PGO binary at $BIN — run \`make\` before measuring" >&2
    exit 1
}

line="$("$BIN" --diagnostics . </dev/null 2>&1 >/dev/null | sed -n 's/^jqf: build=/build=/p' | head -1)"
case "$line" in
    "build=pgo profile=w"*) ;;
    "build=plain"*)
        echo "error: $BIN is a PLAIN release build (build=plain) — benchmarks must come from the PGO binary; run \`make\`" >&2
        exit 1
        ;;
    *)
        echo "error: $BIN prints no provenance line ([$line]) — it is not a jqf built by tools/jqf-pgo-build.sh" >&2
        exit 1
        ;;
esac

profile="${line#build=pgo profile=}"
trained="$(printf '%s' "$profile" | cut -d. -f3)"
trained="${trained%-dirty}"
head="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo nogit)"

if [ "$trained" != "$head" ]; then
    echo "error: $BIN's profile was trained at $trained, but HEAD is $head — the PGO binary predates the last code-touching commit; run \`make\` before measuring" >&2
    exit 1
fi

echo "pgo-fresh: binary=$BIN profile=$profile trained=$trained head=$head GREEN"
