"""Tests for the resident feed: `Session.open_feed` / `Feed` over the
`jqf_feed_open`/`jqf_feed_push`/`jqf_feed_poll`/`jqf_feed_finish`/
`jqf_feed_close` ABI surface.

The feed is the record route fed incrementally (pull-buffer): push input in
pieces, pull bounded batches of output, mark end of stream with `finish`.
Its output must be byte-identical to
the whole-input route over the same records — the feed introduces no new
publication law. The binding's own whole-input path is `run_many`
(adjacent values); for well-formed NDJSON — no blank records, no missing
terminators, no CR-only lines, exactly the streams this test pushes — the
adjacent-value drive and the record drive publish identical bytes, which is
precisely why jq's stdin defaults to adjacent values (the never-infer law).
The tests also pin an explicitly computed per-record expectation as an
independent oracle.

No pytest dependency: the binding has no build step (ctypes only), so its
tests stay stdlib-only too. Build the cdylib first, then run directly (from
the repo root):

    cargo build --release -p jqf-sdk-ffi
    python3 bindings/python/tests/test_feed.py -v
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import jqf  # noqa: E402  (path insert above must run first)


def drain(feed):
    """Polls a feed until it reports no more output; returns the bytes."""
    out = b""
    while True:
        batch = feed.poll()
        if not batch:
            return out
        out += batch


class FeedMatchesTheWholeInputRoute(unittest.TestCase):
    """Byte identity is the feed's whole contract: one poll per batch, but the
    same bytes the whole-input record route would publish for the same
    records — compared both against the binding's whole-input path and
    against an explicitly computed per-record expectation."""

    def test_incremental_pushes_poll_to_the_whole_input_bytes(self):
        records = [{"a": k} for k in range(50)]
        stream = b"".join(
            __import__("json").dumps(r).encode() + b"\n" for r in records
        )
        with jqf.Session() as session:
            program = session.compile(".a")
            # Whole-input reference: the binding's own multi-value path over the complete stream (identical to the
            # record route for well-formed NDJSON).
            expected = program.run_many(stream).output
            # Independent oracle: per-record outputs concatenated.
            self.assertEqual(
                expected,
                b"".join(f"{r['a']}\n".encode() for r in records),
            )

            feed = session.open_feed(program)
            # Push in uneven chunks, deliberately splitting mid-record so the held tail is exercised on every boundary.
            cuts = [1, 7, 13, 21, 30, 44, 61, 90, 120, 157, 200, 250, 300]
            start = 0
            for cut in cuts:
                self.assertGreaterEqual(feed.push(stream[start:cut]), 0)
                start = cut
            self.assertGreaterEqual(feed.push(stream[start:]), 0)
            self.assertEqual(drain(feed), expected)

    def test_a_held_tail_completes_on_a_later_push(self):
        with jqf.Session() as session:
            program = session.compile(".a")
            feed = session.open_feed(program)
            # `{"a":1}\n` is complete; `{"a":` is a held partial record.
            feed.push(b'{"a":1}\n{"a":')
            self.assertEqual(feed.poll(), b"1\n")
            self.assertEqual(feed.poll(), b"")
            # The tail stays held (never framed, never faulted) until its terminator arrives on a later push.
            feed.push(b"2}")
            self.assertEqual(feed.poll(), b"")
            feed.push(b"\n")
            self.assertEqual(feed.poll(), b"2\n")
            self.assertEqual(feed.poll(), b"")

    def test_finish_delivers_the_final_unterminated_record(self):
        # A complete final record WITHOUT its terminator stays held forever under pushes alone — but `finish` marks end
        # of stream, so the held tail becomes the FINAL record under the profile's own tail law, exactly what the
        # whole-input route would publish for these bytes.
        with jqf.Session() as session:
            program = session.compile(".a")
            feed = session.open_feed(program)
            feed.push(b'{"a":1}\n{"a":2}')
            self.assertEqual(feed.poll(), b"1\n")
            self.assertEqual(feed.poll(), b"")
            feed.finish()
            self.assertEqual(feed.poll(), b"2\n")
            self.assertEqual(feed.poll(), b"")
            # The stream is over: later pushes are accepted-and-ignored.
            self.assertGreaterEqual(feed.push(b'{"a":3}\n'), 0)
            self.assertEqual(feed.poll(), b"")

    def test_finish_on_a_dead_feed_id_raises(self):
        with jqf.Session() as session:
            program = session.compile(".a")
            feed = session.open_feed(program)
            feed.close()
            with self.assertRaises(jqf.JqfError):
                feed.finish()

    def test_a_strict_fault_is_a_terminal_error_with_the_failure_record(self):
        with jqf.Session() as session:
            program = session.compile(".a")
            feed = session.open_feed(program, profile="strict")
            # Record 2's payload `{"a":2` is not one complete strict-JSON text: record 1 publishes first, then the fault
            # is terminal.
            feed.push(b'{"a":1}\n{"a":2\n')
            self.assertEqual(feed.poll(), b"1\n")
            with self.assertRaises(jqf.JqfError) as ctx:
                feed.poll()
            self.assertEqual(ctx.exception.record.name, "MACHINE_INPUT")
            self.assertEqual(ctx.exception.record.severity, "E")
            # The feed is dead: further polls keep reporting the failure.
            with self.assertRaises(jqf.JqfError):
                feed.poll()

    def test_a_recovering_feed_continues_past_faults(self):
        with jqf.Session() as session:
            program = session.compile(".a")
            feed = session.open_feed(program, profile="recovering")
            # Record 2 is blank (advisory); record 4's payload is malformed (error-severity issue). The good records
            # still publish, in order, and the feed stays alive.
            feed.push(b'{"a":1}\n\n{"a":3}\n{bad\n{"a":5}\n')
            self.assertEqual(drain(feed), b"1\n3\n5\n")


class FeedLifetimeIsDefined(unittest.TestCase):
    """A feed id is a handle-local u32 with the same defined-failure law as a
    program id: every misuse raises `JqfError`, never a segfault."""

    def test_a_feed_dies_with_its_session(self):
        feed = None
        with jqf.Session() as session:
            program = session.compile(".")
            feed = session.open_feed(program)
            feed.push(b"1\n")
            self.assertEqual(feed.poll(), b"1\n")
        with self.assertRaises(jqf.JqfError) as ctx:
            feed.push(b"2\n")
        self.assertIn("session was closed", ctx.exception.record.payload)

    def test_closing_a_feed_makes_its_id_dead(self):
        with jqf.Session() as session:
            program = session.compile(".")
            feed = session.open_feed(program)
            feed.push(b"1\n")
            self.assertEqual(feed.poll(), b"1\n")
            feed.close()
            with self.assertRaises(jqf.JqfError):
                feed.push(b"2\n")
            with self.assertRaises(jqf.JqfError):
                feed.poll()
            # A second close is the same dead-id failure, not a no-op.
            with self.assertRaises(jqf.JqfError):
                feed.close()
            # The session and its programs survive the dead feed.
            self.assertEqual(program.run(b"2").output, b"2\n")

    def test_an_unknown_profile_is_rejected_at_open(self):
        with jqf.Session() as session:
            program = session.compile(".")
            with self.assertRaises(ValueError):
                session.open_feed(program, profile="compatible")


if __name__ == "__main__":
    unittest.main()
