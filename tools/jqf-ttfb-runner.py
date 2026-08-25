#!/usr/bin/env python3
"""Time-to-first-output lane runner: the measurement class hyperfine cannot see.

`hyperfine` measures a process from exec to exit, so it scores TOTAL
throughput and is structurally blind to WHEN output starts. jqf makes
streaming claims — the record route publishes per morsel, the projected
publishing drive publishes per element, a fold publishes once on completion —
and none of them is observable in `tools/jqf-e2e-ladder.sh`. This runner is
that missing instrument.

Each lane launches the tool with stdout piped, blocks on the FIRST byte,
timestamps it, then drains to exit and timestamps that. Two numbers per
(lane, tool): `first_byte_ms` and `total_ms`. Their ratio is the whole point
— a program that buffers its whole answer reads ~1.00 no matter how fast it
is, and a regression that turns a streaming route back into a buffered one
moves the ratio while leaving `total_ms` untouched.

The CONTROL lane is what makes the other numbers mean anything. `T4
catalog-collect` folds to a single number, so it CANNOT stream: its ratio is
asserted to stay near 1.00. If T4 ever read like a streaming lane, the
instrument would be measuring pipe buffering rather than publication, and
every other row would be worthless.

The `L*` FOLLOW lanes are the same measurement class taken to its limit. A
`--follow` process never exits, so it has no total at all — only "when did this
record come out", which is the question this runner already asks. They report
per-record latency under a growing file, the pipe control that explains it, and
the steady-state drain rate. No competitor has `--follow`, so those lanes are
jqf-only and their agreement check compares the tail's published bytes against
`jq -c` over the file it grew into.

Two house rules from `AGENTS.md` bind here. Output agreement is validated
BEFORE any timing — a tool that does not produce jq's bytes is EXCLUDED from
the lane and is never timed, so no row can time the wrong answer. And a
number that carries weight must come from the PGO binary (`target/pgo/jqf`,
built by `make pgo`); this script prefers it and falls back to the plain
release build only when no PGO binary exists (printing `build=plain` on the
receipt), printing `build=` from `--diagnostics` on every row so it can
always be attributed to the binary that produced it.

Usage: tools/jqf-ttfb-runner.py [--json] [--runs N] [--warmup N] [--no-assert]
                                [--skip-follow]
  JQF_BIN          jqf binary (default: target/pgo/jqf, then target/release/jqf)
  JQF_E2E_FIXDIR   reuse/persist fixtures here (default: a removed mktemp dir)
  JQ / JAQ / GOJQ  override the detected competitor paths
"""

from __future__ import annotations

import argparse
import json
import os
# Hermeticity: a developer's .jqf.toml must never reach a gate — the
# harnesses are hermetic by construction, not by convention.
os.environ["JQF_NO_CONFIG"] = "1"
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Lane registry. `flags` are jqf-only; competitors always run the bare
# program, because none of them has a switch that means what jqf's means.
# `jqf_only` marks a lane that exists to A/B jqf against itself.
#
# `klass` is a CLAIM about publication, asserted below:
#   streaming  — output must begin well before the run ends (ratio <= 0.80)
#   completion — output cannot begin before the run ends (ratio >= 0.85)
LANES = [
    {
        "id": "T1",
        "name": "ndjson-fanout",
        "program": ".v",
        "fixture": "ndjson-200k.ndjson",
        "flags": [],
        "klass": "streaming",
        "jqf_only": False,
        "why": "record route, publishes per morsel",
    },
    {
        "id": "T2",
        "name": "ndjson-serial",
        "program": ".v",
        "fixture": "ndjson-200k.ndjson",
        "flags": ["--no-parallel"],
        "klass": "streaming",
        "jqf_only": True,
        "why": "the --no-parallel A/B for T1: what the default flip costs at the head of the stream",
    },
    {
        "id": "T3",
        "name": "catalog-fanout",
        "program": ".catalog[].name",
        "fixture": "catalog-10mb.json",
        "flags": [],
        "klass": "streaming",
        "jqf_only": False,
        "why": "projected publishing drive, publishes per element",
    },
    {
        "id": "T4",
        "name": "catalog-collect",
        "program": "[.catalog[].name] | length",
        "fixture": "catalog-10mb.json",
        "flags": [],
        "klass": "completion",
        "jqf_only": False,
        "why": "CONTROL: a fold publishes exactly one item, on completion",
    },
]

