#!/usr/bin/env python3
"""CLI panel: jqf against the pinned binaries in `.deps/`.

No cell is timed until stdout matches the oracle as JSON values (or SHA-256
when stdout exceeds 1 MiB). Oracle is the jq-family expression when the
case has one, otherwise jqf. Blank cells: n/a, disagreed, timeout, error.
Every case is written to `.cache/cells/<stamp>/<id>.json` as it finishes;
a later run skips those unless `--force`. Also writes `results.md` and
`results.tsv`. Compact flags so pretty-print is not the thing timed.
"""

from __future__ import annotations

import argparse
import copy
import fnmatch
import hashlib
import json
import math
import os
import shlex
import statistics
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from decimal import Decimal
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from catalog import expand, exclusions_for
from fixtures import generate
from host import facts as host_facts
from host import git_commit
from measure import MeasurementFailure, median_of, self_check
from report import PANEL, cell_pair, ratios_vs_jqf, write_md, write_tsv
from setup import TOOLS, ensure_tools

ROOT = HERE.parent
CACHE = HERE / ".cache"
CASES_PATH = HERE / "cases.json"
MAX_INLINE_OUTPUT = 1 << 20


@dataclass(frozen=True)
class CapturedOutput:
    size: int
    sha256: str
    snippet: str
    data: bytes | None


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def nonnegative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be nonnegative")
    return parsed


def hermetic_env() -> dict[str, str]:
    env = dict(os.environ)
    env["JQF_NO_CONFIG"] = "1"
    env["NO_COLOR"] = "1"
    return env


def load_spec() -> dict:
    return json.loads(CASES_PATH.read_text())


def jqf_bin(explicit: str | None) -> Path:
    if explicit:
        path = Path(explicit)
    elif os.environ.get("JQF_BIN"):
        path = Path(os.environ["JQF_BIN"])
    else:
        path = ROOT / "target" / "pgo" / "jqf"
    if not path.is_file() or not os.access(path, os.X_OK):
        raise SystemExit(f"run: no PGO jqf at {path}; make pgo")
    try:
        proc = subprocess.run(
            [str(path), "--diagnostics", "-n", "null"],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=5,
            env=hermetic_env(),
        )
    except (OSError, subprocess.TimeoutExpired) as err:
        raise SystemExit(f"run: cannot inspect jqf at {path}: {err}") from err
    diagnostics = proc.stderr or proc.stdout or ""
    if not any(line.startswith("jqf: build=pgo ") for line in diagnostics.splitlines()):
        raise SystemExit(f"run: {path} is not a PGO build; run make pgo")
    fresh_env = hermetic_env()
    fresh_env["JQF_PGO_BIN"] = str(path)
    try:
        fresh = subprocess.run(
            [str(ROOT / "tools" / "pgo" / "jqf-pgo-freshness.sh")],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=10,
            env=fresh_env,
        )
    except (OSError, subprocess.TimeoutExpired) as err:
        raise SystemExit(f"run: cannot check whether {path} is fresh: {err}") from err
    if fresh.returncode != 0:
        reason = (fresh.stderr or fresh.stdout or "").strip()
        raise SystemExit(f"run: {path} is not fresh: {reason}")
    return path


def argv_for(tool: str, bin_path: Path, query: str, fixture: Path, spec: dict, case: dict) -> list[str]:
    tool_spec = spec["tools"][tool]
    if tool == "yq" and case.get("yq_compact"):
        compact = list(case["yq_compact"])
    else:
        compact = list(tool_spec["compact"])
    extra = list(case.get("extra", {}).get(tool, []))
    program = shlex.split(query) if tool_spec.get("words") else [query]
    argv = [str(bin_path), *compact, *extra, *program]
    if not tool_spec.get("stdin"):
        argv.append(str(fixture))
    return argv


def stdin_for(spec: dict, tool: str, fixture: Path) -> str | None:
    if spec["tools"].get(tool, {}).get("stdin"):
        return str(fixture)
    return None


