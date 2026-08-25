#!/usr/bin/env python3
"""jq's own official test suite, run against jqf as a standing oracle.

`tools/jq-test-suite/jq-1.8.2/jq.test` is jq's test file, verbatim. This lane
runs every case through the jqf CLI and sorts each into exactly one of four
buckets:

    PASS         jqf produced the values jq's suite expects (or, for a
                 `%%FAIL` case, rejected the program as jq does).
    UNSUPPORTED  jqf declared the case outside its implemented surface — exit
                 3 with an `unsupported construct` or `... is not defined`
                 diagnostic. This is jqf's OWN taxonomy and nothing else
                 reaches this bucket, and it applies to VALUE rows only: a
                 `%%FAIL` row that exits with a rejection class (3 or 5) is a
                 PASS whatever its diagnostic, because rejection is that row's
                 whole contract (the rejection law below).
    FAIL         jqf compiled and ran the case and produced the wrong values,
                 the wrong exit class, or rejected a program it never declared
                 unsupported.
    ERROR        the harness could not classify the case at all.

**The boundary between UNSUPPORTED and FAIL is the whole point.** A case is
never demoted to UNSUPPORTED because its output differed — only jqf's own
"I have not implemented this" diagnostic puts it there. The verticals jqf HAS
implemented are expected to PASS, so wrong bytes from implemented surface are a
bug, and they surface here as FAIL rather than hiding.

Comparison is SEMANTIC, matching jq's own `run_tests`: jq parses each expected
line and compares with `jv_equal`, so `2.0` equals `2` and `{"a":1, "b":2}`
equals the compact spelling. That makes this lane an oracle for jq SEMANTICS.
It is deliberately not a second byte oracle — `tools/jqf-cli-jq-compat.sh` owns
byte identity, and duplicating it here would only re-report its failures.
`true` never equals `1` and `1` never equals `"1"`: the comparison is
type-aware, so a kind confusion is still a FAIL.

`%%FAIL` cases carry one deliberate harness divergence, written down because it
is a real difference rather than a detail. jq's runner only COMPILES a `%%FAIL`
program; jqf has no compile-only mode, so the harness RUNS it on `null` and
requires a nonzero exit. jqf may therefore satisfy such a case at run time where
jq rejects it at compile time — `{(0):1}` is the standing example, rejected by
jq's constant folding and by jqf's object-key check. The expected message text
is not compared, on the same reasoning as the compat corpus: two implementations
report the same fault in different words.

**The rejection law makes that consistent.** A `%%FAIL` row is jq's own list of
programs it refuses, so a REJECTION-CLASS EXIT (3 or 5) is the row's entire
contract, checked BEFORE the unsupported markers: a rejection spelled `unsupported construct` or
`is not defined` is still a rejection — the label/undefined rows reject with
jq's own message text, which carries the latter marker. The receipt's
`rejected=` field counts every rejection-satisfied `%%FAIL` row, so the
accounting survives: a jqf that starts ACCEPTING one of these programs flips
its row to FAIL, and `rejected` dropping below the suite's `%%FAIL` count is
the gate saying it no longer sees them.

Every FAIL that is not a jqf bug to fix must be named in `ALLOWLIST` below, with
its reason and the exact condition that retires it. A stale entry — one whose
case no longer fails — is itself an error, so a fix cannot leave its waiver
behind.

**Two vendored suites run through this one harness**, selected by `--suite`,
because they are the same FILE FORMAT and the same comparison law — a second
copy of the parser and the classifier would only be a second thing to keep
right. `jq.test` is jq's general suite; `onig.test` is jq's oniguruma-derived
REGEX suite, vendored as ledger 043's W6 so the regex family has an oracle at
all (`jq.test` carries almost no regex case, which is why the two-tier engine's
divergences had nothing standing to measure them).

Receipt lines, one per suite:

    jq-test-suite: total=N pass=P rejected=R unsupported=U fail=F error=E allowed=A
    onig-suite:    total=N pass=P rejected=R unsupported=U fail=F error=E allowed=A

Exits nonzero when any FAIL or ERROR is outside the allowlist, when an
allowlist entry is stale, or when the vendored suite no longer matches the
sha256 recorded in PROVENANCE.md.

Usage:
    tools/jqf-jq-test-suite.py [path-to-jqf] [--suite jq|onig] [--verbose]
"""

