"""Expand `cases.json` into datasets and cases.

Sizes × widths × kinds. Slice bounds scale with `n` so a 10-row file still
has a non-empty window. Sibling of run.py.
"""

from __future__ import annotations

from typing import Any

SIZE_LABEL = {
    100: "100",
    1000: "1k",
    5000: "5k",
    25000: "25k",
    50000: "50k",
    100000: "100k",
    200000: "200k",
}


def slice_bounds(n: int) -> tuple[int, int]:
    lo = n // 4
    hi = lo + max(1, n // 10)
    if hi > n:
        hi = n
    if lo >= hi:
        lo, hi = 0, n
    return lo, hi


def dataset_name(kind: str, width: str, n: int) -> str:
    return f"{kind}-{width}-{SIZE_LABEL[n]}"


def exclusions_for(spec: dict[str, Any], row: dict[str, Any]) -> dict[str, str]:
    out: dict[str, str] = {}
    for rule in spec.get("exclude") or []:
        if rule.get("id") != row.get("id"):
            continue
        out[rule["tool"]] = rule["why"]
    return out


def expand(spec: dict[str, Any], sizes: list[int]) -> tuple[dict[str, dict], list[dict]]:
    datasets: dict[str, dict] = {}
    cases: list[dict] = []
    for kind, kspec in spec["kinds"].items():
        kind_sizes = kspec.get("sizes")
        for width in kspec.get("widths", spec["widths"]):
            for n in sizes:
                if kind_sizes is not None and n not in kind_sizes:
                    continue
                name = dataset_name(kind, width, n)
                datasets[name] = {
                    "kind": kind,
                    "width": width,
                    "rows": n,
                    "suffix": kspec["suffix"],
                }
                lo, hi = slice_bounds(n)
                for query in kspec["queries"]:
                    panel = list(query.get("panel", kspec["panel"]))
                    expressions = {}
                    for tool in panel:
                        template = query[tool] if tool in query else query["expr"]
                        expressions[tool] = template.format(lo=lo, hi=hi, n=n)
                    case: dict[str, Any] = {
                        "id": f"{name}-{query['id']}",
                        "dataset": name,
                        "kind": kind,
                        "width": width,
                        "rows": n,
                        "query_id": query["id"],
                        "expressions": expressions,
                    }
                    extra: dict[str, list[str]] = {}
                    for tool in expressions:
                        bits: list[str] = []
                        key = f"{tool}_extra"
                        if kspec.get(key):
                            bits.extend(kspec[key])
                        if query.get(key):
                            bits.extend(query[key])
                        if bits:
                            extra[tool] = bits
                    if "jqf" in expressions:
                        expressions["jqf-serial"] = expressions["jqf"]
                        if extra.get("jqf"):
                            extra["jqf-serial"] = list(extra["jqf"])
                    if extra:
                        case["extra"] = extra
                    if kspec.get("yq_compact"):
                        case["yq_compact"] = list(kspec["yq_compact"])
                    cases.append(case)
    return datasets, cases
