#!/usr/bin/env python3
"""Shared measurement core for jqf bench harnesses.

One job: run a child command and report its wall time AND its per-process
resource usage (user/sys/minflt/majflt/maxrss) from one coherent run, using
`os.wait4` rather than `subprocess` so the rusage of exactly the child we
timed is available on both hosts.

Why not /usr/bin/time:
- BSD `/usr/bin/time -l` (macOS) exists but GNU/`time -v` (Debian) is a
  different output vocabulary. `wait4` is POSIX and identical everywhere
  this tree measures (macOS + linux-arm64 container).
- `resource.getrusage(RUSAGE_CHILDREN)` deltas give correct user/sys/minflt
  (cumulative sums) but a WRONG per-run maxrss: `ru_maxrss` for the children
  set is a running maximum, so a run that peaked below an earlier sibling
  reports a zero delta. `wait4` returns the rusage of the one reaped child.

Usage (module):
    (the file is dash-named, so import it via importlib under the module
    name jqf_measure — see tools/jqf-broad-bench.py for the loader)
    run = run_measured(["jqf", ".", fixture], timeout=60)
    best = best_of(["jqf", ".", fixture], runs=7, timeout=60)
    line = provenance_of(jqf_bin)          # first `jqf: build=...` stderr line

Usage (CLI, for shell harnesses):
    jqf-measure.py --runs N --timeout S -- cmd...
    prints one line: user=.. wall=.. sys=.. minflt=.. majflt=.. maxrss_bytes=.. exit=..
    (best-of-N by user time; the line is the winning run, coherent fields)
    jqf-measure.py --runs N --timeout S --phase LABEL -- cmd...
    same line with a phase=LABEL field
    jqf-measure.py --provenance --jqf BIN
    prints the binary's provenance line from `jqf --diagnostics`
    jqf-measure.py --self-test
    runs a trivial child (yes/head) and asserts the receipt shape.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from typing import Optional

# The only ambiguity that must never reach a receipt: a timed run that
# crashed before doing its work. Every field except wall is zero for such a
# run and would render as a suspiciously fast lane, so callers check `exit`.
EXIT_EXEC_FAILED = 127
EXIT_TIMED_OUT = 124  # same convention as coreutils timeout


@dataclass
class MeasuredRun:
    wall_s: float
    user_s: float
    sys_s: float
    minflt: int
    majflt: int
    maxrss_bytes: int
    exit_code: int
    timed_out: bool = False


def run_measured(argv: list[str], timeout: float, env: Optional[dict] = None,
                 stdin=os.devnull) -> MeasuredRun:
    """Run `argv` once, reporting wall time and the child's own rusage.

    The child's stdin/stdout/stderr are `/dev/null` (a timed run never
    publishes bytes); the fixture is passed as a positional argument, which is
    how every jqf lane is invoked. Timeout kills the child and reports
    `timed_out`; the receipt must not treat a timed-out run as a number.
    """
    t0 = time.monotonic()
    child_env = dict(os.environ)
    if env:
        child_env.update(env)
    devnull = os.open(stdin, os.O_RDWR)
    pid = os.fork()
    if pid == 0:
        try:
            os.dup2(devnull, 0)
            os.dup2(devnull, 1)
            os.dup2(devnull, 2)
            os.close(devnull)
            os.execvpe(argv[0], argv, child_env)
        except BaseException:
            os._exit(EXIT_EXEC_FAILED)
    os.close(devnull)
    deadline = time.monotonic() + timeout
    while True:
        done, status, usage = os.wait4(pid, os.WNOHANG)
        if done == pid:
            break
        if time.monotonic() > deadline:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.wait4(pid, 0)
            return MeasuredRun(
                wall_s=time.monotonic() - t0,
                user_s=0.0, sys_s=0.0, minflt=0, majflt=0,
                maxrss_bytes=0, exit_code=EXIT_TIMED_OUT, timed_out=True,
            )
        time.sleep(0.002)
    wall = time.monotonic() - t0
    if os.WIFSIGNALED(status):
        exit_code = -os.WTERMSIG(status)
    else:
        exit_code = os.WEXITSTATUS(status)
    # ru_maxrss is bytes on macOS (and other BSDs), kilobytes on Linux;
    # normalize to bytes so a receipt row means the same thing on both hosts.
    maxrss = usage.ru_maxrss
    if sys.platform.startswith("linux"):
        maxrss *= 1024
    return MeasuredRun(
        wall_s=wall,
        user_s=usage.ru_utime,
        sys_s=usage.ru_stime,
        minflt=usage.ru_minflt,
        majflt=usage.ru_majflt,
        maxrss_bytes=maxrss,
        exit_code=exit_code,
    )


def best_of(argv: list[str], runs: int, timeout: float, env: Optional[dict] = None,
            min_runs: int = 1) -> MeasuredRun:
    """Best-of-N by user time, the decision metric for an A/B verdict.

    Wall confirms (recorded on the same winning run); user time decides.
    A run that timed out or crashed is never the
    winner; if fewer than `min_runs` clean runs complete the call fails by
    raising RuntimeError so no receipt row can be fabricated.
    """
    measured = []
    for _ in range(max(1, runs)):
        run = run_measured(argv, timeout, env=env)
        if run.timed_out or run.exit_code != 0:
            continue
        measured.append(run)
    if len(measured) < min_runs:
        raise RuntimeError(
            f"best_of: only {len(measured)}/{runs} clean runs for {argv!r}"
        )
    measured.sort(key=lambda r: r.user_s)
    return measured[0]


def provenance_of(jqf_bin: str, timeout: float = 30.0) -> str:
    """The binary's own `--diagnostics` provenance line, verbatim.

    `jqf --diagnostics .` with empty stdin prints one line
    `jqf: build=... profile=... allocator=... platform=...` on stderr before
    the request runs. Every receipt row is only as good as this line; a caller
    that gets an empty result must refuse to record rows.
    """
    proc = subprocess.run(
        [jqf_bin, "--diagnostics", "."],
        input=b"", capture_output=True, timeout=timeout,
        env={**os.environ, "JQF_NO_CONFIG": "1"},
    )
    for line in proc.stderr.decode("utf-8", "replace").splitlines():
        if line.startswith("jqf: build="):
            return line
    return ""


def receipt_line(run: MeasuredRun, **fields) -> str:
    """One canonical receipt line: `receipt: key=value ...`.

    The provenance string may contain spaces, so it rides quoted. Every other
    field is a bare token. This is the line a harness greps for; its shape is
    the receipt contract (`user=... sys=... minflt=... maxrss_bytes=...` plus
    the binary's provenance).
    """
    base = {
        "wall_s": f"{run.wall_s:.4f}",
        "user_s": f"{run.user_s:.4f}",
        "sys_s": f"{run.sys_s:.4f}",
        "minflt": str(run.minflt),
        "majflt": str(run.majflt),
        "maxrss_bytes": str(run.maxrss_bytes),
        "exit": str(run.exit_code),
    }
    base.update(fields)
    tokens = []
    for key, value in base.items():
        if key == "provenance":
            tokens.append(f'provenance="{value}"')
        else:
            tokens.append(f"{key}={value}")
    return "receipt: " + " ".join(tokens)


def _cli() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=1, help="best-of-N by user time")
    parser.add_argument("--timeout", type=float, default=60.0)
    parser.add_argument("--provenance", action="store_true",
                        help="print the --diagnostics provenance line of --jqf")
    parser.add_argument("--jqf", default=None, help="jqf binary for --provenance")
    parser.add_argument("--phase", default=None,
                        help="phase label; lands as phase=... on the receipt line")
    parser.add_argument("--self-test", action="store_true",
                        help="run a trivial child and assert the receipt shape")
    parser.add_argument("cmd", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.self_test:
        probe = [sys.executable, "-c", "import time; time.sleep(0.05)"]
        run = run_measured(probe, timeout=10.0)
        if run.exit_code != 0 or run.timed_out or run.wall_s < 0.04:
            print(f"jqf-measure: self-test FAILED {run}", file=sys.stderr)
            return 2
        line = receipt_line(run, probe="self-test")
        print(line)
        print("jqf-measure: self-test PASS", file=sys.stderr)
        return 0
    if args.provenance:
        if not args.jqf:
            parser.error("--provenance requires --jqf BIN")
        line = provenance_of(args.jqf)
        if not line:
            print("jqf-measure: no provenance line from --diagnostics", file=sys.stderr)
            return 2
        print(line)
        return 0
    if not args.cmd:
        parser.error("a command is required")
    if args.cmd[0] == "--":
        args.cmd = args.cmd[1:]
    best = best_of(args.cmd, runs=args.runs, timeout=args.timeout)
    fields = {}
    if args.phase:
        fields["phase"] = args.phase
    print(receipt_line(best, **fields))
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
