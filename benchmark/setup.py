#!/usr/bin/env python3
"""Pinned competitor binaries into `.deps/bin`.

Invariant: `--version` matches `TOOLS[name]['version_prefix']` or the file
is re-fetched. PATH is never consulted. Sibling of run.py. Release URLs
are direct; the GitHub API is not used.
"""

from __future__ import annotations

import os
import platform
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEPS = HERE / ".deps"
BIN = DEPS / "bin"
CACHE = DEPS / "cache"

TOOLS: dict[str, dict] = {
    "jq": {
        "version": "1.8.2",
        "version_prefix": "jq-1.8.2",
        "assets": {
            ("darwin", "arm64"): (
                "https://github.com/jqlang/jq/releases/download/jq-1.8.2/jq-macos-arm64",
                "jq",
            ),
            ("darwin", "x86_64"): (
                "https://github.com/jqlang/jq/releases/download/jq-1.8.2/jq-macos-amd64",
                "jq",
            ),
            ("linux", "x86_64"): (
                "https://github.com/jqlang/jq/releases/download/jq-1.8.2/jq-linux-amd64",
                "jq",
            ),
            ("linux", "arm64"): (
                "https://github.com/jqlang/jq/releases/download/jq-1.8.2/jq-linux-arm64",
                "jq",
            ),
        },
    },
    "jaq": {
        "version": "3.1.1",
        "version_prefix": "jaq 3.1.1",
        "assets": {
            ("darwin", "arm64"): (
                "https://github.com/01mf02/jaq/releases/download/v3.1.1/jaq-aarch64-apple-darwin",
                "jaq",
            ),
            ("darwin", "x86_64"): (
                "https://github.com/01mf02/jaq/releases/download/v3.1.1/jaq-x86_64-apple-darwin",
                "jaq",
            ),
            ("linux", "x86_64"): (
                "https://github.com/01mf02/jaq/releases/download/v3.1.1/jaq-x86_64-unknown-linux-gnu",
                "jaq",
            ),
            ("linux", "arm64"): (
                "https://github.com/01mf02/jaq/releases/download/v3.1.1/jaq-aarch64-unknown-linux-gnu",
                "jaq",
            ),
        },
    },
    "gojq": {
        "version": "0.12.19",
        "version_prefix": "gojq 0.12.19",
        "assets": {
            ("darwin", "arm64"): (
                "https://github.com/itchyny/gojq/releases/download/v0.12.19/gojq_v0.12.19_darwin_arm64.zip",
                "gojq",
            ),
            ("darwin", "x86_64"): (
                "https://github.com/itchyny/gojq/releases/download/v0.12.19/gojq_v0.12.19_darwin_amd64.zip",
                "gojq",
            ),
            ("linux", "x86_64"): (
                "https://github.com/itchyny/gojq/releases/download/v0.12.19/gojq_v0.12.19_linux_amd64.tar.gz",
                "gojq",
            ),
            ("linux", "arm64"): (
                "https://github.com/itchyny/gojq/releases/download/v0.12.19/gojq_v0.12.19_linux_arm64.tar.gz",
                "gojq",
            ),
        },
    },
    "yq": {
        "version": "4.53.6",
        "version_prefix": "yq (https://github.com/mikefarah/yq/) version v4.53.6",
        "assets": {
            ("darwin", "arm64"): (
                "https://github.com/mikefarah/yq/releases/download/v4.53.6/yq_darwin_arm64",
                "yq",
            ),
            ("darwin", "x86_64"): (
                "https://github.com/mikefarah/yq/releases/download/v4.53.6/yq_darwin_amd64",
                "yq",
            ),
            ("linux", "x86_64"): (
                "https://github.com/mikefarah/yq/releases/download/v4.53.6/yq_linux_amd64",
                "yq",
            ),
            ("linux", "arm64"): (
                "https://github.com/mikefarah/yq/releases/download/v4.53.6/yq_linux_arm64",
                "yq",
            ),
        },
    },
    "dasel": {
        "version": "3.11.2",
        "version_prefix": "3.11.2",
        "version_args": ["version"],
        "assets": {
            ("darwin", "arm64"): (
                "https://github.com/TomWright/dasel/releases/download/v3.11.2/dasel_darwin_arm64",
                "dasel",
            ),
            ("darwin", "x86_64"): (
                "https://github.com/TomWright/dasel/releases/download/v3.11.2/dasel_darwin_amd64",
                "dasel",
            ),
            ("linux", "x86_64"): (
                "https://github.com/TomWright/dasel/releases/download/v3.11.2/dasel_linux_amd64",
                "dasel",
            ),
            ("linux", "arm64"): (
                "https://github.com/TomWright/dasel/releases/download/v3.11.2/dasel_linux_arm64",
                "dasel",
            ),
        },
    },
    "mlr": {
        "version": "6.21.0",
        "version_prefix": "mlr 6.21.0",
        "assets": {
            ("darwin", "arm64"): (
                "https://github.com/johnkerl/miller/releases/download/v6.21.0/miller-6.21.0-darwin-arm64.tar.gz",
                "mlr",
            ),
            ("darwin", "x86_64"): (
                "https://github.com/johnkerl/miller/releases/download/v6.21.0/miller-6.21.0-darwin-amd64.tar.gz",
                "mlr",
            ),
            ("linux", "x86_64"): (
                "https://github.com/johnkerl/miller/releases/download/v6.21.0/miller-6.21.0-linux-amd64.tar.gz",
                "mlr",
            ),
            ("linux", "arm64"): (
                "https://github.com/johnkerl/miller/releases/download/v6.21.0/miller-6.21.0-linux-arm64.tar.gz",
                "mlr",
            ),
        },
    },
}