import hashlib
import json
import os
from jqfgate import proc
import subprocess
import sys

# Hermeticity (plan 064): a developer's .jqf.toml must never reach a gate —
# the harnesses are hermetic by construction, not by convention.
proc.set_hermetic()

ROOT = proc.ROOT

# The vendored suite and the jq it was taken from. The ORACLE is the vendored
# file, and its integrity is pinned by the sha256 recorded in
# `tools/jq-test-suite/PROVENANCE.md` — the single source of truth for what
# this lane is allowed to expect. The installed jq's version is NOT part of the
# oracle: this harness never runs jq, so a machine whose jq differs from the
# suite version still gets the verbatim vendored expectations.
SUITE_VERSION = "1.8.2"
SUITE_DIR = os.path.join(ROOT, "tools", "jq-test-suite", f"jq-{SUITE_VERSION}")
MODULES_PATH = os.path.join(SUITE_DIR, "tests", "modules")
PROVENANCE_PATH = os.path.join(ROOT, "tools", "jq-test-suite", "PROVENANCE.md")


class Suite:
    """One vendored jq test file: where it lives, what the receipt calls it,
    and which waivers apply to it."""

    def __init__(self, key, filename, receipt):
        self.key = key
        self.filename = filename
        self.receipt = receipt
        self.path = os.path.join(SUITE_DIR, filename)
        self.allowlist = []

    def allowed_by_key(self):
        return {entry.key(): entry for entry in self.allowlist}

CASE_TIMEOUT_SECONDS = 30

# jqf's program-rejection exit class. Exit 3 alone is not the unsupported
# taxonomy — it also covers ordinary parse errors — so the message is what
# separates "I have not implemented this" from "this is malformed".
PROGRAM_EXIT = 3
# The rejection-class exits: 3 = compile/parse rejection, 5 = runtime error.
# A `%%FAIL` row's contract is one of these — a panic (101) or a crash (139)
# is a FAIL, never a rejection.
REJECTION_EXITS = (PROGRAM_EXIT, 5)
UNSUPPORTED_MARKERS = ("unsupported construct", "is not defined")


class Allow:
    """One waived FAIL, its reason, and the condition that retires it."""

    def __init__(self, program, stdin, reason, retire):
        self.program = program
        self.stdin = stdin
        self.reason = reason
        self.retire = retire
        self.hit = False

    def key(self):
        return (self.program, self.stdin)


# Every waiver is one of two things and says which: a CATALOGUED intentional
# divergence, or a defect diagnosed and recorded in
# `.docs-intenal/deferred-and-ideas.md` because fixing it is structural.
ALLOWLIST = []

# The catalogued exact-integer divergence, the same family the compat corpus
# carries as its `intdiff` rows: jq's arithmetic is IEEE double, jqf's is exact,
# so a literal past 2^53 subtracts to a different number. jq's expectation is
# the double result of a value it had already rounded on the way in.
_INTDIFF_REASON = (
    "INTENTIONAL DIVERGENCE, catalogued. jq computes in IEEE doubles, so "
    "13911860366432393 arrives already rounded to ...392 and 10 less is "
    "...382. jqf's numbers are exact and it answers ...383. Same family as the "
    "compat corpus's `intdiff` rows."
)
_INTDIFF_RETIRE = (
    "never, while jqf keeps exact integer arithmetic; retire only if jqf "
    "adopts jq's double law"
)
for _intdiff_program, _intdiff_stdin in (
    (". - 10", "13911860366432393"),
    (".[0] - 10", "[13911860366432393]"),
    (".x - 10", '{"x":13911860366432393}'),
    # The same law read through `tostring`: `$n+0` rounds in jq and does not in
    # jqf, so the rendered text differs for exactly the literals past 2^53. The
    # renderer itself is byte-identical to jq's — this case never reaches a
    # rendering disagreement, only a numeric one.
    (
        ".[] as $n | $n+0 | [., tostring, . == $n]",
        "[-9007199254740993, -9007199254740992, 9007199254740992, "
        "9007199254740993, 13911860366432393]",
    ),
):
    ALLOWLIST.append(
        Allow(
            program=_intdiff_program,
            stdin=_intdiff_stdin,
            reason=_INTDIFF_REASON,
            retire=_INTDIFF_RETIRE,
        )
    )