def json_values(raw: bytes) -> list[object]:
    text = raw.decode("utf-8")
    stripped = text.strip()
    if not stripped:
        return []
    try:
        return [json.loads(stripped, parse_int=Decimal, parse_float=Decimal)]
    except json.JSONDecodeError:
        return [
            json.loads(line, parse_int=Decimal, parse_float=Decimal)
            for line in stripped.splitlines()
            if line
        ]


def json_equal(left: object, right: object) -> bool:
    if isinstance(left, bool) or isinstance(right, bool):
        return type(left) is type(right) and left == right
    if isinstance(left, Decimal) and isinstance(right, Decimal):
        return left == right
    if type(left) is not type(right):
        return False
    if isinstance(left, list):
        return len(left) == len(right) and all(json_equal(a, b) for a, b in zip(left, right))
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(json_equal(left[key], right[key]) for key in left)
    return left == right


def captured_bytes(raw: bytes) -> CapturedOutput:
    text = raw[:200].decode("utf-8", errors="replace")
    return CapturedOutput(
        size=len(raw),
        sha256=hashlib.sha256(raw).hexdigest(),
        snippet=text + ("…" if len(raw) > 200 else ""),
        data=raw if len(raw) <= MAX_INLINE_OUTPUT else None,
    )


def same_output(oracle: bytes | CapturedOutput, got: bytes | CapturedOutput) -> bool:
    left = captured_bytes(oracle) if isinstance(oracle, bytes) else oracle
    right = captured_bytes(got) if isinstance(got, bytes) else got
    if left.size == right.size and left.sha256 == right.sha256:
        return True
    if left.data is None or right.data is None:
        return False
    try:
        return json_equal(json_values(left.data), json_values(right.data))
    except (json.JSONDecodeError, UnicodeDecodeError):
        return False


def validate_capture(
    oracle_status: str,
    oracle_raw: bytes | CapturedOutput,
    status: str,
    raw: bytes | CapturedOutput,
    oracle_tool: str,
) -> dict | None:
    if oracle_status != "ok":
        return {"status": f"oracle-{oracle_status}", "oracle_tool": oracle_tool}
    if status != "ok":
        return {"status": status}
    if not same_output(oracle_raw, raw):
        return {
            "status": "disagreed",
            "oracle_tool": oracle_tool,
            "expected": output_preview(oracle_raw),
            "got": output_preview(raw),
        }
    return None


def output_preview(raw: bytes | CapturedOutput) -> dict:
    output = captured_bytes(raw) if isinstance(raw, bytes) else raw
    return {
        "bytes": output.size,
        "sha256": output.sha256,
        "snippet": output.snippet,
    }


def captured_file(fh) -> CapturedOutput:
    size = fh.tell()
    fh.seek(0)
    if size <= MAX_INLINE_OUTPUT:
        return captured_bytes(fh.read())
    snippet = fh.read(200).decode("utf-8", errors="replace") + "…"
    fh.seek(0)
    digest = hashlib.sha256()
    while chunk := fh.read(1024 * 1024):
        digest.update(chunk)
    return CapturedOutput(size=size, sha256=digest.hexdigest(), snippet=snippet, data=None)


def capture(
    argv: list[str],
    env: dict[str, str],
    timeout: float,
    stdin_path: str | None = None,
) -> tuple[str, CapturedOutput]:
    with tempfile.SpooledTemporaryFile(max_size=MAX_INLINE_OUTPUT) as stdout:
        try:
            if stdin_path:
                with open(stdin_path, "rb") as fh:
                    proc = subprocess.run(
                        argv,
                        stdout=stdout,
                        stderr=subprocess.PIPE,
                        env=env,
                        timeout=timeout,
                        stdin=fh,
                    )
            else:
                proc = subprocess.run(
                    argv,
                    stdout=stdout,
                    stderr=subprocess.PIPE,
                    env=env,
                    timeout=timeout,
                )
        except subprocess.TimeoutExpired:
            return "timeout", captured_bytes(b"")
        output = captured_file(stdout)
    if proc.returncode != 0:
        return "error", output
    return "ok", output


