#!/usr/bin/env python3
"""Wall and per-child peak RSS via wait4.

Linux reports KiB and macOS bytes; this module stores bytes. A timed cell is
median wall across `runs` after `warmup`; RSS is that median run's.
"""

from __future__ import annotations

import os
import signal
import sys
import time
from dataclasses import dataclass

EXIT_EXEC_FAILED = 127
EXIT_TIMED_OUT = 124


class _WaitTimedOut(Exception):
    pass


class MeasurementFailure(RuntimeError):
    def __init__(self, status: str, message: str) -> None:
        super().__init__(message)
        self.status = status


def _raise_wait_timeout(_signum, _frame) -> None:
    raise _WaitTimedOut


@dataclass(frozen=True)
class MeasuredRun:
    wall_s: float
    user_s: float
    sys_s: float
    maxrss_bytes: int
    exit_code: int
    timed_out: bool = False


def run_measured(
    argv: list[str],
    timeout: float,
    env: dict[str, str] | None = None,
    stdin_path: str | None = None,
) -> MeasuredRun:
    """stdout/stderr at `/dev/null`. Fixture is an argv path, or `stdin_path` for stdin tools."""
    if timeout <= 0:
        raise ValueError("timeout must be positive")
    t0 = time.monotonic()
    child_env = os.environ if env is None else env
    devnull = os.open(os.devnull, os.O_RDWR)
    pid = os.fork()
    if pid == 0:
        try:
            if stdin_path:
                fd = os.open(stdin_path, os.O_RDONLY)
                os.dup2(fd, 0)
                os.close(fd)
            else:
                os.dup2(devnull, 0)
            os.dup2(devnull, 1)
            os.dup2(devnull, 2)
            os.close(devnull)
            os.execvpe(argv[0], argv, child_env)
        except BaseException:
            os._exit(EXIT_EXEC_FAILED)
    os.close(devnull)
    old_handler = signal.signal(signal.SIGALRM, _raise_wait_timeout)
    old_timer = signal.setitimer(signal.ITIMER_REAL, timeout)
    try:
        _, status, usage = os.wait4(pid, 0)
    except _WaitTimedOut:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os.wait4(pid, 0)
        return MeasuredRun(
            wall_s=time.monotonic() - t0,
            user_s=0.0,
            sys_s=0.0,
            maxrss_bytes=0,
            exit_code=EXIT_TIMED_OUT,
            timed_out=True,
        )
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, old_handler)
        if old_timer[0] > 0:
            signal.setitimer(signal.ITIMER_REAL, *old_timer)
    wall = time.monotonic() - t0
    if os.WIFSIGNALED(status):
        exit_code = -os.WTERMSIG(status)
    else:
        exit_code = os.WEXITSTATUS(status)
    maxrss = usage.ru_maxrss
    if sys.platform.startswith("linux"):
        maxrss *= 1024
    return MeasuredRun(
        wall_s=wall,
        user_s=usage.ru_utime,
        sys_s=usage.ru_stime,
        maxrss_bytes=maxrss,
        exit_code=exit_code,
    )


def median_of(
    argv: list[str],
    *,
    warmup: int,
    runs: int,
    timeout: float,
    env: dict[str, str] | None = None,
    stdin_path: str | None = None,
) -> MeasuredRun:
    """See module doc: median wall, RSS from that run."""
    if warmup < 0:
        raise ValueError("warmup must be nonnegative")
    if runs < 1:
        raise ValueError("runs must be positive")
    for _ in range(warmup):
        warm = run_measured(argv, timeout, env=env, stdin_path=stdin_path)
        if warm.timed_out or warm.exit_code != 0:
            status = "timeout" if warm.timed_out else "error"
            raise MeasurementFailure(
                status,
                f"warmup failed exit={warm.exit_code} timed_out={warm.timed_out} argv={argv!r}",
            )
    measured: list[MeasuredRun] = []
    for index in range(runs):
        run = run_measured(argv, timeout, env=env, stdin_path=stdin_path)
        if run.timed_out or run.exit_code != 0:
            status = "timeout" if run.timed_out else "error"
            raise MeasurementFailure(
                status,
                f"timed run {index + 1} failed exit={run.exit_code} timed_out={run.timed_out} argv={argv!r}"
            )
        measured.append(run)
    measured.sort(key=lambda r: r.wall_s)
    return measured[len(measured) // 2]


def self_check() -> None:
    true = "/usr/bin/true" if os.path.exists("/usr/bin/true") else "true"
    run = run_measured([true], timeout=5)
    assert run.exit_code == 0 and not run.timed_out, run
    assert run.wall_s >= 0, run
    assert run.maxrss_bytes >= 0, run


if __name__ == "__main__":
    self_check()
    print("measure: ok")