STREAMING_RATIO_MAX = 0.80
COMPLETION_RATIO_MIN = 0.85

# --- follow lanes ---------------------------------------------------------
#
# `--follow` is sold on per-record latency under a growing file, and that
# number had never been taken: `tools/jqf-follow-e2e.py` runs seven CORRECTNESS
# lanes and contains no timing call at all. It belongs here rather than in the
# broad bench because a followed process never exits, so there is no
# exec-to-exit number to take — only "when did this record come out", which is
# exactly the measurement class this runner owns.
#
# No competitor has --follow, so every lane is jqf-only. Agreement is still
# checked first, against `jq -c` over the FINISHED file: a live tail that
# publishes the wrong bytes is not a faster tail.
#
# Each lane carries its own `runs`, unlike the TTFB lanes above, because the
# file-latency lane's wall cost is records x poll interval — 25 records is
# already 2.5 seconds, and repeating it seven times would buy nothing: the
# number it reports is a constant of the reader, not a distribution.
FOLLOW_LANES = [
    {
        "id": "L1",
        "name": "follow-file-latency",
        "source": "file",
        "program": ".level",
        "records": 25,
        "runs": 2,
        "klass": "poll-bound",
        "why": "a regular file grows one record at a time; the reader learns "
               "about it by sleep+stat, so this lane measures the poll interval "
               "and should be read as the price of the portable primitive",
    },
    {
        "id": "L2",
        "name": "follow-pipe-latency",
        "source": "pipe",
        "program": ".level",
        "records": 200,
        "runs": 3,
        "klass": "blocking",
        "why": "the same tail over a PIPE, where the read blocks instead of "
               "polling. This is the control that proves L1's number is the "
               "poll interval and not the follow route's own cost",
    },
    {
        "id": "L3",
        "name": "follow-drain-50k",
        "source": "file",
        "program": ".level",
        "records": 50000,
        "runs": 3,
        "batch": True,
        "klass": "throughput",
        "why": "50k records appended AT ONCE: steady-state drain with the poll "
               "interval amortised, which is the number a log pipeline lives on",
    },
    {
        "id": "L4",
        "name": "follow-drain-200k",
        "source": "file",
        "program": ".level",
        "records": 200000,
        "runs": 3,
        "batch": True,
        "klass": "throughput",
        "why": "the second point on the drain curve. One point cannot separate "
               "the per-record rate from the fixed poll wait that every batch "
               "pays once; two points at a 4x record ratio can",
    },
]

# A pipe tail must not be poll-bound: it is a blocking read, so anything near
# the file poll interval means the blocking path was lost.
FOLLOW_PIPE_LATENCY_MAX_MS = 10.0
# A file tail is poll-bound BY DESIGN (jqf-cli/src/routes/follow.rs pins
# FOLLOW_POLL_INTERVAL at 100ms). The bound is 2.5x that, which passes the
# documented behaviour with headroom and still catches a tail that has started
# batching records across polls.
FOLLOW_FILE_LATENCY_MAX_MS = 250.0
# A batch appended at once must not be delivered one poll at a time. This floor
# is two orders of magnitude under the measured rate and two orders OVER the ~10
# rec/s a per-record poll would give, so it fails the collapse without pinning
# the machine's actual throughput.
FOLLOW_MIN_RECORDS_PER_S = 100_000.0
# How long a batch lane lets the tail reach its steady poll before appending, so
# the drain measures the drain and not the tail's own startup.
FOLLOW_SETTLE_S = 0.25

FOLLOW_LEVELS = ("info", "warn", "error", "debug")


def follow_record(index: int) -> str:
    """One NDJSON record for the follow lanes.

    `level` CYCLES rather than staying constant so the projected stream encodes
    order and count, not merely length: a dropped, duplicated or reordered
    record cannot survive the byte comparison against jq.
    """
    return json.dumps({"id": index, "level": FOLLOW_LEVELS[index % len(FOLLOW_LEVELS)]}) + "\n"


