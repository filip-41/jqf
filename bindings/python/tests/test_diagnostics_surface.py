"""Tests for the 083 correct-core and 084 diagnostic-channel surfaces: the
binding's own view of the ABI v2 changes.

No pytest dependency: the binding has no build step (ctypes only), so its
tests stay stdlib-only too. Build the cdylib first, then run directly (from
the repo root):

    cargo build --release -p jqf-sdk-ffi
    python3 bindings/python/tests/test_diagnostics_surface.py -v
"""
import os
import sys
import threading
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import jqf  # noqa: E402  (path insert above must run first)


class HaltSurfacesStatusAndMessage(unittest.TestCase):
    """`halt_error(n)` is how a jq program signals a controlled
    failure — a binding that loses the status and the message cannot
    distinguish it from `halt`."""

    def test_halt_error_delivers_status_and_message(self):
        with self.assertRaises(jqf.JqfError) as ctx:
            jqf.run('halt_error(5)', b'"boom"')
        record = ctx.exception.record
        self.assertEqual(record.name, "RAISE_HALT")
        self.assertEqual(record.halt_status, 5)
        self.assertEqual(record.payload, "boom")

    def test_bare_halt_is_distinguishable(self):
        with self.assertRaises(jqf.JqfError) as ctx:
            jqf.run('halt', b'null')
        record = ctx.exception.record
        self.assertEqual(record.name, "RAISE_HALT")
        self.assertEqual(record.halt_status, 0)
        self.assertIsNone(record.payload)

    def test_a_raised_object_carries_its_payload(self):
        # `error({"code":42})` must arrive as its compact JSON, recoverable from the record's payload.
        with self.assertRaises(jqf.JqfError) as ctx:
            jqf.run('error({"code":42})', b'null')
        record = ctx.exception.record
        self.assertEqual(record.name, "RAISE_PROGRAM")
        self.assertEqual(record.payload, '{"code":42}')

    def test_a_nul_bearing_payload_survives_the_record_channel(self):
        # The C-string channel cannot carry a NUL byte: the Rust side hands back NULL for it and the binding recovers
        # the text through the length-carrying getter. The full three bytes `a\0b` must arrive.
        with self.assertRaises(jqf.JqfError) as ctx:
            jqf.run('error("a\\u0000b")', b'null')
        record = ctx.exception.record
        self.assertEqual(record.name, "RAISE_PROGRAM")
        self.assertEqual(record.payload, "a\x00b")

    def test_the_diagnostic_locators_are_readable(self):
        # A typed error's record carries its failing step; a single-value run has no stream position, so ordinal and
        # offset arrive as the documented absent markers.
        with self.assertRaises(jqf.JqfError) as ctx:
            jqf.run('.a', b'1')
        record = ctx.exception.record
        self.assertIsNotNone(record.step_index)
        self.assertIsNone(record.input_ordinal)
        self.assertIsNone(record.byte_offset)


class BindingsReachTheProgram(unittest.TestCase):
    """Host data reaches a program as a `$name` constant (JSON-parsed
    by the engine, never spliced into the source), and `$ARGS` resolves."""

    def test_a_binding_answers_like_the_cli(self):
        with jqf.Session() as session:
            program = session.compile_args("$x + 1", x=41)
            self.assertEqual(program.run(b"null").output, b"42\n")

    def test_args_resolves(self):
        with jqf.Session() as session:
            program = session.compile_args("$ARGS", x=41)
            self.assertEqual(
                program.run(b"null").output,
                b'{"positional":[],"named":{"x":41}}\n',
            )

    def test_a_bad_binding_value_is_rejected_clearly(self):
        with jqf.Session() as session:
            # `object()` is not JSON-encodable: the binding must refuse with a clear ValueError, never splice garbage
            # into the source.
            with self.assertRaises(ValueError):
                session.compile_args("$x", x=object())


class RecoveringFeedIssuesAreObservable(unittest.TestCase):
    """The recovering profile's ordered framing/payload issues ride
    the diagnostic stream on a SUCCESSFUL poll — a binding that reads the
    stream only on failure loses them."""

    def test_a_malformed_record_produces_an_observable_issue(self):
        with jqf.Session() as session:
            program = session.compile(".a")
            feed = session.open_feed(program, profile="recovering")
            # Record 4's payload `{bad` is malformed JSON: an error-severity issue; the good records still publish and
            # the feed stays alive.
            feed.push(b'{"a":1}\n{"a":2}\n{"a":3}\n{bad\n{"a":5}\n')
            # The whole stream is one batch: the first poll runs the drive (publishing the good records AND recording
            # the malformed record's issue), and `last_diagnostics` is read right after it — a later poll clears the
            # stream, exactly as a later run does.
            output = feed.poll()
            self.assertEqual(output, b"1\n2\n3\n5\n")
            issues = feed.last_diagnostics()
            self.assertTrue(
                any(r.severity == "E" for r in issues),
                "the recovering feed's malformed record must surface as an "
                "observable issue on a successful poll",
            )
            self.assertTrue(
                any(r.name == "MACHINE_INPUT" for r in issues),
                "the issue must be the codec-input family, not a whisper",
            )
            # The issue's own locators survive the ctypes channel: which record faulted and where its payload starts in
            # the pushed bytes (`{bad` is the fourth record, at byte 24).
            located = [r for r in issues if r.name == "MACHINE_INPUT" and r.severity == "E"]
            self.assertEqual(len(located), 1)
            self.assertEqual(located[0].input_ordinal, 3)
            self.assertEqual(located[0].byte_offset, 24)
            # Draining to completion stays clean (the final empty polls keep the stream current but never republish the
            # batch).
            while True:
                batch = feed.poll()
                if not batch:
                    break


