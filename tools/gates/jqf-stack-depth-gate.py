#!/usr/bin/env python3
"""Standing stack-depth gate: the depth guard fires before the Rust stack does.

Nine lanes, each a guarded recursion at its own measured floor. A silent
failure the corpus cannot see: on a machine with spare stack, a guard that
let a recursion run twice as deep as it should would still answer correctly
there and abort somewhere else. This gate removes the spare room.

Each lane runs the SAME shape twice, at ONE stack size pinned to that lane:

  * the under twin, which must SUCCEED — that proves the stack really holds
    this recursion at the pinned depth, so the lane is testing the guard and
    not testing exhaustion;
  * the over case, which must raise the lane's ceiling message — with the
    twin passing at the same size, only the guard can be what stopped it.

Under/over depths are per-lane (see `LANES`): equality, ordering,
object-merge, messagepack-nesting, and path-length are 10000/10001;
toml-nesting is 9999/10000; yaml-nesting and html-nesting are 9998/9999;
program-nesting is 9999 grouped sources vs 50000.

A lane fails on any stack overflow, in either direction. Rust prints
`has overflowed its stack` and aborts, which is distinguishable from both a
clean answer and a raised error.

Lanes run against a DEBUG binary by default because debug frames are the fattest
this tree produces; passing here is the strict statement. Every lane's release
floor is recorded beside its debug one.

HOW A LANE'S FLOOR IS APPLIED. The CLI runs the whole request on a thread of
its OWN, and a thread's stack is mapped independently of `RLIMIT_STACK` —
which is exactly why a `ulimit -s` squeeze reaches nothing at all, making
every pinned floor DECORATIVE while the pair still passed. The knob that
sizes that thread is `JQF_REQUEST_STACK_BYTES` (`request_stack_bytes` in
`jqf-cli/src/main.rs`), and each lane here runs both of its shapes with
`JQF_REQUEST_STACK_BYTES = stack_kib * 1024`, so the pinned floor is the
stack the recursion really gets. There is deliberately NO `ulimit` in the
lane command: it constrains only the main thread, which never recurses, and
a small `RLIMIT_STACK` would starve the process's own startup — the knob is
the whole mechanism, and a lane that lost it would run at the 256 MiB default
and silently prove nothing (that is the failure the teeth probe's floor
corruption exists to catch). The knob's own 64 KiB floor is the gate's lower
bound: a lane whose recursion fits under it is pinned AT 64 KiB and says so,
rather than pretending a lower number was measured.

Receipt line:

    stack-depth-gate: lanes=N pass=N overflows=0 min_stack_kib=N max_stack_kib=N

Usage:
    tools/gates/jqf-stack-depth-gate.py [path-to-jqf] [--verbose]
"""

import os
from jqfgate import proc
import subprocess
import sys

proc.set_hermetic()

ROOT = proc.ROOT
DEFAULT_BIN = os.path.join(ROOT, "target", "debug", "jqf")

# The stack overflow handler's own words. Rust prints this on the aborting
# thread before it raises SIGABRT, so it is visible on stderr even though the
# process leaves no useful exit status.
OVERFLOW_MARKER = "has overflowed its stack"

LANE_TIMEOUT_SECONDS = 120

DEEP_ARRAY = "(reduce range({n}) as $_ ([]; [.]))"

# The TOML nesting lane's input: a value nested n levels deep under the root
# key. The SEMANTIC depth is n + 1 (the root object counts), so the ceiling
# case is n = 9999 and the rejected case n = 10000.
def toml_nested_array(n):
    return b"a = " + b"[" * n + b"1" + b"]" * n + b"\n"


def toml_nested_array_expected(n):
    return b'{"a":' + b"[" * n + b"1" + b"]" * n + b"}"
# The YAML nesting lane's input: a flow sequence nested n levels deep under
# the root key. The scanner's flow-level guard (libyaml's reference MAX_NESTING
# of 1000) is the codec's ceiling: n = 1000 is the semantic ceiling and
# n = 1001 raises the scanner's nesting error before any recursive build.
def yaml_nested_array(n):
    return b"a: " + b"[" * n + b"1" + b"]" * n + b"\n"


def yaml_nested_array_expected(n):
    return b'{"a":' + b"[" * n + b"1" + b"]" * n + b"}"


# The MessagePack nesting lane's input: a fixarray header nested n levels deep
# around one integer leaf. The shared resource ceiling counts the same 10000
# levels every codec guards, and this codec's boundary is exact: n = 10000
# answers and n = 10001 raises.
def messagepack_nested_array(n):
    return b"\x91" * n + b"\x01"


def messagepack_nested_array_expected(n):
    return "[" * n + "1" + "]" * n


