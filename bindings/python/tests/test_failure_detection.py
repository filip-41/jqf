"""Tests for the Python binding's failure-detection contract.

No pytest dependency: the binding itself has no build step (ctypes only), so
its tests stay stdlib-only too. Build the cdylib first, then run directly
(from the repo root):

    cargo build --release -p jqf-sdk-ffi
    python3 bindings/python/tests/test_failure_detection.py -v
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import jqf  # noqa: E402  (path insert above must run first)


class SelectFailureIsolatesCaughtFromCatchable(unittest.TestCase):
    """A pure unit test directly on `_Handle._select_failure`, with no ctypes
    or ABI call in sight. This is the test that catches a drifted predicate
    even when everything on the Rust side already reports failure correctly:
    the binding's own selector can still pick the wrong field on a
    diagnostic record it already received correctly.
    """

    @staticmethod
    def record(catchable, caught):
        # `caught=0xFFFFFFFF` is the ABI's "never caught" sentinel; `Record` translates it to `None` in its own
        # constructor, exactly as `_collect_diags` does when reading `jqf_diag_get`'s out-param.
        return jqf.Record(
            code=11, revision=1, class_="S", severity="E",
            catchable=catchable, caught=(0xFFFFFFFF if caught is None else caught),
            step_index=0xFFFFFFFF, input_ordinal=0xFFFFFFFFFFFFFFFF,
            byte_offset=0xFFFFFFFFFFFFFFFF, halt_status=-1,
            kind=None, operand=None, payload=None,
        )

    # `written=0` is the success sentinel throughout this class: these cases are about the diagnostic-record predicate
    # in isolation, not about the ABI return code, which `_select_failure` also independently consults (see
    # `SelectFailureAlsoTrustsTheAbiSentinel` below).
    def test_an_uncaught_error_of_a_catchable_class_is_the_failure(self):
        uncaught = self.record(catchable=True, caught=None)
        self.assertIs(jqf._Handle._select_failure(0, [uncaught]), uncaught)

    def test_a_caught_error_of_the_same_catchable_class_is_not_the_failure(self):
        caught = self.record(catchable=True, caught=0)
        self.assertIsNone(jqf._Handle._select_failure(0, [caught]))

    def test_picks_the_uncaught_record_out_of_a_mix(self):
        caught = self.record(catchable=True, caught=0)
        uncaught = self.record(catchable=True, caught=None)
        self.assertIs(jqf._Handle._select_failure(0, [caught, uncaught]), uncaught)

    def test_no_records_means_no_failure(self):
        self.assertIsNone(jqf._Handle._select_failure(0, []))


class SelectFailureAlsoTrustsTheAbiSentinel(unittest.TestCase):
    """`written < 0` is an INDEPENDENT failure signal, not a fallback that
    needs a diagnostic record to agree with it first. A pure unit test here
    proves the zero-record case is handled without any ctypes/ABI round-trip
    — the same reasoning that made `SelectFailureIsolatesCaughtFromCatchable`
    the right test for the caught/catchable distinction itself.
    """

    def test_negative_written_with_no_records_is_still_a_failure(self):
        failure = jqf._Handle._select_failure(-1, [])
        self.assertIsNotNone(failure, "written < 0 must raise even with an empty diagnostic stream")
        self.assertEqual(failure.severity, "E")
        self.assertIsNone(failure.caught)

    def test_negative_written_does_not_override_a_real_record(self):
        # When a real record already explains the failure, the ABI sentinel must not paper over it with a synthesized
        # one.
        uncaught = SelectFailureIsolatesCaughtFromCatchable.record(catchable=True, caught=None)
        self.assertIs(jqf._Handle._select_failure(-1, [uncaught]), uncaught)

    def test_non_negative_written_with_no_records_is_success(self):
        self.assertIsNone(jqf._Handle._select_failure(0, []))


class FeedFailuresRouteThroughTheCanonicalSelector(unittest.TestCase):
    """`open_feed` and `feed_push` must consult the SAME failure predicate
    every other entry point uses (`_select_failure`: the LAST uncaught
    error-or-halt record, plus the `written < 0` sentinel), never a private
    first-error scan that can drift from it. The spy pins the routing."""

    def tearDown(self):
        # Restore through `staticmethod`: the original was fetched as a plain function, and assigning one directly would
        # bind it as an instance method, breaking every other test in the process.
        if hasattr(self, "_original_select"):
            jqf._Handle._select_failure = staticmethod(self._original_select)

    def _spy_on_select(self):
        self._original_select = jqf._Handle._select_failure
        calls = []

        def spy(written, diagnostics):
            calls.append(written)
            return self._original_select(written, diagnostics)

        jqf._Handle._select_failure = staticmethod(spy)
        return calls

    def test_open_feed_failure_consults_the_selector(self):
        calls = self._spy_on_select()
        handle = jqf._Handle()
        try:
            # A feed over a dead program id is the ABI's defined open failure.
            with self.assertRaises(jqf.JqfError):
                handle.open_feed(0xF00D, 0)
        finally:
            handle.close()
        self.assertEqual(calls, [-1], "open_feed's failure must be selected "
                                      "by the canonical predicate")

    def test_feed_push_failure_consults_the_selector(self):
        handle = jqf._Handle()
        try:
            program = handle.compile(".")
            feed_id = handle.open_feed(program, 0)
            handle.close_feed(feed_id)
            calls = self._spy_on_select()
            with self.assertRaises(jqf.JqfError):
                handle.feed_push(feed_id, b"1\n")
            self.assertEqual(
                calls, [-1],
                "feed_push's failure must be selected by the canonical "
                "predicate",
            )
        finally:
            handle.close()


class UncaughtErrorsAreReportedAsFailures(unittest.TestCase):
    """The failure predicate reads `record.caught is None` — "this
    occurrence was never actually caught" — never `record.catchable`,
    which means "this error CLASS is catch-eligible in principle". Nearly
    every runtime error class IS catch-eligible in principle, so a
    catchability test would almost never fire: `jqf.run("1/0", ...)` would
    report `ok=True` with silently empty output instead of raising
    `JqfError`.
    """

    def assert_raises_jqf_error(self, program, data=b"null"):
        with self.assertRaises(jqf.JqfError) as ctx:
            jqf.run(program, data)
        return ctx.exception.record

    def test_divide_by_zero(self):
        record = self.assert_raises_jqf_error("1/0")
        self.assertEqual(record.name, "RAISE_DIVIDE_BY_ZERO")

    def test_type_mismatch(self):
        record = self.assert_raises_jqf_error('1 + "a"')
        self.assertEqual(record.name, "RAISE_ARITHMETIC")

    def test_iterate_over_a_scalar(self):
        record = self.assert_raises_jqf_error("1 | .[]")
        self.assertEqual(record.name, "RAISE_ITERATE")

    def test_non_integer_slice_index(self):
        record = self.assert_raises_jqf_error('"abc"[1:"x"]')
        self.assertEqual(record.name, "RAISE_SLICE_INDICES")

    def test_explicit_error_call(self):
        record = self.assert_raises_jqf_error('error("boom")')
        self.assertEqual(record.name, "RAISE_PROGRAM")
        self.assertEqual(record.payload, "boom")

    def test_a_program_that_fails_to_compile_raises_not_returns_ok(self):
        # A syntax error is a SETUP failure, not a pipeline failure — it happens before `execute` ever runs, so it is
        # the case a diagnostic-record-only predicate could not catch on its own: `written < 0` with an empty diagnostic
        # stream must still raise, and it is the first thing a new binding user hits.
        record = self.assert_raises_jqf_error("this is not valid jq (((")
        self.assertEqual(record.severity, "E")
        # The MACHINE_SETUP payload is the worded parse rejection (Display law), never a `Parse(ParseRejection { … })`
        # Debug struct literal — the Rust type names must not reach a binding user.
        self.assertIn("cannot parse program", record.payload)
        self.assertNotIn("ParseRejection", record.payload)


class TheInverseDirectionsStillReportSuccess(unittest.TestCase):
    """The other half of the failure contract: the predicate must not turn
    genuinely successful runs into false failures.
    """

    def test_an_error_caught_by_the_program_is_not_a_failure(self):
        result = jqf.run('try (1/0) catch "x"', b"null")
        self.assertTrue(result.ok, f"expected success, got failure: {result.failure}")
        self.assertEqual(result.output, b'"x"\n')

    def test_legitimately_empty_output_is_not_a_failure(self):
        result = jqf.run("empty", b"null")
        self.assertTrue(result.ok, f"expected success, got failure: {result.failure}")
        self.assertEqual(result.output, b"")


if __name__ == "__main__":
    unittest.main()