def host() -> tuple[str, str]:
    sysname = platform.system().lower()
    machine = platform.machine().lower()
    if machine in ("aarch64", "arm64"):
        machine = "arm64"
    elif machine in ("amd64", "x86_64"):
        machine = "x86_64"
    return sysname, machine


def _download(url: str, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    req = urllib.request.Request(url, headers={"User-Agent": "jqf-benchmark"})
    tmp_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(dir=dest.parent, prefix=f".{dest.name}.", delete=False) as fh:
            tmp_path = Path(fh.name)
            with urllib.request.urlopen(req, timeout=60) as resp:
                while chunk := resp.read(1024 * 256):
                    fh.write(chunk)
        tmp_path.replace(dest)
    finally:
        if tmp_path is not None:
            tmp_path.unlink(missing_ok=True)


def _chmod_exec(path: Path) -> None:
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def _extract_named(archive: Path, name: str, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    if archive.suffix == ".zip" or archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as zf:
            members = [m for m in zf.namelist() if Path(m).name == name and not m.endswith("/")]
            if not members:
                raise RuntimeError(f"{archive.name}: no member named {name}")
            with zf.open(members[0]) as src, dest.open("wb") as out:
                out.write(src.read())
        return
    if archive.name.endswith(".tar.gz") or archive.suffixes[-2:] == [".tar", ".gz"]:
        with tarfile.open(archive) as tf:
            members = [m for m in tf.getmembers() if Path(m.name).name == name and m.isfile()]
            if not members:
                raise RuntimeError(f"{archive.name}: no member named {name}")
            extracted = tf.extractfile(members[0])
            if extracted is None:
                raise RuntimeError(f"{archive.name}: could not extract {name}")
            with extracted as src, dest.open("wb") as out:
                out.write(src.read())
        return
    raise RuntimeError(f"unknown archive shape {archive.name}")


def tool_bin(name: str) -> Path:
    return BIN / name


def ensure_tools() -> dict[str, Path]:
    sysname, machine = host()
    BIN.mkdir(parents=True, exist_ok=True)
    CACHE.mkdir(parents=True, exist_ok=True)
    resolved: dict[str, Path] = {}
    for name, spec in TOOLS.items():
        dest = tool_bin(name)
        version_args = spec.get("version_args", ["--version"])
        if dest.is_file() and _version_ok(dest, spec["version_prefix"], version_args):
            resolved[name] = dest
            continue
        key = (sysname, machine)
        if key not in spec["assets"]:
            raise SystemExit(f"setup: no {name} {spec['version']} asset for {sysname}/{machine}")
        url, inner = spec["assets"][key]
        print(f"setup: fetching {name} {spec['version']}", file=sys.stderr)
        cached = CACHE / spec["version"] / Path(url).name
        if not cached.is_file():
            _download(url, cached)
        if cached.name.endswith(".zip") or cached.name.endswith(".tar.gz"):
            _extract_named(cached, inner, dest)
        else:
            dest.write_bytes(cached.read_bytes())
        _chmod_exec(dest)
        if not _version_ok(dest, spec["version_prefix"], version_args):
            got = _version_line(dest, version_args)
            raise SystemExit(f"setup: {name} at {dest} version {got!r} != {spec['version_prefix']!r}")
        resolved[name] = dest
    return resolved


def _version_line(path: Path, args: list[str] | None = None) -> str:
    env = dict(os.environ)
    env["PATH"] = str(path.parent) + os.pathsep + env.get("PATH", "")
    proc = subprocess.run(
        [str(path), *(args or ["--version"])], capture_output=True, text=True, env=env
    )
    text = (proc.stdout or proc.stderr or "").strip().splitlines()
    return text[0] if text else f"exit {proc.returncode}"


def _version_ok(path: Path, prefix: str, args: list[str] | None = None) -> bool:
    try:
        line = _version_line(path, args)
        return line == prefix or line.startswith(prefix + " ")
    except OSError:
        return False


def main() -> None:
    tools = ensure_tools()
    for name, path in tools.items():
        args = TOOLS[name].get("version_args", ["--version"])
        print(f"{name}: {path} ({_version_line(path, args)})")


if __name__ == "__main__":
    main()