# The HTML nesting lane's input: unclosed <div> elements under the document
# skeleton. The WHATWG tree construction nests every open element, so the
# semantic depth is 2 (html, body) plus the divs: n = 9998 answers and
# n = 9999 raises the shared ceiling. The recovery parser must refuse the
# deeper document through that ceiling, never abort on the stack.
def html_nested_divs(n):
    return b"<html><body>" + b"<div>" * n + b"x" + b"</div>" * n + b"</body></html>"


def html_nested_divs_expected(n):
    # The div chain closes, then the body and html elements the skeleton opened.
    return '{"html":{"head":null,"body":' + '{"div":' * n + '"x"' + "}" * n + "}}"


DEEP_OBJECT = "(reduce range({n}) as $_ ({{}}; {{a:.}}))"

# The program-nesting lane's programs. Unlike every other lane these are deep
# SOURCE rather than deep values: `(`*n nests the parser's descent n times and
# the lowering walk n times, while the value stays the scalar 1.
def nested_groups(n):
    return "(" * n + "1" + ")" * n


class Lane:
    """One guarded recursion, at its boundary and one level under it.

    `stack_kib` is the lane's own MEASURED FLOOR, not a ceiling with headroom,
    and that is deliberate: the lane's whole value is running with as little
    spare stack as the shape itself permits. A change that makes the recursion
    cheaper should bring this number DOWN rather than leave slack behind.

    `under` must print `expected`; `over` must raise `message`. Neither may
    overflow. Both run at `stack_kib`, and the pair is the point — `under`
    passing is what turns `over`'s error from "something stopped it" into "the
    guard stopped it".

    A lane may carry `args` (extra CLI flags) and per-phase stdin `input`
    bytes; the engine lanes leave both at their defaults.
    """

    def __init__(self, name, stack_kib, under, expected, over, message, floor_note,
                 args=(), under_input=b"null\n", over_input=b"null\n"):
        self.name = name
        self.stack_kib = stack_kib
        self.under = under
        self.expected = expected
        self.over = over
        self.message = message
        self.floor_note = floor_note
        self.args = args
        self.under_input = under_input
        self.over_input = over_input


LANES = [
    Lane(
        "equality",
        6849,
        f"{DEEP_ARRAY.format(n=10000)} as $a | {DEEP_ARRAY.format(n=10000)} as $b | $a == $b",
        "true",
        f"{DEEP_ARRAY.format(n=10001)} as $a | {DEEP_ARRAY.format(n=10001)} as $b | $a == $b",
        "Equality check too deep",
        "debug 6849 / release ~625; shared with the build-and-drop recursion",
    ),
    Lane(
        "ordering",
        6865,
        f"{DEEP_ARRAY.format(n=10000)} as $x | [$x,$x] | sort | length",
        "2",
        f"{DEEP_ARRAY.format(n=10001)} as $x | [$x,$x] | sort | length",
        "Comparison too deep",
        "debug 6865 / release 625; same shape as the equality lane",
    ),
    Lane(
        "object-merge",
        22449,
        f"{DEEP_OBJECT.format(n=10000)} as $x | $x * $x | length",
        "1",
        f"{DEEP_OBJECT.format(n=10001)} as $x | $x * $x | length",
        "Object merge too deep",
        "debug 22449 / release 3459; the merge recursion owns most of it",
    ),
    Lane(
        "toml-nesting",
        59900,
        ".",
        toml_nested_array_expected(9999).decode(),
        ".",
        "nesting depth limit exceeded",
        "debug 59900 / release ~6097; the codec's parse+build+encode recursion",
        args=("--input-format", "toml"),
        under_input=toml_nested_array(9999),
        over_input=toml_nested_array(10000),
    ),
    Lane(
        "yaml-nesting",
        32100,
        ".",
        yaml_nested_array_expected(9998).decode(),
        ".",
        "nesting depth limit exceeded",
        "debug 32100 / release ~4081; the document-build guard is the codec's own ceiling",
        args=("--input-format", "yaml"),
        under_input=yaml_nested_array(9998),
        over_input=yaml_nested_array(9999),
    ),
    Lane(
        "messagepack-nesting",
        # Floor measured on the debug build by the lane's own twin pair:
        # 85296 KiB overflows the under twin, 85297 passes it, and the over
        # twin raises the guard at the pin — roughly 8.5 KiB of debug frames
        # per nested level (the scan's depth guard, the document build, and
        # the JSON encode all recurse over the same depth). The pin is the
        # round hundred above the measured floor.
        85400,
        ".",
        messagepack_nested_array_expected(10000),
        ".",
        "nesting depth limit exceeded",
        "debug 85400; ~8.5 KiB of debug frames per nested fixarray level",
        args=("--input-format", "messagepack"),
        under_input=messagepack_nested_array(10000),
        over_input=messagepack_nested_array(10001),
    ),
    Lane(
        "html-nesting",
        # Same measurement: 64827 KiB overflows the under twin, 64843 passes
        # it, and the over twin raises the guard at this pin. Unclosed
        # elements nest in the tree construction, so the document build is
        # the recursion — roughly 6.5 KiB of debug frames per element.
        64900,
        ".",
        html_nested_divs_expected(9998),
        ".",
        "nesting depth limit exceeded",
        "debug 64900; ~6.5 KiB of debug frames per open element in the build",
        args=("--input-format", "html"),
        under_input=html_nested_divs(9998),
        over_input=html_nested_divs(9999),
    ),
    Lane(
        "program-nesting",
        157265,
        nested_groups(9999),
        "1",
        # Five times the ceiling: a 10001-level over case could pass on a
        # guard that leaked by an order of magnitude. Keep both programs
        # under 128 KiB — Linux caps a SINGLE argv string there, and the
        # over case is already 100 KB.
        nested_groups(50000),
        "nesting depth limit exceeded",
        "debug 157265 / release 99452; ~15.7 KiB of debug frames per nested level",
    ),
    Lane(
        "path-length",
        561,
        "getpath([range(10000)|tostring])",
        "null",
        "getpath([range(10001)|tostring])",
        "Path too deep",
        "debug 561 / release 64 (the knob's own floor; the true floor is under it)",
    ),
]


