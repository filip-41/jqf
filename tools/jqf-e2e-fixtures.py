#!/usr/bin/env python3
"""Deterministic fixture generator for tools/jqf-e2e-ladder.sh.

Most fixtures are generated from a pure function of their index — no
randomness, no timestamps, no host-dependent state — so those output bytes
are stable across machines and runs. The BINARY fixtures are the exception:
`cbor-catalog.bin` (when python3 has no `cbor2`), `jqfb-pool-large.jqfb`, and
`messagepack-catalog.bin` are encoded by a jqf binary at generation time
(`_find_jqf`: `JQF_BIN`, then a `jqf` on PATH, then the repo's release build),
so generating them needs such a binary and their bytes depend on that
encoder's version. `GEN_VERSION` is the cache key: bump it whenever a
generation law below changes so a stale cache directory is never silently
reused with fixtures that no longer match this file.

Usage: jqf-e2e-fixtures.py <fixture-dir>

Exits 0 and (re)writes every fixture if the cache directory is missing,
incomplete, or stamped with a different `GEN_VERSION`. Exits 0 and does
nothing if a matching, complete cache is already present.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

from jqfgate import proc

# Hermeticity (plan 064): a developer's .jqf.toml must never reach a gate.
proc.set_hermetic()

GEN_VERSION = "e2e-fixtures-v8"

CATALOG_ITEMS = 36_000
NDJSON_RECORDS = 200_000
DEEP_DEPTH = 500
SMALL_ITEMS = 108  # lands the slice at ~29KB

COLORS = [
    "red", "blue", "green", "black", "white",
    "silver", "gold", "navy", "charcoal", "ivory",
]
WAREHOUSES = ["W1", "W2", "W3", "W4", "W5", "W6", "W7", "W8"]

FIXTURE_NAMES = [
    "catalog-10mb.json",
    "escape-10mb.json",
    "deep-500.json",
    "ndjson-200k.ndjson",
    "ndjson-tiny-400k.ndjson",
    "small-29kb.json",
    "corrupt-late.json",
    "toml-catalog-10mb.toml",
    "toml-deep-table.toml",
    "yaml-catalog-10mb.yaml",
    "xml-catalog-10mb.xml",
    "cbor-catalog.bin",
    "yaml-comment-dense.yaml",
    "toml-comment-dense.toml",
    "spill-300k.ndjson",
    "spill-fat-200k.ndjson",
    # 125 H1: fat NDJSON records for the worker ambient ceiling lane (twelve
    # records of ~32 KiB each — see the writer's docstring for the rationale).
    "h1-ambient-ndjson.ndjson",
    # Plan 118 V1c: the fixtures the non-JSON codec audits say nothing
    # exhibits. Each is the fixture a V3 package ships with its asymptotic
    # fix (the new-fixture law): the perf-gate csv-projected lane's 40-column
    # table, a comment-dense YAML large enough for the O(comments x nodes)
    # curve, a wide inline-table TOML (the O(n^2) duplicate check, V3g), an
    # HTML with many sibling elements (the O(E^2) encode, V3a), a jqfb image
    # with a large string pool (the Theta(N^2) pool walk, V3b), and a
    # many-container document for the render tree's O(C^2) slot scan (V3f).
    "csv-wide-40col.csv",
    "csv-headered-40col.csv",
    "toml-wide-inline.toml",
    "html-many-siblings.html",
    "jqfb-pool-large.jqfb",
    "render-many-containers.json",
    # Plan 118 V3c: deep-chain XML/HTML exhibiting the per-ancestor
    # descendant-text re-walk (O(text x depth) CPU) the bottom-up
    # accumulation removes. The existing xml/html fixtures are FLAT (depth
    # 2-3), so none of them shows the curve; these do.
    "xml-deep-chain.xml",
    "html-deep-chain.html",
    # Plan 164 X0: A/B lanes the later codec waves need. Existing names
    # above are reused; these are the generators that were missing.
    "messagepack-catalog.bin",
    "ini-dense.ini",
    "json-long-string.json",
    "yaml-multi-doc.yaml",
    "yaml-bail-comment-head.yaml",
    "yaml-bail-comment-mid.yaml",
    "yaml-bail-comment-tail.yaml",
    "yaml-bail-anchor-head.yaml",
    "yaml-bail-anchor-mid.yaml",
    "yaml-bail-anchor-tail.yaml",
    "yaml-bail-tag-head.yaml",
    "yaml-bail-tag-mid.yaml",
    "yaml-bail-tag-tail.yaml",
]

VERSION_MARKER = ".gen-version"


def catalog_item(index: int) -> dict:
    """One catalog record: integers and strings only, per the ladder spec."""
    return {
        "id": index,
        "name": f"item-{index:06d}",
        "sku": f"SKU-{index:08d}",
        "tags": [f"tag-{(index + offset) % 200:03d}" for offset in range(12)],
        "stock": index % 5000,
        "attrs": {
            "color": COLORS[index % len(COLORS)],
            "warehouse": WAREHOUSES[index % len(WAREHOUSES)],
            "weight": index % 500,
            "aisle": (index % 40) + 1,
            "bin": f"B{index % 999:03d}",
        },
    }


def write_catalog(path: Path, count: int) -> None:
    with path.open("w", encoding="ascii") as handle:
        handle.write('{"meta":{"count":%d},"catalog":[' % count)
        for index in range(count):
            if index:
                handle.write(",")
            handle.write(json.dumps(catalog_item(index), separators=(",", ":")))
        handle.write("]}")


def escape_description(index: int) -> str:
    """A JSON string body mixing `\\"` `\\\\` `\\n` `\\uXXXX` escapes with raw
    non-ASCII UTF-8 bytes, written directly (not via json.dumps) so the
    escape sequences land on disk exactly as literal characters rather than
    being re-escaped."""
    return (
        f'Item {index} \\"premium\\" batch\\\\lot #{index % 97}\\n'
        f"notes: café naïve résumé "
        f"\\u4e2d\\u6587\\u6d4b\\u8bd5 \\ud83d\\ude00 "
        f"path C:\\\\Users\\\\{index}\\\\file.txt tab:\\tend-{index:05d}"
    )


def write_escape_catalog(path: Path, count: int) -> None:
    with path.open("w", encoding="utf-8") as handle:
        handle.write('{"meta":{"count":%d},"catalog":[' % count)
        for index in range(count):
            if index:
                handle.write(",")
            handle.write(
                '{"id":%d,"name":"item-%06d","sku":"SKU-%08d","stock":%d,'
                '"description":"%s"}'
                % (index, index, index, index % 5000, escape_description(index))
            )
        handle.write("]}")
    # Every record must independently round-trip through a real JSON
    # decoder before this fixture is trusted for the ladder's lanes.
    with path.open("r", encoding="utf-8") as handle:
        json.load(handle)


def write_deep(path: Path, depth: int) -> None:
    path.write_text("[" * depth + "0" + "]" * depth, encoding="ascii")


def write_h1_ambient_ndjson(path: Path) -> None:
    """125 H1: fat NDJSON records for the worker ambient ceiling lane.

    Twelve records of ~32 KiB each (~400 KB total): large enough that the
    parallel lane engages (two workers, morsels above the 128 KiB floor) yet
    small enough that a regression that SAILS PAST the ceiling completes in
    well under a second instead of timing out over the 200 k-record fixtures —
    a fast wrong-exit failure, not a 300-second hang.
    """
    with path.open("w", encoding="ascii") as handle:
        for index in range(12):
            handle.write('{"payload":"%s","id":%d}\n' % ("x" * (32 * 1024), index))


def write_tiny_ndjson(path: Path, count: int) -> None:
    """Tiny 2-node records: the decode FLOOR lane (L12) is fixed-cost-bound,
    so the record count matters and the byte count does not."""
    with path.open("w", encoding="ascii") as handle:
        for index in range(count):
            handle.write('{"a":{"v":%d}}\n' % index)


def write_ndjson(path: Path, count: int) -> None:
    with path.open("w", encoding="ascii") as handle:
        for index in range(count):
            handle.write('{"v":%d,"name":"n%d"}\n' % (index, index))


def plant_corruption(source: Path, dest: Path) -> None:
    """Copy `source` to `dest` with a single structural break planted just
    outside a string, near the 90% byte offset: the `:` after a
    `"warehouse"` key becomes `{`, which breaks the grammar without
    touching any string content."""
    data = source.read_bytes()
    target_offset = int(len(data) * 0.9)
    needle = b'"warehouse":'

    idx = data.find(needle, target_offset)
    if idx == -1:
        idx = data.rfind(needle, 0, target_offset)
    if idx == -1:
        raise RuntimeError('no "warehouse": occurrence found to corrupt')

    colon_pos = idx + len(needle) - 1
    if data[colon_pos : colon_pos + 1] != b":":
        raise RuntimeError(
            f"expected ':' at offset {colon_pos}, found {data[colon_pos : colon_pos + 1]!r}"
        )

    corrupted = data[:colon_pos] + b"{" + data[colon_pos + 1 :]
    dest.write_bytes(corrupted)


def fixtures_fresh(fixdir: Path) -> bool:
    marker = fixdir / VERSION_MARKER
    if not marker.exists() or marker.read_text().strip() != GEN_VERSION:
        return False
    return all((fixdir / name).exists() for name in FIXTURE_NAMES)


TOML_CATALOG_ITEMS = 100_000
TOML_DEEP_DEPTH = 500

# The V3c deep-chain fixtures: chains of nested elements whose per-ancestor
# descendant-text RE-walk (the O(text x depth) recomputation) is the defect
# being exhibited. Each chain is DEEP so the quadratic re-walk dominates the
# linear parse+build at the fixture's size, and there are a handful of chains
# so the 4x measurement regenerates locally at depth 4x (still under the
# 10000 `enter_nesting` ceiling) rather than by scaling chains.
XML_DEEP_CHAINS = 40
XML_DEEP_DEPTH = 2000
HTML_DEEP_CHAINS = 40
HTML_DEEP_DEPTH = 2000


def write_yaml_catalog(path: Path, count: int) -> None:
    """A large YAML block-mapping document: a `catalog` sequence of records.

    The YAML twin of `toml-catalog-10mb.toml` — same record shape, same
    ~100 bytes per element (quoted strings and plain scalars), so 100,000
    elements land near 10 MB and the YAML RSS lanes are comparable to their
    JSON/TOML siblings. Plain scalars only (no tags, no anchors), so the
    routes being measured are the ordinary decode routes.
    """
    with path.open("w", encoding="ascii") as handle:
        handle.write("catalog:\n")
        for index in range(count):
            handle.write(
                "  - id: " + str(index) + "\n"
                f'    name: "item-{index:06d}"\n'
                f'    color: "{COLORS[index % len(COLORS)]}"\n'
                f'    warehouse: "{WAREHOUSES[index % len(WAREHOUSES)]}"\n'
                f"    stock: {index % 5000}\n"
            )


def write_xml_catalog(path: Path, count: int) -> None:
    """A large XML document: a `catalog` element of `item` elements.

    The XML twin of `yaml-catalog-10mb.yaml` — same record shape carried as
    attributes (~100 bytes per item), so 100,000 items land near 10 MB and
    the XML RSS lanes are comparable to their siblings. The projection makes
    the document element an ARRAY of its children, so the count gate
    (`length`) runs the measure route that never builds the tree.
    """
    with path.open("w", encoding="ascii") as handle:
        handle.write("<catalog>")
        for index in range(count):
            handle.write(
                f'<item id="{index}" name="item-{index:06d}" '
                f'color="{COLORS[index % len(COLORS)]}" '
                f'warehouse="{WAREHOUSES[index % len(WAREHOUSES)]}" '
                f'stock="{index % 5000}"/>'
            )
        handle.write("</catalog>")


def write_toml_catalog(path: Path, count: int) -> None:
    """A large TOML array-of-tables document, the record shape's TOML twin.

    ~100 bytes per element (scalars and quoted strings only), so 100,000
    elements land near 10 MB — the same order as `catalog-10mb.json`, which
    keeps the whole-document TOML lane comparable to its JSON sibling.
    """
    with path.open("w", encoding="ascii") as handle:
        handle.write('title = "catalog"\n\n')
        for index in range(count):
            handle.write(
                "[[catalog]]\n"
                f"id = {index}\n"
                f'name = "item-{index:06d}"\n'
                f'color = "{COLORS[index % len(COLORS)]}"\n'
                f'warehouse = "{WAREHOUSES[index % len(WAREHOUSES)]}"\n'
                f"stock = {index % 5000}\n\n"
            )


def write_toml_deep_table(path: Path, depth: int) -> None:
    """A TOML table chain `[t1]`, `[t1.t2]`, ... down to `[t1...t{depth}]`.

    Each header's dotted path is longer than the last, so the flat
    table-definition state machine resolves O(depth^2) path components and the
    built tree is one table nested `depth` deep — the deep-table RSS lane.
    """
    with path.open("w", encoding="ascii") as handle:
        for level in range(1, depth + 1):
            handle.write("[" + ".".join(f"t{i}" for i in range(1, level + 1)) + "]\n")
        handle.write('leaf = "x"\n')


# Plan 118 V1c: the comment-dense fixtures were enlarged 10x (2,000 ->
# 20,000 records, ~6,000 -> ~60,000 commented keys) so the YAML comment
# association curve (V3e, O(comments x nodes)) and the comment-fact memory
# lanes are visible at scale. The 2,000-record fixture's ~1.2e7 comment x node
# pairs were sub-millisecond — exactly the shape that hides the defect. The
# RSS gate's yaml-comments/toml-comments family was re-pinned with the
# enlargement (cause named at the re-pin).
COMMENT_CATALOG_ITEMS = 20_000

# Plan 118 V1c: the wide-inline-table TOML (V3g). One top-level inline table
# with 100,000 keys: the duplicate check is O(n^2/2) String compares per
# inline table, so the curve shows on the WIDTH of a single table (1x/4x =
# 100k/400k keys) rather than on record count. ~1.1 MB.
INLINE_TABLE_KEYS = 100_000

# Plan 118 V1c: HTML with many sibling elements (V3a). 100,000 sibling
# <p> elements: the encoder opens a fresh FactDemand::All reader per element
# (read_name_fact/read_attrs_fact/read_comments_fact), so the encode cost is
# O(elements x facts) ~ O(E^2). ~1.6 MB.
HTML_SIBLING_ELEMENTS = 100_000

# Plan 118 V1c: the jqfb image with a large string pool (V3b). 100,000
# distinct strings: pool_bytes restarts the pool scan from offset 8 per index,
# so decode is Theta(N^2) over the pool. Generated by jqf itself
# (--output-format jqfb) from a JSON array of distinct strings, the same
# self-encoding path the CBOR fixture takes.
JQFB_POOL_STRINGS = 100_000

# Plan 118 V1c: the render tree's O(C^2) slot scan (V3f). An array of 100,000
# sibling objects (one container per item): the sharing prepass's linear
# slot scan runs per container regardless of sharing, so any many-container
# document exhibits the curve; a V3f package that also wants the SHARED case
# pairs this document with a sharing program (e.g. `. as $x | [$x, $x]`, whose
# try_clone is a refcount bump). ~2.2 MB.
RENDER_CONTAINERS = 100_000

# Plan 118 V1c: the 40-column CSV (the perf-gate csv-projected lane, V5b).
# Widest today is 20 and there was no projection lane at all. 50,000 rows x
# 40 columns of short values lands ~14 MB. Headerless by design: the record
# route's csv.rfc4180@1 dialect reads every row as a record, so a header row
# would publish as record 0 and pollute the projection lane's output.
CSV_WIDE_ROWS = 50_000
CSV_WIDE_COLS = 40

# Plan 164 X0: dense INI (~thousands of unique `[section]` headers). 10,000
# sections at five short keys each land near 1 MB; duplicate headers are a
# terminal INI failure, so names are unique.
INI_SECTIONS = 10_000

# Plan 164 X0: one JSON document whose root is a 4 MiB string (inside the
# 2-8 MiB window). Mostly ASCII, three JSON escapes at fixed offsets.
LONG_STRING_BYTES = 4 * 1024 * 1024

# Plan 164 X0: YAML multi-document stream. 100 catalog-shaped documents of
# 200 items each (~2 MB). The CLI sequence drive publishes one value per
# document; the whole-document access session rejects a second.
YAML_MULTI_DOCS = 100
YAML_MULTI_ITEMS = 200

# Plan 164 X0: YAML bail fixtures. Small-to-medium catalog-shaped documents
# with one comment / anchor / tag at head, mid, or tail. 2,000 items keep
# them well under the 10 MB catalog twin.
YAML_BAIL_ITEMS = 2_000

# The spill gate's input: descending adjacent numbers, one per line. 300,000
# keys at ~7 bytes each land near 2.1 MB — a dataset an external sort must
# spill under a 128 KiB per-run budget (the W3 gate lanes' shape), and small
# enough that both the ceiling-refusal lane and the mode/signal lane run in a
# second or two.
SPILL_INPUT_COUNT = 300_000

# The small-key/large-object spill lane's fixture: 200,000 records, each a
# small numeric sort key plus a ~160-byte payload string. The key estimate is
# tiny next to the record, which is the shape that defeated the old
# key-only spill meter — the subject entries dominate the collection's
# growth while the keys never crossed a sane budget.
SPILL_FAT_COUNT = 200_000
SPILL_FAT_BLOB = "b" * 160


def write_spill_fat(path: Path, count: int) -> None:
    """Small-key/fat-payload records, descending by key — the repro class.

    `jqf -s -c --max-spill-bytes N 'sort_by(.ms)'` over this fixture is the
    small-key/large-object shape: each `.ms` key estimates ~24 bytes while
    its record carries a 160-byte payload, so the collection's residency is
    dominated by the buffered entries, not the keys. Descending keys keep a
    wrong order byte-visible, exactly like `write_spill_input`.
    """
    with path.open("w", encoding="ascii") as handle:
        for index in range(count, 0, -1):
            handle.write('{"ms":%d,"blob":"%s"}\n' % (index, SPILL_FAT_BLOB))


def write_spill_input(path: Path, count: int) -> None:
    """Descending adjacent numbers, one per line — the spill gate's input.

    `jqf -s --max-spill-bytes N 'sort_by([.])'` over this fixture both
    reorders the values and spills: the keys are scalars (the closed table),
    the run budget is tiny relative to the dataset, and the general-drive key
    graph (a construct-array, not a bare key) is the shape that takes the
    spill path. The numbers are DESCENDING so a wrong order is byte-visible.
    """
    with path.open("w", encoding="ascii") as handle:
        for index in range(count, 0, -1):
            handle.write(str(index))
            handle.write("\n")



def write_yaml_comment_dense(path: Path, count: int) -> None:
    """A comment-dense YAML catalog, the comment-fact RSS lanes' fixture.

    A `catalog` sequence of records where every key carries a leading or
    trailing comment, so the whole-document decode attaches one
    `yaml.comment@1` list-payload fact to nearly every value node. `count`
    records at three keys each land at ~6000 commented keys — "a few
    thousand keys", per plan 050 item 1. The `--comment-dense` fixtures are
    deliberately far smaller than the 10 MB catalogs: the lane exists to pin
    the comment-fact memory, and the record/comment count is the axis that
    matters, not raw bytes.
    """
    with path.open("w", encoding="ascii") as handle:
        handle.write("catalog:\n")
        for index in range(count):
            handle.write(
                f"  - # record {index}: the {index}th stock keeping unit in the sample catalog\n"
            )
            handle.write(f"    # numeric identifier unique within this catalog\n")
            handle.write(f"    id: {index}\n")
            handle.write(f"    # display name used across pick lists and invoices\n")
            handle.write(f'    name: "item-{index:06d}"\n')
            handle.write(
                f"    stock: {index % 5000} # on-hand count in the owning warehouse\n"
            )


def write_toml_comment_dense(path: Path, count: int) -> None:
    """The TOML twin of `yaml-comment-dense.yaml`: an array-of-tables catalog
    whose every key carries a leading or trailing comment, so the
    whole-document decode attaches one `toml.comment@1` fact per commented
    statement. Same record shape and comment text as the YAML fixture, so the
    two lanes are comparable.
    """
    with path.open("w", encoding="ascii") as handle:
        handle.write('title = "catalog"\n\n')
        for index in range(count):
            handle.write("[[catalog]]\n")
            handle.write(
                f"# record {index}: the {index}th stock keeping unit in the sample catalog\n"
            )
            handle.write(f"# numeric identifier unique within this catalog\n")
            handle.write(f"id = {index}\n")
            handle.write(f"# display name used across pick lists and invoices\n")
            handle.write(f'name = "item-{index:06d}"\n')
            handle.write(
                f"stock = {index % 5000} # on-hand count in the owning warehouse\n\n"
            )


def write_csv_wide(path: Path, rows: int, cols: int) -> None:
    """A wide headerless CSV: `rows` data rows of `cols` short fields.

    The perf-gate csv-projected lane (`[.[0], .[1]]`) and V5b's prune-tree
    work read this table; every row is a record under csv.rfc4180@1, and the
    fields are `v{col}_{row}` so any two-column projection is byte-distinct
    from the full row.
    """
    with path.open("w", encoding="ascii") as handle:
        for row in range(rows):
            handle.write(",".join(f"v{col}_{row}" for col in range(cols)))
            handle.write("\n")


def write_csv_headered(path: Path, rows: int, cols: int) -> None:
    """A wide HEADERED CSV: `rows` data rows of `cols` short fields.

    Plan 118 V5b's prune-tree fixture: the headered dialect publishes every
    row as an OBJECT keyed by the header names, so a program like `{name,
    age}` reads two named members and the decode may OMIT the other 38. The
    first two columns are `name`/`age` (the user's standing law: `{name,
    age}` over 40 columns must not materialize the other 38); the remaining
    columns are `c2..c39`. The array-dialect sibling `csv-wide-40col.csv`
    exists for the pinned perf-gate lane, whose `[.[0], .[1]]` shape the
    prune tree deliberately cannot express (static indices keep arrays
    whole).
    """
    header_cols = [f"c{i}" for i in range(cols)]
    header_cols[0], header_cols[1] = "name", "age"
    with path.open("w", encoding="ascii") as handle:
        handle.write(",".join(header_cols))
        handle.write("\n")
        for row in range(rows):
            handle.write(",".join(f"v{i}_{row}" for i in range(cols)))
            handle.write("\n")


def write_toml_wide_inline(path: Path, keys: int) -> None:
    """One top-level inline table with `keys` members.

    The O(n^2/2) duplicate check (V3g) is per inline table, so a single very
    wide table exhibits the curve on its width; the keys are
    `k00000 = 0 ... k99999 = 99999` (no quotes, no escapes, all short).
    """
    with path.open("w", encoding="ascii") as handle:
        handle.write("wide = {")
        for index in range(keys):
            if index:
                handle.write(", ")
            handle.write(f"k{index:05d} = {index}")
        handle.write("}\n")


def write_html_many_siblings(path: Path, count: int) -> None:
    """An HTML document of `count` sibling paragraph elements.

    The O(E^2) encode (V3a) opens a whole-fact-table reader per element, so
    the sibling count is the axis that matters; each element carries its own
    text child so the document round-trips through the WHATWG parser.
    """
    with path.open("w", encoding="ascii") as handle:
        handle.write("<!doctype html><html><body>")
        for index in range(count):
            handle.write(f"<p>item-{index}</p>")
        handle.write("</body></html>\n")


def write_xml_deep_chain(path: Path, chains: int, depth: int) -> None:
    """An XML document of `chains` nested chains, each `depth` elements deep.

    The V3c exhibit: the whole-document route's `build_node` used to
    recompute each element's descendant text by re-walking its whole subtree
    at every ancestor, so a chain pays O(depth^2) visits; the bottom-up
    accumulation makes it O(depth). A chain is the only shape where depth
    grows without exploding node count, so the curve is visible at ~1 MB.
    Each chain carries one small text leaf so the `.@content` facts are real.
    """
    with path.open("w", encoding="ascii") as handle:
        handle.write("<root>")
        for chain in range(chains):
            handle.write("<chain>" + "<d>" * depth + f"leaf-{chain}" + "</d>" * depth + "</chain>")
        handle.write("</root>")


def write_html_deep_chain(path: Path, chains: int, depth: int) -> None:
    """An HTML document of `chains` nested div chains, `depth` elements deep.

    The HTML twin of `xml-deep-chain.xml` (V3c): deep nesting of legal
    WHATWG elements, each chain with one text leaf, so the descendant-text
    re-walk is the dominant cost of the whole-document decode.
    """
    with path.open("w", encoding="ascii") as handle:
        handle.write("<!doctype html><html><body>")
        for chain in range(chains):
            handle.write("<div>" * depth + f"leaf-{chain}" + "</div>" * depth)
        handle.write("</body></html>\n")


def write_render_many_containers(path: Path, count: int) -> None:
    """An array of `count` small sibling objects.

    The render tree's O(C^2) slot scan (V3f) is over the container count in
    the sharing prepass and at emit; render is an OUTPUT-only format, so the
    fixture is the document a `--output-format render` run consumes.
    """
    with path.open("w", encoding="ascii") as handle:
        handle.write("[")
        for index in range(count):
            if index:
                handle.write(",")
            handle.write(f'{{"a":{index},"b":"item-{index:06d}"}}')
        handle.write("]\n")


def write_jqfb_pool(path: Path, count: int) -> None:
    """A jqfb image whose string pool holds `count` distinct strings.

    The Theta(N^2) pool walk (V3b) restarts the pool scan from offset 8 per
    index, so the pool size is the axis. Generated by jqf itself from a JSON
    array of distinct strings (the CBOR fixture's self-encoding path).
    """
    import subprocess

    jqf = _find_jqf()
    if not jqf:
        raise RuntimeError(
            "jqfb fixture needs jqf to encode; set JQF_BIN or build "
            "target/release/jqf first"
        )
    document = "[" + ",".join(
        f'"item-{index:08d}"' for index in range(count)
    ) + "]"
    with path.open("wb") as out:
        # `run_command`, not `run_gate`: the fixture generator must serve the
        # binary it is given (JQF_BIN may name a pre-fix build for a teeth
        # probe or a verify-prefix run), so the gate-only freshness law does
        # not apply here.
        result = proc.run_command(
            [jqf, "--input-format", "json", "--output-format", "jqfb", "-c", "."],
            input=document.encode("ascii"),
            stdout=out,
            stderr=subprocess.PIPE,
            check=False,
        )
    if result.returncode != 0:
        raise RuntimeError(
            f"jqf {jqf} failed to encode jqfb fixture: "
            + result.stderr.decode("utf-8", "replace")[:200]
        )


def _find_jqf() -> str | None:
    """Locate a jqf binary for CBOR fixture generation, or None.

    Prefers `JQF_BIN` (the same override the ladder scripts honour), then a
    `jqf` on PATH, then the repo's plain release binary. The caller builds it
    before invoking this generator (the ladder scripts do), so `make fixtures`
    alone may need `JQF_BIN` set.
    """
    import os
    import shutil

    candidate = os.environ.get("JQF_BIN")
    if candidate:
        return candidate
    on_path = shutil.which("jqf")
    if on_path:
        return on_path
    repo_release = Path(proc.DEFAULT_RELEASE_JQF)
    if repo_release.is_file() and os.access(repo_release, os.X_OK):
        return str(repo_release)
    return None


def write_messagepack_catalog(path: Path, catalog: Path) -> None:
    """The MessagePack twin of `catalog-10mb.json`.

    Encoded by jqf itself (`--output-format messagepack`) at fixture-gen
    time, the same self-encoding path `write_cbor_catalog` takes when
    python3 has no `cbor2`. MessagePack has no third-party encoder in this
    generator.
    """
    import subprocess

    jqf = _find_jqf()
    if not jqf:
        raise RuntimeError(
            "no jqf binary was found to encode the MessagePack fixture; "
            "set JQF_BIN or build target/release/jqf first"
        )
    with catalog.open("r", encoding="ascii") as handle, path.open("wb") as out:
        result = proc.run_command(
            [
                jqf,
                "--input-format",
                "json",
                "--output-format",
                "messagepack",
                "-c",
                ".",
            ],
            stdin=handle,
            stdout=out,
            stderr=subprocess.PIPE,
            check=False,
        )
    if result.returncode != 0:
        raise RuntimeError(
            f"jqf {jqf} failed to encode MessagePack fixture: "
            + result.stderr.decode("utf-8", "replace")[:200]
        )


def write_ini_dense(path: Path, sections: int) -> None:
    """A dense INI document of `sections` unique `[section]` headers.

    Each section holds five short keys (id/name/color/warehouse/stock).
    Headers are unique: a repeated `[section]` is a terminal INI failure.
    """
    with path.open("w", encoding="ascii") as handle:
        for index in range(sections):
            handle.write(
                f"[section-{index:05d}]\n"
                f"id = {index}\n"
                f"name = item-{index:05d}\n"
                f"color = {COLORS[index % len(COLORS)]}\n"
                f"warehouse = {WAREHOUSES[index % len(WAREHOUSES)]}\n"
                f"stock = {index % 5000}\n\n"
            )


def write_json_long_string(path: Path, payload_bytes: int) -> None:
    """One JSON document whose root is a `payload_bytes`-long string.

    Mostly ASCII `x`, with three JSON escapes at fixed decoded offsets so
    the string_step sees both a clean run and the escape machine: a quote
    at 1024, a backslash at midpoint, a newline 2048 bytes from the end.
    The file is a single quoted JSON string, no wrapping object.
    """
    if payload_bytes < 4096:
        raise RuntimeError("long-string fixture needs at least 4 KiB")
    mid = payload_bytes // 2
    near = payload_bytes - 2048
    with path.open("w", encoding="ascii") as handle:
        handle.write('"')
        handle.write("x" * 1024)
        handle.write('\\"')
        handle.write("x" * (mid - 1025))
        handle.write("\\\\")
        handle.write("x" * (near - mid - 1))
        handle.write("\\n")
        handle.write("x" * (payload_bytes - near - 1))
        handle.write('"')
    with path.open("r", encoding="ascii") as handle:
        loaded = json.load(handle)
    if not isinstance(loaded, str) or len(loaded) != payload_bytes:
        raise RuntimeError(
            f"long-string fixture decoded to {type(loaded).__name__} "
            f"len={len(loaded) if isinstance(loaded, str) else 'n/a'}, "
            f"expected str len={payload_bytes}"
        )


def _emit_yaml_catalog_item(handle, index: int, mark: str | None = None) -> None:
    """One yaml-catalog record. `mark` is 'anchor' or 'tag' on this item."""
    if mark == "anchor":
        handle.write("  - &jqf_bail\n")
        prefix = "    "
        handle.write(f"{prefix}id: {index}\n")
    elif mark == "tag":
        handle.write("  - !jqf-bail\n")
        prefix = "    "
        handle.write(f"{prefix}id: {index}\n")
    else:
        handle.write("  - id: " + str(index) + "\n")
        prefix = "    "
    handle.write(
        f'{prefix}name: "item-{index:06d}"\n'
        f'{prefix}color: "{COLORS[index % len(COLORS)]}"\n'
        f'{prefix}warehouse: "{WAREHOUSES[index % len(WAREHOUSES)]}"\n'
        f"{prefix}stock: {index % 5000}\n"
    )


def write_yaml_multi_doc(path: Path, docs: int, items: int) -> None:
    """A `---`-separated YAML stream of `docs` catalog-shaped documents.

    Each document carries a `doc` ordinal and a `catalog` sequence of
    `items` records, the same record shape as `yaml-catalog-10mb.yaml`.
    The CLI sequence drive (`--input-format yaml .`) publishes one value
    per document; the whole-document access session rejects a second
    (`UnsupportedRepresentation`).
    """
    with path.open("w", encoding="ascii") as handle:
        for doc in range(docs):
            handle.write("---\n")
            handle.write(f"doc: {doc}\n")
            handle.write("catalog:\n")
            for index in range(items):
                _emit_yaml_catalog_item(handle, index)


def write_yaml_bail(path: Path, kind: str, position: str, count: int) -> None:
    """A yaml-catalog-shaped document with one graph-forcing construct.

    `kind` is comment, anchor, or tag. `position` is head, mid, or tail.
    Everything else is the plain catalog the graph-skipping fast path
    would accept, so a later wave's bail receipt has a deterministic
    reason to drop to the graph route.
    """
    if kind not in ("comment", "anchor", "tag"):
        raise RuntimeError(f"unknown yaml bail kind {kind!r}")
    if position not in ("head", "mid", "tail"):
        raise RuntimeError(f"unknown yaml bail position {position!r}")
    mid = count // 2
    with path.open("w", encoding="ascii") as handle:
        if kind == "comment" and position == "head":
            handle.write("# jqf-bail-comment-head\n")
        if kind == "anchor" and position == "head":
            handle.write("catalog: &jqf_bail\n")
        elif kind == "tag" and position == "head":
            handle.write("catalog: !jqf-bail\n")
        else:
            handle.write("catalog:\n")
        for index in range(count):
            inject = (position == "mid" and index == mid) or (
                position == "tail" and index == count - 1
            )
            if kind == "comment" and position == "mid" and index == mid:
                handle.write("  # jqf-bail-comment-mid\n")
            mark = None
            if inject and kind in ("anchor", "tag"):
                mark = kind
            _emit_yaml_catalog_item(handle, index, mark=mark)
        if kind == "comment" and position == "tail":
            handle.write("# jqf-bail-comment-tail\n")


def write_cbor_catalog(path: Path, catalog: Path) -> None:
    """The CBOR twin of `catalog-10mb.json`: the same catalog, CBOR-encoded.

    Generated from the same catalog JSON so the CBOR identity lanes (B1/B2 in
    `jqf-cross-format-ladder.sh`) are byte-comparable to their JSON/YAML/TOML/
    XML siblings. When python3 has `cbor2` the fixture is encoded by cbor2;
    otherwise it is encoded by jqf itself (`--output-format cbor`), which
    makes the B-lane oracle jqf-self and therefore weaker — a symmetric
    encode/decode bug could not be seen. Recorded in .plans/048.
    """
    import subprocess

    try:
        import cbor2

        with catalog.open("r", encoding="ascii") as handle:
            document = json.load(handle)
        path.write_bytes(cbor2.dumps(document))
        return
    except ImportError:
        pass

    jqf = _find_jqf()
    if not jqf:
        raise RuntimeError(
            "cbor2 is not installed and no jqf binary was found; "
            "set JQF_BIN or build target/release/jqf first"
        )
    with catalog.open("r", encoding="ascii") as handle, path.open("wb") as out:
        # `run_command`, not `run_gate`: same JQF_BIN override law as the
        # jqfb encode above.
        result = proc.run_command(
            [jqf, "--input-format", "json", "--output-format", "cbor", "-c", "."],
            stdin=handle,
            stdout=out,
            stderr=subprocess.PIPE,
            check=False,
        )
    if result.returncode != 0:
        raise RuntimeError(
            f"jqf {jqf} failed to encode CBOR fixture: "
            + result.stderr.decode("utf-8", "replace")[:200]
        )


def main() -> int:
    if len(sys.argv) != 2 or sys.argv[1] in ("-h", "--help"):
        print("usage: jqf-e2e-fixtures.py <fixture-dir>", file=sys.stderr)
        return 2

    fixdir = Path(sys.argv[1])
    fixdir.mkdir(parents=True, exist_ok=True)

    if fixtures_fresh(fixdir):
        print(f"jqf-e2e-fixtures: reusing cached fixtures in {fixdir}", file=sys.stderr)
        return 0

    print(f"jqf-e2e-fixtures: generating fixtures into {fixdir}", file=sys.stderr)

    catalog_path = fixdir / "catalog-10mb.json"
    write_catalog(catalog_path, CATALOG_ITEMS)
    write_escape_catalog(fixdir / "escape-10mb.json", CATALOG_ITEMS)
    write_deep(fixdir / "deep-500.json", DEEP_DEPTH)
    write_ndjson(fixdir / "ndjson-200k.ndjson", NDJSON_RECORDS)
    write_tiny_ndjson(fixdir / "ndjson-tiny-400k.ndjson", NDJSON_RECORDS * 2)
    write_catalog(fixdir / "small-29kb.json", SMALL_ITEMS)
    plant_corruption(catalog_path, fixdir / "corrupt-late.json")
    write_toml_catalog(fixdir / "toml-catalog-10mb.toml", TOML_CATALOG_ITEMS)
    write_toml_deep_table(fixdir / "toml-deep-table.toml", TOML_DEEP_DEPTH)
    write_yaml_catalog(fixdir / "yaml-catalog-10mb.yaml", TOML_CATALOG_ITEMS)
    write_xml_catalog(fixdir / "xml-catalog-10mb.xml", TOML_CATALOG_ITEMS)
    write_cbor_catalog(fixdir / "cbor-catalog.bin", catalog_path)
    write_yaml_comment_dense(fixdir / "yaml-comment-dense.yaml", COMMENT_CATALOG_ITEMS)
    write_toml_comment_dense(fixdir / "toml-comment-dense.toml", COMMENT_CATALOG_ITEMS)
    write_spill_input(fixdir / "spill-300k.ndjson", SPILL_INPUT_COUNT)
    write_spill_fat(fixdir / "spill-fat-200k.ndjson", SPILL_FAT_COUNT)
    write_h1_ambient_ndjson(fixdir / "h1-ambient-ndjson.ndjson")
    # Plan 118 V1c: the V3 fixtures (see the writers' docstrings for the
    # defect each exhibits and the size rationale).
    write_csv_wide(fixdir / "csv-wide-40col.csv", CSV_WIDE_ROWS, CSV_WIDE_COLS)
    write_csv_headered(fixdir / "csv-headered-40col.csv", CSV_WIDE_ROWS, CSV_WIDE_COLS)
    write_toml_wide_inline(fixdir / "toml-wide-inline.toml", INLINE_TABLE_KEYS)
    write_html_many_siblings(fixdir / "html-many-siblings.html", HTML_SIBLING_ELEMENTS)
    write_render_many_containers(fixdir / "render-many-containers.json", RENDER_CONTAINERS)
    write_jqfb_pool(fixdir / "jqfb-pool-large.jqfb", JQFB_POOL_STRINGS)
    # Plan 118 V3c: the deep-chain fixtures (see the writers' docstrings).
    write_xml_deep_chain(fixdir / "xml-deep-chain.xml", XML_DEEP_CHAINS, XML_DEEP_DEPTH)
    write_html_deep_chain(fixdir / "html-deep-chain.html", HTML_DEEP_CHAINS, HTML_DEEP_DEPTH)
    # Plan 164 X0: MessagePack catalog, dense INI, long-string JSON, YAML
    # multi-doc stream, and the nine YAML bail fixtures (comment/anchor/tag
    # at head/mid/tail).
    write_messagepack_catalog(fixdir / "messagepack-catalog.bin", catalog_path)
    write_ini_dense(fixdir / "ini-dense.ini", INI_SECTIONS)
    write_json_long_string(fixdir / "json-long-string.json", LONG_STRING_BYTES)
    write_yaml_multi_doc(
        fixdir / "yaml-multi-doc.yaml", YAML_MULTI_DOCS, YAML_MULTI_ITEMS
    )
    for kind in ("comment", "anchor", "tag"):
        for position in ("head", "mid", "tail"):
            write_yaml_bail(
                fixdir / f"yaml-bail-{kind}-{position}.yaml",
                kind,
                position,
                YAML_BAIL_ITEMS,
            )

    (fixdir / VERSION_MARKER).write_text(GEN_VERSION)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
