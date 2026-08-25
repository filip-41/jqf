"""Tests for `Program.run_many_streaming` — the streaming twin of
`run_many` over the `jqf_run_sequence_streaming` chunk-consumer ABI.

The laws pinned here mirror the Rust-side contract tests
(`jqf-sdk-ffi/tests/contract.rs`, the streaming section): byte identity with
the legacy arm over a single huge value AND across many chunk boundaries,
the bounded-flow law (deliveries at most one chunk except a single oversized
encoder write; the tail flushed), clean cancellation when the consumer
abandons the iterator, and parity of the per-value error channel. The RSS
proof (streamed peak vs staged peak) is `rss_streaming_proof.py` beside this
file — it needs subprocess isolation and does not ride unittest.

No pytest dependency: stdlib-only, like every binding test. Build the cdylib
first, then run directly (from the repo root):

    cargo build --release -p jqf-sdk-ffi
    python3 bindings/python/tests/test_streaming_sequence.py -v
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import jqf  # noqa: E402  (path insert above must run first)
from jqf._ffi import JQF_STREAM_CHUNK_BYTES  # noqa: E402


def stream_all(program, data):
    """One session, one streaming run, concatenated bytes."""
    with jqf.Session() as session:
        program = session.compile(program)
        return b"".join(program.run_many_streaming(data))


class StreamingSequenceTests(unittest.TestCase):
    def test_matches_legacy_over_a_single_huge_value(self):
        # ~52 MB from ONE encoded value: no staged buffer wants this twice.
        program = '"x" * 52000000'
        with jqf.Session() as session:
            compiled = session.compile(program)
            expected = compiled.run_many(b"null").output
            chunks = list(compiled.run_many_streaming(b"null"))
        self.assertEqual(b"".join(chunks), expected, "byte identity")
        self.assertEqual(sum(len(c) for c in chunks), len(expected))
        self.assertGreater(len(chunks), 1, "52 MB crosses in many deliveries")

    def test_matches_legacy_across_many_chunk_boundaries(self):
        # 2000 adjacent values x ~1 KB: several exact chunk seals + a tail.
        program = '"1234567890" * 100'
        data = " ".join(str(i) for i in range(2000)).encode()
        with jqf.Session() as session:
            compiled = session.compile(program)
            expected = compiled.run_many(data).output
            chunks = list(compiled.run_many_streaming(data))
        self.assertEqual(b"".join(chunks), expected)
        self.assertGreater(len(expected), JQF_STREAM_CHUNK_BYTES * 2,
                           "fixture must span more than two chunks")
        for chunk in chunks[:-1]:
            self.assertEqual(len(chunk), JQF_STREAM_CHUNK_BYTES,
                             "every non-final chunk seals at the bound")
        self.assertTrue(chunks[-1], "the final partial chunk is flushed")

    def test_cancel_on_abandonment_stops_cleanly(self):
        # Abandon mid-stream: closing the generator cancels the run inside the FFI call; nothing may raise out of
        # close(), and the session stays usable afterwards.
        program = '"1234567890" * 100'
        data = " ".join(str(i) for i in range(5000)).encode()
        with jqf.Session() as session:
            compiled = session.compile(program)
            it = compiled.run_many_streaming(data)
            next(it)  # the run is live inside the FFI call now
            it.close()
            # The handle survived the cancellation: a normal run answers.
            result = compiled.run_many(data)
            self.assertEqual(result.failure, None)

    def test_per_value_errors_match_the_legacy_channel(self):
        # error("boom") raises per input value; the prefix (nothing here) yields first, then the structured failure
        # raises. The legacy arm raises the SAME structured record.
        program = 'error("boom")'
        data = b"1 2"
        with jqf.Session() as session:
            compiled = session.compile(program)
            with self.assertRaises(jqf.JqfError) as legacy:
                compiled.run_many(data)
            chunks = []
            with self.assertRaises(jqf.JqfError) as streamed:
                for chunk in compiled.run_many_streaming(data):
                    chunks.append(chunk)
        self.assertEqual(chunks, [], "a first-value raise publishes nothing")
        self.assertEqual(streamed.exception.record.code,
                         legacy.exception.record.code)

    def test_empty_input_yields_nothing_and_succeeds(self):
        self.assertEqual(stream_all(".", b""), b"")

    def test_dead_session_raises_the_defined_error(self):
        session = jqf.Session()
        compiled = session.compile(".")
        session.close()
        with self.assertRaises(jqf.JqfError):
            list(compiled.run_many_streaming(b"1"))


if __name__ == "__main__":
    unittest.main()