# The one case in the file where JQ ITSELF exits nonzero. `classify` requires
# exit 0 from a value case, and it must: a wrong exit class is normally a real
# defect. Here the program deliberately raises after emitting, and jqf's stdout,
# exit code and stderr are byte-identical to jq's:
#
#   $ echo '["hi","ho"]' | jq -c '<the program>'   → "hi there!" / exit 5 /
#                                                     jq: error (at <stdin>:1): ho
#   $ echo '["hi","ho"]' | jqf -c '<the program>'  → "hi there!" / exit 5 /
#                                                     jqf: error (at <stdin>:1): ho
#
# Same class as the `%%FAIL` divergence in this module's docstring: jq's own
# runner compares VALUES and never looks at the exit status, so the case is a
# harness artifact and not a jqf disagreement.
ALLOWLIST.append(
    Allow(
        program=(
            '.[]|(try . catch (if .=="ho" then "BROKEN"|error else empty end)) | '
            'if .=="ho" then error else "\\(.) there!" end'
        ),
        stdin='["hi","ho"]',
        reason=(
            "HARNESS DIVERGENCE, catalogued. The program emits one value and "
            "THEN raises, so jq exits 5 on it too. jqf's stdout bytes, exit "
            "code and stderr text all match jq's exactly; only this harness's "
            "exit-0 requirement for a value case is unmet, and jq's own "
            "run_tests never checks the exit status."
        ),
        retire=(
            "when this harness grows a bucket for a case whose expected values "
            "are followed by a raise"
        ),
    )
)

ALLOWLIST.append(
    Allow(
        program="map(try implode catch .)",
        stdin='[123,["a"],[nan]]',
        reason=(
            "CATALOGUED NARROWING. The INPUT is not JSON: jq's parser accepts "
            "the bare literal `nan`, and jqf's strict decode refuses it before "
            "the program runs, so the case never reaches `implode` at all. "
            "jqf's number model carries no non-finite value that renders, "
            "orders and compares as jq's does, and making one constructible "
            "would silently break the NaN ordering tripwire in "
            "jqf-engine/src/semantics/order.rs. Catalogued in "
            ".docs-intenal/engine-vertical-strings.md (i.2). The two shapes "
            "the case actually tests — a non-array input and a non-number "
            "element — are byte-oracled in the compat corpus instead."
        ),
        retire=(
            "when jqf's number model carries jq's non-finite family and the "
            "decoder accepts its literals"
        ),
    )
)

# The four "Destructuring DUP/POP issues" cases. Their bodies are a single `#`
# comment naming a runtime error, so after `parse_suite` learned to skip `#`
# lines (see its docstring) they read as value cases expecting NO output — and
# jq itself exits 5 on all four. Three IDENTICAL alternatives cannot rescue
# `[3]`: the last one's failure has nowhere left to restart, so it escapes.
# jqf raises the same error, with the same stderr text and the same exit code:
#
#   $ echo '[[3],[4],[5],6]' | jq  -c '.[] | . as {a:$a} ?// {a:$a} ?// {a:$a} | $a'
#     jq: error (at <stdin>:1): Cannot index array with string ("a")   → exit 5
#   $ echo '[[3],[4],[5],6]' | jqf -c '.[] | . as {a:$a} ?// {a:$a} ?// {a:$a} | $a'
#     jqf: error (at <stdin>:1): Cannot index array with string ("a")  → exit 5
#
# (jq's own comment says `"c"`; the error it actually raises names `"a"`. The
# comment is stale in jq 1.8.2 and nothing reads it.) Their twelve siblings with
# a trailing `$a` alternative all succeed and all convert.
_DUP_POP_REASON = (
    "HARNESS DIVERGENCE, catalogued. The case body is only a `#` comment "
    "naming a runtime error, so it reads as a value case expecting no output, "
    "and jq itself exits 5: three identical `?//` alternatives cannot rescue "
    "`[3]` and the last one's failure escapes. jqf's stdout bytes, exit code "
    "and stderr text all match jq's exactly. Same class as the "
    "emit-then-raise entry above. Catalogued in "
    ".docs-intenal/engine-vertical-patterns.md (b.8)."
)
_DUP_POP_RETIRE = (
    "when this harness grows a bucket for a case whose body records a runtime "
    "error as a comment rather than an expected value"
)
for _dup_pop_program, _dup_pop_stdin in (
    (".[] | . as {a:$a} ?// {a:$a} ?// {a:$a} | $a", "[[3],[4],[5],6]"),
    (".[] as {a:$a} ?// {a:$a} ?// {a:$a} | $a", "[[3],[4],[5],6]"),
    ("[[3],[4],[5],6][] | . as {a:$a} ?// {a:$a} ?// {a:$a} | $a", "null"),
    ("[[3],[4],[5],6] | .[] as {a:$a} ?// {a:$a} ?// {a:$a} | $a", "null"),
):
    ALLOWLIST.append(
        Allow(
            program=_dup_pop_program,
            stdin=_dup_pop_stdin,
            reason=_DUP_POP_REASON,
            retire=_DUP_POP_RETIRE,
        )
    )

