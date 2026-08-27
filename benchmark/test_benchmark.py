"""Regression tests for benchmark provenance, validation, and measurement.

These tests keep cached cells attributable and reject data that was not
validated or measured under the requested contract.
"""

from __future__ import annotations

import json
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import measure
import run
import setup


class OutputValidationTests(unittest.TestCase):
    def test_boolean_and_number_outputs_are_not_equal(self) -> None:
        self.assertFalse(run.same_output(b"true\n", b"1\n"))
        self.assertFalse(run.same_output(b'{"ok":false}\n', b'{"ok":0}\n'))

    def test_json_numbers_are_compared_without_float_rounding(self) -> None:
        self.assertFalse(run.same_output(b"1.00000000000000001\n", b"1.0\n"))
        self.assertFalse(run.same_output(b"9007199254740992.0\n", b"9007199254740993.0\n"))

    def test_large_outputs_require_identical_bytes(self) -> None:
        compact = b"[" + b",".join([b"0"] * 600_000) + b"]"
        spaced = b"[" + b", ".join([b"0"] * 600_000) + b"]"

        self.assertFalse(run.same_output(compact, spaced))

    def test_failed_oracle_blocks_measurement(self) -> None:
        validate = getattr(run, "validate_capture", None)
        self.assertIsNotNone(validate)
        self.assertEqual(
            validate("timeout", b"", "ok", b"1\n", "jq"),
            {"status": "oracle-timeout", "oracle_tool": "jq"},
        )


class ReceiptTests(unittest.TestCase):
    def test_receipt_loading_never_falls_back_to_another_stamp(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            cache = Path(tmp)
            old = cache / "cells" / "old" / "case.json"
            old.parent.mkdir(parents=True)
            old.write_text(json.dumps({"id": "case", "cells": {}}))

            with patch.object(run, "CACHE", cache):
                got = run.load_all_receipts("new")

            self.assertEqual(got, [])

    def test_cases_file_changes_the_receipt_stamp(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            cases = Path(tmp) / "cases.json"
            cases.write_text('{"revision":1}')
            with patch.object(run, "CASES_PATH", cases):
                first = run.workload_stamp(
                    quick=False,
                    warmup=1,
                    runs=3,
                    tools={"jq": "1"},
                )
                cases.write_text('{"revision":2}')
                second = run.workload_stamp(
                    quick=False,
                    warmup=1,
                    runs=3,
                    tools={"jq": "1"},
                )

            self.assertNotEqual(first, second)

    def test_host_changes_the_workload_stamp(self) -> None:
        common = {"quick": False, "warmup": 1, "runs": 3, "tools": {"jq": "1"}}
        with patch.object(run, "host_facts", return_value={"cpu": "host-a"}):
            first = run.workload_stamp(**common)
        with patch.object(run, "host_facts", return_value={"cpu": "host-b"}):
            second = run.workload_stamp(**common)

        self.assertNotEqual(first, second)

    def test_jqf_build_has_separate_receipt_provenance(self) -> None:
        row = {"jqf_diagnostics": "jqf: build=pgo profile=one"}

        self.assertTrue(run.jqf_receipt_is_current(row, "jqf: build=pgo profile=one"))
        self.assertFalse(run.jqf_receipt_is_current(row, "jqf: build=pgo profile=two"))

    def test_stale_jqf_cells_are_not_reported_under_a_new_build(self) -> None:
        mask = getattr(run, "mask_stale_jqf", None)
        self.assertIsNotNone(mask)
        row = {
            "jqf_diagnostics": "jqf: build=pgo profile=one",
            "cells": {
                "jqf": {"status": "ok", "wall_s": 1.0},
                "jqf-serial": {"status": "ok", "wall_s": 2.0},
                "jq": {"status": "ok", "wall_s": 3.0},
            },
        }

        got = mask(row, "jqf: build=pgo profile=two")

        self.assertEqual(got["cells"]["jqf"], {"status": "stale"})
        self.assertEqual(got["cells"]["jqf-serial"], {"status": "stale"})
        self.assertEqual(got["cells"]["jq"]["wall_s"], 3.0)

    def test_legacy_fixture_stamp_is_regenerated(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            cache = Path(tmp)
            dest = cache / "users-narrow-100.json"
            stamp = cache / "users-narrow-100.json.stamp"
            dest.write_text("stale")
            stamp.write_text("users:narrow:100\n")
            datasets = {
                "users-narrow-100": {
                    "kind": "users",
                    "width": "narrow",
                    "rows": 100,
                    "suffix": ".json",
                }
            }

            def generate_new(_kind: str, _width: str, _rows: int, path: Path) -> None:
                path.write_text("fresh")

            with patch.object(run, "CACHE", cache), patch.object(run, "generate", generate_new):
                run.prepare_fixtures(datasets)

            self.assertEqual(dest.read_text(), "fresh")


class MeasurementTests(unittest.TestCase):
    def test_waiting_for_a_child_does_not_poll(self) -> None:
        with patch.object(measure.time, "sleep", side_effect=AssertionError("polled")):
            result = measure.run_measured([sys.executable, "-c", "pass"], timeout=5)

        self.assertEqual(result.exit_code, 0)

    def test_timeout_failure_keeps_its_status(self) -> None:
        failure = getattr(measure, "MeasurementFailure", None)
        self.assertIsNotNone(failure)
        with self.assertRaises(failure) as caught:
            measure.median_of(
                [sys.executable, "-c", "import time; time.sleep(1)"],
                warmup=0,
                runs=1,
                timeout=0.01,
            )

        self.assertEqual(caught.exception.status, "timeout")

    def test_a_failed_timed_trial_invalidates_the_cell(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            counter = root / "counter"
            child = root / "child.py"
            child.write_text(
                "import pathlib, sys\n"
                "path = pathlib.Path(sys.argv[1])\n"
                "count = int(path.read_text()) + 1 if path.exists() else 1\n"
                "path.write_text(str(count))\n"
                "raise SystemExit(1 if count == 2 else 0)\n"
            )

            with self.assertRaisesRegex(RuntimeError, "timed run 2 failed"):
                measure.median_of(
                    [sys.executable, str(child), str(counter)],
                    warmup=0,
                    runs=3,
                    timeout=5,
                )


class PgoContractTests(unittest.TestCase):
    def test_plain_jqf_binary_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "jqf"
            binary.write_text("#!/bin/sh\necho 'jqf: build=plain profile=none' >&2\n")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)

            with self.assertRaisesRegex(SystemExit, "not a PGO build"):
                run.jqf_bin(str(binary))

    def test_stale_pgo_binary_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "jqf"
            binary.write_text(
                "#!/bin/sh\n"
                "echo 'jqf: build=pgo profile=deadbeef.deadbeef.aarch64-apple-darwin.deadbeef' >&2\n"
            )
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)

            with self.assertRaisesRegex(SystemExit, "not fresh"):
                run.jqf_bin(str(binary))


class ToolVersionTests(unittest.TestCase):
    def test_version_suffix_must_start_after_a_token_boundary(self) -> None:
        path = Path("/unused")
        with patch.object(setup, "_version_line", return_value="gojq 0.12.19 (rev: abc)"):
            self.assertTrue(setup._version_ok(path, "gojq 0.12.19"))
        with patch.object(setup, "_version_line", return_value="jq-1.8.20"):
            self.assertFalse(setup._version_ok(path, "jq-1.8.2"))


if __name__ == "__main__":
    unittest.main()
