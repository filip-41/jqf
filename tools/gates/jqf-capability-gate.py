#!/usr/bin/env python3
"""Job-level capability gate: the CLI either serves the requested format or fails loudly.

A codec arrives into a tree that tests every cell of the input x output
matrix. The CLI had a long tail of seam defects that silently produced the
WRONG FORMAT with exit 0 — the flagship being `--output-format` ignored on
record routes (JSON bytes, exit 0, for a TOML request). Each of these would
have been caught as it landed:

  * every cell of the input x output matrix either produces the requested format
    (exit 0, output that really decodes as that format) or exits nonzero;
  * `--in-place` preserves file mode and symlinks across BOTH write modes
    (atomic default and `--no-atomic`);
  * unknown short flags exit 2 (and program-lookalikes like `-1`,
    `-.5` and `-nan` stay programs, which is the compat half of the same rule).

The matrix is a PINNED expectation table, not a property probe: each (input
format, output format, fixture) cell names serve or fail, so a cell that used to
serve and stopped is a failure, and a cell that used to fail cleanly and now
silently emits bytes is a failure. This is what makes the gate have teeth against
a capability regression, not just against a wrong-format fallback. A new codec
adds its input row (and output column) to `FIXTURES`/`MATRIX` in the same commit
it lands.

Output validation is native where Python has a parser (json/ndjson/toml/csv) and
delegates to jqf's own decoder for the rest, whose only Python parsers would be
a new dependency; `render` is output-only and validates as non-empty. Delegating
to jqf's own decoder still catches the defect class this gate exists for — a
fallback to a DIFFERENT format fails the requested format's decode — and the
native parsers keep the load-bearing cells independent of the binary under test.

Receipt line (shape as printed; the counts are properties of the binary
under test — a codec that lands a new representable cell moves serve and
pass, and the flag-table count tracks the pinned FLAGS table — so the line
the gate prints is the authoritative receipt):

    capability-gate: cells=… serve=… surface=… in-place=… flags=…
    flag-table=… discovery=… hermeticity=… subcommands=… pass=…
    deviations=0 GREEN

Usage:
    tools/gates/jqf-capability-gate.py [path-to-jqf]
"""

import csv
import io
import json
import os
from jqfgate import proc
import stat
import sys
import tempfile

proc.set_hermetic()

ROOT = proc.ROOT


class InputFixture:
    """One input format: its bytes and a pinned serve/fail row over the outputs."""

    def __init__(self, fmt, name, bytes_, row):
        self.fmt = fmt
        self.name = name
        self.bytes = bytes_
        self.row = row  # {output_format: True/False}


OUTPUTS = ["json", "jsonc", "json5", "ndjson", "json-seq", "toml", "csv", "tsv", "cbor", "cbor-seq", "yaml", "jqft", "jqfjson", "jqfb", "render", "xml", "html", "properties", "ini", "dotenv", "messagepack"]

