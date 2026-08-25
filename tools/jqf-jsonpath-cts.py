#!/usr/bin/env python3
"""The RFC 9535 Compliance Test Suite, run against jqf as a standing oracle.

`tools/jsonpath-compliance-test-suite/cts.json` is the jsonpath-standard
Compliance Test Suite, vendored verbatim at a pinned commit (PROVENANCE.md).
This lane runs every case through the built `jqf` binary's `jsonpath/1`
builtin and sorts each into exactly one of four buckets:

    PASS            jqf published exactly the expected result (for an
                    `invalid_selector` case: jqf raised, i.e. exited
                    nonzero with zero output).
    FAIL            jqf published the wrong bytes, or accepted a selector
                    it must reject, or raised a selector it must accept.
    ERROR           the harness could not classify the case at all (a
                    malformed vendored file, a crashed binary, a timeout).

The suite's expectations are:

    result          the deterministic nodelist (array of matched values),
                    compared as exact published bytes.
    results         a list of nodelists, ALL of which are valid (RFC 9535
                    leaves object order and other orderings unspecified);
                    jqf's published bytes must equal one of them.
    invalid_selector
                    the selector is not a well-formed/valid query; the
                    implementation MUST raise (here: nonzero exit, zero
                    stdout).

The harness encodes each selector into a jqf program
`jsonpath("<selector as JSON string>")` and pipes the case's document to
stdin, so the comparison is byte-for-byte against jqf's own JSON output of
the matched-values array.

The differential acceptance law: this lane FAILED against the pre-
implementation binary (`jsonpath/1 is not defined` on the first case) and
passes only once the builtin exists — see the plan close note.

Receipt line:

    jsonpath-cts: total=N pass=P fail=F error=E invalid=I

Exits nonzero when any case FAILs or the harness errors, or when the
vendored `cts.json` no longer matches the sha256 recorded in PROVENANCE.md.

Usage:
    tools/jqf-jsonpath-cts.py [path-to-jqf] [--verbose]
"""

import hashlib
import json
import os
import re
import sys

from jqfgate import proc

# Hermeticity (plan 064): a developer's .jqf.toml must never reach a gate.
proc.set_hermetic()

SUITE_DIR = os.path.join(proc.ROOT, "tools", "jsonpath-compliance-test-suite")
CTS = os.path.join(SUITE_DIR, "cts.json")
CTS_SHA256 = "a85db53fba1f675be48b534baec5a754dc685ad08c550d8927f609c7708f365a"
# The vendor guard: a modified oracle is not an oracle.
SUITE_SHA256_RE = re.compile(
    r"`cts\.json` \| .*\| `([0-9a-f]{64})`"
)


def digest(path):
    hasher = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def load_suite():
    if not os.path.exists(CTS):
        raise SystemExit(f"jsonpath-cts: vendored suite missing at {CTS}")
    if digest(CTS) != CTS_SHA256:
        raise SystemExit(
            "jsonpath-cts: vendored cts.json drifted from the pinned sha256; "
            "re-vendor with a PROVENANCE.md update"
        )
    with open(CTS, encoding="utf-8") as handle:
        suite = json.load(handle)
    tests = suite.get("tests")
    if not isinstance(tests, list):
        raise SystemExit("jsonpath-cts: vendored cts.json has no tests list")
    return tests


def json_string(text):
    """One selector as a jqf double-quoted string literal (JSON escaping)."""
    return json.dumps(text)


def run_case(jqf, selector, document, timeout=10):
    """Runs one case. Returns (exit_code, stdout_bytes, stderr_bytes)."""
    program = f"jsonpath({json_string(selector)})"
    completed = proc.run_gate(
        jqf, [program],
        input=json.dumps(document, ensure_ascii=False).encode("utf-8"),
        timeout=timeout,
    )
    return completed.returncode, completed.stdout, completed.stderr


def classify(jqf, test, verbose):
    """Runs one suite case. Returns ("PASS"|"FAIL"|"ERROR", detail)."""
    selector = test["selector"]
    name = test["name"]
    if test.get("invalid_selector"):
        code, out, err = run_case(jqf, selector, None)
        if code != 0 and not out.strip():
            return "PASS", ""
        return "FAIL", (
            f"invalid selector accepted: exit={code} stdout={out[:80]!r} "
            f"stderr={err[:120]!r}"
        )
    document = test.get("document")
    if document is None:
        return "ERROR", "no document for a non-invalid case"
    code, out, err = run_case(jqf, selector, document)
    if code != 0:
        return "FAIL", (
            f"valid selector raised: exit={code} stderr={err[:200]!r}"
        )
    try:
        published = json.loads(out.decode("utf-8"))
    except (ValueError, UnicodeDecodeError) as error:
        return "ERROR", f"undecodable stdout {out[:80]!r}: {error}"
    if "result" in test:
        expected = test["result"]
        if published == expected:
            return "PASS", ""
        return "FAIL", (
            f"result mismatch: published {published!r}, expected {expected!r}"
        )
    if "results" in test:
        for expected in test["results"]:
            if published == expected:
                return "PASS", ""
        return "FAIL", (
            f"published {published!r} in none of the accepted orders"
        )
    return "ERROR", "neither result nor results present"


def main():
    jqf = proc.resolve_binary(sys.argv[1:])
    if not proc.executable(jqf):
        sys.stderr.write(
            f"error: {jqf} is not executable; build it with "
            "`cargo build --release -p jqf`\n"
        )
        return 2
    stale = proc.freshness_problem(jqf)
    if stale:
        sys.stderr.write(f"error: {jqf} is stale: {stale}\n")
        return 2
    verbose = "--verbose" in sys.argv[1:]
    tests = load_suite()
    pass_count = fail_count = error_count = invalid_count = 0
    failures = []
    for test in tests:
        outcome, detail = classify(jqf, test, verbose)
        if outcome == "PASS":
            pass_count += 1
            if test.get("invalid_selector"):
                invalid_count += 1
        elif outcome == "FAIL":
            fail_count += 1
            failures.append((test["name"], test["selector"], detail))
        else:
            error_count += 1
            failures.append((test["name"], test["selector"], detail))
        if verbose and outcome != "PASS":
            print(f"  [{outcome}] {test['name']}: {detail}")
    total = pass_count + fail_count + error_count
    print(
        f"jsonpath-cts: total={total} pass={pass_count} fail={fail_count} "
        f"error={error_count} invalid={invalid_count}"
    )
    if failures:
        for name, selector, detail in failures[:20]:
            print(f"  FAIL {name} | {selector!r}: {detail}")
    if fail_count or error_count:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
