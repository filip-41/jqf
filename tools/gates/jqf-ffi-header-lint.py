#!/usr/bin/env python3
"""Two-way signature check: the checked-in C header vs the Rust ABI.

`jqf-sdk-ffi/include/jqf_sdk_ffi.h` is what a C embedder compiles against.
A drifted signature is memory corruption, not a Rust compile error. This
lane fails when:

  * an exported `extern "C" fn` in `jqf-sdk-ffi/src/lib.rs` is missing from
    the header, or differs in parameter count, pointer depth, base type, or
    return type;
  * a `jqf_*` function in the header is not an exported entry point;
  * `cc` or `c++` reject `#include "jqf_sdk_ffi.h"` (`-fsyntax-only`).

Base types are compared, so `int32_t` vs `uint32_t` fails. Pointer
const-ness is not (same ABI). `u8` and `i8`/`char` match: the header
spells program bytes `const char *` and input bytes `const uint8_t *`,
both `*const u8` in Rust.

The Rust signatures are the authority.

Usage: tools/gates/jqf-ffi-header-lint.py
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LIB = ROOT / "jqf-sdk-ffi" / "src" / "lib.rs"
HEADER = ROOT / "jqf-sdk-ffi" / "include" / "jqf_sdk_ffi.h"

RUST_BASE = {
    "c_void": "void",
    "c_int": "i32",
    "c_uint": "u32",
    "c_char": "i8",
    "i32": "i32",
    "u32": "u32",
    "u8": "u8",
    "u16": "u16",
    "u64": "u64",
    "i64": "i64",
    "usize": "usize",
}
C_BASE = {
    "void": "void",
    "int32_t": "i32",
    "uint32_t": "u32",
    "int64_t": "i64",
    "uint64_t": "u64",
    "uint16_t": "u16",
    "uint8_t": "u8",
    "size_t": "usize",
    "char": "i8",
}


def _close_paren(text, open_paren):
    depth = 0
    for k in range(open_paren, len(text)):
        if text[k] == "(":
            depth += 1
        elif text[k] == ")":
            depth -= 1
            if depth == 0:
                return k
    return None


def _top_level_segments(body):
    """Declaration pieces split on commas at paren depth zero."""
    depth = 0
    segments = []
    start = 0
    for i, ch in enumerate(body):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        elif ch == "," and depth == 0:
            segments.append(body[start:i])
            start = i + 1
    segments.append(body[start:])
    return [segment.strip() for segment in segments if segment.strip()]


def _snake(name):
    return re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", name).lower()


def _shape_match(rust, header):
    r_depth, r_base = rust
    h_depth, h_base = header
    if r_depth != h_depth:
        return False
    if r_base == h_base:
        return True
    # program bytes are `const char *` in the header and `*const u8` in Rust.
    return r_depth > 0 and {r_base, h_base} <= {"u8", "i8"}


def _rust_shape(type_spelling):
    spelling = re.sub(r"\s+", "", type_spelling)
    depth = 0
    while True:
        if spelling.startswith("Option<") and spelling.endswith(">"):
            spelling = spelling[7:-1]
            continue
        if spelling.startswith("*const"):
            depth += 1
            spelling = spelling[6:]
            continue
        if spelling.startswith("*mut"):
            depth += 1
            spelling = spelling[4:]
            continue
        break
    if not spelling:
        return (depth, "?")
    if spelling[0].isupper():
        return (depth, _snake(spelling))
    return (depth, RUST_BASE.get(spelling, spelling))


def _c_shape(segment):
    tokens = re.findall(r"const|[A-Za-z_][A-Za-z0-9_]*|\*", segment)
    if tokens and re.match(r"^[A-Za-z_]", tokens[-1]) and tokens[-1] not in C_BASE and tokens[-1] != "const":
        if any(token in C_BASE or token[0:4] == "jqf_" for token in tokens[:-1]):
            tokens = tokens[:-1]
    depth = tokens.count("*")
    bases = [token for token in tokens if token not in ("const", "*")]
    if not bases:
        return (depth, "void")
    base = bases[-1]
    return (depth, C_BASE.get(base, base))


def _attached_attrs(text, extern_pos):
    """`#[...]` immediately on this item, not the previous one.

    Walks back over `pub` / `unsafe` / whitespace, then over consecutive
    attributes. Stops at the first non-attribute so a neighbour's
    `no_mangle` cannot leak across a missing blank line.
    """
    head = text[:extern_pos]
    while True:
        trimmed = re.sub(r"(?:pub|unsafe|\s)+$", "", head)
        if trimmed == head:
            break
        head = trimmed
    attrs = []
    while True:
        head = head.rstrip()
        if not head.endswith("]"):
            break
        depth = 0
        open_bracket = None
        for j in range(len(head) - 1, -1, -1):
            if head[j] == "]":
                depth += 1
            elif head[j] == "[":
                depth -= 1
                if depth == 0:
                    open_bracket = j
                    break
        if open_bracket is None or open_bracket == 0 or head[open_bracket - 1] != "#":
            break
        attrs.append(head[open_bracket - 1 :])
        head = head[: open_bracket - 1]
    return "".join(reversed(attrs))


def rust_sigs(text):
    """`{name: (params, ret)}` for `#[no_mangle] extern "C" fn` entry points."""
    sigs = {}
    fn = re.compile(r"\bextern \"C\" fn\s+(\w+)\s*\(")
    for m in fn.finditer(text):
        name = m.group(1)
        if "no_mangle" not in _attached_attrs(text, m.start()):
            continue
        close = _close_paren(text, m.end() - 1)
        if close is None:
            raise SystemExit(f"ffi-header-lint: unbalanced signature: {name}")
        params = [_rust_shape(seg.split(":", 1)[1]) for seg in _top_level_segments(text[m.end() : close])]
        after = text[close + 1 : text.find("{", close)]
        ret_m = re.search(r"->\s*([^/{]+)", after)
        ret = _rust_shape(ret_m.group(1)) if ret_m else (0, "void")
        sigs[name] = (params, ret)
    return sigs


def header_sigs(text):
    """`{name: (params, ret)}` for `jqf_*` function declarations."""
    sigs = {}
    decl = re.compile(
        r"^\s*((?:[A-Za-z_][A-Za-z0-9_]*\s+)+)"
        r"([A-Za-z_][A-Za-z0-9_]*)\s*\(",
        re.MULTILINE,
    )
    for m in decl.finditer(text):
        name = m.group(2)
        if not name.startswith("jqf_"):
            continue
        close = _close_paren(text, m.end() - 1)
        if close is None:
            continue
        body = text[m.end() : close]
        params = [] if body.strip() in ("", "void") else [_c_shape(seg) for seg in _top_level_segments(body)]
        ret = _c_shape(m.group(1))
        sigs[name] = (params, ret)
    return sigs


def _describe(shape):
    depth, base = shape
    return base + "*" * depth


def main():
    rust = rust_sigs(LIB.read_text())
    header = header_sigs(HEADER.read_text())

    problems = []
    for name, (params, ret) in sorted(rust.items()):
        if name not in header:
            problems.append(
                f"  {name} is exported from lib.rs but not declared in "
                f"{HEADER.relative_to(ROOT)}"
            )
            continue
        h_params, h_ret = header[name]
        if len(params) != len(h_params):
            problems.append(
                f"  {name}: lib.rs has {len(params)} params, the header has {len(h_params)}"
            )
            continue
        for i, (r_shape, h_shape) in enumerate(zip(params, h_params), 1):
            if not _shape_match(r_shape, h_shape):
                problems.append(
                    f"  {name} param {i}: lib.rs {_describe(r_shape)}, "
                    f"header {_describe(h_shape)}"
                )
        if not _shape_match(ret, h_ret):
            problems.append(
                f"  {name} return: lib.rs {_describe(ret)}, header {_describe(h_ret)}"
            )
    for name in sorted(header):
        if name not in rust:
            problems.append(
                f"  {name} is declared in the header but not exported from lib.rs"
            )

    if problems:
        print("ffi-header-lint: the header drifted from the Rust signatures:", file=sys.stderr)
        for problem in problems:
            print(problem, file=sys.stderr)
        print(
            "ffi-header-lint: fix jqf-sdk-ffi/include/jqf_sdk_ffi.h to match "
            "jqf-sdk-ffi/src/lib.rs",
            file=sys.stderr,
        )
        return 1

    include = str(HEADER.parent)
    compile_problems = []
    for compiler, lang in (("cc", "c"), ("c++", "c++")):
        try:
            completed = subprocess.run(
                [compiler, "-fsyntax-only", "-I", include, "-x", lang, "-"],
                input='#include "jqf_sdk_ffi.h"\n',
                text=True,
                capture_output=True,
                check=False,
            )
        except FileNotFoundError:
            compile_problems.append(f"  {compiler} not found")
            continue
        if completed.returncode != 0:
            compile_problems.append(
                f"  {compiler} -fsyntax-only failed:\n{completed.stderr.strip()}"
            )
    if compile_problems:
        print("ffi-header-lint: header failed to compile:", file=sys.stderr)
        for problem in compile_problems:
            print(problem, file=sys.stderr)
        return 1

    print(
        "ffi-header-lint: fresh (%d entry points, %d header declarations, C/C++ syntax-only)"
        % (len(rust), len(header))
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
