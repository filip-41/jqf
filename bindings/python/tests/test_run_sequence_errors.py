"""Tests for `run_many`'s per-value error plumbing: per-value errors are
retained by the run and read back through the length-carrying
`jqf_run_errors_count` / `jqf_run_error_get` getters (a NUL byte is a valid
payload character, so a delimiter-joined text channel could collapse two
errors into one). The binding surfaces them as `RunResult.errors`; this file
pins the contract from OUTSIDE the ABI, through the public surface.

No pytest dependency: the binding itself has no build step (ctypes only), so
its tests stay stdlib-only too. Build the cdylib first, then run directly
(from the repo root):

    cargo build --release -p jqf-sdk-ffi
    python3 bindings/python/tests/test_run_sequence_errors.py -v
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import jqf  # noqa: E402  (path insert above must run first)


class SequenceErrorsContinuePastAndAreRetrievable(unittest.TestCase):
    """A per-value error does not kill the sequence (jq's
    continue-on-error law), and every error's TEXT is retrievable —
    including one whose text contains a NUL byte, which a NUL-terminated
    channel cannot carry.
    """

    def test_run_many_continues_past_a_per_value_error(self):
        # Value 1 raises `error("boom")`; value 2 publishes. The run completes with value 2's output (the CLI's
        # last-value law), and the failed value's error text rides beside it on the result.
        result = jqf.run_many('if . == 1 then error("boom") else . end',
                              b"1\n2\n")
        self.assertTrue(result.ok)
        self.assertEqual(result.output, b"2\n")
        self.assertEqual(result.errors, ["boom"])

    def test_a_nul_inside_an_error_text_survives_the_channel(self):
        # `error("a\0b")` on values 1 and 2: two errors, both with a NUL byte in the middle of their text. A successful
        # run must carry both texts exactly.
        result = jqf.run_many(
            'if . <= 2 then error("a\\u0000b") else . end',
            b"1\n2\n3\n",
        )
        self.assertTrue(result.ok)
        self.assertEqual(result.output, b"3\n")
        self.assertEqual(result.errors, ["a\x00b", "a\x00b"])


if __name__ == "__main__":
    unittest.main()
