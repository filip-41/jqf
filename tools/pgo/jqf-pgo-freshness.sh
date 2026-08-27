#!/usr/bin/env bash
#
# The PGO freshness gate: the measurement binary must PROVE it is not older
# than the code being measured.
#
# `jqf --diagnostics` prints one provenance line on every request:
#     jqf: build=pgo profile=<train>.<code>.<triple>.<digest> allocator=...
# `<code>` is train.py's hash of the product crates. This gate fails when that
# hash is not the current tree: a binary trained on older sources answers every
# query correctly and is merely slower.
#
# Usage: tools/jqf-pgo-freshness.sh
#   JQF_PGO_BIN   the binary to judge (default: target/pgo/jqf)
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${JQF_PGO_BIN:-$ROOT/target/pgo/jqf}"

[ -x "$BIN" ] || {
    echo "error: no PGO binary at $BIN — run \`make pgo\` before measuring" >&2
    exit 1
}

line="$("$BIN" --diagnostics . </dev/null 2>&1 >/dev/null | sed -n 's/^jqf: build=/build=/p' | head -1)"
case "$line" in
    "build=pgo profile="*) ;;
    "build=plain"*)
        echo "error: $BIN is a PLAIN release build (build=plain) — benchmarks must come from the PGO binary; run \`make pgo\`" >&2
        exit 1
        ;;
    *)
        echo "error: $BIN prints no provenance line ([$line]) — it is not a jqf built by tools/pgo/jqf-pgo-build.sh" >&2
        exit 1
        ;;
esac

profile="${line#build=pgo profile=}"
profile="${profile%% *}"
trained_workload="$(printf '%s' "$profile" | cut -d. -f1)"
trained_code="$(printf '%s' "$profile" | cut -d. -f2)"
trained_target="$(printf '%s' "$profile" | cut -d. -f3)"
profile_digest="$(printf '%s' "$profile" | cut -d. -f4)"
hashes="$(python3 "$ROOT/tools/pgo/train.py" --hash)" || exit 2
current_workload="$(printf '%s' "$hashes" | awk '{print $1}')"
current_code="$(printf '%s' "$hashes" | awk '{print $2}')"
[ -n "$trained_workload" ] && [ -n "$trained_code" ] \
    && [ -n "$trained_target" ] && [ -n "$profile_digest" ] \
    && [ -n "$current_workload" ] && [ -n "$current_code" ] || {
    echo "error: malformed profile identity (profile=[$profile] hash=[$hashes])" >&2
    exit 1
}
case "$trained_workload$trained_code$profile_digest" in
    *[!0-9a-f]*) echo "error: malformed profile hashes: $profile" >&2; exit 1 ;;
esac
[ "${#trained_workload}" -eq 8 ] && [ "${#trained_code}" -eq 8 ] && [ "${#profile_digest}" -eq 8 ] || {
    echo "error: malformed profile hashes: $profile" >&2
    exit 1
}

if [ "$trained_workload" != "$current_workload" ]; then
    echo "error: $BIN's training hash is $trained_workload, but the current workload is $current_workload — run \`make pgo\` before measuring" >&2
    exit 1
fi

if [ "$trained_code" != "$current_code" ]; then
    echo "error: $BIN's profile was trained on code $trained_code, but the tree is $current_code — run \`make pgo\` before measuring" >&2
    exit 1
fi

echo "pgo-fresh: binary=$BIN profile=$profile workload=$current_workload code=$current_code GREEN"
