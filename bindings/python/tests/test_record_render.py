"""Tests for the binding's own pure surfaces: `Record.render()`'s template
table and `_run_with_growth`'s truncated-output retry budget
(`JqfTruncatedError`). Both are exercised without any ctypes call — a
`Record` is constructed directly, exactly as `_collect_diags` does.

No pytest dependency: the binding has no build step (ctypes only), so its
tests stay stdlib-only too. Build the cdylib first, then run directly
(from the repo root):

    cargo build --release -p jqf-sdk-ffi
    python3 bindings/python/tests/test_record_render.py -v
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import jqf  # noqa: E402  (path insert above must run first)


def record(code, kind="number", operand='"a"', payload=None):
    """One diagnostic record as the ABI delivers it (a caught-less typed
    error by default), with every locator absent."""
    return jqf.Record(
        code=code, revision=1, class_="S", severity="E",
        catchable=True, caught=None,
        step_index=None, input_ordinal=None, byte_offset=None,
        halt_status=-1,
        kind=kind, operand=operand, payload=payload,
    )


class RenderTemplatesCoverTheRegistry(unittest.TestCase):
    """Every code with a render template produces its jq-shaped sentence;
    codes without one degrade to their payload, then to a named stub — a
    record never renders as nothing."""

    def test_typed_raise_templates(self):
        cases = [
            (jqf.codes.Code.RAISE_ITERATE, 'Cannot iterate over number ("a")'),
            (jqf.codes.Code.RAISE_INDEX, 'Cannot index number with "a"'),
            (jqf.codes.Code.RAISE_OBJECT_KEY, 'Cannot use number ("a") as object key'),
            (jqf.codes.Code.RAISE_NO_LENGTH, 'number ("a") has no length'),
            (jqf.codes.Code.RAISE_NO_KEYS, 'number ("a") has no keys'),
            (jqf.codes.Code.RAISE_SLICE_INDICES,
             "Array/string slice indices must be integers"),
            (jqf.codes.Code.RAISE_DIVIDE_BY_ZERO, "division by zero"),
            (jqf.codes.Code.RAISE_NONTERMINATING, "non-terminating decimal division"),
        ]
        for code, expected in cases:
            with self.subTest(code=code):
                self.assertEqual(record(code).render(), expected)

    def test_an_index_error_without_an_operand_names_only_the_kind(self):
        rendered = record(jqf.codes.Code.RAISE_INDEX, operand=None).render()
        self.assertEqual(rendered, "Cannot index number")

    def test_the_program_raise_renders_its_payload(self):
        self.assertEqual(
            record(jqf.codes.Code.RAISE_PROGRAM, payload="boom").render(),
            "boom",
        )

    def test_a_payload_less_program_raise_still_says_something(self):
        rendered = record(jqf.codes.Code.RAISE_PROGRAM, payload=None).render()
        self.assertEqual(rendered, "error")

    def test_informational_templates(self):
        self.assertEqual(
            record(jqf.codes.Code.ROUTE_SELECTED, operand="input-sequence").render(),
            "route: input-sequence",
        )
        self.assertEqual(
            record(jqf.codes.Code.COST_SNAPSHOT, operand="42b").render(),
            "cost: 42b",
        )
        self.assertEqual(
            record(jqf.codes.Code.PRECISION_BOUNDARY).render(),
            "exact-to-binary64 contagion",
        )

    def test_an_untemplated_code_falls_back_to_payload_then_name(self):
        untemplated = 0xFFFF
        self.assertEqual(record(untemplated, payload="setup broke").render(),
                         "setup broke")
        self.assertEqual(
            record(untemplated, payload=None).render(),
            "<unknown> (no template in v1)",
        )


class TheTruncatedRetryBudgetRaises(unittest.TestCase):
    """`_run_with_growth` re-calls while the ABI reports a larger required
    size; when the size never stabilizes within the budget it raises
    `JqfTruncatedError` rather than returning a silently truncated buffer."""

    def test_a_never_stabilizing_output_raises_truncated(self):
        attempts = []

        def attempt(out_cap):
            attempts.append(out_cap)
            # Always claim more than offered: the output never fits.
            return out_cap + 1, b"\0" * out_cap

        with self.assertRaises(jqf.JqfTruncatedError) as ctx:
            jqf._Handle._run_with_growth(attempt, 0)
        self.assertIsInstance(ctx.exception, RuntimeError)
        self.assertEqual(len(attempts), jqf._MAX_GROW_ATTEMPTS)

    def test_an_output_that_stabilizes_fits_within_the_budget(self):
        def attempt(out_cap):
            # A fixed 5-byte answer that fits any buffer of 5 or more.
            written = min(5, out_cap)
            return written, b"\0" * out_cap

        written, buf = jqf._Handle._run_with_growth(attempt, 0)
        self.assertEqual(written, 5)


class TheSeedBufferIsCapped(unittest.TestCase):
    """The growth loop's FIRST buffer seeds from the input length, but a
    huge input must not speculatively stage sixteen times its own size
    before any output byte is known: the seed caps at `_MAX_SEED_OUT_CAP`
    and the ABI-reported required size drives the exact re-call."""

    def test_a_huge_input_seeds_at_the_cap_not_sixteen_times_its_size(self):
        seen = []

        def attempt(out_cap):
            seen.append(out_cap)
            return 0, b"\0" * out_cap

        jqf._Handle._run_with_growth(attempt, 1 << 30)
        self.assertEqual(seen[0], jqf._MAX_SEED_OUT_CAP)

    def test_small_inputs_keep_the_floor_seed(self):
        seen = []

        def attempt(out_cap):
            seen.append(out_cap)
            return 0, b"\0" * out_cap

        jqf._Handle._run_with_growth(attempt, 0)
        self.assertEqual(seen[0], 65536)


if __name__ == "__main__":
    unittest.main()
