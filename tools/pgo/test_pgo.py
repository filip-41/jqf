"""Regression tests for PGO workload identity and native build constraints.

The tests exercise the scripts as operators use them, without compiling jqf.
"""

from __future__ import annotations

import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import train

HERE = Path(__file__).resolve().parent


class TrainerTests(unittest.TestCase):
    def test_nonpositive_repeats_are_rejected(self) -> None:
        proc = subprocess.run(
            [sys.executable, str(HERE / "train.py"), "--hash", "--repeats", "0"],
            capture_output=True,
            text=True,
        )

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("positive", proc.stderr)


class FreshnessTests(unittest.TestCase):
    def test_stale_training_hash_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "jqf"
            current_code = train.code_hash(train.ROOT)
            binary.write_text(
                "#!/bin/sh\n"
                f"echo 'jqf: build=pgo profile=deadbeef.{current_code}.aarch64-apple-darwin.deadbeef' >&2\n"
            )
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            env = dict(os.environ)
            env["JQF_PGO_BIN"] = str(binary)

            proc = subprocess.run(
                [str(HERE / "jqf-pgo-freshness.sh")],
                capture_output=True,
                text=True,
                env=env,
            )

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("training", proc.stderr)


class BuildScriptTests(unittest.TestCase):
    def test_foreign_target_is_rejected_before_building(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            env = dict(os.environ)
            env.update(
                {
                    "CARGO": "/usr/bin/false",
                    "JQF_PGO_DIR": str(Path(tmp) / "pgo"),
                    "JQF_PGO_OUT": str(Path(tmp) / "jqf"),
                    "JQF_PGO_TARGET": "foreign-unknown-target",
                    "LLVM_PROFDATA": "/usr/bin/true",
                }
            )
            proc = subprocess.run(
                [str(HERE / "jqf-pgo-build.sh")],
                capture_output=True,
                text=True,
                env=env,
            )

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("cross-target PGO is unsupported", proc.stderr)


if __name__ == "__main__":
    unittest.main()
