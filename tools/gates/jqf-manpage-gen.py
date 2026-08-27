#!/usr/bin/env python3
"""jqf.1 man page generator.

The man page is generated from `jqf --help`, which is itself built from the
same acceptance tables the parser reads — so the roff text cannot drift from
the flags the binary accepts. Regenerate with `make manpage` (reads $(JQF))
whenever the help surface changes.

Run: python3 tools/gates/jqf-manpage-gen.py --jqf target/release/jqf
Output: docs/jqf.1 (committed; the release tree ships it)
"""
import argparse
import re
import sys

from jqfgate import proc


def esc(text):
    """Escape roff specials: backslash first, then a leading dot/hyphen."""
    text = text.replace("\\", "\\\\")
    if text.startswith(".") or text.startswith("'"):
        text = "\\&" + text
    return text


def render(help_text, version):
    lines = help_text.rstrip("\n").splitlines()
    sections = []          # (title, [(tag|text, content)])
    for raw in lines:
        if not raw.strip():
            continue
        m = re.match(r"^([A-Za-z][A-Za-z ]*):$", raw)
        if m:
            sections.append((m.group(1), []))
            continue
        if not sections:
            sections.append(("__top__", []))
        # A tag is a line with EXACTLY two leading spaces then non-space
        # (subcommand name, option spelling). Description continuation lines
        # are indented six spaces and fall through to text.
        tag = re.match(r"^  (\S.*)$", raw)
        if tag:
            sections[-1][1].append(("tag", tag.group(1)))
        else:
            sections[-1][1].append(("text", raw.strip()))

    out = []
    out.append('.TH JQF 1 "" "jqf" "' + version + '"')
    out.append('.SH NAME')
    out.append('jqf \\- a jq-compatible document processor')
    for title, items in sections:
        if title == "__top__":
            # Fixed template shape: the first two lines are the usage forms,
            # the rest is the formats paragraph.
            out.append('.SH SYNOPSIS')
            for _, t in items[:2]:
                out.append('.B ' + esc(t))
                out.append('.br')
            out.append('.SH DESCRIPTION')
            for _, t in items[2:]:
                out.append(esc(t))
            out.append(
                ".PP This manual page is generated from the binary's "
                "\\fB--help\\fR surface, which is built from the same "
                "acceptance tables the parser reads; it cannot drift from "
                "the flags the binary accepts.")
            continue
        out.append('.SH ' + title.upper().replace(' ', '-'))
        for kind, t in items:
            if kind == "tag":
                out.append('.TP')
                out.append('.B ' + esc(t))
            else:
                out.append(esc(t))
    out.append('.SH SEE-ALSO')
    out.append('The full generated discovery surfaces, all from the same tables:')
    out.append('.TP')
    out.append('.B jqf --help builtins, --help flags, --help codes, --help mismatch')
    out.append('Topic pages for the builtin list, the flag table, the diagnostic')
    out.append('codes, and the mismatch dial.')
    out.append('.TP')
    out.append('.B jqf --help generators, --help facts, --help diff')
    out.append('Topic pages for the engine-namespace generators, node/value facts')
    out.append('and markup attributes, and the --diff lane.')
    out.append('.TP')
    out.append('.B jqf --help-format \\fIformat\\fR')
    out.append('One focused page per input/output format and its dialects.')
    out.append('.TP')
    out.append('.B jqf --list-formats, --list-builtins')
    out.append('Machine-readable enumerations of the format table and the builtin registry.')
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--jqf", required=True, help="path to the jqf binary")
    ap.add_argument("--output", default="docs/jqf.1")
    args = ap.parse_args()
    help_text = proc.run_gate(args.jqf, ["--help"], text=True, check=True).stdout
    version = proc.run_gate(args.jqf, ["--version"], text=True, check=True).stdout.strip()
    out = render(help_text, version)
    with open(args.output, "w") as f:
        f.write(out)
    print(f"manpage: wrote {args.output} ({len(out.splitlines())} roff lines, {version})")


if __name__ == "__main__":
    main()
