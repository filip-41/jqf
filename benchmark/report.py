"""Format one run as a Markdown table and long-form TSV.

Successful cells show wall and RSS; other cells show one status. TSV is one
row per (case, tool) with raw seconds and bytes. Sibling of run.py.
"""

from __future__ import annotations

import math
import statistics
from pathlib import Path

from catalog import SIZE_LABEL

PANEL = ["jqf", "jqf-serial", "jq", "jaq", "gojq", "yq", "dasel", "mlr"]


def fmt_ms(seconds: float) -> str:
    ms = seconds * 1000
    if ms >= 100:
        return f"{ms:.0f} ms"
    if ms >= 10:
        return f"{ms:.1f} ms"
    return f"{ms:.2f} ms"


def fmt_rss(nbytes: int) -> str:
    mb = nbytes / (1024 * 1024)
    if mb >= 10:
        return f"{mb:.0f} MB"
    return f"{mb:.1f} MB"


def cell_pair(cell: dict) -> tuple[str, str]:
    if cell.get("status") != "ok":
        status = cell.get("status", "error")
        return status, status
    return fmt_ms(cell["wall_s"]), fmt_rss(cell["maxrss_bytes"])


def _geomean(values: list[float]) -> float | None:
    if not values:
        return None
    return math.exp(sum(math.log(v) for v in values) / len(values))


def ratios_vs_jqf(results: list[dict], tool: str, field: str) -> list[float]:
    out = []
    for row in results:
        a, b = row["cells"]["jqf"], row["cells"].get(tool, {})
        if a.get("status") == "ok" and b.get("status") == "ok" and a[field] > 0:
            out.append(b[field] / a[field])
    return out


def _ratio_cell(results: list[dict], tool: str, field: str) -> str:
    values = ratios_vs_jqf(results, tool, field)
    if not values:
        return "n/a"
    geo = _geomean(values)
    return f"{geo:.2f}× (median {statistics.median(values):.2f}×)"


def _geomean_rows(results: list[dict]) -> list[str]:
    lines = ["| tool | wall | rss | n |", "| --- | --- | --- | --- |"]
    for tool in PANEL[1:]:
        n = len(ratios_vs_jqf(results, tool, "wall_s"))
        lines.append(
            f"| {tool} | {_ratio_cell(results, tool, 'wall_s')} | {_ratio_cell(results, tool, 'maxrss_bytes')} | {n} |"
        )
    return lines


def _is_stream(row: dict) -> bool:
    return row.get("kind") in {"ndjson", "csv"}


def build_kind(diagnostics: str) -> str:
    if "build=pgo" in diagnostics:
        return "pgo"
    if "build=plain" in diagnostics:
        return "plain"
    return "unknown"


def _geomean_split_rows(results: list[dict]) -> list[str]:
    doc = [row for row in results if not _is_stream(row)]
    stream = [row for row in results if _is_stream(row)]
    lines = [
        "| tool | document wall | document rss | n | streaming wall | streaming rss | n |",
        "| --- | --- | --- | --- | --- | --- | --- |",
    ]
    for tool in PANEL[1:]:
        n_doc = len(ratios_vs_jqf(doc, tool, "wall_s"))
        n_stream = len(ratios_vs_jqf(stream, tool, "wall_s"))
        lines.append(
            "| "
            + " | ".join(
                [
                    tool,
                    _ratio_cell(doc, tool, "wall_s"),
                    _ratio_cell(doc, tool, "maxrss_bytes"),
                    str(n_doc),
                    _ratio_cell(stream, tool, "wall_s"),
                    _ratio_cell(stream, tool, "maxrss_bytes"),
                    str(n_stream),
                ]
            )
            + " |"
        )
    return lines


def _preview_block(title: str, blob: dict | None) -> list[str]:
    if not blob:
        return [f"{title}: not captured (remeasure to fill)", ""]
    sha = blob.get("sha256", "")
    lines = [f"{title} ({blob.get('bytes', '?')} bytes, sha256 {sha[:16]}…):", ""]
    lines.append("```")
    lines.append(blob.get("snippet", ""))
    lines.append("```")
    lines.append("")
    return lines


def _known_disagreements(entries: list[dict]) -> list[str]:
    lines = ["## known disagreements", ""]
    if not entries:
        lines.append("none.")
        lines.append("")
        return lines
    groups: dict[str, list[dict]] = {}
    for entry in entries:
        groups.setdefault(entry["why"], []).append(entry)
    for why, items in groups.items():
        tools = ", ".join(sorted({item["tool"] for item in items}))
        ids = ", ".join(item["id"] for item in items)
        lines.append(f"{tools}: {why}")
        lines.append("")
        lines.append(ids)
        lines.append("")
    return lines


