# jqf Contracts

Invariants for this crate and for hosts. Type overview and examples live
in [README.md](README.md).

This crate does not parse a format grammar, evaluate a program, or pick
an SDK drive. It parses argv, opens host I/O, walks the route chain, and
exits with a class. The binary name is `jqf`; the directory is `jqf-cli/`.

## Arguments

- `parse_arguments` is the only argv entry. `run` consumes the resolved
  `CliArguments` and never re-parses or re-derives them.
- Help text and acceptance tables are one table. A spelling the help
  advertises is a spelling the parser accepts.
- Route facts at plan time come from each codec's
  `route_capabilities()` through the catalog. They are not re-declared
  as `match` arms in the CLI.
- A named file's format follows its extension. Stdin is JSON. An
  explicit `--input-format` / `--output-format` always wins. Content is
  never sniffed.
- `--args` / `--jsonargs` consume every remaining positional. Options
  still parse after them.
- The first `--arg` / `--argjson` / `--slurpfile` / `--rawfile` of a
  name wins. A program binder always beats a CLI binding.

## Config

- Every flag carries a tier. Tier P may default from a file. Tier S is
  argv-only. An unclassified flag is a compile error
  (`args::tests::every_flag_carries_a_tier`).
- Only one discovery file is read — the nearest — and it overlays the
  global file. `--config` replaces both. Unknown keys warn and are
  ignored.
- `--no-config` and a non-empty `JQF_NO_CONFIG` disable reading. Gates
  and tests set `JQF_NO_CONFIG`.

## Routes

- `plan::resolve` builds an ordered chain from static request facts.
  Each candidate serves, or `Declined`s and publishes nothing.
- `Route::Execute` is always last and never declines.
- A record input owns its physical stream, including `-n` / `-s`.
  `--diff` is the exception: it owns two files and never touches stdin.
- `--follow` serves by itself. The whole-input lanes behind it are
  unreachable.
- A non-seekable stdin with a record-route input takes `Stream` for the
  per-record model only. `-s`, `-R`, edit, and diff keep their own
  models.
- `-n` / `-R` take precedence over `--stream`. The event route is not
  on the chain; `-n --stream` is the streamed null-first input-family
  lane, not a refusal.
- `--header` is a whole-stream fact. `--follow` and a non-seekable
  stdin pipe refuse it.

## Publication

- There is no output ceiling and no default memory ceiling. The RSS
  governor is the operator's physical bound. `--max-memory-bytes` is
  opt-in.
- An atomic write is temp-file + fsync(data) + rename + fsync(parent).
  Process failure leaves the original file. `--no-atomic` writes the
  original inode.
- Colour is a rendering of bytes that are already decided. Off, the
  sink writes the bytes it received. On, the only added bytes are SGR
  spans. Colour renders JSON-family output only. `--edit` / `--diff` /
  `--in-place` never colour.
- `-C` forces colour on. `-M` forces colour off and is applied last.
  Default is TTY and `NO_COLOR` unset or empty.
- Bindings and diff files are read before stdin.

## Exit

- Usage is 2, compile is 3, runtime is 5. A malformed input is 5.
- An unreadable positional file prints one line and continues. The
  process then exits 2 unless a compile error already won.
- A per-value runtime error is reported once and is not reprinted at
  exit (`CliFailure::Reported`).
- `halt` / `halt_error` exit with the named status. `halt_error` writes
  the value compact, without the error frame.
- A failed stderr write never aborts a run.

## Allocator and RSS

- The global allocator is declared only in this crate. Library crates
  stay allocator-agnostic. The default is mimalloc;
  `--no-default-features` restores the platform allocator.
- The RSS governor's decision lives here. `jqf-resource` carries only
  the `MemoryExceeded` observation seam.
- On Linux the enforcement number is OS RSS from `/proc/self/statm`.
  Elsewhere it is the allocator's footprint. The default ceiling is
  80% of effective memory. `--max-rss N|N%|0` is the dial.

## Discovery

- Every enumeration the CLI prints is generated from the registry that
  owns the fact. `--list-builtins` matches `null | builtins`.
- `--explain-code` reads the generated diagnostic-code table. This
  crate does not author that table.
