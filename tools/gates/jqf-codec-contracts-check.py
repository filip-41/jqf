#!/usr/bin/env python3
"""The codec-contracts gate: every code identifier named in CONTRACTS.md
resolves in the tree.

`jqf-codec/CONTRACTS.md` is the recipe a new codec author is told to follow.
This lane greps every backtick identifier in the document against the tracked
sources so a name the code no longer has fails the gate instead of the next
author.

Resolution rules (deliberately a cheap grep, not a type-checker):
  * the corpus is the tracked `.rs` sources only — a harness that QUOTES a
    dead identifier in a probe or fixture comment must not be able to
    satisfy it (the teeth probe for this lane itself names the deleted
    `AccessSession::poll`);
  * a plain `TypeName` / `fn_name` resolves if the word appears in the
    corpus;
  * a `Type::member` path resolves if the full path appears as a word, or —
    for the common case where the doc names a method/variant by path but the
    source declares it bare — if the member appears inside the type's own
    declaring body (`trait` body for methods, `enum` body for variants);
  * Rust keywords used as prose (`unsafe`, `deny`, `dyn`, `match`) are skipped.

Usage: tools/gates/jqf-codec-contracts-check.py [CONTRACTS.md]
Exit 0 when every identifier resolves; exit 1 listing the missing ones.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DOC = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "jqf-codec" / "CONTRACTS.md"

# Rust keywords used as prose in the doc.
KEYWORDS = {"deny", "dyn", "match", "unsafe"}

IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*$")


def mentions(text: str, word: str) -> bool:
    return re.search(r"\b" + re.escape(word) + r"\b", text) is not None


def tracked_sources() -> list[Path]:
    files = subprocess.run(
        ["git", "-C", str(ROOT), "ls-files", "*.rs"],
        capture_output=True, text=True, check=True,
    ).stdout.split()
    return [ROOT / f for f in files]


def extract_identifiers() -> set[str]:
    spans = re.findall(r"`([^`]+)`", DOC.read_text())
    identifiers: set[str] = set()
    for span in spans:
        token = span.strip()
        if not token:
            continue
        # Paths, crates-with-dashes, flags, and prose spans are not identifiers.
        if any(c in token for c in " /\\-") and not re.match(r"^[A-Za-z_]\w*$", token):
            continue
        token = re.sub(r"[\*\(\)].*$", "", token)  # `try_new_source*`, `fn()`
        if IDENT.match(token):
            identifiers.add(token)
    return identifiers


def member_in_declaring_body(text: str, head: str, member: str) -> bool:
    """True when `member` is declared inside a `trait head { ... }` (methods)
    or `enum head { ... }` (variants) body, brace-balanced from the
    declaration's opening brace. `impl` bodies are deliberately excluded:
    every codec's `impl AccessSession for X` block coexists with the parse
    machine's own unrelated `fn poll` in the same file, which is exactly the
    false-positive that would let a deleted trait method pass."""
    kind = "trait" if member[0].islower() else "enum"
    for m in re.finditer(r"\b" + kind + r"\s+" + re.escape(head) + r"\b", text):
        brace = text.find("{", m.end())
        if brace == -1:
            continue
        depth = 0
        for i in range(brace, len(text)):
            if text[i] == "{":
                depth += 1
            elif text[i] == "}":
                depth -= 1
                if depth == 0:
                    body = text[brace:i]
                    if member[0].islower():
                        if f"fn {member}" in body:
                            return True
                    elif mentions(body, member):
                        return True
                    break
    return False


def resolve(token: str, corpus: str, by_file: list[str]) -> bool:
    if token in KEYWORDS:
        return True
    if "::" not in token:
        return mentions(corpus, token)
    head, *_, member = token.split("::")
    if mentions(corpus, token):
        return True
    # A `Type::member` path the source declares bare: the member must appear
    # inside the type's own declaring body (see member_in_declaring_body), so
    # neither a doc comment mentioning the type elsewhere nor an unrelated
    # same-name method in the file can satisfy a deleted member.
    return any(member_in_declaring_body(t, head, member) for t in by_file)


def main() -> int:
    identifiers = extract_identifiers()
    sources = tracked_sources()
    by_file = [p.read_text(errors="replace") for p in sources]
    corpus = "\n".join(by_file)
    missing = sorted(i for i in identifiers
                     if not resolve(i, corpus, by_file))
    if missing:
        print(f"contracts-check: identifiers={len(identifiers)} "
              f"missing={','.join(missing)}")
        for name in missing:
            print(f"  {name} does not resolve anywhere in {ROOT}")
        return 1
    print(f"contracts-check: identifiers={len(identifiers)} missing=0 GREEN")
    return 0


if __name__ == "__main__":
    sys.exit(main())