# The oniguruma-derived regex suite's own waivers. Each is one line of
# `.docs-intenal/regex-divergence-catalogue-2026-08-04.md`, which is the
# enumerated catalogue this suite exists to MEASURE: a divergence that is not
# in the catalogue fails this lane, and a catalogue line whose case stopped
# diverging fails it as a stale entry. That is what keeps the catalogue honest
# rather than small.
ONIG_ALLOWLIST = []

SUITES = {
    "jq": Suite("jq", "jq.test", "jq-test-suite"),
    "onig": Suite("onig", "onig.test", "onig-suite"),
}
SUITES["jq"].allowlist = ALLOWLIST
SUITES["onig"].allowlist = ONIG_ALLOWLIST


class Case:
    """One suite case: a program, its input, and what jq says it produces."""

    def __init__(self, lineno, program, must_fail, stdin, expected):
        self.lineno = lineno
        self.program = program
        self.must_fail = must_fail
        self.stdin = stdin
        self.expected = expected


def parse_suite(path):
    """Parse jq's test format, the same shape jq's own `run_tests` reads.

    Comments and blank lines separate cases. A case is a program line, an input
    line, and zero or more expected-output lines. A `#` line is a COMMENT
    everywhere, including INSIDE a case's expected block — jq's own `run_tests`
    skips it before it looks at anything else, so four cases whose only trailing
    line is `# Runtime error: ...` expect NO output rather than that text
    (jq.test 945, 949, 953, 957). Reading it as an expectation made those four
    unfalsifiable. A case introduced by `%%FAIL`
    is instead a program line followed by the error message jq reports; the
    `IGNORE MSG` variant differs only in whether jq compares that message, and
    since this harness never compares it the two are one shape here.
    """
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().split("\n")

    cases = []
    index = 0
    total = len(lines)
    while index < total:
        line = lines[index]
        if not line.strip() or line.startswith("#"):
            index += 1
            continue
        lineno = index + 1
        if line.startswith("%%FAIL"):
            index += 1
            if index >= total:
                break
            program = lines[index]
            index += 1
            # The expected message line, then the rest of the block.
            while index < total and lines[index].strip():
                index += 1
            # jqf has no compile-only mode, so a rejection case runs on `null`.
            cases.append(Case(lineno, program, True, "null", []))
            continue
        program = line
        index += 1
        if index >= total:
            break
        stdin = lines[index]
        index += 1
        expected = []
        while index < total and lines[index].strip():
            if not lines[index].startswith("#"):
                expected.append(lines[index])
            index += 1
        cases.append(Case(lineno, program, False, stdin, expected))
    return cases


def values_equal(left, right):
    """jq's `jv_equal`, with its kinds kept apart.

    Numbers compare as doubles (jq's own law, which is what lets `2.0` match
    `2`), but a bool is never a number and a number is never a string — Python
    would happily call `True == 1` true, and a kind confusion must stay a FAIL.
    Past 2^53 the double conversion collapses neighbouring integers onto one
    value — jqf's own comparison does the same here by fidelity to
    `jv_equal`, so an expectation that differs from the output only above 2^53
    matches; jqf's exact-integer divergence is catalogued and pinned in the
    byte-oracle corpus, which owns that difference.
    """
    if isinstance(left, bool) != isinstance(right, bool):
        return False
    if isinstance(left, bool):
        return left == right
    if isinstance(left, (int, float)) and isinstance(right, (int, float)):
        return float(left) == float(right)
    if isinstance(left, list) and isinstance(right, list):
        return len(left) == len(right) and all(
            values_equal(one, other) for one, other in zip(left, right)
        )
    if isinstance(left, dict) and isinstance(right, dict):
        return left.keys() == right.keys() and all(
            values_equal(left[key], right[key]) for key in left
        )
    if type(left) is not type(right):
        return False
    return left == right


