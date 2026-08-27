"""Deterministic fixtures: bytes = (kind, width, rows).

narrow is `{id, score}`. broad adds nested maps, tags, a 512-byte bio, and
32 extra scalar keys. `generate` is the only entry; sibling of run.py.
"""

from __future__ import annotations

import csv
import json
from pathlib import Path

COUNTRIES = ["US", "DE", "PL", "JP", "BR", "IN", "GB", "CA"]
TIERS = ["free", "pro", "team", "enterprise"]
DEPARTMENTS = ["engineering", "sales", "support", "finance", "ops", "marketing"]
TAGS = ["alpha", "beta", "gamma", "delta", "priority", "trial", "internal"]
_BIO = "x" * 512


def user_at(i: int, width: str) -> dict:
    if width == "narrow":
        return {"id": i, "score": (i * 37) % 1000}
    tag_count = 1 + (i % 4)
    row = {
        "id": i,
        "name": f"user-{i}",
        "email": f"user-{i}@example.com",
        "age": 16 + (i % 55),
        "active": i % 3 != 0,
        "score": (i * 37) % 1000,
        "tier": TIERS[i % len(TIERS)],
        "country": COUNTRIES[i % len(COUNTRIES)],
        "tags": [TAGS[(i + j) % len(TAGS)] for j in range(tag_count)],
        "bio": _BIO,
        "profile": {
            "company": f"company-{i % 128}",
            "department": DEPARTMENTS[i % len(DEPARTMENTS)],
            "title": f"role-{i % 16}",
        },
        "metrics": {
            "logins": (i * 13) % 500,
            "incidents": i % 7,
            "latency_ms": 20 + ((i * 29) % 900),
        },
    }
    for k in range(32):
        row[f"k{k:02d}"] = (i * (k + 3)) % 997
    return row


def _write_users_json(path: Path, rows: int, width: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        fh.write('{"users":[')
        for i in range(rows):
            if i:
                fh.write(",")
            json.dump(user_at(i, width), fh, separators=(",", ":"))
        fh.write("]}")


def _write_yaml_value(fh, key: str, value, indent: int) -> None:
    pad = " " * indent
    if isinstance(value, bool):
        fh.write(f"{pad}{key}: {'true' if value else 'false'}\n")
    elif isinstance(value, (int, float)):
        fh.write(f"{pad}{key}: {value}\n")
    elif isinstance(value, str):
        fh.write(f"{pad}{key}: {value}\n")
    elif isinstance(value, list):
        fh.write(f"{pad}{key}:\n")
        for item in value:
            fh.write(f"{pad}  - {item}\n")
    elif isinstance(value, dict):
        fh.write(f"{pad}{key}:\n")
        for inner_key, inner in value.items():
            _write_yaml_value(fh, inner_key, inner, indent + 2)
    else:
        raise TypeError(type(value))


def _write_users_yaml(path: Path, rows: int, width: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        fh.write("users:\n")
        for i in range(rows):
            row = user_at(i, width)
            fh.write(f"- id: {row['id']}\n")
            for key, value in row.items():
                if key == "id":
                    continue
                _write_yaml_value(fh, key, value, 2)


def _write_ndjson(path: Path, rows: int, width: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        for i in range(rows):
            json.dump(user_at(i, width), fh, separators=(",", ":"))
            fh.write("\n")


def _toml_value(value) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, list):
        return "[" + ", ".join(_toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        inner = ", ".join(f"{key} = {_toml_value(inner)}" for key, inner in value.items())
        return "{ " + inner + " }"
    raise TypeError(type(value))


def _write_users_toml(path: Path, rows: int, width: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        for i in range(rows):
            row = user_at(i, width)
            fh.write("[[users]]\n")
            for key, value in row.items():
                fh.write(f"{key} = {_toml_value(value)}\n")
            fh.write("\n")


def _csv_row(row: dict) -> dict:
    return {key: value for key, value in row.items() if not isinstance(value, (list, dict))}


def _write_users_csv(path: Path, rows: int, width: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    first = _csv_row(user_at(0, width))
    with path.open("w", encoding="utf-8", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=list(first))
        writer.writeheader()
        if rows:
            writer.writerow(first)
            writer.writerows(_csv_row(user_at(i, width)) for i in range(1, rows))


def generate(kind: str, width: str, rows: int, dest: Path) -> None:
    if kind == "users":
        _write_users_json(dest, rows, width)
    elif kind == "yaml":
        _write_users_yaml(dest, rows, width)
    elif kind == "ndjson":
        _write_ndjson(dest, rows, width)
    elif kind == "toml":
        _write_users_toml(dest, rows, width)
    elif kind == "csv":
        _write_users_csv(dest, rows, width)
    else:
        raise ValueError(f"unknown dataset kind {kind!r}")