def _disagreements(results: list[dict]) -> list[str]:
    lines = ["## disagreements", ""]
    hits = 0
    for row in results:
        for tool, cell in row.get("cells", {}).items():
            if cell.get("status") != "disagreed":
                continue
            hits += 1
            oracle = cell.get("oracle_tool", "?")
            lines.append(f"### {row['id']} · {tool} (oracle {oracle})")
            lines.append("")
            lines.extend(_preview_block("expected", cell.get("expected")))
            lines.extend(_preview_block("got", cell.get("got")))
    if not hits:
        lines.append("none.")
        lines.append("")
    return lines


def _by_size(results: list[dict]) -> list[tuple[object, list[dict]]]:
    groups: dict[object, list[dict]] = {}
    for row in results:
        groups.setdefault(row.get("rows", row.get("dataset", "?")), []).append(row)

    def sort_key(key: object):
        if isinstance(key, int):
            return (0, key)
        return (1, str(key))

    return sorted(groups.items(), key=lambda kv: sort_key(kv[0]))


def write_tsv(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    preamble = []
    if payload.get("commit"):
        preamble.append(f"# commit\t{payload['commit']}")
    preamble.append(f"# jqf_build\t{build_kind(payload.get('diagnostics') or '')}")
    if payload.get("diagnostics"):
        preamble.append(f"# diagnostics\t{payload['diagnostics']}")
    if payload.get("time"):
        preamble.append(f"# time\t{payload['time']}")
    for key, value in (payload.get("host") or {}).items():
        preamble.append(f"# {key}\t{value}")
    cols = [
        "case",
        "kind",
        "width",
        "rows",
        "query",
        "tool",
        "status",
        "wall_s",
        "maxrss_bytes",
    ]
    lines = ["\t".join(cols)]
    for row in payload["results"]:
        for tool in PANEL:
            cell = row["cells"].get(tool, {"status": "n/a"})
            status = cell.get("status", "n/a")
            wall = f"{cell['wall_s']:.9f}" if status == "ok" else ""
            rss = str(cell["maxrss_bytes"]) if status == "ok" else ""
            lines.append(
                "\t".join(
                    [
                        row["id"],
                        row["kind"],
                        row["width"],
                        str(row["rows"]),
                        row["query_id"],
                        tool,
                        status,
                        wall,
                        rss,
                    ]
                )
            )
    path.write_text("\n".join(preamble + lines) + "\n")


def write_md(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    lines: list[str] = []
    lines.append("# jqf benchmark")
    lines.append("")
    lines.append(
        "These numbers are a local snapshot for guidance, not a published result."
    )
    lines.append("")
    build = build_kind(payload.get("diagnostics") or "")
    if payload.get("commit"):
        lines.append(f"- jqf: {build} · `{payload['commit']}`")
    else:
        lines.append(f"- jqf: {build}")
    if payload.get("time"):
        lines.append(f"- time: {payload['time']}")
    if payload.get("diagnostics"):
        diag = payload["diagnostics"].strip()
        if diag.startswith("jqf:"):
            lines.append(f"- diagnostics: `{diag}`")
    for name, version in payload["tools"].items():
        lines.append(f"- {name}: {version}")
    lines.append(
        f"- warmup {payload['warmup']}, runs {payload['runs']}, median wall; RSS from that run"
    )
    if payload.get("quick"):
        lines.append("- mode: quick (small sizes, not a publication run)")
    host = payload.get("host") or {}
    if host:
        lines.append("")
        lines.append("## host")
        lines.append("")
        for key, value in host.items():
            lines.append(f"- {key}: {value}")
    lines.append("")
    lines.append("## geomean vs jqf")
    lines.append("")
    lines.extend(_geomean_rows(payload["results"]))
    lines.append("")
    lines.append("document = json/yaml/toml. streaming = ndjson/csv records.")
    lines.append("")
    lines.extend(_geomean_split_rows(payload["results"]))
    lines.append("")
    for size, group in _by_size(payload["results"]):
        label = SIZE_LABEL.get(size, str(size)) if isinstance(size, int) else str(size)
        lines.append(f"## geomean vs jqf · {label}")
        lines.append("")
        lines.extend(_geomean_split_rows(group))
        lines.append("")
    header = ["case", *PANEL]
    lines.append("## results")
    lines.append("")
    lines.append("| " + " | ".join(header) + " |")
    lines.append("| " + " | ".join("---" for _ in header) + " |")
    for row in payload["results"]:
        cols = [row["id"]]
        for tool in PANEL:
            wall, rss = cell_pair(row["cells"].get(tool, {"status": "n/a"}))
            cols.append(wall if wall == rss else f"{wall} / {rss}")
        lines.append("| " + " | ".join(cols) + " |")
    lines.append("")
    lines.extend(_known_disagreements(payload.get("disagree") or []))
    lines.extend(_disagreements(payload["results"]))
    path.write_text("\n".join(lines))