class SessionSerializesItsAbiCalls(unittest.TestCase):
    """The Rust handle is one-thread-at-a-time and ctypes releases
    the GIL during a call, so the Python Session serializes every ABI call
    with a lock. Two threads hammering one session must not race it."""

    def test_two_threads_share_one_session_without_racing(self):
        with jqf.Session() as session:
            program = session.compile(".x")
            feed = session.open_feed(program, profile="recovering")
            results = []
            feed_outputs = []

            def worker():
                for _ in range(50):
                    results.append(program.run(b'{"x":1}').output)
                    # The feed path reads the diagnostic stream after the poll — a multi-call ABI sequence — so hammer
                    # it against `run`, which clears and repopulates the same handle stream, to exercise the locked poll
                    # sequence.
                    feed.push(b'{"x":1}\n')
                    feed_outputs.append(feed.poll())

            threads = [threading.Thread(target=worker) for _ in range(4)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()
            self.assertEqual(len(results), 200)
            self.assertTrue(all(r == b"1\n" for r in results))
            # Every one of the 200 pushed records is published exactly once (`1\n` = 2 bytes), and every batch is a
            # WHOLE sequence of such items — never a torn or raced partial.
            self.assertEqual(sum(len(o) for o in feed_outputs), 200 * len(b"1\n"))
            self.assertTrue(
                all(
                    o == b""
                    or (o.endswith(b"\n") and set(o.split(b"\n")) <= {b"", b"1"})
                    for o in feed_outputs
                ),
                "every feed batch must be whole `1\\n` items, never torn",
            )


class AFloodedStreamReportsItsDroppedCount(unittest.TestCase):
    """The ABI's diagnostic buffer keeps at most 4096 records and evicts
    oldest-first: a run emitting more must SAY how many were lost, or a
    consumer cannot tell a complete stream from a truncated one."""

    def test_more_records_than_the_cap_reports_the_overflow(self):
        handle = jqf._Handle()
        try:
            # One caught raise per input value: more diagnostic records than the cap, none terminal.
            result = handle.run_sequence(
                'try .a catch empty', b"5\n" * 5000
            )
            self.assertTrue(result.ok)
            self.assertEqual(
                len(result.diagnostics), 4096,
                "the flood must saturate the cap for this pin to mean anything",
            )
            self.assertGreater(result.diag_dropped, 0)
        finally:
            handle.close()

    def test_an_unflooded_stream_reports_zero_dropped(self):
        handle = jqf._Handle()
        try:
            result = handle.run_sequence(".a", b'{"a":1}\n{"a":2}\n')
            self.assertTrue(result.ok)
            self.assertEqual(result.output, b"1\n2\n")
            self.assertEqual(result.diag_dropped, 0)
        finally:
            handle.close()


class CloseRacingALockedCallIsDefined(unittest.TestCase):
    """The close-vs-use race: a caller that resolves the session handle,
    passes its liveness check, and THEN blocks on the session lock while
    `Session.close` frees the handle must raise the defined `JqfError` —
    never touch the freed C session. The lock is the fence: `_Handle.close`
    marks the handle dead (and drops the pointer) under the lock, and every
    locked entry re-checks that mark after acquiring it.

    The interleave is staged deterministically: hold the lock, start the
    racer (it passes its pre-lock check and blocks), then run
    `Session.close` on THIS thread — the RLock is re-entrant per thread, so
    close proceeds exactly as an unsynchronized close would — and release.
    """

    def _race_close_against(self, exercise):
        session = jqf.Session()
        program = session.compile(".x")
        feed = session.open_feed(program)
        handle = session._handle
        started = threading.Event()
        outcome = {}

        def racer():
            started.set()
            try:
                exercise(program, feed)
                outcome["kind"] = "ok"
            except jqf.JqfError as exc:
                outcome["kind"] = "jqf_error"
                outcome["payload"] = exc.record.payload
            except BaseException as exc:  # noqa: BLE001 - the point of the test
                outcome["kind"] = f"unexpected:{type(exc).__name__}"

        with handle._lock:
            thread = threading.Thread(target=racer)
            thread.start()
            started.wait()
            session.close()
        thread.join()
        self.assertEqual(
            outcome.get("kind"),
            "jqf_error",
            "a call racing a close must raise the defined dead-session "
            "JqfError, never another exception class",
        )
        self.assertIn("session was closed", outcome.get("payload", ""))
        # What the racer's post-lock re-check read: the death mark is set under the lock and the pointer died with the
        # allocation.
        self.assertTrue(handle._closed)
        self.assertIsNone(handle._ptr)

    def test_program_run_racing_close_raises(self):
        # The locked compiled-run path: one FFI call plus its finish.
        self._race_close_against(
            lambda program, feed: program.run(b'{"x":1}')
        )

    def test_feed_poll_racing_close_raises(self):
        # The locked multi-call poll sequence: feed_poll AND its diagnostic read-back must not survive into a freed
        # handle either.
        def exercise(program, feed):
            feed.push(b'{"x":1}\n')
            feed.poll()

        self._race_close_against(exercise)

    def test_streaming_worker_racing_close_raises(self):
        # The streaming worker captures its handle before it starts; its locked region must refuse a handle closed while
        # the consumer was still draining.
        def exercise(program, feed):
            list(program.run_many_streaming(b'{"x":1}\n{"x":2}\n'))

        self._race_close_against(exercise)


if __name__ == "__main__":
    unittest.main()