def run(binary, program, stack_kib, args=(), stdin=b"null\n"):
    """Runs one program on a request thread of exactly `stack_kib` KiB.

    The stack is set through `JQF_REQUEST_STACK_BYTES`, the one knob that
    reaches the thread the recursion actually runs on (see `request_stack_bytes`
    in `jqf-cli/src/main.rs`). The knob is FAIL-LOUD: a value below its 64 KiB
    floor makes jqf exit 2, so a lane pinned to an impossible floor cannot pass
    by accident.
    """
    env = dict(os.environ)
    env["JQF_REQUEST_STACK_BYTES"] = str(stack_kib * 1024)
    completed = proc.run_gate(
        binary, [*args, "-c", program],
        input=stdin,
        env=env,
        timeout=LANE_TIMEOUT_SECONDS,
        check=False,
    )
    return (
        completed.returncode,
        completed.stdout.decode("utf-8", "replace").strip(),
        completed.stderr.decode("utf-8", "replace").strip(),
    )


def check(binary, lane, verbose):
    """Runs one lane's pair. Returns (passed, overflows)."""
    failures = []
    overflows = 0

    for phase, program, stdin in (
        ("under", lane.under, lane.under_input),
        ("over", lane.over, lane.over_input),
    ):
        try:
            code, out, err = run(binary, program, lane.stack_kib, lane.args, stdin)
        except subprocess.TimeoutExpired:
            # A named lane + timeout, never an unhandled traceback out of the
            # run-to-completion drive.
            print(
                f"FAIL {lane.name}/{phase}: timed out after "
                f"{LANE_TIMEOUT_SECONDS}s at {lane.stack_kib} KiB",
                file=sys.stderr,
            )
            return False, overflows
        if OVERFLOW_MARKER in err:
            overflows += 1
            failures.append(
                f"{lane.name}/{phase}: the Rust stack ran out at {lane.stack_kib} KiB "
                f"({lane.floor_note})"
            )
        elif phase == "under":
            if code != 0 or out != lane.expected:
                failures.append(
                    f"{lane.name}/under: expected a clean exit printing "
                    f"{lane.expected!r}, got exit={code} {out!r} {err!r}"
                )
        elif code == 0 or lane.message not in err:
            # The over twin must FAIL THROUGH THE GUARD — nonzero exit carrying
            # jq's ceiling message — not answer, and not stop any other way.
            failures.append(
                f"{lane.name}/over: expected exit≠0 raising {lane.message!r}, "
                f"got exit={code} {out!r} {err!r}"
            )

    for text in failures:
        print(f"FAIL {text}", file=sys.stderr)
    if not failures and verbose:
        print(
            f"ok   {lane.name}: at {lane.stack_kib} KiB, under answers and over "
            f"raises {lane.message!r} ({lane.floor_note})"
        )
    return not failures, overflows


def main():
    args = [arg for arg in sys.argv[1:] if arg != "--verbose"]
    verbose = "--verbose" in sys.argv[1:]
    binary = args[0] if args else DEFAULT_BIN
    if not proc.executable(binary):
        print(
            f"error: jqf binary not found at {binary} (build with: cargo build -p jqf)",
            file=sys.stderr,
        )
        return 2

    passed = 0
    overflows = 0
    for lane in LANES:
        ok, lane_overflows = check(binary, lane, verbose)
        passed += 1 if ok else 0
        overflows += lane_overflows

    print(
        f"stack-depth-gate: lanes={len(LANES)} pass={passed} overflows={overflows} "
        f"min_stack_kib={min(lane.stack_kib for lane in LANES)} "
        f"max_stack_kib={max(lane.stack_kib for lane in LANES)}"
    )
    return 0 if passed == len(LANES) and overflows == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