def workload_stamp(
    *,
    quick: bool,
    warmup: int,
    runs: int,
    tools: dict[str, str],
    host: dict[str, str] | None = None,
) -> str:
    semantic = hashlib.sha256()
    for path in [CASES_PATH, HERE / "catalog.py", HERE / "fixtures.py", HERE / "measure.py", Path(__file__)]:
        semantic.update(path.name.encode())
        semantic.update(b"\0")
        semantic.update(path.read_bytes())
        semantic.update(b"\0")
    raw = json.dumps(
        {
            "quick": quick,
            "warmup": warmup,
            "runs": runs,
            "tools": tools,
            "host": host if host is not None else host_facts(),
            "semantics": semantic.hexdigest(),
        },
        sort_keys=True,
    ).encode()
    return hashlib.sha256(raw).hexdigest()[:16]


def jqf_receipt_is_current(row: dict, diagnostics: str) -> bool:
    return row.get("jqf_diagnostics") == diagnostics


def mask_stale_jqf(row: dict, diagnostics: str) -> dict:
    if jqf_receipt_is_current(row, diagnostics):
        return row
    row = copy.deepcopy(row)
    for tool in ("jqf", "jqf-serial"):
        if tool in row.get("cells", {}):
            row["cells"][tool] = {"status": "stale"}
    return row


def write_case_receipt(path: Path, row: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(row) + "\n")
    tmp.replace(path)


def load_all_receipts(stamp: str) -> list[dict]:
    cells = CACHE / "cells" / stamp
    if not cells.is_dir():
        return []
    rows = [json.loads(path.read_text()) for path in sorted(cells.glob("*.json"))]
    rows.sort(key=result_sort_key)
    return rows


def result_sort_key(row: dict) -> tuple:
    return (
        row.get("kind", ""),
        row.get("width", ""),
        row.get("rows", 0),
        row.get("query_id", ""),
        row["id"],
    )


def mask_excluded(row: dict, excluded: dict[str, str], with_excluded: bool) -> dict:
    if with_excluded or not excluded:
        return row
    row = copy.deepcopy(row)
    for tool in excluded:
        cell = row.get("cells", {}).get(tool)
        if cell and cell.get("status") not in (None, "n/a"):
            row["cells"][tool] = {"status": "excluded"}
    return row


def filter_cases(cases: list[dict], globs: list[str]) -> list[dict]:
    if not globs:
        return cases
    matched = [case for case in cases if any(fnmatch.fnmatch(case["id"], glob) for glob in globs)]
    if not matched:
        raise SystemExit(f"run: no cases match {globs}")
    return matched


def prepare_fixtures(datasets: dict[str, dict]) -> dict[str, Path]:
    CACHE.mkdir(parents=True, exist_ok=True)
    generator = hashlib.sha256((HERE / "fixtures.py").read_bytes()).hexdigest()
    out: dict[str, Path] = {}
    for name, spec in datasets.items():
        dest = CACHE / f"{name}{spec['suffix']}"
        stamp = dest.with_name(dest.name + ".stamp")
        token = f"{spec['kind']}:{spec['width']}:{spec['rows']}:{generator}\n"
        if dest.is_file() and stamp.is_file() and stamp.read_text() == token:
            out[name] = dest
            continue
        print(f"fixtures: {name}", file=sys.stderr)
        generate(spec["kind"], spec["width"], spec["rows"], dest)
        stamp.write_text(token)
        out[name] = dest
    return out


