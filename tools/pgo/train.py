#!/usr/bin/env python3
"""PGO trainer with a slice of cases.json

A profile needs routes, not sizes. `expand(spec, [25000])` is the only size;
`pgo_width` (broad) is the only width — narrow is a subset of the same
grammar. A query runs only with `"pgo": true`. Same-route extras stay off
(project-names, ndjson score) so they cannot drown YAML and record lanes.
Repeats are the shell's WORKLOAD_REPEATS; this file just runs the set.

Stdout is `lanes train_hash code_hash`. `--hash` prints `train_hash code_hash`
and does not run jqf. train_hash is the pgo case set (plus fixture generator
bytes); code_hash is the product crates that `-p jqf` builds.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BENCH = ROOT / "benchmark"
if str(BENCH) not in sys.path:
    sys.path.insert(0, str(BENCH))

from catalog import expand
from fixtures import generate

CODE_ROOTS = (
    "Cargo.toml",
    "Cargo.lock",
    "jqf-cli",
    "jqf-sdk",
    "jqf-runtime",
    "jqf-engine",
    "jqf-builtins",
    "jqf-data",
    "jqf-syntax",
    "jqf-source",
    "jqf-resource",
    "jqf-codec",
)
SKIP_DIR = {"target", "tests", "benches"}


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def pgo_cases(spec: dict) -> tuple[dict[str, dict], list[dict]]:
    datasets, cases = expand(spec, [25000])
    marked = {
        (kind, query["id"])
        for kind, kspec in spec["kinds"].items()
        for query in kspec["queries"]
        if query.get("pgo")
    }
    width = spec["pgo_width"]
    cases = [case for case in cases if case["width"] == width and (case["kind"], case["query_id"]) in marked]
    return datasets, cases


def _sha_bytes(parts: list[bytes]) -> str:
    h = hashlib.sha256()
    for part in parts:
        h.update(part)
    return h.hexdigest()[:8]


def train_hash(spec: dict, cases: list[dict], repeats: int) -> str:
    payload = {
        "repeats": repeats,
        "pgo_width": spec["pgo_width"],
        "trainer": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "catalog": hashlib.sha256((BENCH / "catalog.py").read_bytes()).hexdigest(),
        "fixtures": hashlib.sha256((BENCH / "fixtures.py").read_bytes()).hexdigest(),
        "cases": [
            {
                "id": case["id"],
                "kind": case["kind"],
                "rows": case["rows"],
                "query": case["expressions"]["jqf"],
                "extra": list(case.get("extra", {}).get("jqf", [])),
            }
            for case in cases
        ],
    }
    blob = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
    return _sha_bytes([blob])


def code_hash(root: Path) -> str:
    files: list[Path] = []
    for name in CODE_ROOTS:
        path = root / name
        if path.is_file():
            files.append(path)
            continue
        if not path.is_dir():
            continue
        for child in path.rglob("*"):
            if not child.is_file():
                continue
            if any(part in SKIP_DIR for part in child.relative_to(path).parts):
                continue
            if child.suffix == ".rs" or child.name in {"Cargo.toml", "Cargo.lock", "build.rs"}:
                files.append(child)
    h = hashlib.sha256()
    for path in sorted(files, key=lambda p: p.as_posix()):
        h.update(path.relative_to(root).as_posix().encode())
        h.update(b"\0")
        h.update(path.read_bytes())
        h.update(b"\0")
    return h.hexdigest()[:8]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--jqf", default=None)
    parser.add_argument("--fixtures", default=None, type=Path)
    parser.add_argument("--repeats", type=positive_int, default=2)
    parser.add_argument("--timeout", type=positive_float, default=30.0)
    parser.add_argument("--hash", action="store_true")
    args = parser.parse_args()
    spec = json.loads((BENCH / "cases.json").read_text())
    datasets, cases = pgo_cases(spec)
    t_hash = train_hash(spec, cases, args.repeats)
    c_hash = code_hash(ROOT)
    if args.hash:
        print(f"{t_hash} {c_hash}")
        return 0
    if not args.jqf or not args.fixtures:
        raise SystemExit("train: --jqf and --fixtures are required unless --hash")
    os.environ.setdefault("JQF_NO_CONFIG", "1")
    args.fixtures.mkdir(parents=True, exist_ok=True)
    paths = {}
    for name in {case["dataset"] for case in cases}:
        ds = datasets[name]
        dest = args.fixtures / f"{name}{ds['suffix']}"
        generate(ds["kind"], ds["width"], ds["rows"], dest)
        paths[name] = dest
    env = dict(os.environ)
    env["JQF_NO_CONFIG"] = "1"
    lanes = 0
    for _ in range(args.repeats):
        for case in cases:
            query = case["expressions"].get("jqf")
            if not query:
                continue
            extra = list(case.get("extra", {}).get("jqf", []))
            argv = [args.jqf, "-c", *extra, query, str(paths[case["dataset"]])]
            try:
                proc = subprocess.run(
                    argv,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    env=env,
                    timeout=args.timeout,
                )
            except subprocess.TimeoutExpired:
                print(f"pgo-train: timed out {case['id']} after {args.timeout:g}s", file=sys.stderr)
                return 2
            if proc.returncode != 0:
                print(
                    f"pgo-train: failed {case['id']}: {proc.stderr.decode('utf-8', 'replace')[:300]}",
                    file=sys.stderr,
                )
                return 2
            lanes += 1
    print(f"pgo-train: cases={len(cases)} lanes={lanes} train={t_hash} code={c_hash}", file=sys.stderr)
    print(f"{lanes} {t_hash} {c_hash}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