def spawn_follow(jqf: Path, program: str, path: Path | None) -> subprocess.Popen:
    """Launch a tail. `path` of None means it follows stdin, i.e. a pipe."""
    argv = [str(jqf), "-c", "--follow", program]
    if path is not None:
        argv.append(str(path))
    return subprocess.Popen(
        argv,
        stdin=subprocess.PIPE if path is None else subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )


def stop_follow(proc: subprocess.Popen) -> None:
    """End a tail. A followed process never exits on its own — that IS the
    feature — so closing stdin is the graceful ask and TERM is the fallback."""
    if proc.stdin is not None and not proc.stdin.closed:
        try:
            proc.stdin.close()
        except BrokenPipeError:
            pass
    try:
        proc.wait(timeout=2)
    except subprocess.TimeoutExpired:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    if proc.stdout is not None:
        proc.stdout.close()


def follow_latency_once(jqf: Path, lane: dict, workdir: Path, tag: str) -> dict | str:
    """One record in, one record out, `records` times.

    The producer WAITS for each record's output before writing the next, so a
    sample is a full round trip and no queue can build up behind a slow reader
    and flatter the median.

    The first sample is reported separately and excluded from the latency
    statistics: at that moment the tail is still starting, so its record is
    already in the file when the reader first looks and no poll is involved.
    Averaging it in would understate exactly the number the lane exists to show.
    """
    path = None
    if lane["source"] == "file":
        path = workdir / f"follow-{lane['id']}-{tag}.ndjson"
        path.write_text("")
    launch = time.perf_counter()
    proc = spawn_follow(jqf, lane["program"], path)
    sink = path.open("a") if path is not None else proc.stdin
    assert sink is not None and proc.stdout is not None
    latencies: list[float] = []
    lines: list[bytes] = []
    first_byte = None
    try:
        for index in range(lane["records"]):
            record = follow_record(index)
            sink.write(record if path is not None else record.encode())
            sink.flush()
            written = time.perf_counter()
            line = proc.stdout.readline()
            arrived = time.perf_counter()
            if not line:
                return f"stream closed after {index} records"
            if first_byte is None:
                first_byte = (arrived - launch) * 1000.0
            latencies.append((arrived - written) * 1000.0)
            lines.append(line)
    finally:
        if path is not None:
            sink.close()
        stop_follow(proc)
    steady = latencies[1:] or latencies
    return {
        "first_byte_ms": first_byte,
        "latency_median_ms": statistics.median(steady),
        "latency_p95_ms": sorted(steady)[max(0, int(len(steady) * 0.95) - 1)],
        "latency_max_ms": max(steady),
        "records": len(latencies),
        "output": b"".join(lines),
    }


def follow_throughput_once(jqf: Path, lane: dict, workdir: Path, tag: str) -> dict | str:
    """A whole batch appended at once, then drained.

    `drain_ms` INCLUDES one poll wait — the tail is asleep when the append
    lands — and the producer's own write. Both are fixed costs per batch rather
    than per record, which is why this lane comes in two sizes: the pair
    separates the per-record rate from the constant, and neither point alone
    can. Read the rate as a floor.
    """
    path = workdir / f"follow-{lane['id']}-{tag}.ndjson"
    path.write_text("")
    payload = "".join(follow_record(index) for index in range(lane["records"]))
    proc = spawn_follow(jqf, lane["program"], path)
    assert proc.stdout is not None
    lines: list[bytes] = []
    try:
        time.sleep(FOLLOW_SETTLE_S)
        start = time.perf_counter()
        with path.open("a") as handle:
            handle.write(payload)
            handle.flush()
        appended = time.perf_counter()
        while len(lines) < lane["records"]:
            line = proc.stdout.readline()
            if not line:
                return f"stream closed after {len(lines)} of {lane['records']} records"
            lines.append(line)
        drained = time.perf_counter()
    finally:
        stop_follow(proc)
    drain_ms = (drained - start) * 1000.0
    return {
        "append_ms": (appended - start) * 1000.0,
        "drain_ms": drain_ms,
        "records_per_s": lane["records"] / (drain_ms / 1000.0) if drain_ms > 0 else None,
        "records": len(lines),
        "output": b"".join(lines),
    }