def is_unsupported(status, stderr):
    """jqf's own unimplemented-surface declaration, and nothing else."""
    return status == PROGRAM_EXIT and any(marker in stderr for marker in UNSUPPORTED_MARKERS)


def classify(case, status, stdout, stderr):
    """Sort one completed run into a bucket, with a note when it is not a PASS.

    The rejection law is checked FIRST: a `%%FAIL` row's contract is a
    rejection-class exit (3 or 5), and its message text is out of scope exactly
    as a value row's is, so a rejection spelled with an unsupported marker is
    still a PASS. The UNSUPPORTED taxonomy therefore applies to value rows
    only.
    """
    if case.must_fail:
        if status in REJECTION_EXITS:
            return "PASS", None
        return (
            "FAIL",
            f"jq rejects this program; jqf exited {status}, not a rejection class (3 or 5)",
        )

    if is_unsupported(status, stderr):
        return "UNSUPPORTED", None

    try:
        expected = [json.loads(line) for line in case.expected]
    except ValueError as error:
        return "ERROR", f"suite expectation is not JSON: {error}"

    if status != 0:
        return "FAIL", f"exit {status}: {stderr.strip().splitlines()[0] if stderr.strip() else ''}"

    lines = stdout.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    try:
        produced = [json.loads(line) for line in lines]
    except ValueError as error:
        return "ERROR", f"jqf output is not one JSON value per line: {error}"

    if len(produced) == len(expected) and all(
        values_equal(one, other) for one, other in zip(produced, expected)
    ):
        return "PASS", None
    return "FAIL", f"expected {expected!r}, produced {produced!r}"


