"""Tests for the compiled-program path: `Session`/`Program` over the
`jqf_compile`/`jqf_run_compiled`/`jqf_run_sequence_compiled`/`jqf_program_free`
ABI surface.

No pytest dependency: the binding itself has no build step (ctypes only), so
its tests stay stdlib-only too. Build the cdylib first, then run directly
(from the repo root):

    cargo build --release -p jqf-sdk-ffi
    python3 bindings/python/tests/test_compile_once.py -v
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import jqf  # noqa: E402  (path insert above must run first)


class CompiledRunsMatchPerCallRuns(unittest.TestCase):
    """The compiled path is additive, not a rewrite: same bytes, same
    diagnostics, same failure contract as the one-shot `run`."""

    def test_output_and_success_are_byte_identical(self):
        # The identity claim runs BOTH paths: the one-shot `jqf.run` and the compiled `Program.run` over identical
        # input, compared byte for byte (the fixed bytes keep the shared answer anchored).
        one_shot = jqf.run(".a", b'{"a":1}')
        with jqf.Session() as session:
            program = session.compile(".a")
            result = program.run(b'{"a":1}')
        self.assertTrue(result.ok, f"expected success, got {result.failure}")
        self.assertEqual(result.output, b"1\n")
        self.assertEqual(one_shot.output, result.output)

    def test_an_uncaught_error_raises_exactly_like_the_one_shot_path(self):
        with jqf.Session() as session:
            program = session.compile("1/0")
            with self.assertRaises(jqf.JqfError) as ctx:
                program.run(b"null")
        self.assertEqual(ctx.exception.record.name, "RAISE_DIVIDE_BY_ZERO")

    def test_a_caught_error_is_still_success(self):
        with jqf.Session() as session:
            program = session.compile('try (1/0) catch "x"')
            result = program.run(b"null")
        self.assertTrue(result.ok)
        self.assertEqual(result.output, b'"x"\n')

    def test_run_many_matches_the_sequence_route(self):
        # The sequence route itself runs beside its compiled twin: the one-shot `jqf.run_many` and `Program.run_many`
        # over the same multi-value input must publish byte-identical output.
        one_shot = jqf.run_many(".", b"1\n2\n")
        with jqf.Session() as session:
            program = session.compile(".")
            result = program.run_many(b"1\n2\n")
        self.assertTrue(result.ok)
        self.assertEqual(result.output, b"1\n2\n")
        self.assertEqual(one_shot.output, result.output)


class DiagnosticStreamIsPerRun(unittest.TestCase):
    """The record stream contract travels to the compiled path: the compiled
    path reads the stream when the run FAILED (the explaining record becomes
    the raised JqfError), and a successful run's stream is not copied per
    record — the informational records stay in the handle, pollable through
    the ABI, but the binding's per-record path does not pay for them."""

    def test_a_failed_run_raises_with_the_explaining_record(self):
        with jqf.Session() as session:
            failing = session.compile("1/0")
            with self.assertRaises(jqf.JqfError) as ctx:
                failing.run(b"null")
            self.assertEqual(ctx.exception.record.name, "RAISE_DIVIDE_BY_ZERO")
            # The explaining record is the uncaught error itself, not a synthesized fallback: severity E and never
            # caught.
            self.assertEqual(ctx.exception.record.severity, "E")
            self.assertIsNone(ctx.exception.record.caught)

    def test_a_compile_failure_raises_with_the_setup_record(self):
        with jqf.Session() as session:
            with self.assertRaises(jqf.JqfError) as ctx:
                session.compile("this is not valid jq (((")
            self.assertEqual(ctx.exception.record.severity, "E")
            self.assertEqual(ctx.exception.record.name, "MACHINE_SETUP")


class ProgramLifetimeIsDefined(unittest.TestCase):
    """The four lifetime hazards, from the binding's side: every misuse is a
    defined `JqfError`, never a segfault in the host process."""

    def test_free_after_use_is_clean(self):
        with jqf.Session() as session:
            program = session.compile(".")
            self.assertEqual(program.run(b"1").output, b"1\n")
            program.free()
            # The id is dead: running it raises, and the session survives.
            with self.assertRaises(jqf.JqfError):
                program.run(b"1")
            # The session still works for a NEW program.
            second = session.compile(".")
            self.assertEqual(second.run(b"2").output, b"2\n")

    def test_double_free_raises_the_defined_error(self):
        with jqf.Session() as session:
            program = session.compile(".")
            program.free()
            # The ABI answers a defined -1 for the dead id, and `free()` surfaces that as the documented JqfError —
            # never a crash, never a silent no-op.
            with self.assertRaises(jqf.JqfError):
                program.free()

    def test_use_after_free_raises_with_the_recorded_cause(self):
        with jqf.Session() as session:
            program = session.compile(".")
            program.free()
            with self.assertRaises(jqf.JqfError) as ctx:
                program.run(b"1")
            self.assertEqual(ctx.exception.record.name, "MACHINE_SETUP")
            self.assertIn("not a live program", ctx.exception.record.payload)

    def test_a_program_dies_with_its_session(self):
        program = None
        with jqf.Session() as session:
            program = session.compile(".")
            self.assertEqual(program.run(b"1").output, b"1\n")
        # The session is closed; the Program object is dead but its use is a defined error, not a segfault.
        with self.assertRaises(jqf.JqfError) as ctx:
            program.run(b"1")
        self.assertIn("session was closed", ctx.exception.record.payload)

    def test_compile_after_close_raises_jqferror_not_attributeerror(self):
        # `Session.close()` swaps `_handle` for None; every Session entry point that dereferences it must answer the
        # defined JqfError — the same dead-session law Program/Feed keep — never an AttributeError on the None handle.
        session = jqf.Session()
        session.close()
        with self.assertRaises(jqf.JqfError) as ctx:
            session.compile(".")
        self.assertIn("session was closed", ctx.exception.record.payload)
        with self.assertRaises(jqf.JqfError) as ctx:
            session.compile_args("$x", x=1)
        self.assertIn("session was closed", ctx.exception.record.payload)
        with self.assertRaises(jqf.JqfError) as ctx:
            session.open_feed(jqf.Session().compile("."))
        self.assertIn("session was closed", ctx.exception.record.payload)

    def test_diagnostics_are_lazy_on_the_compiled_path(self):
        # The compiled path's per-record contract: a SUCCESSFUL run does not copy the retained stream (informational
        # records stay in the handle), and a FAILED run's explaining record is read and raised.
        with jqf.Session() as session:
            clean = session.compile(".")
            result = clean.run(b"1")
        self.assertTrue(result.ok)
        self.assertEqual(result.diagnostics, [])
        with jqf.Session() as session:
            failing = session.compile("1/0")
            with self.assertRaises(jqf.JqfError):
                failing.run(b"null")


if __name__ == "__main__":
    unittest.main()