# The pinned matrix. Each input row's serve/fail outcome is a fact about BOTH
# the format pair AND this fixture's value shape: CSV cannot represent a nested
# value, XML cannot represent a fact-less value (a non-XML codec's documents
# carry no element names or attributes, so `UnsupportedRepresentation`, exit
# 5), and the record routes (ndjson/csv input) reject every output except
# json/ndjson/json-seq/csv/tsv/render as a usage error before a byte is read
# (render joined the record route when its registration declared Record +
# AdjacentValues: one frame per record). The reasons are the
# same representability laws the codecs enforce; a codec change that moves a
# cell must say so in the commit. The json fixture's toml cell moved False→True
# on 2026-08-05 with the D1 numbers slice: its `1.5` decodes as an EXACT
# decimal (jqf-data Decimal), which the TOML profile now encodes (it previously
# rejected every Decimal as UnrepresentableSemantic — the defect the slice
# closed).
FIXTURES = [
    InputFixture(
        "json",
        "nested object (object, array, number, string)",
        b'{"name":"ada","id":1,"tags":["a","b"],"attrs":{"x":1.5}}',
        {"json": True, "jsonc": True, "ndjson": True, "toml": True, "csv": False, "tsv": False,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": False},
    ),
    InputFixture(
        "ndjson",
        "two records",
        b'{"id":1,"name":"ada"}\n{"id":2,"name":"linus"}\n',
        {"json": True, "jsonc": False, "ndjson": True, "toml": False, "csv": True, "tsv": True,
         "cbor": False, "cbor-seq": False, "yaml": False, "jqft": False, "jqfjson": False,
         "jqfb": False, "render": True, "xml": False, "json-seq": True, "html": False,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": False},
    ),
    InputFixture(
        "toml",
        "table plus array of tables",
        b'title = "catalog"\n[[item]]\nid = 1\nname = "ada"\n',
        {"json": True, "jsonc": True, "ndjson": True, "toml": True, "csv": False, "tsv": False,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": True},
    ),
    InputFixture(
        "csv",
        "header plus two rows",
        b"id,name\n1,ada\n2,linus\n",
        {"json": True, "jsonc": False, "ndjson": True, "toml": False, "csv": True, "tsv": True,
         "cbor": False, "cbor-seq": False, "yaml": False, "jqft": False, "jqfjson": False,
         "jqfb": False, "render": True, "xml": False, "json-seq": True, "html": False,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": False},
    ),
    InputFixture(
        "tsv",
        "header plus two rows, tab-separated, no quotes",
        b"id\tname\n1\tada\n2\tlinus\n",
        {"json": True, "jsonc": False, "ndjson": True, "toml": False, "csv": True, "tsv": True,
         "cbor": False, "cbor-seq": False, "yaml": False, "jqft": False, "jqfjson": False,
         "jqfb": False, "render": True, "xml": False, "json-seq": True, "html": False,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": False},
    ),
    InputFixture(
        "cbor",
        'flat map {"name":"ada","id":1}',
        b"\xa2\x64name\x63ada\x62id\x01",
        {"json": True, "jsonc": True, "ndjson": True, "toml": True, "csv": True, "tsv": True,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": True, "ini": True, "dotenv": True,
         "messagepack": True},
    ),    InputFixture(
        "cbor-seq",
        'one item: flat map {"name":"ada","id":1} (a one-item RFC 8742 sequence)',
        b"\xa2\x64name\x63ada\x62id\x01",
        {"json": True, "jsonc": True, "ndjson": True, "toml": True, "csv": True, "tsv": True,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": True, "ini": True, "dotenv": True,
         "messagepack": True},
    ),

    InputFixture(
        "yaml",
        "mapping with a sequence",
        b"name: ada\nid: 1\nitems:\n  - a\n  - b\n",
        {"json": True, "jsonc": True, "ndjson": True, "toml": True, "csv": False, "tsv": False,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": True},
    ),
    InputFixture(
        "jqft",
        "a jqft object with a sequence",
        b'%jqft 1\n{name: "ada", id: 1, items: ["a", "b"]}\n',
        {"json": True, "jsonc": True, "ndjson": True, "toml": True, "csv": False, "tsv": False,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": True},
    ),
    InputFixture(
        "jqfjson",
        "a jqfjson object",
        b'{"name":"ada","id":1,"tags":["a","b"]}\n',
        {"json": True, "jsonc": True, "ndjson": True, "toml": True, "csv": False, "tsv": False,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": True},
    ),
    InputFixture(
        "xml",
        "catalog element with an item",
        b'<catalog><item id="1"><name>ada</name></item></catalog>',
        {"json": True, "jsonc": True, "ndjson": True, "toml": False, "csv": False, "tsv": False,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": True},
    ),
    InputFixture(
        "json-seq",
        "two RS-framed records",
        b"\x1e{\"id\":1,\"name\":\"ada\"}\n\x1e{\"id\":2,\"name\":\"linus\"}\n",
        {"json": True, "jsonc": False, "ndjson": True, "toml": False, "csv": True, "tsv": True,
         "cbor": False, "cbor-seq": False, "yaml": False, "jqft": False, "jqfjson": False,
         "jqfb": False, "render": True, "xml": False, "json-seq": True, "html": False,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": False},
    ),
    InputFixture(
        "html",
        "a recovered document with a paragraph",
        b'<!DOCTYPE html><html><head></head><body><p>hi</p></body></html>',
        # The doctype-bearing document REFUSES html.document-serialize@1
        # output (a serialized document element re-decodes into quirks mode),
        # so the identity cell expects the clean refusal; the doctype-free
        # twin below proves html->html still SERVES.
        {"json": True, "jsonc": True, "ndjson": True, "toml": False, "csv": False, "tsv": False,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True,
         "json-seq": True, "html": False,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": True},
    ),
    InputFixture(
        "html",
        "a doctype-free document with a paragraph",
        b'<html><head></head><body><p>hi</p></body></html>',
        {"json": True, "jsonc": True, "ndjson": True, "toml": False, "csv": False, "tsv": False,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True,
         "json-seq": True, "html": True,
         "properties": False, "ini": False, "dotenv": False,
         "messagepack": True},
    ),
    InputFixture(
        "properties",
        "a flat map of strings",
        b"name=ada\nid=1\nitems=a,b\n",
        {"json": True, "jsonc": True, "ndjson": True, "toml": True, "csv": True, "tsv": True,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": True, "ini": True, "dotenv": True,
         "messagepack": True},
    ),
    InputFixture(
        "ini",
        "a section plus a root key",
        b"root=1\n[db]\nhost=localhost\nport=5432\n",
        {"json": True, "jsonc": True, "ndjson": True, "toml": True, "csv": False, "tsv": False,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": False, "ini": True, "dotenv": False,
         "messagepack": True},
    ),
    InputFixture(
        "dotenv",
        "a flat map with an export prefix and a quoted value",
        b"export A=1\nNAME=ada\nQUOTED=\"x y\"\n",
        {"json": True, "jsonc": True, "ndjson": True, "toml": True, "csv": True, "tsv": True,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": True, "ini": True, "dotenv": True,
         "messagepack": True},
    ),
]

# `-nan` is a negative-NaN literal, NOT an unknown flag: it stays a program.
# `-1`/`-.5` are numeric programs.
UNKNOWN_FLAGS = ["-now", "-pi", "-nZ", "-eZ"]
VALID_FLAGLIKE_PROGRAMS = ["-1", "-.5", "-nan"]

# Every flag the CLI accepts, pinned. Each entry is (long name, short letter
# or None). The two-direction law, at the PROCESS level:
#   * every pinned flag is ACCEPTED by the live binary — a probe that answers
#     "unknown option" means the parser dropped a flag the surface promises;
#   * every flag `--help`'s Options section advertises is PINNED — a help line
#     naming a flag the table does not know is a lie, and a pinned flag absent
#     from the help is accepted-but-unadvertised.
# The in-crate FLAG_TIERS test (`jqf-cli/src/args.rs`) holds the same law
# against the parser's own tables; this table is the independent
# process-level pin, exactly as `SURFACE` is for formats/dialects. Adding a
# flag updates this table AND `FLAG_TIERS` in args.rs in the same commit.
FLAGS = [
    ("input-format", None), ("input-dialect", None),
    ("output-format", None), ("output-dialect", None),
    ("csv-delimiter", None), ("header", None),
    ("ndjson-terminator", None), ("render-header", None),
    ("render-width", None), ("render-shape", None), ("render-max-width", None),
    ("null-input", "n"), ("raw-output", "r"), ("raw-input", "R"),
    ("slurp", "s"), ("from-file", "f"), ("library-path", "L"),
    ("binary", "b"), ("build-configuration", None),
    ("arg", None), ("argjson", None), ("slurpfile", None),
    ("rawfile", None), ("schema", None), ("args", None), ("jsonargs", None),
    ("stream", None), ("stream-errors", None), ("follow", None), ("exit-status", "e"),
    ("sort-keys", "S"), ("join-output", "j"), ("raw-output0", None),
    ("ascii-output", "a"), ("seed", None), ("mismatch-policy", None),
    ("strictness", None), ("types-as-strings", None),
    ("json-facts", None), ("no-json-facts", None),
    ("with-source", None),
    ("unbuffered", None), ("color-output", "C"),
    ("monochrome-output", "M"), ("compact-output", "c"),
    ("indent", None), ("tab", None), ("max-memory-bytes", None),
    ("max-rss", None), ("max-spill-bytes", None),
    ("max-spill-disk-bytes", None), ("max-iterations", None),
    ("parallel", None),
    ("no-parallel", None), ("workers", None), ("diagnostics", None),
    ("explain", None), ("plan-out", None), ("plan-file", None),
    ("edit", None), ("diff", None), ("old-format", None), ("new-format", None),
    ("edit-expand-alias", None),  # the alias escape hatch
    ("output", None), ("in-place", None),
    ("split-exp", None), ("split-exp-file", None),
    ("check", None),  # the edit lane's verdict dial
    ("no-atomic", None), ("list-builtins", None), ("list-formats", None),
    ("help-format", None), ("explain-code", None), ("config", None),
    ("no-config", None), ("show-config", None), ("help", "h"),
    ("version", "V"),
    ("seq", None),
]
FLAG_LONG = {name for name, _ in FLAGS}
FLAG_SHORT = {short for _, short in FLAGS if short is not None}

# The complete accepted format/dialect surface, pinned. The help text and the
# parser must enumerate exactly this set and nothing else: a spelling the help
# omits is unreachable, a spelling the parser drops is dead advertisement, and
# a spelling in neither is a lie. `xml` was once a legal `--output-format` and
# two legal `--output-dialect`s while `--help` listed neither — this table is
# what refuses to let that class recur. Adding a format or dialect updates this
# dict AND the acceptance tables in `jqf-cli/src/args.rs` (the single table both
# the parser and the help read) in the same commit; a drift in either direction
# fails here.
SURFACE = {
    "input-format": [
        "json", "jsonc", "json5", "ndjson", "json-seq", "toml", "csv", "tsv", "cbor", "cbor-seq", "yaml", "jqft", "jqfjson", "jqfb", "xml", "html", "properties", "ini", "dotenv", "messagepack",
    ],
    "input-dialect": [
        "rfc8259", "jsonc.trailing@1", "jsonc.default@1", "json5.document@1", "ndjson.strict@1", "ndjson.recovering@1", "json-seq.strict@1",
        "toml-1.0",
        "toml-1.1", "csv.utf8@1", "csv.utf8-header@1", "csv.rfc4180@1", "csv.rfc4180-header@1", "tsv.utf8@1", "tsv.utf8-header@1",
        "cbor.rfc8949-generic@1", "cbor-seq.rfc8742-generic@1", "yaml.core@1", "yaml.json@1",
        "yaml.failsafe@1", "jqft.document@1", "jqfjson.document@1",
        "jqfb.document@1", "xml.document@1", "html.document@1",
        "html.fragment@1",
        "properties.jdk@1", "ini.jqf-strict@1", "dotenv.jqf-strict@1",
        "messagepack.utf8@1", "messagepack.key-equivalence@1",
    ],
    "output-format": [
        "json", "jsonc", "json5", "ndjson", "json-seq", "toml", "csv", "tsv", "cbor", "cbor-seq", "yaml", "jqft", "jqfjson", "jqfb", "xml", "html", "properties", "ini", "dotenv", "messagepack", "render",
    ],
    "output-dialect": [
        "rfc8259", "jsonc.trailing-jqf@1", "jsonc.default-jqf@1", "jsonc.jqf-1.0@1", "json5.jqf@1", "json5.jqf-1.0@1", "ndjson.strict@1", "json-seq.jqf@1", "toml.jqf-1.0@1",
        "toml.jqf-1.1@1",
        "csv.jqf-utf8@1", "csv.jqf-utf8-header@1", "csv.jqf-rfc4180@1", "csv.jqf-rfc4180-header@1", "tsv.jqf-lf@1", "tsv.jqf-lf-header@1", "cbor.source@1",
        "cbor.preferred@1", "cbor.core-deterministic@1", "cbor.length-first@1", "cbor-seq.jqf@1",
        "yaml.block@1", "yaml.stream-canonical@1", "yaml.single-document@1",
        "yaml.jqf-1.0@1",
        "jqft.canonical@1", "jqfjson.canonical@1",
        "jqfb.canonical@1",
        "xml.source@1", "xml.jqf-deterministic@1", "html.source@1",
        "html.document-serialize@1", "properties.jqf-1.0@1", "ini.jqf-1.0@1",
        "dotenv.jqf-1.0@1", "messagepack.deterministic@1",
        "messagepack.deterministic-float64@1", "render.plain@1",
        "render.gfm-table@1", "render.html-table@1", "render.grid-table@1",
        "render.tree@1", "render.terminal@1", "render.shell@1",
        "render.hist@1",
    ],
}


def decode_adjacent_json(text):
    decoder = json.JSONDecoder()
    position = 0
    seen = 0
    try:
        while True:
            while position < len(text) and text[position] in " \t\r\n":
                position += 1
            if position >= len(text):
                break
            _value, position = decoder.raw_decode(text, position)
            seen += 1
    except ValueError:
        # Bytes that do not decode are simply not the requested format — the
        # cell fails, exactly as the jsonc/json5/toml validators answer False,
        # instead of the gate dying on a traceback mid-run.
        return False
    return seen > 0


def _decode_json_seq(data):
    """Every RS-delimited fragment of `data` must parse as one JSON text."""
    text = data.decode("utf-8", "replace")
    seen = False
    for fragment in text.split("\x1e"):
        if fragment.strip():
            try:
                json.loads(fragment)
            except ValueError:
                return False
            seen = True
    return seen


# Native Python has no honest parser for these profiles; jqf's own decoder
# is the oracle. A silent fallback to a different format fails that decode.
SELF_ORACLE = frozenset({
    "jsonc", "json5", "cbor", "cbor-seq", "yaml", "jqft", "jqfb", "xml",
    "html", "properties", "ini", "dotenv", "messagepack",
})


def validate_output(fmt, data, jqf):
    """True when `data` genuinely decodes as `fmt` (or is non-empty for render)."""
    if fmt in SELF_ORACLE:
        completed = proc.run_gate(
            jqf, ["--input-format", fmt, "--output-format", "json", "."],
            input=data, timeout=60,
        )
        return completed.returncode == 0
    if fmt == "json":
        return decode_adjacent_json(data.decode("utf-8", "replace"))
    if fmt == "ndjson":
        text = data.decode("utf-8", "replace")
        lines = text.split("\n")
        try:
            return any(line.strip() for line in lines) and all(
                not line.strip() or json.loads(line) for line in lines
            )
        except ValueError:
            # A line that does not parse means the bytes are not NDJSON —
            # the cell fails, never a traceback out of the validator.
            return False
    if fmt == "toml":
        import tomllib

        try:
            tomllib.loads(data.decode("utf-8"))
            return True
        except (tomllib.TOMLDecodeError, UnicodeDecodeError):
            return False
    if fmt in ("csv", "tsv"):
        try:
            delimiter = "," if fmt == "csv" else "\t"
            rows = list(csv.reader(io.StringIO(data.decode("utf-8", "replace")), delimiter=delimiter))
        except csv.Error:
            return False
        # `len(rows) > 0` was vacuous: csv.reader yields at least one row for
        # ANY non-empty text, so wrong-format bytes validated as a table. A
        # served table has a shape — every row non-empty and every row the
        # same width as the first. The record routes emit HEADERLESS tables
        # (a flat map renders as one data row), so no header is demanded.
        return (
            len(rows) > 0
            and all(len(row) > 0 for row in rows)
            and len({len(row) for row in rows}) == 1
        )
    if fmt == "jqfjson":
        # A jqfjson document IS strict JSON, so the native parser is the
        # oracle (the load-bearing independence the native parsers keep).
        try:
            json.loads(data.decode("utf-8"))
            return True
        except (ValueError, UnicodeDecodeError):
            return False
    if fmt == "json-seq":
        return _decode_json_seq(data)
    if fmt == "render":
        return len(data) > 0
    raise AssertionError(f"unknown output format {fmt}")


def run_matrix(jqf):
    failures = []
    cells = serve = passed = 0
    fixtures = list(FIXTURES)
    # The json5 input fixture: the json5 grammar arms over the SAME value
    # shape as the json fixture (nested, so the representability cells
    # repeat) — unquoted keys, single quotes, and comments decode to the
    # same value the strict spelling does.
    fixtures.append(InputFixture(
        "json5",
        "json5 grammar arms over the json fixture's shape",
        b'{\n  // lead\n  name: \'ada\',\n  id: 1,\n  tags: [\'a\', \'b\'],\n  attrs: {x: 1.5},\n}\n',
        dict(fixtures[0].row, json5=True),
    ))
    # The jqfb input fixture is generated ONCE per run by encoding the jqft
    # fixture through jqf itself (the canonical image is deterministic), so
    # the gate never embeds a binary blob that could drift from the codec it
    # tests. The row mirrors the jqft fixture's: the image decodes to the
    # same fact-less object, so the representability cells repeat.
    image = proc.run_gate(
        jqf, [ "--input-format", "jqft", "--output-format", "jqfb", "."],
        input=b'%jqft 1\n{name: "ada", id: 1, items: ["a", "b"]}\n', timeout=120,
    )
    fixtures.append(InputFixture(
        "jqfb",
        "the jqft fixture's canonical image",
        image.stdout,
        {"json": True, "jsonc": True, "json5": True, "ndjson": True, "toml": True, "csv": False, "tsv": False,
         "cbor": True, "cbor-seq": True, "yaml": True, "jqft": True, "jqfjson": True,
         "jqfb": True, "render": True, "xml": True, "json-seq": True, "html": True,
         "properties": False, "ini": False, "dotenv": False, "messagepack": True},
    ))
    # json5 output carries the same value model as json (exact Decimal/Integer
    # plus the pinned non-finite Floats), so every fixture's
    # json5 output cell mirrors its json cell — after every fixture (the
    # generated jqfb included) exists. The record-input fixtures are the one
    # exception: a record route's output whitelist is
    # json/ndjson/json-seq/csv/tsv, so json5 output is a usage error there
    # (the same refusal jsonc's record cells ride).
    for fixture in fixtures:
        if fixture.fmt in ("ndjson", "csv", "tsv", "json-seq"):
            fixture.row["json5"] = False
        else:
            fixture.row["json5"] = fixture.row["json"]
    for fixture in fixtures:
        for fmt in OUTPUTS:
            cells += 1
            expected = fixture.row[fmt]
            completed = proc.run_gate(
                jqf, [ "--input-format", fixture.fmt, "--output-format", fmt, "."],
                input=fixture.bytes, timeout=120,
            )
            code = completed.returncode
            data = completed.stdout
            if expected:
                serve += 1
                if code != 0:
                    failures.append(
                        f"{fixture.fmt}->{fmt}: expected to SERVE, exited {code} "
                        f"({completed.stderr.decode('utf-8', 'replace').strip()})"
                    )
                elif not data:
                    failures.append(
                        f"{fixture.fmt}->{fmt}: served but published zero bytes"
                    )
                elif not validate_output(fmt, data, jqf):
                    failures.append(
                        f"{fixture.fmt}->{fmt}: served but the bytes do not decode "
                        f"as {fmt} (the silent-wrong-format defect class)"
                    )
                else:
                    passed += 1
            else:
                if code == 0:
                    failures.append(
                        f"{fixture.fmt}->{fmt}: expected to FAIL cleanly, exited 0 "
                        f"with {len(data)} bytes — a silent fallback"
                    )
                elif data:
                    failures.append(
                        f"{fixture.fmt}->{fmt}: expected to FAIL cleanly, exited "
                        f"{code} but published {len(data)} bytes"
                    )
                else:
                    passed += 1
    return failures, cells, serve, passed


def run_multi_value(jqf):
    """The flagless adjacent-value lane over a MULTI-VALUE input (the sharded
    lane) must never silently publish JSON bytes for a non-JSON output format.

    The value lane's relay concatenates JSON-family byte streams and cannot
    encode a one-document-per-run format per value, so a >262144-byte
    multi-value input with `--output-format toml|csv|yaml` must plan serial
    (the `output-format` decline): the cell either publishes bytes that
    genuinely decode as the requested format, or exits nonzero with zero
    bytes — never JSON with exit 0 on the sharded lane. The control row is
    the small-input twin below the crossover,
    which must behave exactly as before.
    """
    failures = []
    total = 0
    # 14000 adjacent records: safely past the value lane's 262144-byte
    # break-even, so `auto` would engage the sharded lane without the fix.
    fixture = b'{"name":"ada","id":1}\n' * 14000
    assert len(fixture) > 270 * 1024
    # Fed through a real FILE, never stdin: the sharded lane needs a SEEKABLE
    # source, and a pipe is non-seekable (it plans the serial streaming drive
    # and prints no plan line — the cell would test the wrong route and the
    # ENGAGED guard below could never fire).
    source = tempfile.NamedTemporaryFile(prefix="jqf-capability-multi-")
    source.write(fixture)
    source.flush()
    for fmt in ("toml", "csv", "yaml"):
        total += 1
        source.seek(0)
        completed = proc.run_gate(
            jqf, [ "--diagnostics", "--output-format", fmt, "."],
            stdin=source, timeout=120,
        )
        code = completed.returncode
        data = completed.stdout
        if code == 0:
            if not data:
                failures.append(
                    f"multi-value->{fmt}: served but published zero bytes"
                )
            elif not validate_output(fmt, data, jqf):
                failures.append(
                    f"multi-value->{fmt}: served but the bytes do not decode "
                    f"as {fmt} (the silent-wrong-format defect class)"
                )
        else:
            if data and not validate_output(fmt, data, jqf):
                # A nonzero exit may legitimately publish a correct-format
                # prefix: toml/cbor are one-document-per-run, so the first
                # value encodes and the multi-value refusal follows (exit 5).
                # What must never happen is wrong-format bytes — JSON for a
                # non-JSON request, the defect this cell exists to catch.
                failures.append(
                    f"multi-value->{fmt}: exited {code} but published bytes "
                    f"that do not decode as {fmt}"
                )
        # The ENGAGED guard: the sharded lane must DECLINE this request
        # (decision=output-format, workers=0). The plan line prints under
        # `--diagnostics` — the run above carries the flag, so this is one
        # subprocess per cell, no new lanes. A crossover drift above this
        # fixture's size would silently plan below-break-even (same bytes, no
        # mechanism) and the cell would test the serial path, not the decline.
        plan_line = next(
            (line for line in completed.stderr.decode("utf-8", "replace").splitlines()
             if line.startswith("jqf: plan:")), None
        )
        if plan_line is None or "decision=output-format" not in plan_line:
            failures.append(
                f"multi-value->{fmt}: the sharded lane must plan the "
                f"output-format decline, got {plan_line!r}"
            )
    # The small-input control: below the crossover the lane plans serial
    # anyway, so a non-JSON output format must answer exactly as before.
    total += 1
    small = b'{"name":"ada","id":1}\n' * 3
    completed = proc.run_gate(
        jqf, [ "--output-format", "toml", "."],
        input=small, timeout=60,
    )
    if completed.returncode == 0:
        if not completed.stdout:
            failures.append("multi-value-small->toml: served but published zero bytes")
        elif not validate_output("toml", completed.stdout, jqf):
            failures.append(
                "multi-value-small->toml: served but the bytes do not decode as toml"
            )
    elif completed.stdout and not validate_output("toml", completed.stdout, jqf):
        # Same prefix law as the large cells: a nonzero exit with a
        # correct-format prefix (one-document-per-run toml) is legal.
        failures.append(
            "multi-value-small->toml: exited "
            f"{completed.returncode} but published bytes that do not decode as toml"
        )
    return failures, total - len(failures)


def run_in_place(jqf):
    """`--in-place` (the positional file is the edit target) preserves mode
    and symlinks across both write modes."""
    failures = []
    total = 0
    with tempfile.TemporaryDirectory(prefix="jqf-capability-") as tmp:
        def write_pair(name, body, mode):
            path = os.path.join(tmp, name)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(body)
            os.chmod(path, mode)
            return path

        for label, mode, flag in (("atomic", 0o600, []), ("non-atomic", 0o640, ["--no-atomic"])):
            path = write_pair(f"mode-{label}.json", "{\"a\":1}", mode)
            total += 1
            completed = proc.run_gate(
                jqf, [ *flag, "--in-place", ".a = 2", path], timeout=120,
            )
            kept = stat.S_IMODE(os.stat(path).st_mode)
            if completed.returncode != 0:
                failures.append(f"in-place mode [{label}]: edit failed "
                                f"({completed.stderr.decode('utf-8', 'replace').strip()})")
            elif kept != mode:
                failures.append(f"in-place mode [{label}]: {oct(mode)} became "
                                f"{oct(kept)} across the {label} write")

        for label, flag in (("atomic", []), ("non-atomic", ["--no-atomic"])):
            real = os.path.join(tmp, f"real-{label}.json")
            link = os.path.join(tmp, f"link-{label}.json")
            with open(real, "w", encoding="utf-8") as handle:
                handle.write("{\"a\":1}")
            os.symlink(os.path.basename(real), link)
            total += 1
            completed = proc.run_gate(
                jqf, [ *flag, "--in-place", ".a = 9", link], timeout=120,
            )
            still_link = os.path.islink(link)
            target = os.readlink(link) if still_link else None
            if completed.returncode != 0:
                failures.append(f"in-place symlink [{label}]: edit failed "
                                f"({completed.stderr.decode('utf-8', 'replace').strip()})")
            elif not still_link:
                failures.append(f"in-place symlink [{label}]: the {label} write "
                                f"replaced the symlink with a regular file")
            elif target != os.path.basename(real):
                failures.append(f"in-place symlink [{label}]: link now points at "
                                f"{target!r}, expected {os.path.basename(real)!r}")
    return failures, total - len(failures)


def run_flags(jqf):
    failures = []
    total = 0
    for flag in UNKNOWN_FLAGS:
        total += 1
        completed = proc.run_gate(
            jqf, [ flag, "."], input=b"{}\n", timeout=60,
        )
        err = completed.stderr.decode("utf-8", "replace")
        if completed.returncode != 2:
            failures.append(
                f"unknown flag {flag}: expected exit 2, got {completed.returncode}"
            )
        elif "unknown option" not in err:
            failures.append(f"unknown flag {flag}: exited 2 but the diagnostic "
                            f"does not say 'unknown option': {err.strip()!r}")
    for program in VALID_FLAGLIKE_PROGRAMS:
        total += 1
        completed = proc.run_gate(
            jqf, [ program], input=b"1\n", timeout=60,
        )
        if completed.returncode != 0:
            failures.append(
                f"program-lookalike {program}: jq treats it as a program, jqf must "
                f"too — exited {completed.returncode} "
                f"({completed.stderr.decode('utf-8', 'replace').strip()})"
            )
    return failures, total - len(failures)


def run_flag_table(jqf):
    """The pinned flag table, bidirectionally.

    Two directions, both against the live binary:
      * every pinned long flag is ACCEPTED — a bare `--flag` probe whose
        diagnostic says "unknown option" means the parser dropped a flag the
        surface promises. A missing value or a valid request is acceptance;
        only "unknown option" is a rejection.
      * every flag `--help`'s Options section advertises is PINNED, in both
        spellings — a help line naming a flag (or short letter) the table does
        not know is a lie, and a pinned flag absent from the help is
        accepted-but-unadvertised.
    """
    failures = []
    total = 0
    for name, _short in FLAGS:
        total += 1
        completed = proc.run_gate(
            jqf, [ "--" + name], input=b"{}", timeout=60,
        )
        err = completed.stderr.decode("utf-8", "replace")
        if "unknown option" in err:
            failures.append(
                f"flag-table: pinned flag --{name} is rejected as unknown: "
                f"{err.strip()!r}"
            )
    for short in sorted(FLAG_SHORT):
        total += 1
        completed = proc.run_gate(
            jqf, [ "-" + short], input=b"{}", timeout=60,
        )
        err = completed.stderr.decode("utf-8", "replace")
        if "unknown option" in err:
            failures.append(
                f"flag-table: pinned short flag -{short} is rejected as unknown: "
                f"{err.strip()!r}"
            )
    completed = proc.run_gate(
        jqf, [ "--help"], timeout=60,
    )
    help_text = completed.stdout.decode("utf-8", "replace")
    advertised_long = set()
    advertised_short = set()
    in_options = False
    for line in help_text.splitlines():
        if line == "Options:":
            in_options = True
            continue
        if not in_options:
            continue
        if line == "Configuration:":
            break
        rest = line[2:] if line.startswith("  ") else None
        if rest is None or rest.startswith(" "):
            continue
        tokens = rest.split()
        for token in tokens:
            if token.startswith("--"):
                advertised_long.add(token[2:])
            else:
                short = token.rstrip(",")
                if len(short) == 2 and short[0] == "-":
                    advertised_short.add(short[1])
    for name in sorted(advertised_long - FLAG_LONG):
        total += 1
        failures.append(f"flag-table: --help advertises unpinned flag --{name}")
    for name in sorted(FLAG_LONG - advertised_long):
        total += 1
        failures.append(f"flag-table: pinned flag --{name} has no --help line")
    for short in sorted(advertised_short - FLAG_SHORT):
        total += 1
        failures.append(f"flag-table: --help advertises unpinned short flag -{short}")
    for short in sorted(FLAG_SHORT - advertised_short):
        total += 1
        failures.append(f"flag-table: pinned short flag -{short} has no --help line")
    return failures, total - len(failures)


def run_help_surface(jqf):
    """The help surface must equal the accepted surface.

    Two directions, both checked against the live binary:
      * every pinned spelling appears in `--help` AND is accepted by the parser
        (a probe that answers "unknown ... value" means the CLI dropped a
        spelling the surface still promises — that is the xml-class drift);
      * every spelling `--help` advertises is pinned (a help line that names
        something the parser does not accept is a lie).

    A dialect spelling is probed WITHOUT its format pair: a known spelling with
    a mismatched format answers "invalid ... pair" (a pair law, not an unknown
    spelling), so only "unknown ... value" counts as a rejection.
    """
    failures = []
    total = 0
    completed = proc.run_gate(
        jqf, [ "--help"], timeout=60,
    )
    help_text = completed.stdout.decode("utf-8", "replace")
    advertised = {}
    for flag in SURFACE:
        prefix = "  --" + flag + " "
        lines = iter(help_text.splitlines())
        for line in lines:
            if not line.startswith(prefix):
                continue
            # The pipe-delimited enumeration may wrap onto continuation
            # physical lines; matching the first line alone would read a
            # truncated surface and fail pinned spellings that are really
            # advertised. Join while the list's own separator says it
            # continues (a trailing `|`) — prose description lines never do,
            # so descriptions cannot leak into the spelling set.
            text = line[len(prefix):]
            while text.endswith("|"):
                text += next(lines, "").strip()
            advertised[flag] = set(text.split("|"))
            break
    for flag, expected in SURFACE.items():
        if flag not in advertised:
            failures.append(f"surface: --help has no {flag} line")
            continue
        for spelling in expected:
            total += 1
            if spelling not in advertised[flag]:
                failures.append(f"surface: --help omits {flag} spelling {spelling}")
                continue
            probe = proc.run_gate(
                jqf, [ "--" + flag, spelling, "."],
                input=b"{}\n", timeout=60,
            )
            err = probe.stderr.decode("utf-8", "replace")
            if "unknown --" + flag + " value" in err:
                failures.append(
                    f"surface: {flag} spelling {spelling} is advertised but the "
                    f"parser rejects it as unknown: {err.strip()!r}"
                )
    for flag, expected in SURFACE.items():
        for spelling in advertised.get(flag, ()):
            total += 1
            if spelling not in expected:
                failures.append(
                    f"surface: --help advertises unpinned {flag} spelling {spelling}"
                )
    return failures, total - len(failures)


def run_discovery_surface(jqf):
    """The discovery surfaces must agree with the pinned surface.

    The generated enumerations exist so a doc cannot describe a jqf that does
    not exist. This is the same two-direction law as `run_help_surface`, now
    against the machine-readable surfaces:

      * `--list-formats` lists every pinned format/dialect spelling and NOTHING
        that is not pinned (a listed surface is a promise: reachable but unlisted
        is a hole, listed but unreachable is a lie);
      * `--list-builtins` enumerates exactly what the `builtins` builtin does
        (the one-law two-doors pin, which the Rust integration test also holds);
      * `--explain-code <id>` answers every live code in the manifest and
        rejects an unknown id (exit 2, never a silent empty page).
    """
    failures = []
    total = 0

    listed = proc.run_gate(
        jqf, [ "--list-formats"], timeout=60,
    )
    if listed.returncode != 0:
        total += 1
        failures.append(
            f"discovery: --list-formats exited {listed.returncode}: "
            f"{listed.stderr.decode('utf-8', 'replace').strip()}"
        )
        return failures, total - len(failures)
    formats_text = listed.stdout.decode("utf-8", "replace")
    for flag, expected in SURFACE.items():
        for spelling in expected:
            total += 1
            if spelling not in formats_text:
                failures.append(
                    f"discovery: --list-formats omits pinned {flag} spelling {spelling}"
                )
    # The reverse direction: every format or dialect the list prints is pinned.
    # Structural words (`direction:`, `input`, …) are the page's own labels,
    # so only the SPELLINGS are collected: a standalone line is a format name,
    # and a token after an `input dialects:`/`output dialects:` label is a
    # dialect spelling.
    printed_spellings = []
    for line in formats_text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("input formats:") or stripped.startswith("output formats:"):
            continue
        if line.startswith("  ") and ("dialects:" in stripped):
            printed_spellings.extend(stripped.split("dialects:", 1)[1].split())
        elif not line.startswith("  ") and not stripped.startswith("direction:"):
            printed_spellings.append(stripped)
    pinned = set(spelling for spellings in SURFACE.values() for spelling in spellings)
    for spelling in printed_spellings:
        total += 1
        if spelling not in pinned:
            failures.append(
                f"discovery: --list-formats prints unpinned spelling {spelling!r}"
            )

    builtins = proc.run_gate(
        jqf, [ "--list-builtins"], timeout=60,
    )
    if builtins.returncode != 0:
        total += 1
        failures.append(
            f"discovery: --list-builtins exited {builtins.returncode}: "
            f"{builtins.stderr.decode('utf-8', 'replace').strip()}"
        )
        return failures, total - len(failures)
    cli_names = set(builtins.stdout.decode("utf-8", "replace").split())
    builtin_value = proc.run_gate(
        jqf, [ "builtins"], input=b"null\n", timeout=60,
    )
    if builtin_value.returncode != 0:
        total += 1
        failures.append(
            f"discovery: `builtins` builtin exited {builtin_value.returncode}: "
            f"{builtin_value.stderr.decode('utf-8', 'replace').strip()}"
        )
        return failures, total - len(failures)
    try:
        lang_names = set(json.loads(builtin_value.stdout.decode("utf-8", "replace")))
    except ValueError as error:
        total += 1
        failures.append(f"discovery: `builtins` output is not JSON: {error}")
        return failures, total - len(failures)
    total += 1
    if cli_names != lang_names:
        only_cli = sorted(cli_names - lang_names)
        only_lang = sorted(lang_names - cli_names)
        failures.append(
            f"discovery: --list-builtins and the `builtins` builtin disagree "
            f"(cli-only={only_cli[:5]} lang-only={only_lang[:5]})"
        )

    # Every live code in the manifest must be explainable; an unknown id must
    # fail loudly (usage exit 2), never print a silent empty page.
    import tomllib

    manifest = os.path.join(ROOT, "jqf-resource", "src", "diag", "codes.toml")
    with open(manifest, "rb") as handle:
        codes = tomllib.load(handle)["code"]
    for code in codes:
        if code.get("reserved"):
            continue
        total += 1
        explained = proc.run_gate(
            jqf, [ "--explain-code", str(code["id"])], timeout=60,
        )
        text = explained.stdout.decode("utf-8", "replace")
        if explained.returncode != 0 or code["name"] not in text:
            failures.append(
                f"discovery: --explain-code {code['id']} ({code['name']}) "
                f"failed rc={explained.returncode}"
            )
    total += 1
    unknown = proc.run_gate(
        jqf, [ "--explain-code", "9999"], timeout=60,
    )
    if unknown.returncode != 2 or unknown.stdout:
        failures.append(
            f"discovery: --explain-code 9999 should exit 2 with no stdout, "
            f"got rc={unknown.returncode} stdout={unknown.stdout!r}"
        )

    # `--help <topic>`: every pinned format/dialect spelling plus the
    # four fixed topics is a working topic (exit 0, non-empty page), and an
    # unknown topic is a usage error (exit 2, no stdout) — the same
    # two-direction law as the surfaces above, applied to the topic set. The
    # topic set is derived from the SURFACE table, so a spelling added to the
    # acceptance tables joins the topic surface without a second list.
    fixed_topics = ["builtins", "codes", "flags", "mismatch"]
    for spelling in fixed_topics + [
        s for spellings in SURFACE.values() for s in spellings
    ]:
        total += 1
        page = proc.run_gate(
            jqf, [ "--help", spelling], timeout=60,
        )
        if page.returncode != 0 or not page.stdout:
            failures.append(
                f"discovery: --help {spelling} failed rc={page.returncode} "
                f"stdout={len(page.stdout)} bytes"
            )
    total += 1
    unknown = proc.run_gate(
        jqf, [ "--help", "no-such-topic"], timeout=60,
    )
    err = unknown.stderr.decode("utf-8", "replace")
    if unknown.returncode != 2 or unknown.stdout or "unknown help topic" not in err:
        failures.append(
            f"discovery: --help no-such-topic should exit 2 naming the miss, "
            f"got rc={unknown.returncode} stdout={unknown.stdout!r} err={err.strip()!r}"
        )
    return failures, total - len(failures)


# The reserved subcommand keyword set. `serve` is the only reserved
# subcommand. A keyword is reserved only in the first-positional slot with
# no program-looking prefix: `jqf --follow 'serve'` runs the PROGRAM serve
# (a follow positional is the program) and `jqf -f serve` reads the
# program file `serve`, so both must NOT be subcommand recognitions.
#
# A socket SERVER does not fit this gate's process-per-case shape (it is a
# long-running daemon, not one request per process), so the serve surface
# itself is pinned in `jqf-cli/tests/serve.rs` and `cli_flags.rs`; this block
# pins the process-shaped subcommand answers every keyword must give.
SUBCOMMAND_KEYWORDS = ["serve"]


def run_subcommand_surface(jqf):
    failures = []
    total = 0
    # The help documents every keyword (the same table the parser reads).
    completed = proc.run_gate(
        jqf, [ "--help"], timeout=60,
    )
    help_text = completed.stdout.decode("utf-8", "replace")
    for keyword in SUBCOMMAND_KEYWORDS:
        total += 1
        if keyword not in help_text:
            failures.append(f"subcommand: --help omits the {keyword} keyword")
    total += 1
    if "Subcommands:" not in help_text:
        failures.append("subcommand: --help has no Subcommands section")
    # `serve` without --listen is a usage error that names the flag.
    total += 1
    completed = proc.run_gate(
        jqf, [ "serve"], input=b"", timeout=60,
    )
    err = completed.stderr.decode("utf-8", "replace")
    if completed.returncode != 2 or "--listen" not in err:
        failures.append(
            f"subcommand: jqf serve must demand --listen, got "
            f"{completed.returncode} {err.strip()!r}"
        )
    # The follow precedent: `jqf --follow serve` is the PROGRAM serve (a
    # follow positional is the program), so it compiles as the undefined
    # builtin instead of recognizing the subcommand.
    total += 1
    completed = proc.run_gate(
        jqf, [ "--follow", "serve"], input=b"", timeout=60,
    )
    err = completed.stderr.decode("utf-8", "replace")
    if completed.returncode != 3 or "not defined" not in err:
        failures.append(
            f"subcommand: jqf --follow serve must compile the PROGRAM serve, "
            f"got {completed.returncode} {err.strip()!r}"
        )
    # `-f serve` names the program FILE, never the subcommand.
    total += 1
    completed = proc.run_gate(
        jqf, [ "-f", "serve"], input=b"", timeout=60,
    )
    err = completed.stderr.decode("utf-8", "replace")
    if completed.returncode != 2 or "Could not open serve" not in err:
        failures.append(
            f"subcommand: jqf -f serve must open the file serve, got "
            f"{completed.returncode} {err.strip()!r}"
        )
    return failures, total - len(failures)


def run_hermeticity(jqf):
    """A hostile `.jqf.toml` on disk cannot move a gate.

    The whole gate runs under `JQF_NO_CONFIG` (set at the top of this file),
    so a developer's config cannot reach any lane by construction. This cell
    proves the construction has teeth:

      * vacuity: a hostile `.jqf.toml` in the cwd DOES change the bytes when
        hermeticity is off (so the immunity assertions are not vacuous);
      * `--no-config` and `JQF_NO_CONFIG=1` both answer the clean bytes with
        the hostile file present;
      * the gate's own environment is immune even with the hostile file at
        its cwd;
      * `--show-config` names the hostile file, so "why is it doing that?"
        has an answer.
    """
    failures = []
    total = 0
    # The probes run with cwd=tmp, so a relative jqf path (the make recipe
    # passes `target/release/jqf`) would not resolve there.
    jqf = os.path.abspath(jqf)
    # System temp, NOT dir=ROOT: the hostile `.jqf.toml` must never be able to
    # outlive the gate inside the working tree. A SIGKILL (or a crash) skips
    # TemporaryDirectory's cleanup, and a leftover config at the repo root is
    # exactly the state every other lane's hermeticity exists to prevent; the
    # vacuity law survives because discovery walks UP from cwd=tmp and the
    # probe's own file is always the nearest.
    with tempfile.TemporaryDirectory(prefix="jqf-hostile-") as tmp:
        hostile = os.path.join(tmp, ".jqf.toml")
        with open(hostile, "w", encoding="utf-8") as handle:
            handle.write("[defaults]\ncompact = true\n")
        # The vacuity/immunity probes scrub HOME so the developer's real
        # global config cannot interfere, and drop JQF_NO_CONFIG so the
        # vacuity run really reads the hostile file (the nearest discovery
        # file wins over anything farther up, so the probe is deterministic).
        scrubbed = {k: v for k, v in os.environ.items() if k != "JQF_NO_CONFIG"}
        scrubbed_home = {**scrubbed, "HOME": os.path.join(tmp, "home")}
        os.makedirs(scrubbed_home["HOME"], exist_ok=True)

        def probe(args, env):
            return proc.run_gate(
                jqf, [ *args, "."],
                input=b'{"a":1}\n',
                timeout=60,
                cwd=tmp,
                env=env,
            )

        pretty = b'{\n  "a": 1\n}\n'
        compact = b'{"a":1}\n'

        # Vacuity: without hermeticity the hostile config compacts the output.
        total += 1
        run = probe([], scrubbed_home)
        if run.returncode != 0 or run.stdout != compact:
            failures.append(
                f"hermeticity vacuity: the hostile config must compact, "
                f"got rc={run.returncode} {run.stdout!r}"
            )
        # Immunity via --no-config.
        total += 1
        run = probe(["--no-config"], scrubbed_home)
        if run.returncode != 0 or run.stdout != pretty:
            failures.append(
                f"hermeticity: --no-config must stay byte-identical, "
                f"got rc={run.returncode} {run.stdout!r}"
            )
        # Immunity via JQF_NO_CONFIG=1.
        total += 1
        run = probe([], {**scrubbed_home, "JQF_NO_CONFIG": "1"})
        if run.returncode != 0 or run.stdout != pretty:
            failures.append(
                f"hermeticity: JQF_NO_CONFIG=1 must stay byte-identical, "
                f"got rc={run.returncode} {run.stdout!r}"
            )
        # The gate's own environment is immune: JQF_NO_CONFIG is set at the
        # top of this file, so even a hostile config at the gate's cwd cannot
        # move it.
        total += 1
        run = probe([], dict(os.environ))
        if run.returncode != 0 or run.stdout != pretty:
            failures.append(
                f"hermeticity: the gate's own env must be immune, "
                f"got rc={run.returncode} {run.stdout!r}"
            )
        # Visibility: --show-config names the hostile file.
        total += 1
        run = proc.run_gate(
            jqf, [ "--show-config"],
            timeout=60,
            cwd=tmp,
            env=scrubbed_home,
        )
        if run.returncode != 0 or ".jqf.toml" not in run.stdout.decode("utf-8", "replace"):
            failures.append(
                "hermeticity: --show-config must name the config file "
                f"(rc={run.returncode})"
            )
    return failures, total - len(failures)


def main():
    jqf = proc.resolve_binary(sys.argv[1:], default=proc.DEFAULT_RELEASE_JQF)
    if not proc.executable(jqf):
        print(f"capability-gate: no executable at {jqf}", file=sys.stderr)
        return 2

    failures = []
    matrix_failures, cells, serve, matrix_passed = run_matrix(jqf)
    failures.extend(matrix_failures)
    multi_failures, multi_passed = run_multi_value(jqf)
    failures.extend(multi_failures)
    in_place_failures, in_place_passed = run_in_place(jqf)
    failures.extend(in_place_failures)
    flags_failures, flags_passed = run_flags(jqf)
    failures.extend(flags_failures)
    flag_table_failures, flag_table_passed = run_flag_table(jqf)
    failures.extend(flag_table_failures)
    surface_failures, surface_passed = run_help_surface(jqf)
    failures.extend(surface_failures)
    discovery_failures, discovery_passed = run_discovery_surface(jqf)
    failures.extend(discovery_failures)
    hermeticity_failures, hermeticity_passed = run_hermeticity(jqf)
    failures.extend(hermeticity_failures)
    subcommand_failures, subcommand_passed = run_subcommand_surface(jqf)
    failures.extend(subcommand_failures)

    # The single print: every failure (matrix, in-place, flags, flag-table,
    # surface, discovery, hermeticity, subcommand) appears exactly once.
    for text in failures:
        print(f"FAIL {text}", file=sys.stderr)
    # Every lane reports its PASSED count (its total minus its own failures), so
    # a failing cell can never count itself in pass= — only deviations= and the
    # exit code used to carry the truth. On a RED run the per-cell arithmetic can
    # be off by ±1 per double-fired cell (two failures, one total slot), so the
    # receipt asserts the invariant it can: GREEN is exact, and a RED receipt
    # stays self-consistent by construction (pass + deviations == the total
    # cells the harness ran is NOT asserted because the double-fire makes it
    # approximate; the FAIL lines carry the truth on RED).
    passed = (
        matrix_passed
        + multi_passed
        + in_place_passed
        + flags_passed
        + flag_table_passed
        + surface_passed
        + discovery_passed
        + hermeticity_passed
        + subcommand_passed
    )
    print(
        f"capability-gate: cells={cells} serve={serve} "
        f"surface={surface_passed} in-place={in_place_passed} flags={flags_passed} "
        f"flag-table={flag_table_passed} discovery={discovery_passed} "
        f"hermeticity={hermeticity_passed} "
        f"subcommands={subcommand_passed} "
        f"pass={passed} deviations={len(failures)} "
        + ("GREEN" if not failures else "RED")
    )
    return 0 if not failures else 1


sys.exit(main())