def run_case(jqf, case):
    try:
        completed = proc.run_gate(
            jqf,
            # `-c`: this suite reads jqf's stdout as ONE JSON VALUE PER LINE,
            # and jqf's default output is jq's multi-line pretty print.
            # `-L`: the module cases import the vendored fixtures.
            ["-c", "-L", MODULES_PATH, case.program],
            input=case.stdin.encode("utf-8"),
            timeout=CASE_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return None
    return (
        completed.returncode,
        completed.stdout.decode("utf-8", "replace"),
        completed.stderr.decode("utf-8", "replace"),
    )


def installed_jq_version():
    try:
        completed = subprocess.run(
            ["jq", "--version"], capture_output=True, timeout=30, check=False
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if completed.returncode != 0:
        return None
    text = completed.stdout.decode("utf-8", "replace").strip()
    return text[3:] if text.startswith("jq-") else text


def recorded_suite_digest(suite):
    """The sha256 PROVENANCE.md records for one vendored suite file, or None.

    The digest is read from the provenance table rather than hard-coded here so
    there is exactly one record of what this lane's oracle is. Re-vendoring a
    suite means editing PROVENANCE.md, and this lane follows it.

    The match is STANDALONE, not a word inclusion: the row must carry a whole
    cell that IS `jq-<version>/<file>` and exactly one whole cell that IS a
    64-hex digest. A prose line that happens to name the file plus any
    unrelated 64-hex string (a commit sha, say) must never read as provenance,
    and an ambiguous row is skipped rather than guessed.
    """
    marker = f"jq-{SUITE_VERSION}/{suite.filename}"
    try:
        with open(PROVENANCE_PATH, encoding="utf-8") as handle:
            for line in handle:
                cells = [cell.strip().strip("`") for cell in line.split("|")]
                if marker not in cells:
                    continue
                digests = [
                    cell for cell in cells
                    if len(cell) == 64 and all(c in "0123456789abcdef" for c in cell)
                ]
                if len(digests) == 1:
                    return digests[0]
    except OSError:
        return None
    return None


def vendored_suite_matches(suite, digest):
    """The vendored suite file on disk matches the recorded provenance digest."""
    try:
        with open(suite.path, "rb") as handle:
            return hashlib.sha256(handle.read()).hexdigest() == digest
    except OSError:
        return False


def main():
    argv = [arg for arg in sys.argv[1:] if arg != "--verbose"]
    verbose = len(argv) != len(sys.argv[1:])
    suite = SUITES["jq"]
    if "--suite" in argv:
        marker = argv.index("--suite")
        name = argv[marker + 1] if marker + 1 < len(argv) else ""
        if name not in SUITES:
            print(
                f"error: unknown suite {name!r}; expected one of {', '.join(sorted(SUITES))}",
                file=sys.stderr,
            )
            return 2
        suite = SUITES[name]
        del argv[marker : marker + 2]
    jqf = argv[0] if argv else os.path.join(ROOT, "target", "release", "jqf")

    if not os.access(jqf, os.X_OK):
        print(f"error: {jqf} is not executable; build it with `cargo build --release -p jqf`",
              file=sys.stderr)
        return 2

    digest = recorded_suite_digest(suite)
    if digest is None:
        print(
            f"error: PROVENANCE.md records no sha256 for jq-{SUITE_VERSION}/{suite.filename}; "
            "a suite without a recorded digest is not an oracle",
            file=sys.stderr,
        )
        return 2
    if not vendored_suite_matches(suite, digest):
        print(
            f"error: vendored {suite.path} does not match the sha256 recorded in "
            "PROVENANCE.md; a modified oracle is not an oracle",
            file=sys.stderr,
        )
        return 2

    # Informational only, never a stop: the oracle is the vendored file, whose
    # integrity the digest above pins. A machine running a different jq still
    # sees the verbatim vendored expectations, so it can gate jqf.
    version = installed_jq_version()
    if version is not None and version != SUITE_VERSION:
        print(
            f"warning: installed jq reports {version!r} but the vendored suite is "
            f"jq {SUITE_VERSION}; the suite's sha256 (PROVENANCE.md) is the oracle, "
            "so this run proceeds",
            file=sys.stderr,
        )

    cases = parse_suite(suite.path)
    allowed_by_key = suite.allowed_by_key()
    buckets = {"PASS": 0, "UNSUPPORTED": 0, "FAIL": 0, "ERROR": 0}
    rejected = 0
    unwaived = []
    waived = 0

    for case in cases:
        run = run_case(jqf, case)
        if run is None:
            bucket, note = "ERROR", f"timed out after {CASE_TIMEOUT_SECONDS}s"
        else:
            bucket, note = classify(case, *run)
        buckets[bucket] += 1
        if case.must_fail and bucket == "PASS":
            rejected += 1
            if verbose:
                print(f"rejected {suite.filename}:{case.lineno} {case.program!r}", file=sys.stderr)
        if bucket == "UNSUPPORTED" and verbose:
            print(f"UNSUPPORTED {suite.filename}:{case.lineno} {case.program!r}", file=sys.stderr)
        if bucket not in ("FAIL", "ERROR"):
            continue
        entry = allowed_by_key.get((case.program, case.stdin))
        if entry is None:
            unwaived.append((case, bucket, note))
        else:
            entry.hit = True
            waived += 1
            if verbose:
                print(
                    f"allowed {suite.filename}:{case.lineno} {case.program!r}\n"
                    f"  {entry.reason}\n  retires: {entry.retire}",
                    file=sys.stderr,
                )

    for case, bucket, note in unwaived:
        print(
            f"{bucket} {suite.filename}:{case.lineno} program={case.program!r}\n"
            f"  input {case.stdin!r}\n  {note}",
            file=sys.stderr,
        )

    stale = [entry for entry in suite.allowlist if not entry.hit]
    for entry in stale:
        print(
            f"error: stale allowlist entry for program={entry.program!r} input={entry.stdin!r}; "
            f"the case no longer fails, so its waiver must be removed",
            file=sys.stderr,
        )

    print(
        f"{suite.receipt}: total={len(cases)} pass={buckets['PASS']} "
        f"rejected={rejected} unsupported={buckets['UNSUPPORTED']} "
        f"fail={buckets['FAIL']} error={buckets['ERROR']} "
        f"allowed={waived} jq={SUITE_VERSION}"
    )
    return 1 if unwaived or stale else 0


sys.exit(main())