def run_panel(args: argparse.Namespace) -> int:
    spec = load_spec()
    sizes = spec["quick_sizes"] if args.quick else spec["sizes"]
    datasets, cases = expand(spec, sizes)
    catalog_ids = {case["id"] for case in cases}
    cases = filter_cases(cases, args.case)
    needed = {case["dataset"] for case in cases}
    datasets = {name: spec_ for name, spec_ in datasets.items() if name in needed}
    env = hermetic_env()
    tools = ensure_tools()
    jqf = jqf_bin(args.jqf)
    bins = {"jqf": jqf, "jqf-serial": jqf, **tools}
    fixtures = prepare_fixtures(datasets)
    timeout = 30.0 if args.quick else float(spec["timeout_s"])
    warmup = args.warmup if args.warmup is not None else spec["warmup"]
    runs = args.runs if args.runs is not None else spec["runs"]

    diag = subprocess.run([str(jqf), "--diagnostics"], capture_output=True, text=True, env=env)
    diag_text = (diag.stderr or diag.stdout or "").strip()
    diagnostics = ""
    for line in diag_text.splitlines():
        if line.startswith("jqf: build="):
            diagnostics = line
            break
    if not diagnostics and diag_text:
        diagnostics = diag_text.splitlines()[0]
    print(f"jqf: {jqf}")
    if diagnostics:
        print(diagnostics)
    for name in PANEL[1:]:
        if name not in TOOLS:
            print(f"{name}: {bins[name]} --no-parallel")
            continue
        print(f"{name}: {TOOLS[name]['version']} ({bins[name]})")
    print(f"warmup {warmup}, runs {runs}, cases {len(cases)}")
    if args.quick:
        print("mode: --quick")
    if args.case:
        print(f"filter: {args.case}")
    tools_versions = {name: TOOLS[name]["version"] for name in PANEL[1:] if name in TOOLS}
    host = host_facts()
    stamp = workload_stamp(
        quick=args.quick,
        warmup=warmup,
        runs=runs,
        tools=tools_versions,
        host=host,
    )
    if spec.get("exclude") and not args.with_excluded:
        for rule in spec["exclude"]:
            print(f"excluded: {rule['tool']} {rule['id']} ({rule['why']})")
    print(f"stamp {stamp}")
    if args.force:
        print("mode: --force")
    if args.with_excluded:
        print("mode: --with-excluded")
    if args.jqf_only:
        print("mode: --jqf-only")
    print()

    stdout_header = ["case", *PANEL]
    rows_out: list[list[str]] = []
    results: list[dict] = []
    out_dir = Path(args.out) if args.out else CACHE
    stem = "results-quick" if args.quick else "results"
    md_path = out_dir / f"{stem}.md"
    tsv_path = out_dir / f"{stem}.tsv"
    skipped = 0
    started = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    commit = git_commit(ROOT)
    receipt_rows = {row["id"]: row for row in load_all_receipts(stamp)}

    for i, case in enumerate(cases, 1):
        write_dest = CACHE / "cells" / stamp / f"{case['id']}.json"
        skip = exclusions_for(spec, case)
        cached = None if args.force else receipt_rows.get(case["id"])
        if (
            cached is not None
            and args.with_excluded
            and any(
                tool in case["expressions"]
                and cached.get("cells", {}).get(tool, {}).get("status") in (None, "excluded")
                for tool in skip
            )
        ):
            cached = None
        if args.jqf_only and cached is None:
            raise SystemExit(f"run: no compatible cached receipt for {case['id']}; run the full panel first")
        jqf_stale = cached is not None and not jqf_receipt_is_current(cached, diagnostics)
        if cached is not None and (args.jqf_only or jqf_stale):
            row = copy.deepcopy(cached)
            fixture = fixtures[case["dataset"]]
            oracle_tool = "jq" if "jq" in case["expressions"] else "jqf"
            oracle_argv = argv_for(
                oracle_tool,
                bins[oracle_tool],
                case["expressions"][oracle_tool],
                fixture,
                spec,
                case,
            )
            oracle_status, oracle_raw = capture(
                oracle_argv, env, timeout, stdin_path=stdin_for(spec, oracle_tool, fixture)
            )
            for jqf_tool in ("jqf", "jqf-serial"):
                query = case["expressions"].get(jqf_tool)
                if not query:
                    continue
                argv = argv_for(jqf_tool, bins[jqf_tool], query, fixture, spec, case)
                status, raw = capture(argv, env, timeout, stdin_path=stdin_for(spec, jqf_tool, fixture))
                invalid = validate_capture(oracle_status, oracle_raw, status, raw, oracle_tool)
                if invalid is not None:
                    row["cells"][jqf_tool] = invalid
                else:
                    try:
                        measured = median_of(
                            argv,
                            warmup=warmup,
                            runs=runs,
                            timeout=timeout,
                            env=env,
                            stdin_path=stdin_for(spec, jqf_tool, fixture),
                        )
                    except MeasurementFailure as err:
                        row["cells"][jqf_tool] = {"status": err.status}
                    else:
                        row["cells"][jqf_tool] = {
                            "status": "ok",
                            "wall_s": measured.wall_s,
                            "maxrss_bytes": measured.maxrss_bytes,
                        }
            if jqf_stale and oracle_tool == "jqf":
                for tool in PANEL[2:]:
                    query = case["expressions"].get(tool)
                    existing = row["cells"].get(tool, {})
                    if not query or existing.get("status") in ("n/a", "excluded"):
                        continue
                    argv = argv_for(tool, bins[tool], query, fixture, spec, case)
                    status, raw = capture(
                        argv, env, timeout, stdin_path=stdin_for(spec, tool, fixture)
                    )
                    invalid = validate_capture(oracle_status, oracle_raw, status, raw, oracle_tool)
                    if invalid is not None:
                        row["cells"][tool] = invalid
                        continue
                    if existing.get("status") == "ok":
                        continue
                    try:
                        measured = median_of(
                            argv,
                            warmup=warmup,
                            runs=runs,
                            timeout=timeout,
                            env=env,
                            stdin_path=stdin_for(spec, tool, fixture),
                        )
                    except MeasurementFailure as err:
                        row["cells"][tool] = {"status": err.status}
                    else:
                        row["cells"][tool] = {
                            "status": "ok",
                            "wall_s": measured.wall_s,
                            "maxrss_bytes": measured.maxrss_bytes,
                        }
            row["jqf_diagnostics"] = diagnostics
            write_case_receipt(write_dest, row)
        elif cached is not None:
            row = cached
            disagreed = [tool for tool, cell in row["cells"].items() if cell.get("status") == "disagreed"]
            if disagreed:
                fixture = fixtures[case["dataset"]]
                oracle_tool = "jq" if "jq" in case["expressions"] else "jqf"
                oracle_argv = argv_for(
                    oracle_tool,
                    bins[oracle_tool],
                    case["expressions"][oracle_tool],
                    fixture,
                    spec,
                    case,
                )
                oracle_status, oracle_raw = capture(
                    oracle_argv, env, timeout, stdin_path=stdin_for(spec, oracle_tool, fixture)
                )
                for tool in disagreed:
                    query = case["expressions"].get(tool)
                    if not query:
                        continue
                    argv = argv_for(tool, bins[tool], query, fixture, spec, case)
                    status, raw = capture(
                        argv, env, timeout, stdin_path=stdin_for(spec, tool, fixture)
                    )
                    invalid = validate_capture(oracle_status, oracle_raw, status, raw, oracle_tool)
                    if invalid is None:
                        try:
                            measured = median_of(
                                argv,
                                warmup=warmup,
                                runs=runs,
                                timeout=timeout,
                                env=env,
                                stdin_path=stdin_for(spec, tool, fixture),
                            )
                        except MeasurementFailure as err:
                            row["cells"][tool] = {"status": err.status}
                            continue
                        row["cells"][tool] = {
                            "status": "ok",
                            "wall_s": measured.wall_s,
                            "maxrss_bytes": measured.maxrss_bytes,
                        }
                    else:
                        row["cells"][tool] = invalid
                write_case_receipt(write_dest, row)
            missing = [
                tool
                for tool in PANEL
                if case["expressions"].get(tool)
                and row.get("cells", {}).get(tool, {}).get("status") in (None, "n/a")
            ]
            if missing:
                fixture = fixtures[case["dataset"]]
                oracle_tool = "jq" if "jq" in case["expressions"] else "jqf"
                oracle_argv = argv_for(
                    oracle_tool,
                    bins[oracle_tool],
                    case["expressions"][oracle_tool],
                    fixture,
                    spec,
                    case,
                )
                oracle_status, oracle_raw = capture(
                    oracle_argv, env, timeout, stdin_path=stdin_for(spec, oracle_tool, fixture)
                )
                for tool in missing:
                    argv = argv_for(
                        tool, bins[tool], case["expressions"][tool], fixture, spec, case
                    )
                    status, raw = capture(
                        argv, env, timeout, stdin_path=stdin_for(spec, tool, fixture)
                    )
                    invalid = validate_capture(oracle_status, oracle_raw, status, raw, oracle_tool)
                    if invalid is not None:
                        row["cells"][tool] = invalid
                        continue
                    try:
                        measured = median_of(
                            argv,
                            warmup=warmup,
                            runs=runs,
                            timeout=timeout,
                            env=env,
                            stdin_path=stdin_for(spec, tool, fixture),
                        )
                    except MeasurementFailure as err:
                        row["cells"][tool] = {"status": err.status}
                    else:
                        row["cells"][tool] = {
                            "status": "ok",
                            "wall_s": measured.wall_s,
                            "maxrss_bytes": measured.maxrss_bytes,
                        }
                write_case_receipt(write_dest, row)
            skipped += 1
        else:
            fixture = fixtures[case["dataset"]]
            oracle_tool = "jq" if "jq" in case["expressions"] else "jqf"
            oracle_argv = argv_for(
                oracle_tool, bins[oracle_tool], case["expressions"][oracle_tool], fixture, spec, case
            )
            oracle_status, oracle_raw = capture(
                oracle_argv, env, timeout, stdin_path=stdin_for(spec, oracle_tool, fixture)
            )
            cells: dict[str, dict] = {}
            for tool in PANEL:
                query = case["expressions"].get(tool)
                if not query:
                    cells[tool] = {"status": "n/a"}
                    continue
                if tool in skip and not args.with_excluded:
                    cells[tool] = {"status": "excluded"}
                    continue
                argv = argv_for(tool, bins[tool], query, fixture, spec, case)
                status, raw = capture(
                    argv, env, timeout, stdin_path=stdin_for(spec, tool, fixture)
                )
                invalid = validate_capture(oracle_status, oracle_raw, status, raw, oracle_tool)
                if invalid is not None:
                    cells[tool] = invalid
                    continue
                try:
                    measured = median_of(
                        argv,
                        warmup=warmup,
                        runs=runs,
                        timeout=timeout,
                        env=env,
                        stdin_path=stdin_for(spec, tool, fixture),
                    )
                except MeasurementFailure as err:
                    cells[tool] = {"status": err.status}
                    continue
                cells[tool] = {
                    "status": "ok",
                    "wall_s": measured.wall_s,
                    "maxrss_bytes": measured.maxrss_bytes,
                }
            row = {
                "id": case["id"],
                "dataset": case["dataset"],
                "kind": case["kind"],
                "width": case["width"],
                "rows": case["rows"],
                "query_id": case["query_id"],
                "jqf_diagnostics": diagnostics,
                "cells": cells,
            }
            write_case_receipt(write_dest, row)
        receipt_rows[row["id"]] = row
        shown = mask_excluded(row, skip, args.with_excluded)
        results.append(shown)
        combined = []
        for tool in PANEL:
            wall, rss = cell_pair(shown["cells"][tool])
            combined.append(wall if wall != rss else wall)
            if shown["cells"][tool].get("status") == "ok":
                combined[-1] = f"{wall} / {rss}"
        rows_out.append([case["id"], *combined])
        if args.jqf_only and cached is not None:
            mark = "jqf-only"
        elif cached is not None:
            mark = "resume"
        else:
            mark = "done"
        print(f"[{i}/{len(cases)}] {mark} {case['id']}  {combined[0]}", flush=True)
        payload = {
            "commit": commit,
            "time": started,
            "diagnostics": diagnostics,
            "quick": args.quick,
            "warmup": warmup,
            "runs": runs,
            "tools": tools_versions,
            "host": host,
            "disagree": spec.get("disagree") or [],
            "results": [
                mask_excluded(
                    mask_stale_jqf(r, diagnostics),
                    exclusions_for(spec, r),
                    args.with_excluded,
                )
                for r in sorted(receipt_rows.values(), key=result_sort_key)
                if r["id"] in catalog_ids
            ],
        }
        write_md(md_path, payload)
        write_tsv(tsv_path, payload)

    widths = [len(h) for h in stdout_header]
    for row in rows_out:
        for i, col in enumerate(row):
            widths[i] = max(widths[i], len(col))

    def fmt_row(cols: list[str]) -> str:
        return "  ".join(c.ljust(widths[i]) for i, c in enumerate(cols))

    print(fmt_row(stdout_header))
    print("  ".join("-" * w for w in widths))
    for row in rows_out:
        print(fmt_row(row))

    print()
    print("geomean vs jqf (ok cells, same-case pairs):")
    for tool in PANEL[1:]:
        wall = ratios_vs_jqf(results, tool, "wall_s")
        rss = ratios_vs_jqf(results, tool, "maxrss_bytes")
        if not wall:
            print(f"  {tool}: n/a")
            continue
        gw = math.exp(sum(math.log(r) for r in wall) / len(wall))
        gr = math.exp(sum(math.log(r) for r in rss) / len(rss)) if rss else None
        rss_s = f", rss {gr:.2f}×" if gr else ""
        print(
            f"  {tool}: wall {gw:.2f}× (median {statistics.median(wall):.2f}×){rss_s}  n={len(wall)}"
        )

    if skipped:
        print(f"resumed {skipped}/{len(cases)} from {CACHE / 'cells' / stamp}")
    print(f"\nwrote {md_path}")
    print(f"wrote {tsv_path}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", nargs="?", default="run", choices=["run", "setup", "self-test"])
    parser.add_argument("--jqf", default=None, help="jqf binary (default: target/pgo/jqf)")
    parser.add_argument(
        "--jqf-only",
        action="store_true",
        help="remeasure jqf only; keep cached jq/jaq/gojq/yq cells",
    )
    parser.add_argument("--quick", action="store_true", help="size 100")
    parser.add_argument(
        "--runs", type=positive_int, default=None, help="timed runs per cell (default: cases.json runs)"
    )
    parser.add_argument(
        "--warmup", type=nonnegative_int, default=None, help="warmup runs per cell (default: cases.json warmup)"
    )
    parser.add_argument("--out", default=None, help="directory for results.md / results.tsv")
    parser.add_argument("--force", action="store_true", help="ignore cached case receipts")
    parser.add_argument(
        "--with-excluded",
        action="store_true",
        help="time excluded (case, tool) pairs too",
    )
    parser.add_argument(
        "--case",
        action="append",
        default=[],
        metavar="GLOB",
        help="run matching case ids only; report still includes every cached case",
    )
    args = parser.parse_args()
    if args.command == "setup":
        from setup import main as setup_main

        setup_main()
        return 0
    if args.command == "self-test":
        self_check()
        spec = load_spec()
        datasets, cases = expand(spec, spec["sizes"])
        assert len(datasets) == 14 + 14 + 12 + 12 + 14, len(datasets)
        assert len(cases) == 294 + 70 + 72 + 72 + 56, len(cases)
        from catalog import slice_bounds

        assert slice_bounds(100) == (25, 35)
        assert "yq" in exclusions_for(spec, {"id": "users-broad-50k-identity"})
        assert "yq" not in exclusions_for(spec, {"id": "users-narrow-100-first-id"})
        assert filter_cases(cases, ["users-narrow-100-first-id"])[0]["id"] == "users-narrow-100-first-id"
        print("self-test: ok")
        return 0
    return run_panel(args)


if __name__ == "__main__":
    sys.exit(main())