def measure_follow(jqf: Path, lane: dict, workdir: Path) -> dict | str:
    """Median-of-`lane['runs']`, with the FIRST run spent on agreement.

    That first run is not a warmup by another name: its bytes are what the
    caller compares against jq, so no follow number in this runner is ever
    taken from a tail whose output was not checked first.
    """
    once = follow_throughput_once if lane.get("batch") else follow_latency_once
    samples = []
    for index in range(max(1, lane["runs"]) + 1):
        sample = once(jqf, lane, workdir, str(index))
        if isinstance(sample, str):
            return f"run {index}: {sample}"
        samples.append(sample)
    output = samples[0]["output"]
    timed = samples[1:] or samples
    merged = {
        key: statistics.median(s[key] for s in timed)
        for key in timed[0]
        if key not in {"output", "records"}
    }
    merged["records"] = timed[0]["records"]
    merged["runs"] = len(timed)
    merged["output"] = output
    return merged


def measure_once(argv: list[str], infile: Path) -> dict:
    """One launch: block on the first stdout byte, then drain to exit.

    Reading a single byte and only then draining is deliberate. Draining
    first would let the OS pipe buffer hide the publication boundary, which
    is the one thing this instrument exists to see.
    """
    with infile.open("rb") as handle:
        start = time.perf_counter()
        proc = subprocess.Popen(
            argv,
            stdin=handle,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        assert proc.stdout is not None
        head = proc.stdout.read(1)
        first_byte = time.perf_counter() - start if head else None
        rest = proc.stdout.read()
        proc.wait()
        total = time.perf_counter() - start
    return {
        "first_byte_ms": None if first_byte is None else first_byte * 1000.0,
        "total_ms": total * 1000.0,
        "bytes": len(head) + len(rest),
        "returncode": proc.returncode,
    }


def measure(argv: list[str], infile: Path, runs: int, warmup: int) -> dict | str:
    """Median-of-`runs`. Returns a failure REASON string rather than raising,
    so one broken tool degrades to an excluded cell instead of aborting."""
    for _ in range(max(0, warmup)):
        measure_once(argv, infile)
    samples = [measure_once(argv, infile) for _ in range(max(1, runs))]
    for index, sample in enumerate(samples, start=1):
        if sample["returncode"] != 0:
            return f"sample {index} exited {sample['returncode']}"
        if sample["first_byte_ms"] is None:
            return f"sample {index} produced no output byte"
    first = statistics.median(s["first_byte_ms"] for s in samples)
    total = statistics.median(s["total_ms"] for s in samples)
    return {
        "first_byte_ms": first,
        "total_ms": total,
        "ratio": first / total if total > 0 else None,
        "runs": len(samples),
        "bytes": samples[0]["bytes"],
    }


def capture(argv: list[str], infile: Path) -> tuple[int, bytes]:
    with infile.open("rb") as handle:
        proc = subprocess.run(
            argv, stdin=handle, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL
        )
    return proc.returncode, proc.stdout


def build_stamp(jqf: Path) -> str:
    """`build=pgo profile=… allocator=…` off --diagnostics, so a receipt can
    always be attributed to the binary that produced it. Numbers that carry
    weight come from the PGO binary; receipts live out-of-tree."""
    proc = subprocess.run(
        [str(jqf), "--diagnostics", "."],
        input=b"null",
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    for line in proc.stderr.decode("utf-8", "replace").splitlines():
        if "build=" in line:
            return line.strip()
    return "build=unknown"


def resolve_jqf() -> Path:
    override = os.environ.get("JQF_BIN")
    if override:
        return Path(override)
    pgo = ROOT / "target" / "pgo" / "jqf"
    if pgo.is_file() and os.access(pgo, os.X_OK):
        return pgo
    return ROOT / "target" / "release" / "jqf"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="one JSON object per row")
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument(
        "--no-assert",
        action="store_true",
        help="report the streaming/completion class ratios without failing on them",
    )
    parser.add_argument(
        "--skip-follow",
        action="store_true",
        help="omit the --follow lanes (L*), whose file lane is poll-bound and "
        "therefore the slowest wall-clock part of this runner",
    )
    args = parser.parse_args()

    jqf = resolve_jqf()
    if not (jqf.is_file() and os.access(jqf, os.X_OK)):
        print(
            f"error: jqf binary not found at {jqf}; run `make` for the PGO build "
            "(the one every recorded measurement must come from) or set JQF_BIN",
            file=sys.stderr,
        )
        return 2
    stamp = build_stamp(jqf)
    if "build=pgo" not in stamp:
        print(
            f"notice: {jqf} reports `{stamp}` — numbers from a non-PGO binary are "
            "smoke/directional and must not be recorded as pinned receipts",
            file=sys.stderr,
        )

    competitors = []
    for label, env in (("jq", "JQ"), ("jaq", "JAQ"), ("gojq", "GOJQ")):
        found = os.environ.get(env) or shutil.which(label)
        if found:
            competitors.append((label, found))
        else:
            print(f"notice: {label} not found on PATH; column skipped", file=sys.stderr)
    have_jq = any(label == "jq" for label, _ in competitors)

    fixdir_env = os.environ.get("JQF_E2E_FIXDIR")
    fixdir = Path(fixdir_env) if fixdir_env else Path(tempfile.mkdtemp(prefix="jqf-ttfb-fix."))
    try:
        if subprocess.run(
            [sys.executable, str(ROOT / "tools" / "jqf-e2e-fixtures.py"), str(fixdir)]
        ).returncode:
            print("error: fixture generation failed", file=sys.stderr)
            return 2

        print(f"jqf-ttfb-runner: jqf={jqf} {stamp}", file=sys.stderr)
        print(f"jqf-ttfb-runner: fixtures in {fixdir}", file=sys.stderr)

        rows: list[dict] = []
        failures: list[str] = []
        excluded: list[str] = []

        for lane in LANES:
            infile = fixdir / lane["fixture"]
            # `-c` to match the competitors below: time-to-first-byte and the
            # published byte count are both formatting-sensitive, so every
            # tool in a lane must be writing the same bytes.
            jqf_argv = [str(jqf), "-c", *lane["flags"], lane["program"]]

            # Agreement BEFORE timing, universally. jq is the oracle, exactly
            # as on the ladder; a tool that disagrees is EXCLUDED, never timed,
            # and an exclusion is not a loss.
            jqf_code, jqf_bytes = capture(jqf_argv, infile)
            if jqf_code != 0:
                failures.append(f"{lane['id']} jqf exited {jqf_code}")
                continue
            agreement = "jq absent; agreement unchecked"
            if have_jq:
                jq_bin = dict(competitors)["jq"]
                jq_code, jq_bytes = capture([jq_bin, "-c", lane["program"]], infile)
                if jq_code != 0:
                    failures.append(f"{lane['id']} jq exited {jq_code}")
                    continue
                if jqf_bytes != jq_bytes:
                    failures.append(f"{lane['id']} jqf output not byte-identical to jq -c")
                    continue
                agreement = "jqf==jq"

            candidates = [("jqf", jqf_argv)]
            if not lane["jqf_only"]:
                for label, path in competitors:
                    argv = [path, "-c", lane["program"]]
                    if label != "jq":
                        _, other = capture(argv, infile)
                        if have_jq and other != jqf_bytes:
                            excluded.append(
                                f"{lane['id']}/{label}: output differs from jq (excluded, NOT slower)"
                            )
                            continue
                    candidates.append((label, argv))

            for label, argv in candidates:
                result = measure(argv, infile, args.runs, args.warmup)
                if isinstance(result, str):
                    excluded.append(f"{lane['id']}/{label}: {result} (excluded, NOT slower)")
                    continue
                verdict = ""
                if label == "jqf" and result["ratio"] is not None:
                    if lane["klass"] == "streaming":
                        ok = result["ratio"] <= STREAMING_RATIO_MAX
                        verdict = "streaming OK" if ok else "streaming FAIL"
                        if not ok and not args.no_assert:
                            failures.append(
                                f"{lane['id']} jqf first_byte/total={result['ratio']:.3f} "
                                f"exceeds the streaming bound {STREAMING_RATIO_MAX}"
                            )
                    else:
                        ok = result["ratio"] >= COMPLETION_RATIO_MIN
                        verdict = "control OK" if ok else "control FAIL"
                        if not ok and not args.no_assert:
                            failures.append(
                                f"{lane['id']} CONTROL jqf first_byte/total="
                                f"{result['ratio']:.3f} is below {COMPLETION_RATIO_MIN}: a fold "
                                "cannot stream, so the instrument is measuring pipe buffering"
                            )
                rows.append(
                    {
                        "lane": lane["id"],
                        "name": lane["name"],
                        "program": lane["program"],
                        "flags": " ".join(lane["flags"]),
                        "tool": label,
                        "class": lane["klass"],
                        "first_byte_ms": round(result["first_byte_ms"], 2),
                        "total_ms": round(result["total_ms"], 2),
                        "ratio": None if result["ratio"] is None else round(result["ratio"], 3),
                        "runs": result["runs"],
                        "agreement": agreement,
                        "verdict": verdict,
                    }
                )

        follow_rows: list[dict] = []
        for lane in FOLLOW_LANES if not args.skip_follow else []:
            result = measure_follow(jqf, lane, fixdir)
            if isinstance(result, str):
                excluded.append(f"{lane['id']}/jqf: {result} (excluded, NOT slower)")
                continue

            # Agreement, on the bytes the tail actually published, against jq
            # over the same records in a finished file. The oracle cannot
            # follow anything — no competitor can — so what is being checked is
            # that following a growing file yields the same stream as reading
            # the file it grew into.
            agreement = "jq absent; agreement unchecked"
            if have_jq:
                oracle_file = fixdir / f"follow-{lane['id']}-oracle.ndjson"
                oracle_file.write_text(
                    "".join(follow_record(i) for i in range(lane["records"]))
                )
                jq_code, jq_bytes = capture(
                    [dict(competitors)["jq"], "-c", lane["program"]], oracle_file
                )
                if jq_code != 0:
                    failures.append(f"{lane['id']} jq exited {jq_code}")
                    continue
                if result["output"] != jq_bytes:
                    failures.append(
                        f"{lane['id']} follow output is not byte-identical to jq -c "
                        "over the same records"
                    )
                    continue
                agreement = "jqf-follow==jq"

            verdict = ""
            if lane["klass"] == "throughput":
                rate = result["records_per_s"]
                ok = rate is not None and rate >= FOLLOW_MIN_RECORDS_PER_S
                verdict = "drain OK" if ok else "drain FAIL"
                if not ok and not args.no_assert:
                    failures.append(
                        f"{lane['id']} drained {rate:,.0f} rec/s, under the "
                        f"{FOLLOW_MIN_RECORDS_PER_S:,.0f} rec/s floor: a batch is "
                        "being delivered poll by poll rather than drained"
                    )
            else:
                bound = (
                    FOLLOW_PIPE_LATENCY_MAX_MS
                    if lane["source"] == "pipe"
                    else FOLLOW_FILE_LATENCY_MAX_MS
                )
                ok = result["latency_median_ms"] <= bound
                verdict = f"{lane['klass']} OK" if ok else f"{lane['klass']} FAIL"
                if not ok and not args.no_assert:
                    failures.append(
                        f"{lane['id']} per-record latency "
                        f"{result['latency_median_ms']:.2f}ms exceeds the {bound}ms "
                        f"bound for a {lane['klass']} tail"
                    )

            row = {
                "kind": "follow",
                "lane": lane["id"],
                "name": lane["name"],
                "program": lane["program"],
                "source": lane["source"],
                "tool": "jqf",
                "class": lane["klass"],
                "records": result["records"],
                "runs": result["runs"],
                "agreement": agreement,
                "verdict": verdict,
            }
            if lane["klass"] == "throughput":
                row["append_ms"] = round(result["append_ms"], 2)
                row["drain_ms"] = round(result["drain_ms"], 2)
                row["records_per_s"] = round(result["records_per_s"])
            else:
                row["first_byte_ms"] = round(result["first_byte_ms"], 2)
                row["latency_median_ms"] = round(result["latency_median_ms"], 3)
                row["latency_p95_ms"] = round(result["latency_p95_ms"], 3)
                row["latency_max_ms"] = round(result["latency_max_ms"], 3)
            follow_rows.append(row)

        if args.json:
            for row in rows:
                print(json.dumps({"kind": "ttfb", **row}))
            for row in follow_rows:
                print(json.dumps(row))
        else:
            print(
                f"{'LANE':<4} {'NAME':<16} {'TOOL':<6} {'CLASS':<11} "
                f"{'TTFB(ms)':>9} {'TOTAL(ms)':>10} {'RATIO':>7}  NOTE"
            )
            print("-" * 104)
            for row in rows:
                note = "; ".join(x for x in (row["agreement"], row["verdict"]) if x)
                print(
                    f"{row['lane']:<4} {row['name']:<16} {row['tool']:<6} {row['class']:<11} "
                    f"{row['first_byte_ms']:>9.2f} {row['total_ms']:>10.2f} "
                    f"{row['ratio']:>7.3f}  {note}"
                )
            print("-" * 104)

            if follow_rows:
                # A separate table because it is a separate measurement class:
                # a followed process never exits, so there is no total and no
                # ratio to put in the columns above.
                print()
                print(
                    f"{'LANE':<4} {'NAME':<20} {'CLASS':<11} {'RECORDS':>8} "
                    f"{'TTFB(ms)':>9} {'LAT(ms)':>9} {'P95(ms)':>9} {'REC/S':>10}  NOTE"
                )
                print("-" * 104)
                for row in follow_rows:
                    note = "; ".join(x for x in (row["agreement"], row["verdict"]) if x)
                    ttfb = row.get("first_byte_ms")
                    lat = row.get("latency_median_ms")
                    p95 = row.get("latency_p95_ms")
                    rate = row.get("records_per_s")
                    print(
                        f"{row['lane']:<4} {row['name']:<20} {row['class']:<11} "
                        f"{row['records']:>8} "
                        f"{'-' if ttfb is None else format(ttfb, '.2f'):>9} "
                        f"{'-' if lat is None else format(lat, '.3f'):>9} "
                        f"{'-' if p95 is None else format(p95, '.3f'):>9} "
                        f"{'-' if rate is None else format(rate, ','):>10}  {note}"
                    )
                print("-" * 104)

        # Machine-greppable receipts, one per row plus one summary. In --json
        # mode they go to stderr so each stream stays exactly one format.
        sink = sys.stderr if args.json else sys.stdout
        for row in rows:
            print(
                f"ttfb: lane={row['lane']} name={row['name']} tool={row['tool']} "
                f"class={row['class']} first_byte_ms={row['first_byte_ms']:.2f} "
                f"total_ms={row['total_ms']:.2f} ratio={row['ratio']:.3f} "
                f"runs={row['runs']} agreement={row['agreement'].replace(' ', '_')}",
                file=sink,
            )
        for row in follow_rows:
            if row["class"] == "throughput":
                numbers = (
                    f"append_ms={row['append_ms']:.2f} drain_ms={row['drain_ms']:.2f} "
                    f"records_per_s={row['records_per_s']}"
                )
            else:
                numbers = (
                    f"first_byte_ms={row['first_byte_ms']:.2f} "
                    f"latency_median_ms={row['latency_median_ms']:.3f} "
                    f"latency_p95_ms={row['latency_p95_ms']:.3f}"
                )
            print(
                f"follow: lane={row['lane']} name={row['name']} source={row['source']} "
                f"class={row['class']} records={row['records']} {numbers} "
                f"runs={row['runs']} agreement={row['agreement'].replace(' ', '_')}",
                file=sink,
            )
        print(
            f"ttfb-runner: lanes={len(LANES)} rows={len(rows)} "
            f"follow_lanes={0 if args.skip_follow else len(FOLLOW_LANES)} "
            f"follow_rows={len(follow_rows)} excluded={len(excluded)} "
            f"failures={len(failures)} timing=median-of-{args.runs} {stamp}",
            file=sink,
        )
        print(
            "A MISSING ROW IS NOT 'THE TOOL WAS SLOWER': it is absent (binary not "
            "installed), jqf-only (the lane has no competitor equivalent), or "
            "excluded (it disagreed with the oracle and was never timed).",
            file=sink,
        )
        if excluded:
            print("excluded cells:", file=sink)
            for item in excluded:
                print(f"  - {item}", file=sink)
        if failures:
            print("failed assertions:", file=sink)
            for item in failures:
                print(f"  - {item}", file=sink)
        return 1 if failures else 0
    finally:
        if not fixdir_env:
            shutil.rmtree(fixdir, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
