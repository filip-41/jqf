"""Host facts for the receipt header. Sibling of report.py."""

from __future__ import annotations

import os
import platform
import subprocess
import sys
from pathlib import Path


def _run(argv: list[str], timeout: float = 5) -> str:
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired):
        return ""
    return (proc.stdout or "").strip()


def _sysctl(key: str) -> str:
    return _run(["sysctl", "-n", key])


def _linux_cpu() -> str:
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name") or line.startswith("Hardware"):
                return line.split(":", 1)[1].strip()
    except OSError:
        return ""
    return platform.processor()


def _linux_mem() -> str:
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                kib = int(line.split()[1])
                return f"{kib / (1024 * 1024):.1f} GiB"
    except (OSError, ValueError):
        return ""
    return ""


def facts() -> dict[str, str]:
    out = {
        "os": platform.platform(),
        "arch": platform.machine(),
        "python": platform.python_version(),
        "cpus": str(os.cpu_count() or ""),
    }
    if sys.platform == "darwin":
        out["cpu"] = _sysctl("machdep.cpu.brand_string")
        mem = _sysctl("hw.memsize")
        if mem.isdigit():
            out["memory"] = f"{int(mem) / (1024 ** 3):.1f} GiB"
        physical = _sysctl("hw.physicalcpu")
        if physical:
            out["physical_cpus"] = physical
    elif sys.platform.startswith("linux"):
        out["cpu"] = _linux_cpu()
        out["memory"] = _linux_mem()
    return {k: v for k, v in out.items() if v}


def git_commit(root: Path) -> str:
    digest = _run(["git", "-C", str(root), "rev-parse", "HEAD"])
    if not digest:
        return ""
    dirty = bool(_run(["git", "-C", str(root), "status", "--porcelain"]))
    return f"{digest} dirty" if dirty else digest
