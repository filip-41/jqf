//! The CLI argument surface: the format/dialect model, the parsing of it, and the help text.
//!
//! Everything the user can type is modeled and rejected here — formats, dialects, bindings, input-model flags, and the
//! record/parallel policy — before any stdin byte or route decision. `run` consumes the resolved [`CliArguments`]; it
//! never re-parses or re-derives them.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use jqf_codec_json::JsonIndent;
use jqf_codec_json::ndjson::{NdjsonProfile, NdjsonTerminator};
pub(crate) use jqf_runtime::records::RecordInputKind;
use jqf_runtime::records::{WORKER_HARD_CAP, WorkerRequest};
use jqf_sdk::{CodecCatalog, RegistryFailure};

use crate::DEFAULT_MAX_MEMORY_BYTES;
use crate::errors::CliFailure;

/// The help template. The `{input_formats}`, `{input_dialects}`, `{output_formats}`, and `{output_dialects}` slots are
/// filled by [`help_text`] from the same acceptance tables [`parse_format`]/
/// [`parse_input_dialect`]/[`parse_output_dialect`] read, so what the help advertises and what the parser accepts are
/// one table. The route facts the CLI needs at plan time — the input model (one document per source vs a stream of
/// adjacent values), the record route, and the edit lane — are read from each codec's `route_capabilities` declaration
/// through the catalog, never re-declared as `match` arms here. The placeholder texts occur nowhere else in the
/// template.
pub(crate) const HELP_TEMPLATE: &str = "\
Usage: jqf [OPTIONS] [PROGRAM]
       jqf serve [SERVE-OPTIONS]

A named FILE's format follows its extension; stdin is JSON/RFC 8259 unless you
say otherwise, and --input-format/--output-format always win.

Subcommands:
  serve
      Resident daemon: `jqf serve --listen <unix-socket|host:port> PROGRAM`
      binds a listener, compiles PROGRAM once, and accepts connections that
      speak NDJSON in and receive NDJSON out — one resident compiled program,
      one record stream per connection, the same drive `--follow` owns fed
      incrementally. A connection is a session: records are framed on newline
      boundaries and held until their terminator arrives, per-value errors are
      reported on the daemon's stderr and never kill the session or the
      daemon, and the held tail is finalized when the client closes (the
      recovering dialect's law). A unix-socket listener's permissions are its
      auth; TCP binds to whatever host:port is named, and an EMPTY host
      binds the loopback default — state the trust model plainly: a unix
      socket is filesystem-authenticated (its mode is the ACL), a TCP
      listener is trusted-network-only by design (no auth, no TLS — do not
      bind a hostile network). A per-connection READ/IDLE TIMEOUT is armed by
      default (`--read-timeout SECONDS`, default 60, 0 disables): a
      connection that sends nothing for that long ends its session cleanly
      and the daemon accepts the next one, so a dribbling client cannot hold
      the serial accept loop. Runs until a signal.
      PROGRAM defaults to the identity filter. `--max-rss` is the RSS
      governor's dial, exactly as on the ordinary request; the governor
      watches the WHOLE daemon.

Options:
  --input-format {input_formats}
  --input-dialect {input_dialects}
      html.fragment@1 parses the input as one HTML FRAGMENT under the
      WHATWG fragment parsing algorithm: the fragment is placed in a div
      context element (the v1 FIXED default context; a per-invocation
      context option is the recorded follow-on), so fragment-shaped input
      with no document wrapper — plain text, partial markup, an element
      soup — is accepted exactly where the document dialect would recover
      it differently. The fragment's elements project as ordinary html
      document values; html.fragment-serialize@1 output stays reserved.
  --old-format {input_formats}
  --new-format {input_formats}
      The per-side formats of `--diff OLD NEW` . Each defaults
      to `--input-format`; name a side only when it differs, so a
      cross-format diff says exactly which side is which. Valid only with
      `--diff`.
  --csv-delimiter BYTE
      Field delimiter for CSV input (default: comma; \t for a tab — the
      TSV dialect). One ASCII byte the CSV codec accepts; valid
      only with CSV input (the registered `tsv` format binds its own tab
      delimiter, so the dial is rejected there).
  --header
      Read row 1 as a HEADER, not data: every later row decodes as an
      object keyed by it (`.salary` instead of `.[3]`), and CSV/TSV output
      writes the header row back. The one-word spelling of
      `--input-dialect csv.rfc4180-header@1` (plus its encode mirror
      `csv.jqf-rfc4180-header@1`), which stay the explicit forms.
      The array dialect remains the default: row 1 of a headerless file
      is data, and no content test tells a header from data — so the
      header is something you SAY, never something jqf guesses. Valid
      only when a delimited side exists; a live tail cannot serve it (the
      header is a whole-stream fact the refilling drive cannot carry, so
      `--follow` and a non-seekable stdin pipe refuse it and name the
      redirect that reads whole).
  --output-format {output_formats}
      (`-o FORMAT` is the short spelling; a `--output PATH` whose extension a
      registration declares infers the format when no `--output-format` is
      pinned.)
  --output-dialect {output_dialects}
  --ndjson-terminator lf|crlf
      NDJSON output terminator (default: lf; valid only with NDJSON output)
  --render-header present|absent
      Render table header policy (default: present; GFM requires present;
      valid only with render output)
      (The render dial set is frozen at these four: --render-header,
      --render-width, --render-shape, --render-max-width.)
  --render-width western|cjk
      Display width of ambiguous-width characters for table layout (default:
      western; valid only with render output)
  --render-shape plain|table|tree
      The render.terminal@1 extraction shape (default: tree; valid only with
      render output)
  --render-max-width N
      Sampled-layout maximum display-cell width per column; 0 disables
      wrapping (default: 0; valid only with render output)
  -n, --null-input
      Run the filter once with null as the input instead of once per input
      value. The input stream is not read unless the program calls the input
      family (input/inputs/input_filename/input_line_number).
  -r, --raw-output
      Print a root string item without quotes or escapes (jq's -r). Other
      items, and strings nested inside containers, print normally.
  -R, --raw-input
      Read the input as raw strings: one JSON string per line (lines split on
      newline only, so a carriage return stays in the line). With -s, the
      whole input becomes one string. Invalid UTF-8 becomes U+FFFD.
  -s, --slurp
      Collect every parsed input value into one array and run the filter once
      over it. With -n, -n wins; with -R, -R wins (the whole input is one
      string, not an array).
  -f, --from-file FILE
      Read the program from FILE instead of the positional argument.
  -L, --library-path DIR
      Prepend DIR to the module search chain `import`/`include` use (jq's
      -L). Repeatable; each DIR is searched in the order given, ahead of
      the built-in defaults (~/.jq, then the binary's own ../lib/jq and
      ../lib).
  --arg NAME VALUE
      Bind $NAME to the string VALUE (jq's --arg). The first binding of a
      duplicate name wins.
  --argjson NAME VALUE
      Bind $NAME to VALUE parsed as exactly one JSON value (jq's --argjson).
      A parse failure is a usage error. The first binding of a duplicate name
      wins.
  --slurpfile NAME FILE
      Bind $NAME to the array of every JSON value read from FILE (jq's
      --slurpfile). The file is read before stdin, and a bad file is a usage
      error. The first binding of a duplicate name wins.
  --rawfile NAME FILE
      Bind $NAME to the raw string contents of FILE (jq's --rawfile). Invalid
      UTF-8 becomes U+FFFD per byte. The first binding of a duplicate name
      wins.
  --schema FILE
      Validate every input value against the value-schema document in FILE
      (a jqf extension beyond jq; the competition is check-jsonschema
      --schemafile). FILE is read as exactly one strict JSON value — the
      schema is always JSON, whatever --input-format the data uses — and is
      bound as $__schema. The program (default .) runs only on values that
      validate; a value that fails publishes its ordered schema_errors array
      RAW to stderr and the request exits 3 (the value-schema failure class,
      distinguishable from a usage error's 2 and a decode error's 5; 3 is
      shared with the compile class, and both mean the request was rejected).
      Earlier valid values' outputs stand. Cannot be combined with --stream,
      --edit, --diff, or --in-place (none of them has a per-input-value
      stream for validation to gate).
  --args
      Every REMAINING positional argument (a file or value after this point)
      is a positional STRING, available as $ARGS.positional (jq's --args).
      jq's --arg-family bindings stay $ARGS.named. Options still parse after
      it, exactly as jq's do.
  --jsonargs
      Like --args, but every remaining positional argument is parsed as
      exactly one JSON value. A parse failure is a usage error, matching
      --argjson's message. Both flags append to the same $ARGS.positional in
      order; each value is parsed under the mode active when it was seen.
  --stream
      Parse each input value in streaming fashion: the filter runs once per
      [path, leaf] item that jq's tostream would emit. For a JSON input this
      is BOUNDED: the document is parsed incrementally and each event is
      published as it lands, so a document larger than memory streams with
      only the path stack resident. With --slurp the stream items are collected into one
      array first. -n and -R change the input model and take precedence,
      exactly as jq's do.
  --stream-errors
      Implies --stream and reports parse errors as [message, path] events in
      the stream instead of aborting, exactly as jq's --stream-errors does:
      the request exits 0 with the error events interleaved, and parsing
      resumes at the next line after a refusal.
  --seq
      Select RFC 7464 (application/json-seq) for BOTH input and output: the
      input is RS-framed (a raw RS always boundaries, even mid-string) and
      read under the flag-scoped recovering profile — a malformed unit is one
      advisory issue and the stream continues, never a fatal parse error,
      exactly jq's --seq — and every output item is prefixed with RS and
      followed by a newline, pretty-printed by default. An explicit
      --input-format or --output-format overrides that side. jq's -r
      exception holds: a root string the raw arm prints gets no RS prefix.
  --follow
      Do not stop at end-of-file: a regular file is read to EOF and then
      polled for growth, a pipe blocks until data arrives and finalizes its
      trailing record at EOF. Records are processed AS THEY ARRIVE, never read
      whole: newline-framed NDJSON records are decoded and published per read,
      so a log file can be monitored with `jqf --follow '.level' app.ndjson`.
      The input is framed on newline boundaries and treated as ndjson.recovering@1
      unless an explicit ndjson input dialect is named; a truncated trailing
      record is HELD until more bytes complete it, never reported as a fault.
      (Without --follow, a pipe or FIFO stdin already streams per record by
      default; the flag's remaining job is exactly the polling.) CSV input is
      followed too: the record cut is the RFC 4180 quote-state walk (a record
      ends at a line feed only outside a quoted field), and a truncated
      trailing record is HELD until its terminating line feed arrives. The
      headered CSV dialect is not served by a live tail: the header row is a
      whole-stream fact the incremental drive cannot carry across refills
      (redirect stdin from a regular file to read it whole). `-n` is
      allowed for an input-family program (`inputs`/`input`) so the
      live-window shape works (`-n --follow 'ewma(0.2; inputs|.ms)'`); a
      non-input `-n --follow` is refused after compile. Cannot be combined
      with -R/-s, --stream, --edit, --diff, --in-place, or --output.
  -e, --exit-status
      Exit 0 if the last output value is truthy (neither false nor null),
      1 if it is false or null, and 4 if no value was ever output. jq's
      boolean-test scripting idiom. The exit-status reads the OUTPUT VALUE,
      so -r never confuses the string \"false\" with the boolean. Reads the
      serial drive, so a -e request plans serial (the parallel relay
      publishes bytes, not values).
  -S, --sort-keys
      Emit every object's keys in ascending byte order, recursively (jq's
      --sort-keys). JSON output only.
  -j, --join-output
      Print results back-to-back with no newline between items, and treat a
      root string item as raw (jq's -j is exactly -r plus no newline). JSON
      output only.
  --raw-output0
      Like -r, but terminate every item with a NUL byte instead of a newline
      (overrides -j's empty terminator). A root string item that itself
      contains a NUL byte is a runtime error rather than a silently
      corrupted terminator. JSON output only.
  -a, --ascii-output
      Escape every non-ASCII character as \\uXXXX (a supplementary character
      as its surrogate pair). Takes precedence over -r for a root string:
      with both, the string prints quoted and escaped, exactly as jq prints
      `-ra` and `-ja`. JSON output only.
  --seed N
      Seed the rand family (rand, randint, choice, sample, shuffle) from N,
      a jqf extension beyond jq. Every otherwise-random draw in the request
      then advances from N instead of fresh entropy, so a repeated run with
      the same seed is byte-identical and a different seed answers
      differently. rand(seed) is unaffected: it is already deterministic
      from its own argument.
  --with-source
      Emit the retained source of the origin document instead of the
      canonical form: a byte-identical echo of the input that produced the
      value. A run whose value has no retained source (a computed or edited
      value) is a clean typed error naming the missing retention — never a
      silently thinner file. jqft/jqfb output only; implies nothing about
      the input format. v1 defines exactly one retention level (this echo);
      the former `--with-presentation` spelling was cut because it aliased
      the same bytes.
  --strictness error|warn|strict|lenient
      The strictness dial (a jqf extension beyond jq; error is the default
      and is byte-identical to today). Warnings — per-value RawNulByte
      failures, record advisories, projection losses — never force the exit
      class at `error`; `warn` surfaces them; `strict` promotes any of them
      to the failure class (exit 5) at run end. `lenient` relaxes the
      strict-JSON DECODE refusals to the jq-compatible lenient reader
      instead: leading-zero / plus / dot number spellings (`01`, `+1`, `.5`,
      `1.`) decode as their canonical value, and a huge-exponent literal
      clamps to the widest finite binary64 (`1e999999999999999999999` ->
      `1.7976931348623157e+308`) instead of refusing; `snan` is accepted at
      every position (a NaN is a NaN).
      Invalid UTF-8 in a string still refuses under `lenient`.
      Lenient requests plan serial. The FATAL set (raises, compile
      rejections, encode refusals, resource failures) forces its class at
      every level; halt keeps its own status. (Distinct from
      --mismatch-policy: strictness governs decode/encode failures and
      advisories, mismatch governs the value-answering sites.)
  --mismatch-policy lenient|warn|strict
      The mismatch dial (a jqf extension beyond jq; lenient is the default
      and IS jq, byte for byte). A mismatch is a site where jq answers a
      VALUE where the query assumed data: a missing object key, an
      out-of-range index, a field or index into null, null as the additive
      identity, a getpath/path miss, a cross-kind ordering comparison, an
      assignment that auto-vivifies a container, a delete path that names
      nothing, a slice bound that clamps, and null answered as the empty      container (length/reverse). warn answers jq's value and exit code and
      prints a capped, aggregated per-cell report on stderr after the run.
      strict turns each mismatch into a raise (exit 5). A cell the program
      marked as expected fires no event: .b // \"x\", .b?, and a try around
      the site are all intent markers. Requests under warn/strict plan
      serial. --strictness is the sibling dial: strictness promotes WARNING
      severities (projection losses, record advisories) to failures, while
      this dial decides what a mismatch EVENT does; neither changes the
      other's cell set.
  --json-facts
      Project attached facts into the JSON output (see `--help facts`):
      markup elements become xq-style trees (element name as key,
      attributes as `@attr`, text as `#text`, repeated elements as arrays);
      comments and tags appear as `@comment`/`@tag` keys; a fact-bearing
      scalar or array is wrapped as `{\"value\": ...}`. Data keys win on
      collision. The projection is lossy, not a round-trippable encoding.
      JSON output only; cannot be combined with --edit/--in-place/
      --diff/--stream. ON BY DEFAULT for xml/html input to JSON output:
      markup keeps element names and attributes as facts, so the bare value
      of <r a=\"1\"><c>x</c><c>y</c></r> is [[\"x\"],[\"y\"]] and every name
      the document carries is missing from the answer.

  --types-as-strings
      Read every extended temporal kind as its canonical text (a plain
      string), so a jq program sees only the six jq types. The document
      still stores the rich kinds, so encoding round-trips untouched
      values; a REBUILT value (materialized and re-inserted) re-encodes as
      its string form, and `--edit` on a date downgrades it to a quoted
      string.
  --no-json-facts
      Answer the bare value instead, with markup names and attributes left
      to the fact accessors (.@name, .&attr). The two dials render the same
      source differently and their ROOT-level paths differ — a path that
      works under one dial does not necessarily work under the other; probe
      with `.` first, and read the runtime hint a missed path prints (it
      names the model in play).
  --unbuffered
      Flush stdout after every item instead of batching (jq's streaming UX
      law). Observable on any streaming input — a non-seekable stdin pipe or
      FIFO, or --follow — where the per-item flush makes a live tail visible
      as each record lands. Whole-input models read the source before
      parsing, so nothing is published early enough for the flush cadence to
      matter.
  -C, --color-output        colorize JSON output
  -M, --monochrome-output   disable colored output
      jq's own switch law: -C forces color on (even
      under NO_COLOR), -M forces it off and always wins, and with neither
      the destination's terminal-ness decides, a non-empty NO_COLOR turning
      the default off. JQ_COLORS sets the eight-field palette
      (null:false:true:numbers:strings:arrays:objects:keys; a malformed
      value falls back to the defaults with a stderr note). Color is a
      RENDERING of bytes already decided: it applies to JSON-family output
      only, never to --edit/--diff/--in-place bytes.
  -c, --compact-output
      One line per output value, no structural whitespace
  --indent N
      Spaces of indentation per level, -1 to 7 (default: 2). -1 selects tabs,
      and 0 keeps every line break but drops the indentation
  --tab
      One tab of indentation per level
      The three indent switches may repeat; the last one given wins. They
      apply to JSON output only -- an NDJSON record always occupies one line
  --max-memory-bytes N
      Accounted memory ceiling in bytes: it bounds the request ledger's own
      tracked residency -- a determinism/portability bound, the same bytes
      on every machine -- not the process's real resident set (the
      physical bound is --max-rss, default ON). There is NO ceiling unless
      this flag names one: memory accounting always runs, but by default no
      admission is ever denied. The ceiling bounds the request's own
      accounted residency,
      read back as COST_SNAPSHOT's current_rss under --diagnostics.
  --max-rss N|N%|0
      Physical memory ceiling: the process's real resident set (on Linux
      the OS working set read from /proc/self/statm; elsewhere the
      allocator's own footprint — not the accounted ledger). N is an
      absolute byte ceiling, with size suffixes accepted (k/K=1024,
      m/M=1024^2, g/G=1024^3); N% is that percent of the detected effective
      memory (physical RAM, or the cgroup/job limit when one binds); 0
      disables the ceiling (measure-only). THE DEFAULT IS ON: 80% of the
      detected effective memory, so a runaway request is refused with a
      release-and-recheck grace step before it can brick the machine.
      Detection never guesses — when it fails, the governor degrades to
      measure-only with a warning. With the system allocator
      (--no-default-features) there are no in-process counters, so on
      non-Linux platforms the ceiling degrades to measure-only there too;
      on Linux the statm read keeps it enforced with any allocator. The
      refusal is its own
      diagnostic code (MACHINE_MEMORY), distinct from the accounted
      rejection, and names the measured RSS, the ceiling's provenance, and
      this flag. --diagnostics reports the physical footprint on every run
  --max-spill-bytes N
      Per-run in-memory ceiling for a bounded-memory sort operator, in
      bytes, before it spills the run to temporary storage (default: 0, no
      spill store installed; sorting stays entirely in memory)
  --max-spill-disk-bytes N
      Cumulative ceiling for the bytes a request may write to the spill
      store's disk runs, in bytes (default: 0 = unset, no ceiling — the
      fallback law is byte-identical). A spill run that would cross the
      ceiling FAILS the request with a spill-disk resource error, named by
      --diagnostics' cost snapshot; it never falls back to the in-memory
      sort, which would trade a bounded disk breach for unbounded memory
      growth. Only meaningful together with a spill budget
      (--max-spill-bytes N)
  --max-iterations N
      Opt-in per-run iteration ceiling: refuse a run whose cumulative
      frame-task transitions (the engine's own cooperative work steps,
      counted beside every work admission) cross N. The default is 0 =
      unlimited, and nothing is ever refused without the flag — this is an
      operator's runaway-loop dial, not a jqf limit. The counter is per
      run (one input value), so each value of a multi-value stream gets
      its own budget. A crossing is a machine resource refusal: exit 5,
      not catchable by try, zero stdout for the refused value.
  --parallel
      Run eligible requests on ordered workers. THE DEFAULT. Two routes
      are eligible, and neither infers anything about the input.
      Explicit NDJSON (`--input-format ndjson`, or an NDJSON
      --input-dialect) splits at the framer's own record boundaries.
      The default adjacent-values stdin splits at top-level value
      boundaries it can PROVE, which is a routing decision: no format is
      detected, no dialect is selected, and the input stays RFC 8259
      adjacent JSON texts either way. On both routes the eligible class
      is a static path program (plus, for records, the strict dialect);
      everything else falls through to the serial drive, publishing
      identical bytes either way.
  --no-parallel
      The explicit off switch, and the permanent single-threaded
      measurement mode. Same code path as pre-flip serial execution.
  --workers N|auto
      Worker width (default: auto); a usage error with --no-parallel. auto
      keeps inputs below a measured break-even size on the serial path and
      otherwise scales the width with input size up to the machine's
      performance cores plus half its efficiency cores. N runs exactly N
      workers, oversubscription included, up to 256.
  --diagnostics
      Print build provenance to stderr on every request (allocator, and
      whether this binary was built against a PGO profile), plus the
      parallel plan and the granted worker count on a record request.
  --explain
      Print the request's plan to stderr: the routing facts the engine
      derived (route-ladder eligibility, demand class, pushed-down path,
      boundary consumer, projected plan), then the route that served the
      request, its wall-clock time, and its cost snapshot. Every fact is
      read through the same accessors the route selector reads, so the
      explain block cannot drift from the route it describes. Never
      changes stdout bytes.
  --plan-out PATH
      Write this program's serialized routing-facts plan to PATH before
      any input is read. The plan is a versioned, byte-stable encoding of
      the routing facts --explain prints.
  --plan-file PATH
      Read a serialized plan from PATH and require it to match the
      compiled program's routing facts. A mismatch is a startup error,
      never a silent fallback: the plan cannot drift from the route.
  --edit
      Make the whole document the output subject: the program's assignments
      edit one input document at a time, and the edited document is
      published instead of the expression outputs. A program with no
      assignment publishes the document unchanged, byte-identical to the
      input. A non-identity edit preserves the untouched bytes verbatim --
      comments, whitespace, and key order survive -- splicing edited leaf
      spans and new statements into the original text; only a change the
      splice policy cannot place re-renders the whole document. A document
      whose run produces zero or multiple outputs errors. Same-format input
      and output on every codec that declares Edit (JSON, JSONC, JSON5,
      TOML, YAML, CSV/TSV, CBOR, XML, MessagePack, properties, INI, dotenv,
      jqfb); the output format defaults to the input's when only the input
      is pinned. A fact assignment (`.port.@comment = [...]`) rewrites the
      node's comment lines in place under --edit -- a value mutation is never
      involved -- and only comment-carrying formats (toml, yaml, jsonc,
      json5, properties, ini, dotenv) can splice those bytes, so a strict-JSON
      --edit fact write is a usage error. Without --edit only comment facts
      encode (they attach to the rendered document); any other fact write --
      an attribute or a YAML style/tag/anchor/alias role -- is refused, since
      no plain-run output path applies it.
  --check
      With --edit, the gofmt -l verdict for the edit lane: exit 0 when the
      would-be output is byte-identical to the input, exit 1 when the edit
      would change the file, print NOTHING and write NOTHING in either case.
      The file on disk is never touched; combine with the identity program
      to ask 'is this file already in canonical form?'. `--check` outside
      `--edit` is a usage error.
  --edit-expand-alias
      The alias-site escape hatch: an edit whose diff touches
      an alias-referenced YAML node normally REFUSES with a prose error,
      because the codec shares ONE document node across the anchor and every
      alias site, so the patch would rewrite the shared anchor's authored
      span and silently change every other alias site. This flag accepts
      exactly that anchor-rewrite semantics: the edit proceeds, the shared
      anchor's bytes change, and EVERY alias site changes with it. It does
      NOT make the edit correct or local — the user is accepting the
      rewrite the refusal describes, and the run warns once on stderr when
      the escape actually engages. The refusal stays the default. Requires
      --edit or --in-place; a usage error otherwise.
  --diff OLD NEW
      Read OLD and NEW as exactly ONE document each (in `--input-format`,
      or per-side `--old-format`/`--new-format`) and print their path-keyed
      semantic diff (accepted, and advertised — an accepted flag must
      appear in the help). The two files are read as
      the request's input; stdin is never touched. A file containing
      multiple documents (a YAML `---` stream, a multi-record NDJSON file)
      is a usage error naming the count; cross-format sides are allowed and
      compare VALUES — a TOML datetime on one side and the same text as a
      YAML string on the other is `changed`, because temporal ≠ string
      (the SEMANTIC in semantic diff). `--help diff` documents the whole
      surface.
  --output PATH
      Write the output to PATH instead of stdout. File writes are atomic
      (temp file + rename) unless --no-atomic is given.
  --split-exp EXPR
      A THIRD destination model (yq's --split-exp, a jqf extension beyond
      jq): write one file per published ITEM, its path the EXPR's single
      string output evaluated over that item, with $index bound to the item
      counter (0-based). File writes are atomic unless --no-atomic is given;
      a missing parent directory is an error naming the path (no mkdir -p:
      the path comes from a program over untrusted input). A usage error
      with --output, --in-place, --edit, and --diff (two destinations or a
      document subject), and with an --arg/--argjson/--slurpfile/--rawfile
      binding named `index` (the $index binding is taken).
  --split-exp-file PATH
      Read the --split-exp expression from PATH instead of the argument.
      Exactly one of --split-exp and --split-exp-file may be given.
  --in-place
      Read every positional input file as the input AND write the output back
      to it, atomically unless --no-atomic is given. Each file is edited
      INDEPENDENTLY — one run per file, that file's output written to itself —
      so several files keep their own bytes. The output format defaults to the
      input's, so a `.yaml` file is rewritten as YAML; `--output-format` opts
      into a conversion. All files must use one input format: `--input-format`
      or `--seq` pins it, otherwise their detected extensions must agree. With
      `--edit`, a file's original trailing bytes are preserved. A usage error with
      -n/-s/--diff/--follow (a run over null, a slurp across files, or a diff
      pair has no single coherent file to write back to) and with --output (two
      destinations).
      The atomic-replace model is honest: the write is a NEW
      inode renamed over the old one, so the original mode and (best-effort,
      when the process may chown) owner survive, but HARDLINKS DETACH (the
      sibling keeps the old inode's content) and ACLs/xattrs/labels are not
      carried. --no-atomic is the same-inode escape: it writes the original
      inode directly, so hardlinks and xattrs survive a successful run at the
      cost of a partial write on failure.
  --no-atomic
      Write file destinations directly instead of atomically. A failed run
      can then leave a partial file; requires --output or --in-place.
  --list-builtins
      Print every registered builtin as name/arity, one per line, sorted —
      the same enumeration the builtins builtin answers, so the CLI surface
      and the language surface share one source. Does not read stdin.
  --list-formats
      Print the format/dialect inventory: every input/output format and the
      dialects that serve it, generated from the same acceptance tables the
      parser reads. Does not read stdin.
  --help-format {output_formats}
      Print the page for one format: direction (input/output), the dialects
      that serve it, and its input model. Generated from the acceptance
      tables; does not read stdin.
  --explain-code ID
      Print one diagnostic-code row (id, name, class, severity, meaning)
      from the codes registry. Does not read stdin.
  --config PATH
      Read configuration defaults from PATH instead of discovering .jqf.toml
      from the current directory. Only the file's [defaults] section is
      read, and only for presentation/resource flags (Tier P); semantic
      flags are argv-only and never read from a file.
  --no-config
      Do not read any configuration file (a non-empty JQF_NO_CONFIG in the
      environment does the same). The run is fully hermetic.
  --show-config
      Print the effective configuration and the origin of every value
      (argv, a config file, or a built-in default), then exit 0 without
      reading stdin.
  -b, --binary
      Accepted and ignored: jq's -b is a no-op on Unix (its Windows-only
      capability is raw binary output), and jq accepts it there, so a
      cross-platform script runs unchanged on both binaries.
  --build-configuration
      Print what this binary was built with — build kind, profile
      identity, allocator, and platform topology, the same facts the
      --diagnostics provenance line carries — and exit 0 without
      reading stdin.
  -h, --help
      -h prints a one-screen summary; --help prints this full reference.
      Both exit 0 without reading stdin
  -V, --version
      Print the version and exit 0 without reading stdin. Prints jqf's own
      name and version (this is not a jq build; jq's own -V prints jq's)

Exit codes (the jq surface):
  0  success; under `-e`, a truthy last output value
  1  `-e` with a false/null last value; `--diff` when the two documents
     differ; `--edit --check` when the edit would change the file
  2  usage error or host/system failure (an unknown flag, a missing file,
     a bad option value)
  3  the program was rejected at compile time
  4  `-e` with no output at all
  5  a runtime error: the input did not parse, a value failed, a codec
     refused, or a resource ceiling was crossed
  N  `halt(N)`/`halt_error(N)` exit with their own code
A run that reports per-value errors but completes keeps exit 0 (or the
`-e` verdict of its last value); one error-severity recovering issue on a
record stream forces 5, except `--seq` whose parse errors are never fatal
(the exit stays with the program's last-record result).

Configuration:
  A .jqf.toml in the current directory or an ancestor can default the
  presentation/resource flags; the global file lives at
  ~/Library/Application Support/jqf/.jqf.toml on macOS and
  $XDG_CONFIG_HOME/jqf/.jqf.toml (~/.config/jqf/ otherwise). Precedence,
  highest first: argv, --config PATH, the nearest .jqf.toml, the global
  file, built-in defaults. --show-config prints the effective values and
  where each one came from.
";

/// The help text with the four enumeration slots filled from the acceptance tables. One call per `--help`; the
/// allocation is the point — the derived string cannot drift from [`parse_format`]/[`parse_input_dialect`]/
/// [`parse_output_dialect`] because it is built from the very tables they iterate.
pub(crate) fn help_text() -> String {
    HELP_TEMPLATE
        .replace("{input_formats}", &join_spellings(&CliFormat::INPUT_FORMATS, "|"))
        .replace("{input_dialects}", &join_spellings(&CliInputDialect::ALL, "|"))
        .replace("{output_formats}", &join_spellings(&CliFormat::OUTPUT_FORMATS, "|"))
        .replace("{output_dialects}", &join_spellings(&CliOutputDialect::ALL, "|"))
}

/// The `-h` page: one screen, the flags a first session reaches for, and a pointer at `--help` for everything else. The
/// format slots are filled from the same acceptance tables as [`help_text`], so even the short page cannot advertise a
/// spelling the parser refuses.
pub(crate) const SHORT_HELP_TEMPLATE: &str = "\
Usage: jqf [OPTIONS] [PROGRAM] [FILE...]
       jqf serve [SERVE-OPTIONS]

jqf runs jq programs against JSON, NDJSON, YAML, TOML, CSV, CBOR, XML, and
HTML, and edits files in place without destroying them. With no format
flags it is a strict JSON-in, JSON-out jq: `jqf '.a[0]' data.json`.

Formats (a named FILE's format follows its extension; stdin is JSON unless
you say otherwise, and --input-format always wins):
  --input-format {input_formats}
  --output-format {output_formats}
  --input-dialect D / --output-dialect D
      pin an exact dialect; `jqf --help <format>` lists a format's dialects
  --header         read CSV/TSV row 1 as a header: rows decode as objects

Editing:
  --edit               assignments edit the whole document; untouched parts
                       keep their bytes (key order, number spelling)
  --in-place           write each input file's output back to it, atomically
  --diff OLD NEW       path-keyed semantic diff of two documents in the
                       input format (--old-format/--new-format per side)
  --output PATH        write to PATH instead of stdout
  --split-exp EXPR     one destination file per published item, named by EXPR
                       (with $index bound to the item counter); see --help

jq options (the jq surface is accepted; the familiar ones):
  -n null input   -r raw output   -s slurp        -c compact   -S sort keys
  -R raw input    -j join output  -e exit status  -a ascii     -C/-M color
  --arg/--argjson NAME VALUE   --args/--jsonargs   -f FILE   --tab
  --indent N   --stream   --seq   --unbuffered   -L DIR

Streaming and residency:
  --follow             tail a growing file, one output per record
  --parallel, --workers N
                       record-parallel execution
  --max-rss N|N%       physical-memory ceiling on the real resident set
  jqf serve --listen <unix-socket|host:port> [PROGRAM]
                       resident daemon serving NDJSON sessions

Observability:
  --explain            print the derived plan and chosen route to stderr
  --plan-out PATH / --plan-file PATH
                       save / pin a serialized plan
  --diagnostics        build, profile, and platform facts

More:
  --help               the full reference: every flag, dialect, subcommand,
                       and the configuration-file surface
  --help <topic>       one page: flags, builtins, codes, mismatch, diff,
                       generators, facts, a format (e.g. `--help yaml`), or a
                       dialect spelling
  --list-builtins, --list-formats, --explain-code ID, --show-config
  -V, --version
";

/// The `-h` text with the two format slots filled — same tables, same no-drift law as [`help_text`].
pub(crate) fn short_help_text() -> String {
    SHORT_HELP_TEMPLATE
        .replace("{input_formats}", &join_spellings(&CliFormat::INPUT_FORMATS, "|"))
        .replace("{output_formats}", &join_spellings(&CliFormat::OUTPUT_FORMATS, "|"))
}

pub(crate) fn join_spellings<T: Copy>(table: &[(&'static str, T)], sep: &str) -> String {
    table
        .iter()
        .map(|(spelling, _)| *spelling)
        .collect::<Vec<_>>()
        .join(sep)
}

/// The selectable input shapes. There is no third, inferred one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CliFormat {
    /// One stream of ADJACENT JSON texts (jq's stdin).
    Json,
    /// One JSONC document (JSON plus comments and trailing commas) occupying the whole input — the `tsconfig.json` / VS
    /// Code `settings.json` / `devcontainer.json` corpus.
    Jsonc,
    /// One JSON5 document (JSON plus the JSON5 grammar: unquoted keys, single quotes, hex, `Infinity`/`NaN`, comments,
    /// trailing commas —
    /// occupying the whole input.
    Json5,
    /// One stream of physically framed NDJSON records.
    Ndjson,
    /// One stream of RS-framed JSON Text Sequence (RFC 7464) records.
    JsonSeq,
    /// One TOML document occupying the whole input.
    Toml,
    /// One stream of physically framed RFC 4180 CSV records.
    Csv,
    /// One stream of physically framed tab-separated, no-quote TSV records (the 134 second delimited grammar).
    Tsv,
    /// One CBOR (RFC 8949) item occupying the whole input.
    Cbor,
    /// One stream of ADJACENT CBOR items concatenated without framing (RFC 8742 `application/cbor-seq`).
    CborSeq,
    /// One YAML document stream occupying the whole input.
    Yaml,
    /// One jqft document stream occupying the whole input (the family's human text profile).
    Jqft,
    /// One jqfjson JSON envelope occupying the whole input (the family's JSON representation).
    Jqfjson,
    /// One jqfb binary image occupying the whole input (the family's machine profile).
    Jqfb,
    /// One XML 1.0 document occupying the whole input.
    Xml,
    /// One WHATWG-recovered HTML document occupying the whole input.
    Html,
    /// One `.properties` document occupying the whole input.
    Properties,
    /// One INI document occupying the whole input.
    Ini,
    /// One dotenv document occupying the whole input.
    Dotenv,
    /// One `MessagePack` object occupying the whole input.
    Messagepack,
    /// The output-only presentation renderers (no input registration).
    Render,
}

impl CliFormat {
    /// Every format the CLI accepts as INPUT, in help order. Each entry is the exact spelling [`parse_format`] accepts
    /// — the help text enumerates this same table, so advertisement and acceptance cannot drift (the route-capabilities
    /// spine: the CLI consumes one declaration instead of re-declaring it). Adding a format means adding its entry
    /// here, to `resolve_input_selection`/`resolve_output_selection`, and to the codec registration.
    pub(crate) const INPUT_FORMATS: [(&'static str, CliFormat); 20] = [
        (jqf_codec_json::FORMAT_ID, Self::Json),
        (jqf_codec_json::jsonc::FORMAT_ID, Self::Jsonc),
        (jqf_codec_json::json5::FORMAT_ID, Self::Json5),
        (jqf_codec_json::ndjson::FORMAT_ID, Self::Ndjson),
        (jqf_codec_json::seq::FORMAT_ID, Self::JsonSeq),
        (jqf_codec_toml::FORMAT_ID, Self::Toml),
        (jqf_codec_delimited::FORMAT_ID, Self::Csv),
        (jqf_codec_delimited::TSV_FORMAT_ID, Self::Tsv),
        (jqf_codec_cbor::FORMAT_ID, Self::Cbor),
        (jqf_codec_cbor::seq::FORMAT_ID, Self::CborSeq),
        (jqf_codec_yaml::FORMAT_ID, Self::Yaml),
        (jqf_codec_jqft::FORMAT_ID, Self::Jqft),
        (jqf_codec_jqft::JQFJSON_FORMAT_ID, Self::Jqfjson),
        (jqf_codec_jqft::FORMAT_ID_JQFB, Self::Jqfb),
        (jqf_codec_xml::FORMAT_ID, Self::Xml),
        (jqf_codec_html::FORMAT_ID, Self::Html),
        (jqf_codec_ini::FORMAT_ID, Self::Properties),
        (jqf_codec_ini::INI_FORMAT_ID, Self::Ini),
        (jqf_codec_ini::DOTENV_FORMAT_ID, Self::Dotenv),
        (jqf_codec_messagepack::FORMAT_ID, Self::Messagepack),
    ];

    /// Every format the CLI accepts as OUTPUT: the input set plus the output-only renderers, in help order.
    pub(crate) const OUTPUT_FORMATS: [(&'static str, CliFormat); 21] = [
        (jqf_codec_json::FORMAT_ID, Self::Json),
        (jqf_codec_json::jsonc::FORMAT_ID, Self::Jsonc),
        (jqf_codec_json::json5::FORMAT_ID, Self::Json5),
        (jqf_codec_json::ndjson::FORMAT_ID, Self::Ndjson),
        (jqf_codec_json::seq::FORMAT_ID, Self::JsonSeq),
        (jqf_codec_toml::FORMAT_ID, Self::Toml),
        (jqf_codec_delimited::FORMAT_ID, Self::Csv),
        (jqf_codec_delimited::TSV_FORMAT_ID, Self::Tsv),
        (jqf_codec_cbor::FORMAT_ID, Self::Cbor),
        (jqf_codec_cbor::seq::FORMAT_ID, Self::CborSeq),
        (jqf_codec_yaml::FORMAT_ID, Self::Yaml),
        (jqf_codec_jqft::FORMAT_ID, Self::Jqft),
        (jqf_codec_jqft::JQFJSON_FORMAT_ID, Self::Jqfjson),
        (jqf_codec_jqft::FORMAT_ID_JQFB, Self::Jqfb),
        (jqf_codec_xml::FORMAT_ID, Self::Xml),
        (jqf_codec_html::FORMAT_ID, Self::Html),
        (jqf_codec_ini::FORMAT_ID, Self::Properties),
        (jqf_codec_ini::INI_FORMAT_ID, Self::Ini),
        (jqf_codec_ini::DOTENV_FORMAT_ID, Self::Dotenv),
        (jqf_codec_messagepack::FORMAT_ID, Self::Messagepack),
        (jqf_codec_render::FORMAT_ID, Self::Render),
    ];

    /// Whether this output format is served by the JSON renderer: `-r`, `-S`, `-a`, `-j`, and `--raw-output0` honor it.
    /// Asked of the codec registration ids, not a hand-listed enum subset.
    pub(crate) fn is_json_renderer(self) -> bool {
        let id = self.id();
        id == jqf_codec_json::FORMAT_ID
            || id == jqf_codec_json::jsonc::FORMAT_ID
            || id == jqf_codec_json::json5::FORMAT_ID
            || id == jqf_codec_json::seq::FORMAT_ID
    }

    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::Json => jqf_codec_json::FORMAT_ID,
            Self::Jsonc => jqf_codec_json::jsonc::FORMAT_ID,
            Self::Json5 => jqf_codec_json::json5::FORMAT_ID,
            Self::Ndjson => jqf_codec_json::ndjson::FORMAT_ID,
            Self::JsonSeq => jqf_codec_json::seq::FORMAT_ID,
            Self::Toml => jqf_codec_toml::FORMAT_ID,
            Self::Csv => jqf_codec_delimited::FORMAT_ID,
            Self::Tsv => jqf_codec_delimited::TSV_FORMAT_ID,
            Self::Cbor => jqf_codec_cbor::FORMAT_ID,
            Self::CborSeq => jqf_codec_cbor::seq::FORMAT_ID,
            Self::Yaml => jqf_codec_yaml::FORMAT_ID,
            Self::Jqft => jqf_codec_jqft::FORMAT_ID,
            Self::Jqfjson => jqf_codec_jqft::JQFJSON_FORMAT_ID,
            Self::Jqfb => jqf_codec_jqft::FORMAT_ID_JQFB,
            Self::Xml => jqf_codec_xml::FORMAT_ID,
            Self::Html => jqf_codec_html::FORMAT_ID,
            Self::Properties => jqf_codec_ini::FORMAT_ID,
            Self::Ini => jqf_codec_ini::INI_FORMAT_ID,
            Self::Dotenv => jqf_codec_ini::DOTENV_FORMAT_ID,
            Self::Messagepack => jqf_codec_messagepack::FORMAT_ID,
            Self::Render => jqf_codec_render::FORMAT_ID,
        }
    }

    // The input-model fact (`single_document`) was DELETED 2026-08-07 with 039 item 1a: the fact now comes from the
    // codec's own `route_capabilities` declaration (the absence of `RouteCapability:AdjacentValues`), queried through
    // the catalog; see `crate:plan:resolve`'s `single_document_input` argument.
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CliInputDialect {
    Rfc8259,
    /// `jsonc.trailing@1`: comments AND trailing commas (the default).
    JsoncTrailing,
    /// `jsonc.default@1`: comments, strict JSON's comma law.
    JsoncDefault,
    /// `json5.document@1`: the complete JSON5 grammar.
    Json5Document,
    NdjsonStrict,
    NdjsonRecovering,
    JsonSeqStrict,
    Toml10,
    Toml11,
    /// `csv.utf8@1`: the Unicode-capable CSV family — the RFC 4180 quoting grammar admitting every valid-UTF-8 scalar.
    /// The short `--input-format csv` default.
    CsvUtf8,
    /// `csv.utf8-header@1`: the headered twin.
    CsvUtf8Header,
    /// `csv.rfc4180@1`: the frozen RFC alphabet (TEXTDATA only), an explicit `--input-dialect` opt-in.
    CsvRfc4180,
    CsvRfc4180Header,
    TsvUtf8,
    TsvUtf8Header,
    CborGeneric,
    /// The RFC 8742 adjacent-item input dialect: cbor's generic payload law under the sequence framing law.
    CborSeqGeneric,
    YamlFailsafe,
    YamlJson,
    YamlCore,
    JqftDocument,
    JqfjsonDocument,
    JqfbDocument,
    XmlDocument,
    HtmlDocument,
    HtmlFragment,
    PropertiesJdk,
    IniJqfStrict,
    DotenvJqfStrict,
    MessagepackUtf8,
    MessagepackKeyEquivalence,
}

impl CliInputDialect {
    /// Every input dialect the CLI accepts, in help order. Each entry is the exact spelling [`parse_input_dialect`]
    /// accepts — the help text enumerates this same table. Every accepted spelling IS the registered dialect identity,
    /// so the CLI consumes the codec's declaration.
    pub(crate) const ALL: [(&'static str, CliInputDialect); 31] = [
        (jqf_codec_json::RFC8259_DIALECT_ID, Self::Rfc8259),
        (jqf_codec_json::jsonc::TRAILING_DIALECT_ID, Self::JsoncTrailing),
        (jqf_codec_json::jsonc::DEFAULT_DIALECT_ID, Self::JsoncDefault),
        (jqf_codec_json::json5::DOCUMENT_DIALECT_ID, Self::Json5Document),
        (jqf_codec_json::ndjson::STRICT_DIALECT_ID, Self::NdjsonStrict),
        (jqf_codec_json::ndjson::RECOVERING_DIALECT_ID, Self::NdjsonRecovering),
        (jqf_codec_json::seq::STRICT_DIALECT_ID, Self::JsonSeqStrict),
        (jqf_codec_toml::TOML_1_0_DIALECT_ID, Self::Toml10),
        (jqf_codec_toml::TOML_1_1_DIALECT_ID, Self::Toml11),
        (jqf_codec_delimited::UTF8_DIALECT_ID, Self::CsvUtf8),
        (jqf_codec_delimited::UTF8_HEADER_DIALECT_ID, Self::CsvUtf8Header),
        (jqf_codec_delimited::RFC4180_DIALECT_ID, Self::CsvRfc4180),
        (jqf_codec_delimited::RFC4180_HEADER_DIALECT_ID, Self::CsvRfc4180Header),
        (jqf_codec_delimited::TSV_UTF8_DIALECT_ID, Self::TsvUtf8),
        (jqf_codec_delimited::TSV_UTF8_HEADER_DIALECT_ID, Self::TsvUtf8Header),
        (jqf_codec_cbor::CBOR_GENERIC_DIALECT_ID, Self::CborGeneric),
        (jqf_codec_cbor::seq::RFC8742_GENERIC_DIALECT_ID, Self::CborSeqGeneric),
        (jqf_codec_yaml::YAML_CORE_DIALECT_ID, Self::YamlCore),
        (jqf_codec_yaml::YAML_JSON_DIALECT_ID, Self::YamlJson),
        (jqf_codec_yaml::YAML_FAILSAFE_DIALECT_ID, Self::YamlFailsafe),
        (jqf_codec_jqft::JQFT_DOCUMENT_DIALECT_ID, Self::JqftDocument),
        (jqf_codec_jqft::JQFJSON_DOCUMENT_DIALECT_ID, Self::JqfjsonDocument),
        (jqf_codec_jqft::JQFB_DOCUMENT_DIALECT_ID, Self::JqfbDocument),
        (jqf_codec_xml::XML_DOCUMENT_DIALECT_ID, Self::XmlDocument),
        (jqf_codec_html::HTML_DOCUMENT_DIALECT_ID, Self::HtmlDocument),
        (jqf_codec_html::HTML_FRAGMENT_DIALECT_ID, Self::HtmlFragment),
        (jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID, Self::PropertiesJdk),
        (jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID, Self::IniJqfStrict),
        (jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID, Self::DotenvJqfStrict),
        (
            jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
            Self::MessagepackUtf8,
        ),
        (
            jqf_codec_messagepack::MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID,
            Self::MessagepackKeyEquivalence,
        ),
    ];

    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::Rfc8259 => jqf_codec_json::RFC8259_DIALECT_ID,
            Self::JsoncTrailing => jqf_codec_json::jsonc::TRAILING_DIALECT_ID,
            Self::JsoncDefault => jqf_codec_json::jsonc::DEFAULT_DIALECT_ID,
            Self::Json5Document => jqf_codec_json::json5::DOCUMENT_DIALECT_ID,
            Self::NdjsonStrict => jqf_codec_json::ndjson::STRICT_DIALECT_ID,
            Self::NdjsonRecovering => jqf_codec_json::ndjson::RECOVERING_DIALECT_ID,
            Self::JsonSeqStrict => jqf_codec_json::seq::STRICT_DIALECT_ID,
            Self::Toml10 => jqf_codec_toml::TOML_1_0_DIALECT_ID,
            Self::Toml11 => jqf_codec_toml::TOML_1_1_DIALECT_ID,
            Self::CsvUtf8 => jqf_codec_delimited::UTF8_DIALECT_ID,
            Self::CsvUtf8Header => jqf_codec_delimited::UTF8_HEADER_DIALECT_ID,
            Self::CsvRfc4180 => jqf_codec_delimited::RFC4180_DIALECT_ID,
            Self::CsvRfc4180Header => jqf_codec_delimited::RFC4180_HEADER_DIALECT_ID,
            Self::TsvUtf8 => jqf_codec_delimited::TSV_UTF8_DIALECT_ID,
            Self::TsvUtf8Header => jqf_codec_delimited::TSV_UTF8_HEADER_DIALECT_ID,
            Self::CborGeneric => jqf_codec_cbor::CBOR_GENERIC_DIALECT_ID,
            Self::CborSeqGeneric => jqf_codec_cbor::seq::RFC8742_GENERIC_DIALECT_ID,
            Self::YamlFailsafe => jqf_codec_yaml::YAML_FAILSAFE_DIALECT_ID,
            Self::YamlJson => jqf_codec_yaml::YAML_JSON_DIALECT_ID,
            Self::YamlCore => jqf_codec_yaml::YAML_CORE_DIALECT_ID,
            Self::JqftDocument => jqf_codec_jqft::JQFT_DOCUMENT_DIALECT_ID,
            Self::JqfjsonDocument => jqf_codec_jqft::JQFJSON_DOCUMENT_DIALECT_ID,
            Self::JqfbDocument => jqf_codec_jqft::JQFB_DOCUMENT_DIALECT_ID,
            Self::XmlDocument => jqf_codec_xml::XML_DOCUMENT_DIALECT_ID,
            Self::HtmlDocument => jqf_codec_html::HTML_DOCUMENT_DIALECT_ID,
            Self::HtmlFragment => jqf_codec_html::HTML_FRAGMENT_DIALECT_ID,
            Self::PropertiesJdk => jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
            Self::IniJqfStrict => jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID,
            Self::DotenvJqfStrict => jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
            Self::MessagepackUtf8 => jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
            Self::MessagepackKeyEquivalence => jqf_codec_messagepack::MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID,
        }
    }

    /// The format this dialect selects (the `--list-formats` grouping).
    pub(crate) const fn format(self) -> CliFormat {
        match self {
            Self::Rfc8259 => CliFormat::Json,
            Self::JsoncTrailing | Self::JsoncDefault => CliFormat::Jsonc,
            Self::Json5Document => CliFormat::Json5,
            Self::NdjsonStrict | Self::NdjsonRecovering => CliFormat::Ndjson,
            Self::JsonSeqStrict => CliFormat::JsonSeq,
            Self::Toml10 | Self::Toml11 => CliFormat::Toml,
            Self::CsvUtf8 | Self::CsvUtf8Header | Self::CsvRfc4180 | Self::CsvRfc4180Header => CliFormat::Csv,
            Self::TsvUtf8 | Self::TsvUtf8Header => CliFormat::Tsv,
            Self::CborGeneric => CliFormat::Cbor,
            Self::CborSeqGeneric => CliFormat::CborSeq,
            Self::YamlFailsafe | Self::YamlJson | Self::YamlCore => CliFormat::Yaml,
            Self::JqftDocument => CliFormat::Jqft,
            Self::JqfjsonDocument => CliFormat::Jqfjson,
            Self::JqfbDocument => CliFormat::Jqfb,
            Self::XmlDocument => CliFormat::Xml,
            Self::HtmlDocument | Self::HtmlFragment => CliFormat::Html,
            Self::PropertiesJdk => CliFormat::Properties,
            Self::IniJqfStrict => CliFormat::Ini,
            Self::DotenvJqfStrict => CliFormat::Dotenv,
            Self::MessagepackUtf8 | Self::MessagepackKeyEquivalence => CliFormat::Messagepack,
        }
    }

    /// The record-route kind this dialect selects, if any.
    ///
    /// A record dialect owns the PHYSICAL stream and dispatches to the record route; everything else goes through the
    /// access ladder.
    pub(crate) const fn record_kind(self) -> Option<RecordInputKind> {
        match self {
            Self::Rfc8259
            | Self::JsoncTrailing
            | Self::JsoncDefault
            | Self::Json5Document
            | Self::Toml10
            | Self::Toml11
            | Self::CborGeneric
            | Self::CborSeqGeneric
            | Self::YamlFailsafe
            | Self::YamlJson
            | Self::YamlCore
            | Self::JqftDocument
            | Self::JqfjsonDocument
            | Self::JqfbDocument
            | Self::XmlDocument
            | Self::HtmlDocument
            | Self::HtmlFragment
            | Self::PropertiesJdk
            | Self::IniJqfStrict
            | Self::DotenvJqfStrict
            | Self::MessagepackUtf8
            | Self::MessagepackKeyEquivalence => None,
            Self::NdjsonStrict | Self::NdjsonRecovering => Some(RecordInputKind::Ndjson),
            Self::JsonSeqStrict => Some(RecordInputKind::JsonSeq),
            // The two unheadered CSV dialects share one record kind, as do the two headered ones (doubled the family).
            Self::CsvUtf8 | Self::CsvRfc4180 => Some(RecordInputKind::Csv {
                header: false,
                tsv: false,
            }),
            Self::CsvUtf8Header | Self::CsvRfc4180Header => Some(RecordInputKind::Csv {
                header: true,
                tsv: false,
            }),
            Self::TsvUtf8 => Some(RecordInputKind::Csv {
                header: false,
                tsv: true,
            }),
            Self::TsvUtf8Header => Some(RecordInputKind::Csv {
                header: true,
                tsv: true,
            }),
        }
    }

    pub(crate) const fn ndjson_profile(self) -> Option<NdjsonProfile> {
        match self {
            Self::NdjsonStrict => Some(NdjsonProfile::Strict),
            Self::NdjsonRecovering => Some(NdjsonProfile::Recovering),
            _ => None,
        }
    }

    /// Whether this CSV input dialect freezes the field alphabet to the RFC's ASCII TEXTDATA. `true` for the explicit
    /// `csv.rfc4180@1` opt-ins, `false` for the Unicode-capable `csv.utf8@1` family; always `false` off the CSV format.
    pub(crate) const fn csv_textdata(self) -> bool {
        matches!(self, Self::CsvRfc4180 | Self::CsvRfc4180Header)
    }

    /// The strict json-seq profile an explicit json-seq input selection names.
    ///
    /// `--seq` itself selects the flag-scoped RECOVERING profile through a separate channel; the registered dialect is
    /// always strict.
    pub(crate) const fn json_seq_profile(self) -> Option<jqf_codec_json::seq::JsonSeqProfile> {
        match self {
            Self::JsonSeqStrict => Some(jqf_codec_json::seq::JsonSeqProfile::Strict),
            _ => None,
        }
    }

    /// Whether this dialect has a RECORD ROUTE — the streaming-stdin capability.
    ///
    /// The four record dialects own the physical stream; the default RFC 8259 input joins them because its
    /// adjacent-value framing is the streaming drive's own: a non-seekable stdin publishes each complete value as its
    /// bytes arrive instead of reading whole. Document-shaped formats (TOML, CBOR, XML, HTML) and YAML are one value or
    /// a sequence of documents and simply do not qualify — the stream route never special-cases them.
    pub(crate) const fn has_record_route(self) -> bool {
        matches!(
            self,
            Self::Rfc8259
                | Self::NdjsonStrict
                | Self::NdjsonRecovering
                | Self::JsonSeqStrict
                | Self::CsvUtf8
                | Self::CsvUtf8Header
                | Self::CsvRfc4180
                | Self::CsvRfc4180Header
                | Self::TsvUtf8
                | Self::TsvUtf8Header
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CliOutputDialect {
    Rfc8259,
    /// `jsonc.trailing-jqf@1`: JSONC with trailing commas (the default).
    JsoncTrailingJqf,
    /// `jsonc.default-jqf@1`: JSONC with strict comma law.
    JsoncDefaultJqf,
    /// `jsonc.jqf-1.0@1`: the edit lane's whole-document floor render.
    JsoncJqf,
    /// `json5.jqf@1`: the deterministic canonical JSON5 output profile.
    Json5Jqf,
    /// `json5.jqf-1.0@1`: the edit lane's whole-document floor render.
    Json5Jqf10,
    NdjsonStrict,
    JsonSeqJqf,
    TomlJqf10,
    TomlJqf11,
    /// `csv.jqf-utf8@1`: the Unicode-capable output family — the same quoting/CRLF encoder as the RFC-named profile
    /// under a name that does not advertise an RFC. The short `--output-format csv` default.
    CsvJqfUtf8,
    CsvJqfUtf8Header,
    /// `csv.jqf-rfc4180@1`: the RFC-named opt-in.
    CsvJqfRfc4180,
    CsvJqfRfc4180Header,
    TsvJqfLf,
    TsvJqfLfHeader,
    CborSource,
    CborPreferred,
    CborCoreDeterministic,
    CborLengthFirst,
    /// The one cbor-seq output dialect (`cbor-seq.jqf@1`); the payload profile travels as an encode option, never as a
    /// dialect.
    CborSeqJqf,
    YamlStreamCanonical,
    YamlSingleDocument,
    YamlBlock,
    YamlJqf,
    JqftCanonical,
    JqfjsonCanonical,
    JqfbCanonical,
    XmlSource,
    XmlDeterministic,
    HtmlSource,
    HtmlDocumentSerialize,
    PropertiesJqf10,
    IniJqf10,
    DotenvJqf10,
    MessagepackDeterministic,
    /// `messagepack.deterministic-float64@1`: the deterministic grammar with one deliberate divergence — a Decimal
    /// encodes as its nearest IEEE-754 binary64 float instead of refusing (the precision loss is in the identity).
    MessagepackDeterministicFloat64,
    RenderPlain,
    RenderGfmTable,
    RenderHtmlTable,
    RenderGridTable,
    RenderTree,
    RenderTerminal,
    RenderShell,
    /// `render.hist@1`: the plain-ASCII frequency histogram.
    RenderHist,
}

impl CliOutputDialect {
    /// Every output dialect the CLI accepts, in help order. Each entry is the exact spelling [`parse_output_dialect`]
    /// accepts — the help text enumerates this same table. Every accepted spelling IS the registered dialect identity
    /// (the YAML output dialects were reconciled onto their registered names on 2026-08-05, so no CLI alias differs
    /// from an identity anymore).
    pub(crate) const ALL: [(&'static str, CliOutputDialect); 45] = [
        (jqf_codec_json::RFC8259_DIALECT_ID, Self::Rfc8259),
        (jqf_codec_json::jsonc::TRAILING_JQF_DIALECT_ID, Self::JsoncTrailingJqf),
        (jqf_codec_json::jsonc::DEFAULT_JQF_DIALECT_ID, Self::JsoncDefaultJqf),
        (jqf_codec_json::jsonc::JQF_1_0_DIALECT_ID, Self::JsoncJqf),
        (jqf_codec_json::json5::JQF_DIALECT_ID, Self::Json5Jqf),
        (jqf_codec_json::json5::JQF_1_0_DIALECT_ID, Self::Json5Jqf10),
        (jqf_codec_json::ndjson::STRICT_DIALECT_ID, Self::NdjsonStrict),
        (jqf_codec_json::seq::JQF_DIALECT_ID, Self::JsonSeqJqf),
        (jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID, Self::TomlJqf10),
        (jqf_codec_toml::TOML_JQF_1_1_DIALECT_ID, Self::TomlJqf11),
        (jqf_codec_delimited::JQF_UTF8_DIALECT_ID, Self::CsvJqfUtf8),
        (jqf_codec_delimited::JQF_UTF8_HEADER_DIALECT_ID, Self::CsvJqfUtf8Header),
        (jqf_codec_delimited::JQF_RFC4180_DIALECT_ID, Self::CsvJqfRfc4180),
        (
            jqf_codec_delimited::JQF_RFC4180_HEADER_DIALECT_ID,
            Self::CsvJqfRfc4180Header,
        ),
        (jqf_codec_delimited::TSV_JQF_LF_DIALECT_ID, Self::TsvJqfLf),
        (jqf_codec_delimited::TSV_JQF_LF_HEADER_DIALECT_ID, Self::TsvJqfLfHeader),
        (jqf_codec_cbor::CBOR_SOURCE_DIALECT_ID, Self::CborSource),
        (jqf_codec_cbor::CBOR_PREFERRED_DIALECT_ID, Self::CborPreferred),
        (
            jqf_codec_cbor::CBOR_CORE_DETERMINISTIC_DIALECT_ID,
            Self::CborCoreDeterministic,
        ),
        (jqf_codec_cbor::CBOR_LENGTH_FIRST_DIALECT_ID, Self::CborLengthFirst),
        (jqf_codec_cbor::seq::JQF_DIALECT_ID, Self::CborSeqJqf),
        (jqf_codec_yaml::YAML_BLOCK_DIALECT_ID, Self::YamlBlock),
        (jqf_codec_yaml::YAML_JQF_1_0_DIALECT_ID, Self::YamlJqf),
        (
            jqf_codec_yaml::YAML_STREAM_CANONICAL_DIALECT_ID,
            Self::YamlStreamCanonical,
        ),
        (
            jqf_codec_yaml::YAML_SINGLE_DOCUMENT_DIALECT_ID,
            Self::YamlSingleDocument,
        ),
        (jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID, Self::JqftCanonical),
        (jqf_codec_jqft::JQFJSON_CANONICAL_DIALECT_ID, Self::JqfjsonCanonical),
        (jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID, Self::JqfbCanonical),
        (jqf_codec_xml::XML_SOURCE_DIALECT_ID, Self::XmlSource),
        (jqf_codec_xml::XML_DETERMINISTIC_DIALECT_ID, Self::XmlDeterministic),
        (jqf_codec_html::HTML_SOURCE_DIALECT_ID, Self::HtmlSource),
        (
            jqf_codec_html::HTML_DOCUMENT_SERIALIZE_DIALECT_ID,
            Self::HtmlDocumentSerialize,
        ),
        (jqf_codec_ini::PROPERTIES_JQF_1_0_DIALECT_ID, Self::PropertiesJqf10),
        (jqf_codec_ini::INI_JQF_1_0_DIALECT_ID, Self::IniJqf10),
        (jqf_codec_ini::DOTENV_JQF_1_0_DIALECT_ID, Self::DotenvJqf10),
        (
            jqf_codec_messagepack::MESSAGEPACK_DETERMINISTIC_DIALECT_ID,
            Self::MessagepackDeterministic,
        ),
        (
            jqf_codec_messagepack::MESSAGEPACK_DETERMINISTIC_FLOAT64_DIALECT_ID,
            Self::MessagepackDeterministicFloat64,
        ),
        (jqf_codec_render::PLAIN_DIALECT_ID, Self::RenderPlain),
        (jqf_codec_render::GFM_TABLE_DIALECT_ID, Self::RenderGfmTable),
        (jqf_codec_render::HTML_TABLE_DIALECT_ID, Self::RenderHtmlTable),
        (jqf_codec_render::GRID_TABLE_DIALECT_ID, Self::RenderGridTable),
        (jqf_codec_render::TREE_DIALECT_ID, Self::RenderTree),
        (jqf_codec_render::TERMINAL_DIALECT_ID, Self::RenderTerminal),
        (jqf_codec_render::SHELL_DIALECT_ID, Self::RenderShell),
        (jqf_codec_render::HIST_DIALECT_ID, Self::RenderHist),
    ];

    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::Rfc8259 => jqf_codec_json::RFC8259_DIALECT_ID,
            Self::JsoncTrailingJqf => jqf_codec_json::jsonc::TRAILING_JQF_DIALECT_ID,
            Self::JsoncDefaultJqf => jqf_codec_json::jsonc::DEFAULT_JQF_DIALECT_ID,
            Self::JsoncJqf => jqf_codec_json::jsonc::JQF_1_0_DIALECT_ID,
            Self::Json5Jqf => jqf_codec_json::json5::JQF_DIALECT_ID,
            Self::Json5Jqf10 => jqf_codec_json::json5::JQF_1_0_DIALECT_ID,
            Self::NdjsonStrict => jqf_codec_json::ndjson::STRICT_DIALECT_ID,
            Self::JsonSeqJqf => jqf_codec_json::seq::JQF_DIALECT_ID,
            Self::TomlJqf10 => jqf_codec_toml::TOML_JQF_1_0_DIALECT_ID,
            Self::TomlJqf11 => jqf_codec_toml::TOML_JQF_1_1_DIALECT_ID,
            Self::CsvJqfUtf8 => jqf_codec_delimited::JQF_UTF8_DIALECT_ID,
            Self::CsvJqfUtf8Header => jqf_codec_delimited::JQF_UTF8_HEADER_DIALECT_ID,
            Self::CsvJqfRfc4180 => jqf_codec_delimited::JQF_RFC4180_DIALECT_ID,
            Self::CsvJqfRfc4180Header => jqf_codec_delimited::JQF_RFC4180_HEADER_DIALECT_ID,
            Self::TsvJqfLf => jqf_codec_delimited::TSV_JQF_LF_DIALECT_ID,
            Self::TsvJqfLfHeader => jqf_codec_delimited::TSV_JQF_LF_HEADER_DIALECT_ID,
            Self::CborSource => jqf_codec_cbor::CBOR_SOURCE_DIALECT_ID,
            Self::CborPreferred => jqf_codec_cbor::CBOR_PREFERRED_DIALECT_ID,
            Self::CborCoreDeterministic => jqf_codec_cbor::CBOR_CORE_DETERMINISTIC_DIALECT_ID,
            Self::CborLengthFirst => jqf_codec_cbor::CBOR_LENGTH_FIRST_DIALECT_ID,
            Self::CborSeqJqf => jqf_codec_cbor::seq::JQF_DIALECT_ID,
            Self::YamlStreamCanonical => jqf_codec_yaml::YAML_STREAM_CANONICAL_DIALECT_ID,
            Self::YamlSingleDocument => jqf_codec_yaml::YAML_SINGLE_DOCUMENT_DIALECT_ID,
            Self::YamlBlock => jqf_codec_yaml::YAML_BLOCK_DIALECT_ID,
            Self::YamlJqf => jqf_codec_yaml::YAML_JQF_1_0_DIALECT_ID,
            Self::JqftCanonical => jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID,
            Self::JqfjsonCanonical => jqf_codec_jqft::JQFJSON_CANONICAL_DIALECT_ID,
            Self::JqfbCanonical => jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
            Self::XmlSource => jqf_codec_xml::XML_SOURCE_DIALECT_ID,
            Self::XmlDeterministic => jqf_codec_xml::XML_DETERMINISTIC_DIALECT_ID,
            Self::HtmlSource => jqf_codec_html::HTML_SOURCE_DIALECT_ID,
            Self::HtmlDocumentSerialize => jqf_codec_html::HTML_DOCUMENT_SERIALIZE_DIALECT_ID,
            Self::PropertiesJqf10 => jqf_codec_ini::PROPERTIES_JQF_1_0_DIALECT_ID,
            Self::IniJqf10 => jqf_codec_ini::INI_JQF_1_0_DIALECT_ID,
            Self::DotenvJqf10 => jqf_codec_ini::DOTENV_JQF_1_0_DIALECT_ID,
            Self::MessagepackDeterministic => jqf_codec_messagepack::MESSAGEPACK_DETERMINISTIC_DIALECT_ID,
            Self::MessagepackDeterministicFloat64 => {
                jqf_codec_messagepack::MESSAGEPACK_DETERMINISTIC_FLOAT64_DIALECT_ID
            }
            Self::RenderPlain => jqf_codec_render::PLAIN_DIALECT_ID,
            Self::RenderGfmTable => jqf_codec_render::GFM_TABLE_DIALECT_ID,
            Self::RenderHtmlTable => jqf_codec_render::HTML_TABLE_DIALECT_ID,
            Self::RenderGridTable => jqf_codec_render::GRID_TABLE_DIALECT_ID,
            Self::RenderTree => jqf_codec_render::TREE_DIALECT_ID,
            Self::RenderTerminal => jqf_codec_render::TERMINAL_DIALECT_ID,
            Self::RenderShell => jqf_codec_render::SHELL_DIALECT_ID,
            Self::RenderHist => jqf_codec_render::HIST_DIALECT_ID,
        }
    }

    /// The format this dialect selects (the `--list-formats` grouping).
    pub(crate) const fn format(self) -> CliFormat {
        match self {
            Self::Rfc8259 => CliFormat::Json,
            Self::JsoncTrailingJqf | Self::JsoncDefaultJqf | Self::JsoncJqf => CliFormat::Jsonc,
            Self::Json5Jqf | Self::Json5Jqf10 => CliFormat::Json5,
            Self::NdjsonStrict => CliFormat::Ndjson,
            Self::JsonSeqJqf => CliFormat::JsonSeq,
            Self::TomlJqf10 | Self::TomlJqf11 => CliFormat::Toml,
            Self::CsvJqfUtf8 | Self::CsvJqfUtf8Header | Self::CsvJqfRfc4180 | Self::CsvJqfRfc4180Header => {
                CliFormat::Csv
            }
            Self::TsvJqfLf | Self::TsvJqfLfHeader => CliFormat::Tsv,
            Self::CborSource | Self::CborPreferred | Self::CborCoreDeterministic | Self::CborLengthFirst => {
                CliFormat::Cbor
            }
            Self::CborSeqJqf => CliFormat::CborSeq,
            Self::YamlStreamCanonical | Self::YamlSingleDocument | Self::YamlBlock | Self::YamlJqf => CliFormat::Yaml,
            Self::JqftCanonical => CliFormat::Jqft,
            Self::JqfjsonCanonical => CliFormat::Jqfjson,
            Self::JqfbCanonical => CliFormat::Jqfb,
            Self::XmlSource | Self::XmlDeterministic => CliFormat::Xml,
            Self::RenderPlain
            | Self::RenderGfmTable
            | Self::RenderHtmlTable
            | Self::RenderGridTable
            | Self::RenderTree
            | Self::RenderTerminal
            | Self::RenderShell
            | Self::RenderHist => CliFormat::Render,
            Self::HtmlSource | Self::HtmlDocumentSerialize => CliFormat::Html,
            Self::PropertiesJqf10 => CliFormat::Properties,
            Self::IniJqf10 => CliFormat::Ini,
            Self::DotenvJqf10 => CliFormat::Dotenv,
            Self::MessagepackDeterministic | Self::MessagepackDeterministicFloat64 => CliFormat::Messagepack,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CliInputSelection {
    pub(crate) format: CliFormat,
    pub(crate) dialect: CliInputDialect,
    /// `--csv-delimiter BYTE`: the field delimiter the CSV record route frames with. `None` is RFC 4180's comma.
    /// Meaningful only for a CSV input selection; naming it for another input is a usage error.
    pub(crate) csv_delimiter: Option<u8>,
}

#[derive(Clone, Copy, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is one CLI formatting flag; grouping them would hide the flag surface"
)]
pub(crate) struct CliOutputSelection {
    pub(crate) format: CliFormat,
    pub(crate) dialect: CliOutputDialect,
    pub(crate) terminator: NdjsonTerminator,
    /// Structural whitespace for JSON output. NDJSON output ignores it: a record has to occupy one line for its
    /// terminator to mean anything.
    pub(crate) indent: JsonIndent,
    /// The render composition options, meaningful only for render output.
    pub(crate) render: RenderCliOptions,
    /// `-r` root-string verbatim mode (`-j` implies it).
    pub(crate) raw_strings: bool,
    /// `-S`/`--sort-keys`.
    pub(crate) sort_keys: bool,
    /// `-a`/`--ascii-output`.
    pub(crate) ascii_output: bool,
    /// `-j`/`--join-output`: JSON items publish with no facade suffix.
    pub(crate) no_newline: bool,
    /// `--raw-output0`: `-r` plus a NUL item terminator instead of a newline, overriding `-j`'s empty suffix. A root
    /// string dumped raw that itself contains a NUL byte is rejected rather than emitted.
    pub(crate) raw_output_nul: bool,
    /// The jqft-family level-composition request `--with-source`: emit the retained source (conformance level 1)
    /// instead of the canonical form. A run without the retention is a clean typed error.
    pub(crate) with_source: bool,
}

/// The render codec's composition options as parsed from the CLI.
///
/// Every field is `None` when the flag was absent, so the codec's own default applies; `Some` pins the profile exactly.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RenderCliOptions {
    /// `--render-header present|absent`.
    pub(crate) header: Option<jqf_codec_render::HeaderPolicy>,
    /// `--render-width western|cjk`.
    pub(crate) width: Option<jqf_codec_render::WidthProfile>,
    /// `--render-shape plain|table|tree`.
    pub(crate) shape: Option<jqf_codec_render::TerminalShape>,
    /// `--render-max-width N` (0 = no wrap cap).
    pub(crate) max_width: Option<usize>,
}

impl RenderCliOptions {
    /// Whether every composition flag is absent.
    const fn is_default(self) -> bool {
        self.header.is_none() && self.width.is_none() && self.shape.is_none() && self.max_width.is_none()
    }

    /// The codec's normalized encode options for this request's dials, shared by every route that serves render output
    /// (the value lane AND the record/follow/streaming lanes), so a dial cannot be honored on one route and silently
    /// dropped on another.
    pub(crate) fn encode_options(self) -> jqf_codec_render::RenderEncodeOptions {
        jqf_codec_render::RenderEncodeOptions {
            header: self.header.unwrap_or_default(),
            width: self.width.unwrap_or_default(),
            terminal_shape: self.shape.unwrap_or_default(),
            max_width: self.max_width.unwrap_or(0),
            ..jqf_codec_render::RenderEncodeOptions::default()
        }
    }
}

/// Default indentation: two spaces per open container.
pub(crate) const DEFAULT_INDENT: JsonIndent = JsonIndent::Spaces(2);

#[allow(
    clippy::large_enum_variant,
    reason = "Run carries the full parsed surface; Help is a zero-sized arm of the same command"
)]
pub(crate) enum CliCommand {
    Help,
    /// `-h`: the one-screen summary. The long form keeps the full reference — the manpage/completions generators and
    /// the surface tests parse `--help`'s output, so the full text stays the contract and the short page is the door
    /// most fingers hit first.
    ShortHelp,
    /// `--help <topic>`: one focused page for a format/dialect spelling or one of the fixed topics, generated from the
    /// acceptance tables the parser reads.
    HelpTopic(HelpTopic),
    /// jq's `-V`/`--version`: print a version and exit 0 without reading stdin.
    Version,
    /// jq's `--build-configuration`: print this binary's build facts and exit 0 without reading stdin (item 5).
    BuildConfiguration,
    /// `--list-builtins`: every registered `name/arity`, sorted — the same enumeration the `builtins` builtin answers
    /// (one law, two doors).
    ListBuiltins,
    /// `--list-formats`: the format/dialect inventory from the acceptance tables, so what is advertised and what is
    /// accepted stay one table.
    ListFormats,
    /// `--help-format <fmt>`: one per-format page built from the same tables (direction, dialects, input model facts).
    HelpFormat(CliFormat),
    /// `--explain-code <id>`: one diagnostic-code row from the codes registry (codes.toml is the manifest).
    ExplainCode(u16),
    /// `--show-config`: the effective configuration with the origin of every value, rendered by the config module after
    /// the argv/config merge. A command, like `--help`: prints and exits 0 without reading stdin.
    ShowConfig(String),
    Run(CliArguments),
    /// `jqf serve`: the resident daemon. Its arguments are parsed by [`parse_serve_subcommand`], not by the run-request
    /// parser — the daemon is a different shape of process, and a half-parsed run request must not leak into it.
    Serve(ServeArguments),
}

/// Every fixed `--help <topic>` page, in help order. The format/dialect topics are NOT listed here: a topic spelling is
/// accepted iff it is a row of the format or dialect acceptance tables (or of this table), so the topic set is one
/// enumeration derived from the tables — never a second list that can drift (053 W4, the generated-enumeration law
/// applied to the topic surface).
pub(crate) const HELP_TOPICS: [(&str, HelpTopic); 7] = [
    ("builtins", HelpTopic::Builtins),
    ("codes", HelpTopic::Codes),
    ("facts", HelpTopic::Facts),
    ("flags", HelpTopic::Flags),
    ("generators", HelpTopic::Generators),
    ("mismatch", HelpTopic::Mismatch),
    ("diff", HelpTopic::Diff),
];

/// One `--help <topic>` page. The fixed topics are rows of [`HELP_TOPICS`]; the format/dialect topics are rows of the
/// acceptance tables themselves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HelpTopic {
    /// `--help builtins`: a summary pointing at `--list-builtins` (the full enumeration lives there, never duplicated).
    Builtins,
    /// `--help codes`: a summary pointing at `--explain-code <id>`.
    Codes,
    /// `--help flags`: the full flag table — the help template's Options section, filled from the acceptance tables
    /// exactly as the help is.
    Flags,
    /// `--help facts`: node/value facts (`.@comment`, `.@tag`, `.@attrs`) and markup attributes (`.&attr`) — the
    /// differentiated read surface and the one write the edit lane supports, with worked examples.
    Facts,
    /// `--help generators`: the `~` engine namespace (`~cursor`, `~generator`, `~inputs`, `~rng`) — first-class
    /// suspended generators pulled one value at a time, with the zip example that motivates the namespace.
    Generators,
    /// `--help mismatch`: the `--mismatch-policy lenient|warn|strict` dial ('s help half), cut from the same template
    /// row.
    Mismatch,
    /// `--help diff`: the `--diff OLD NEW` lane and its per-side format dials — the page that closes 058 item 7's debt
    /// of an accepted flag with no topic page.
    Diff,
    /// `--help <format>`: the per-format page (`--help-format`'s page).
    Format(CliFormat),
    /// `--help <dialect>`: one dialect spelling's page. The spelling is a row of the input or output dialect acceptance
    /// table; the page shows every role the spelling serves (a spelling can be both — `rfc8259` and `ndjson.strict@1`
    /// are in both tables).
    Dialect(&'static str),
    /// `--help <builtin>`: one family's summary and detail from the builtin registry (`resolve_family`). The spelling
    /// is the family's canonical name, not a `name/arity` overload spelling.
    Builtin(&'static str),
}

/// Resolves one `--help <topic>` spelling. The fixed topics come from [`HELP_TOPICS`]; a format/dialect spelling is a
/// topic iff it is a row of the acceptance tables [`parse_format`]/[`parse_input_dialect`]/ [`parse_output_dialect`]
/// read — one enumeration, so the topic surface cannot advertise a spelling the parser does not accept, or vice versa.
pub(crate) fn parse_help_topic(value: &str) -> Result<HelpTopic, CliFailure> {
    if let Some((_, topic)) = HELP_TOPICS.iter().find(|(spelling, _)| *spelling == value) {
        return Ok(*topic);
    }
    if let Some((_, format)) = CliFormat::OUTPUT_FORMATS
        .iter()
        .copied()
        .find(|(spelling, _)| *spelling == value)
    {
        return Ok(HelpTopic::Format(format));
    }
    if let Some((spelling, _)) = CliInputDialect::ALL
        .iter()
        .copied()
        .find(|(spelling, _)| *spelling == value)
    {
        return Ok(HelpTopic::Dialect(spelling));
    }
    if let Some((spelling, _)) = CliOutputDialect::ALL
        .iter()
        .copied()
        .find(|(spelling, _)| *spelling == value)
    {
        return Ok(HelpTopic::Dialect(spelling));
    }
    if let Some(family) = jqf_engine::resolve_family(value) {
        return Ok(HelpTopic::Builtin(family.canonical_name));
    }
    Err(CliFailure::from(format!(
        "unknown help topic: {value:?}\nknown topics: {}",
        help_topic_spellings().join(", ")
    )))
}

/// Every topic spelling, derived from [`HELP_TOPICS`] and the format/dialect acceptance tables — the list an
/// unknown-topic error prints. A spelling in both the input and the output dialect table is one topic.
fn help_topic_spellings() -> Vec<&'static str> {
    let mut spellings: Vec<&'static str> = HELP_TOPICS.iter().map(|(spelling, _)| *spelling).collect();
    spellings.extend(CliFormat::OUTPUT_FORMATS.iter().map(|(spelling, _)| *spelling));
    spellings.extend(CliInputDialect::ALL.iter().map(|(spelling, _)| *spelling));
    spellings.extend(CliOutputDialect::ALL.iter().map(|(spelling, _)| *spelling));
    spellings.sort_unstable();
    spellings.dedup();
    spellings
}

/// The closed reserved subcommand keyword set.
///
/// One table, the same shape as the format/dialect acceptance tables: the help text reads it, the parser reads it, and
/// `053`'s generated-enumeration law covers it the way it covers formats and flags. A keyword is recognized ONLY in the
/// first-positional slot with no program-looking prefix — `-f FILE` names the program file and `--follow`'s positional
/// IS the program (the follow precedent), so `jqf --follow 'serve'` and `jqf -f serve` both work — and everything else
/// is ordinary jq text.
pub(crate) const RESERVED_KEYWORDS: [(&str, ReservedKeyword); 1] = [("serve", ReservedKeyword::Serve)];

/// What one reserved keyword means when it is recognized as a subcommand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReservedKeyword {
    /// `jqf serve`: the resident daemon.
    Serve,
}

/// The tier classification of every CLI flag (§2).
///
/// Tier P (presentation/resource) may be defaulted from the `.jqf.toml` config file; Tier S (semantic) is argv-only and
/// never config-readable. The classification test [`every_flag_carries_a_tier`] asserts every flag in the CLI surface
/// carries a row in [`FLAG_TIERS`] — an unclassified flag fails the build as "you forgot to classify", never as "it
/// defaulted to permitted".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FlagTier {
    /// Presentation/resource: changes how output looks or how much machine jqf may use, never which values a program
    /// produces. May be defaulted from a config file.
    Presentation,
    /// Semantic: changes what the program computes or what its output is. argv only, never config-readable. When in
    /// doubt, a flag is Tier S.
    Semantic,
}

/// The long spelling of every flag the parser accepts, with its tier — the single classification table.
/// `--show-config`/`--no-config`/`--config` are Tier S by ruling: the escape hatch must stay argv-only (a config that
/// could disable config is incoherent), and `--show-config` is a command that changes stdout bytes, like `--help`.
///
/// The `every_flag_carries_a_tier` test cross-checks this against the help text (the surface's enumeration source) plus
/// the parser-accepted flags the help deliberately does not document, so a flag added to the surface without a row here
/// fails the build.
pub(crate) const FLAG_TIERS: &[(&str, FlagTier)] = &[
    ("input-format", FlagTier::Semantic),
    ("input-dialect", FlagTier::Semantic),
    ("csv-delimiter", FlagTier::Semantic),
    ("header", FlagTier::Semantic),
    ("split-exp", FlagTier::Semantic),
    ("split-exp-file", FlagTier::Semantic),
    ("output-format", FlagTier::Presentation),
    ("output-dialect", FlagTier::Semantic),
    ("with-source", FlagTier::Semantic),
    ("ndjson-terminator", FlagTier::Semantic),
    ("render-header", FlagTier::Semantic),
    ("render-width", FlagTier::Semantic),
    ("render-shape", FlagTier::Semantic),
    ("render-max-width", FlagTier::Semantic),
    ("null-input", FlagTier::Semantic),
    ("raw-output", FlagTier::Semantic),
    ("raw-input", FlagTier::Semantic),
    ("slurp", FlagTier::Semantic),
    ("from-file", FlagTier::Semantic),
    ("library-path", FlagTier::Semantic),
    ("arg", FlagTier::Semantic),
    ("argjson", FlagTier::Semantic),
    ("slurpfile", FlagTier::Semantic),
    ("rawfile", FlagTier::Semantic),
    ("schema", FlagTier::Semantic),
    ("args", FlagTier::Semantic),
    ("jsonargs", FlagTier::Semantic),
    ("stream", FlagTier::Semantic),
    ("stream-errors", FlagTier::Semantic),
    ("follow", FlagTier::Semantic),
    ("seq", FlagTier::Semantic),
    ("exit-status", FlagTier::Semantic),
    ("binary", FlagTier::Semantic),
    ("build-configuration", FlagTier::Semantic),
    ("sort-keys", FlagTier::Semantic),
    ("join-output", FlagTier::Semantic),
    ("raw-output0", FlagTier::Semantic),
    ("ascii-output", FlagTier::Semantic),
    ("seed", FlagTier::Semantic),
    ("mismatch-policy", FlagTier::Presentation),
    ("strictness", FlagTier::Presentation),
    ("json-facts", FlagTier::Semantic),
    ("no-json-facts", FlagTier::Semantic),
    ("types-as-strings", FlagTier::Semantic),
    ("unbuffered", FlagTier::Presentation),
    ("color-output", FlagTier::Presentation),
    ("monochrome-output", FlagTier::Presentation),
    ("compact-output", FlagTier::Presentation),
    ("indent", FlagTier::Presentation),
    ("tab", FlagTier::Presentation),
    ("max-memory-bytes", FlagTier::Presentation),
    ("check", FlagTier::Presentation),
    ("max-rss", FlagTier::Presentation),
    ("max-spill-bytes", FlagTier::Presentation),
    ("max-spill-disk-bytes", FlagTier::Presentation),
    ("max-iterations", FlagTier::Presentation),
    ("parallel", FlagTier::Presentation),
    ("no-parallel", FlagTier::Presentation),
    ("workers", FlagTier::Presentation),
    ("diagnostics", FlagTier::Presentation),
    ("explain", FlagTier::Presentation),
    ("plan-out", FlagTier::Semantic),
    ("plan-file", FlagTier::Semantic),
    ("edit", FlagTier::Semantic),
    ("edit-expand-alias", FlagTier::Semantic),
    ("diff", FlagTier::Semantic),
    ("old-format", FlagTier::Semantic),
    ("new-format", FlagTier::Semantic),
    ("output", FlagTier::Semantic),
    ("in-place", FlagTier::Semantic),
    ("no-atomic", FlagTier::Semantic),
    ("list-builtins", FlagTier::Semantic),
    ("list-formats", FlagTier::Semantic),
    ("help-format", FlagTier::Semantic),
    ("explain-code", FlagTier::Semantic),
    ("help", FlagTier::Semantic),
    ("version", FlagTier::Semantic),
    ("config", FlagTier::Semantic),
    ("no-config", FlagTier::Semantic),
    ("show-config", FlagTier::Semantic),
];

/// The `jqf serve` subcommand's whole surface.
///
/// Deliberately small: `--listen` (required), the program (positional or `-f`), the governor dial, and the diagnostics
/// flag. The daemon is NDJSON in / NDJSON out by construction (the session protocol), so the format and dialect flags a
/// run request owns do not exist here.
pub(crate) struct ServeArguments {
    /// The program source, when given positionally.
    pub(crate) program: Option<String>,
    /// The program file (`-f FILE`), when the program comes from a file.
    pub(crate) program_file: Option<PathBuf>,
    /// The `--listen` target: a unix-socket path or `host:port`.
    pub(crate) listen: String,
    /// `--diagnostics`: print provenance, per-session plan lines, and the retained diagnostic records on the daemon's
    /// stderr.
    pub(crate) diagnostics: bool,
    /// `--max-rss N|N%|0`: the RSS governor's dial, per-daemon. `None` is the default 80% of detected effective memory,
    /// exactly as on a run request.
    pub(crate) max_rss: Option<crate::rss::MaxRss>,
    /// `--read-timeout SECONDS` : the per-connection read/idle timeout. A connection that sends nothing for this long
    /// ends its session cleanly and the daemon accepts the next one — a dribbling client must not hold the serial
    /// accept loop. `0` disables the timeout; the default is 60 seconds.
    pub(crate) read_timeout: std::time::Duration,
}

/// Parses the arguments after a `serve` keyword.
///
/// The keyword was already consumed; this parses the rest as the daemon's surface. `--help`/`-h` anywhere returns the
/// global help (which documents the subcommand), and every unknown option is a usage error before any byte is read.
fn parse_serve_subcommand(
    arguments: &mut Box<dyn Iterator<Item = std::ffi::OsString>>,
) -> Result<CliCommand, CliFailure> {
    let mut program = None;
    let mut program_file = None;
    let mut listen = None;
    let mut diagnostics = false;
    let mut max_rss = None;
    let mut read_timeout = std::time::Duration::from_mins(1);
    let mut read_timeout_seen = false;
    let mut end_of_options = false;
    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| CliFailure::from("program argument is not valid UTF-8"))?;
        if !end_of_options && argument == "--" {
            end_of_options = true;
            continue;
        }
        if !end_of_options && argument == "--help" {
            return Ok(CliCommand::Help);
        }
        if !end_of_options && argument == "-h" {
            return Ok(CliCommand::ShortHelp);
        }
        if !end_of_options && (argument == "-f" || argument == "--from-file") {
            if program_file.is_some() {
                return Err("-f may only be given once".into());
            }
            program_file = Some(next_flag_path(arguments, "-f")?);
            continue;
        }
        if !end_of_options && argument == "--listen" {
            if listen.is_some() {
                return Err("--listen may only be given once".into());
            }
            listen = Some(next_flag_value(arguments, "--listen")?);
            continue;
        }
        if !end_of_options && argument == "--diagnostics" {
            diagnostics = true;
            continue;
        }
        if !end_of_options && argument == "--max-rss" {
            if max_rss.is_some() {
                return Err("--max-rss may only be given once".into());
            }
            let value = next_flag_value(arguments, "--max-rss")?;
            max_rss = Some(parse_max_rss(&value)?);
            continue;
        }
        if !end_of_options && argument == "--read-timeout" {
            // Once-only, exactly like its siblings here (--listen, --max-rss, -f): a repeated dial is a mistake, never
            // a silent last-wins.
            if read_timeout_seen {
                return Err("--read-timeout may only be given once".into());
            }
            let value = next_flag_value(arguments, "--read-timeout")?;
            let seconds = value
                .parse::<u64>()
                .map_err(|_| CliFailure::from("--read-timeout value is not a valid nonnegative integer of seconds"))?;
            read_timeout = std::time::Duration::from_secs(seconds);
            read_timeout_seen = true;
            continue;
        }
        if !end_of_options && argument.starts_with("--") {
            return Err(format!("unknown option: {argument}").into());
        }
        if !end_of_options
            && argument.starts_with('-')
            && argument.len() >= 2
            && argument[1..].chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        {
            return Err(format!("unknown option: {argument}").into());
        }
        if program.is_none() && program_file.is_none() {
            program = Some(argument);
        } else {
            return Err("jqf serve takes exactly one PROGRAM".into());
        }
    }
    let Some(listen) = listen else {
        return Err("jqf serve requires --listen <unix-socket|host:port>".into());
    };
    Ok(CliCommand::Serve(ServeArguments {
        program,
        program_file,
        listen,
        diagnostics,
        max_rss,
        read_timeout,
    }))
}

/// One CLI binding, in command-line order. Order is the law: jq's duplicate-name rule is first-positional-wins across
/// ALL four binding flag kinds (`--arg a 1 --slurpfile a f` answers `1`; the reverse answers the file's array), so the
/// four kinds must never be processed in kind batches.
#[derive(Clone, Debug)]
pub(crate) enum CliBinding {
    /// jq's `--arg NAME VALUE`: `$NAME` is the raw string `VALUE`.
    Arg(String, String),
    /// jq's `--argjson NAME VALUE`: `$NAME` is `VALUE` parsed as exactly one strict JSON value.
    ArgJson(String, String),
    /// jq's `--slurpfile NAME FILE`: `$NAME` is the array of every JSON value read from `FILE`.
    SlurpFile(String, PathBuf),
    /// jq's `--rawfile NAME FILE`: `$NAME` is `FILE`'s raw bytes as a string.
    RawFile(String, PathBuf),
}

/// One `--args`/`--jsonargs` positional value, with the parse mode that was active when it was seen.
///
/// jq switches the mode each time the flag reappears (`--args a --jsonargs 2` answers `["a", 2]`), so a single ordered
/// list carries both spellings and the parse law travels with each entry.
#[derive(Clone, Debug)]
pub(crate) enum PositionalArg {
    /// jq's `--args`: the raw string value.
    String(String),
    /// jq's `--jsonargs`: the value parsed as exactly one strict JSON value.
    Json(String),
}

/// The payload of one [`CliBinding`], matched out for value construction. The borrows keep the ordered list untouched
/// while each entry resolves. A file-backed entry carries the bytes read BEFORE stdin was touched; the value
/// construction (parse, array wrap) charges the request ledger.
pub(crate) enum BindingKind<'a> {
    Arg(&'a str),
    ArgJson(&'a str),
    SlurpFile(Option<Vec<u8>>),
    RawFile(Option<Vec<u8>>),
}

/// Parsed command-line arguments: at most one program filter, plus explicit format/dialect selections and request
/// policy.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is one jq CLI flag; grouping them would hide the flag surface"
)]
pub(crate) struct CliArguments {
    pub(crate) program: Option<String>,
    /// jq's `-f`/`--from-file`: read the program from this path instead of the positional argument.
    pub(crate) program_file: Option<PathBuf>,
    /// jq's `-n`/`--null-input`: run the filter once over `null`; the input stream exists only for the input family
    /// (`input`/`inputs`).
    pub(crate) null_input: bool,
    /// jq's `-R`/`--raw-input`: read input as raw strings — one JSON string per line, or (with `--slurp`) the whole
    /// input as one string.
    pub(crate) raw_input: bool,
    /// jq's `-s`/`--slurp`: collect every parsed input into one array and run the filter once over it.
    pub(crate) slurp: bool,
    /// jq's `--arg NAME VALUE` bindings, in CLI order.
    ///
    /// The four binding flags share one ordered list because jq's duplicate-name law is FIRST POSITIONAL WINS across
    /// all of them (`--argjson a 2 --arg a 1` answers `2`): a binding that came later on the command line never
    /// overrides an earlier one, whatever kind it is.
    pub(crate) bindings: Vec<CliBinding>,
    pub(crate) max_memory_bytes: u64,
    /// The external sort's per-run spill budget (deferred item 17); 0 = never spill, the in-memory floor is the only
    /// meaning.
    pub(crate) max_spill_bytes: u64,
    /// The cumulative spill-DISK ceiling ; 0 = unset, no ceiling — the spill fallback law answers exactly as before.
    pub(crate) max_spill_disk_bytes: u64,
    /// The opt-in per-run iteration ceiling (`--max-iterations N`); `None` is unlimited (the default). Carried on the
    /// pipeline policy into every engine run: a run whose cumulative frame-task transitions cross N is refused (the
    /// machine resource family, exit class 5). This is the operator's runaway-loop dial; jqf never imposes a ceiling of
    /// its own.
    pub(crate) max_iterations: Option<u64>,
    /// The physical memory dial: `Some` when `--max-rss` was given, `None` for the default `80%`. Resolution to a byte
    /// ceiling happens in `rss:configure`, beside the detection it needs.
    pub(crate) max_rss: Option<crate::rss::MaxRss>,
    /// jq's `-L`/`--library-path` module search directories, in order.
    pub(crate) library_paths: Vec<PathBuf>,
    pub(crate) input: CliInputSelection,
    pub(crate) output: CliOutputSelection,
    pub(crate) parallel: CliParallelSelection,
    /// The mismatch dial (`052`): `--mismatch-policy lenient|warn|strict`. `None` is the default — lenient IS jq, and
    /// the compat corpus runs under it. Warn answers jq's value and adds a capped per-cell report to stderr after the
    /// run; strict turns each cell into a raise (exit 5).
    pub(crate) mismatch_policy: Option<jqf_resource::policy::MismatchPolicy>,
    /// The strictness dial : `--strictness error|warn|strict`. `None` is the default — `Error`, byte-identical to
    /// today.
    pub(crate) strictness: Option<jqf_resource::policy::StrictnessPolicy>,
    /// `--json-facts`: wrap the program with the `json_facts/0` projection so attached facts appear in JSON output (see
    /// `--help facts`).
    pub(crate) json_facts: bool,
    /// `--types-as-strings` (D5): read every extended temporal kind as its canonical text (a plain string) instead of
    /// its native kind, so a jq program sees only the six jq types. The document still stores the rich kinds; only
    /// program-visible values cast, so encoding round-trips untouched values. A REBUILT value (one that was
    /// materialized and re-inserted) re-encodes as its string form — `--edit` on a date downgrades it to a quoted
    /// string.
    pub(crate) types_as_strings: bool,
    pub(crate) diagnostics: bool,
    /// Print the request's plan, route, timing, and cost to stderr (`--explain`).
    pub(crate) explain: bool,
    /// jq's `-e`/`--exit-status`: the success exit code reflects the LAST output value's truthiness (0 truthy, 1
    /// false/null, 4 no output).
    pub(crate) exit_status: bool,
    /// `--seed N`: primes the rand family's deterministic draw state (a jqf extension beyond jq), stored as
    /// `rand(seed)`'s own bit-reinterpreted `u64` so the two seeding paths agree on what one integer means.
    pub(crate) seed: Option<u64>,
    /// jq's `--unbuffered`: flush stdout after every item.
    pub(crate) unbuffered: bool,
    /// jq's color switches: `-C` forces colour on, `-M` forces it off and always wins, and neither means the
    /// `TTY`/`NO_COLOR` default. The resolved decision happens in `run`, where the destination's terminal-ness and
    /// `NO_COLOR` are known.
    pub(crate) colour: crate::colour::ColourRequest,
    /// jq's `--stream`: parse each input value in streaming fashion, running the filter once per `tostream` item.
    pub(crate) stream: bool,
    /// jq's `--stream-errors`: implies `--stream` and reports parse errors as `[message, path]` events instead of
    /// aborting.
    pub(crate) stream_errors: bool,
    /// Live tailing input mode (`--follow`): records are processed as they arrive from a growing file or pipe, never
    /// read whole.
    pub(crate) follow: bool,
    /// jq's `--seq` input side: true when `--seq` was given with no explicit input format/dialect, selecting the
    /// flag-scoped RECOVERING json-seq profile (the registered dialect stays `json-seq.strict@1`).
    pub(crate) seq_recovering: bool,
    /// jq's `--args`/`--jsonargs` positional values, in command-line order. Each is parsed under the mode active when
    /// it was seen (string or JSON).
    pub(crate) positional_args: Vec<PositionalArg>,
    /// Write this program's serialized routing-facts plan to PATH after compiling, before any input is read
    /// (`--plan-out`).
    pub(crate) plan_out: Option<PathBuf>,
    /// Read a serialized plan from PATH and verify it matches the compiled program's routing facts; a mismatch is a
    /// hard error, never a silent fallback (`--plan-file`).
    pub(crate) plan_file: Option<PathBuf>,
    /// Whole-document output subject: the program's edits, not its outputs.
    pub(crate) edit: bool,
    /// The alias-site escape hatch (/ lane C3): lets an edit descend into an alias-referenced YAML node, accepting the
    /// anchor-rewrite semantics (every alias site changes). The refusal stays the default; the flag is the documented
    /// opt-out.
    pub(crate) edit_expand_alias: bool,
    /// `--edit --check` : do not write; exit 1 if the edit WOULD change the file, exit 0 if it is byte-identical, print
    /// nothing (the gofmt -l verdict shape).
    pub(crate) edit_check: bool,
    /// Write destination; absent means stdout.
    pub(crate) output_path: Option<PathBuf>,
    /// The `--split-exp` destination: one file per published item, its path the expression's single string output over
    /// that item, with `$index` bound to the item counter. The expression text as given; `--split-exp-file`'s path is
    /// carried separately and read by the driver. `None` means the ordinary destinations.
    pub(crate) split_exp: Option<String>,
    /// `--split-exp-file PATH`: read the split expression from PATH (the driver's file read, beside `-f`). Mutually
    /// exclusive with [`Self:split_exp`].
    pub(crate) split_exp_file: Option<PathBuf>,
    /// Edit the positional input files in place: each is read as the request's input and its output is written back to
    /// the same file, independently.
    pub(crate) in_place: bool,
    /// jq-style positional input files, in argument order. Read (and concatenated as one byte stream, exactly as jq
    /// joins files) when no `--in-place` is given; with `--in-place` each is edited independently.
    pub(crate) input_files: Vec<PathBuf>,
    /// The `--diff OLD NEW` pair: read the two files as exactly one document each (in the per-side formats,) and print
    /// their path-keyed semantic diff.
    pub(crate) diff_pair: Option<(PathBuf, PathBuf)>,
    /// The per-side `--diff` selections: `--old-format F` / `--new-format F`, each defaulting to the input selection. A
    /// side's format and dialect resolve like any input selection; the shared `--input-dialect` applies when it is
    /// valid for the side's format.
    pub(crate) diff_old_selection: CliInputSelection,
    pub(crate) diff_new_selection: CliInputSelection,
    /// `--schema FILE` : the value-schema document every input value is validated against before the program runs. The
    /// file is read as exactly one strict JSON value and bound as `$__schema`; the program is rewritten so a failing
    /// value publishes its ordered error objects and the request exits 3.
    pub(crate) schema_file: Option<PathBuf>,
    /// Write file destinations directly instead of atomically.
    pub(crate) no_atomic: bool,
    /// The config file(s) whose values were merged into this request (plan 064), for the --diagnostics provenance line.
    /// `None` under `--no-config`/`JQF_NO_CONFIG` or with no config on disk.
    pub(crate) config_source: Option<String>,
}

/// The parallelism switch and its dial, resolved together.
///
/// The switch and the dial are separate on purpose (design doc §3, §6): the switch decides WHETHER any parallelism
/// engages, and naming the dial against an OFF switch is a usage error rather than a silent half-engagement, so a
/// measurement script can never attribute serial timings to `--workers N`. The switch defaults to ON (design doc §6
/// rollout law, §10); it reaches only the explicit-NDJSON record route, so nothing about the default JSON path changes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CliParallelSelection {
    pub(crate) enabled: bool,
    pub(crate) workers: WorkerRequest,
}

/// Parses jq's `--indent` argument, reproducing its accepted range exactly.
///
/// jq spells "one tab per level" as `--indent -1`, which is the same output as `--tab`, and caps spaces at 7. `--indent
/// 0` keeps every line break and drops the indentation, which is NOT the same output as `--compact-output`.
pub(crate) fn parse_indent(value: &str) -> Result<JsonIndent, CliFailure> {
    let width = value
        .parse::<i8>()
        .map_err(|_| CliFailure::from("--indent takes a number between -1 and 7"))?;
    match width {
        -1 => Ok(JsonIndent::Tabs),
        0..=7 => Ok(JsonIndent::Spaces(
            u8::try_from(width).map_err(|_| CliFailure::from("--indent value is out of range"))?,
        )),
        _ => Err("--indent takes a number between -1 and 7".into()),
    }
}

pub(crate) fn parse_workers(value: &str) -> Result<WorkerRequest, CliFailure> {
    if value == "auto" {
        return Ok(WorkerRequest::Auto);
    }
    let width = value
        .parse::<usize>()
        .map_err(|_| CliFailure::from(format!("--workers value is not `auto` or a count: {value:?}")))?;
    if width == 0 {
        return Err("--workers requires at least one worker".into());
    }
    if width > WORKER_HARD_CAP {
        return Err(format!("--workers exceeds the hard cap of {WORKER_HARD_CAP}").into());
    }
    Ok(WorkerRequest::Explicit(width))
}

/// Resolves the switch and the dial, rejecting a dial against an off switch.
///
/// `--parallel` and `--no-parallel` are exclusive. The switch is ON by default, so `--workers` alone is the ordinary
/// way to set a width; `--workers` with `--no-parallel` is the usage error the contract requires, so the exit class is
/// 2 and no byte of stdin is read.
///
/// A value that reached this check via a config file never blocks an explicit argv flag: argv wins, and an error never
/// names a flag the user did not type.
pub(crate) fn resolve_parallel_selection(
    parallel: Option<bool>,
    workers: Option<WorkerRequest>,
    parallel_from_argv: bool,
    workers_from_argv: bool,
) -> Result<CliParallelSelection, CliFailure> {
    let enabled = parallel.unwrap_or(true);
    if workers.is_some() && !enabled {
        if workers_from_argv && !parallel_from_argv {
            return Ok(CliParallelSelection {
                enabled: true,
                workers: workers.unwrap_or(WorkerRequest::Auto),
            });
        }
        if parallel_from_argv && !workers_from_argv {
            return Ok(CliParallelSelection {
                enabled: false,
                workers: WorkerRequest::Auto,
            });
        }
        if parallel_from_argv && workers_from_argv {
            return Err("--workers cannot be combined with --no-parallel".into());
        }
        return Err("workers cannot be combined with parallel = false".into());
    }
    Ok(CliParallelSelection {
        enabled,
        workers: workers.unwrap_or(WorkerRequest::Auto),
    })
}

/// Parses one `--csv-delimiter` value: a single ASCII byte, or the two-character `\t` escape for a tab. The byte must
/// be one the CSV codec accepts as a field delimiter (`is_valid_delimiter` — the codec's own closed rule, so a byte the
/// framer would refuse cannot be named).
fn parse_csv_delimiter(value: &str) -> Result<u8, CliFailure> {
    let byte = if value == "\\t" {
        b'\t'
    } else if let [byte] = value.as_bytes() {
        *byte
    } else {
        return Err("--csv-delimiter must be a single ASCII byte, or \\t for a tab".into());
    };
    if !jqf_codec_delimited::is_valid_delimiter(byte) {
        return Err(format!("--csv-delimiter {value:?} is not a delimiter the CSV codec accepts").into());
    }
    Ok(byte)
}

/// Parses one `--max-rss` value: `N` bytes, `N%` of the detected effective memory, or `0` to disable. A byte count may
/// carry a size suffix (`k`/`K` = 1024, `m`/`M` = 1024^2, `g`/`G` = 1024^3 — 091 §3, the same law the sibling memory
/// dials own). A percent ceiling must be at least 1 (0 already means "disabled"); the resolution to bytes happens in
/// `rss:configure`.
pub(crate) fn parse_max_rss(value: &str) -> Result<crate::rss::MaxRss, CliFailure> {
    if let Some(percent) = value.strip_suffix('%') {
        let percent = percent
            .parse::<u64>()
            .map_err(|_| CliFailure::from("--max-rss percent value is not a valid positive integer (use N%)"))?;
        if percent == 0 {
            return Err(CliFailure::from(
                "--max-rss 0% is not a ceiling; use --max-rss 0 to disable",
            ));
        }
        return Ok(crate::rss::MaxRss::Percent(percent));
    }
    let (digits, multiplier) = match value.as_bytes() {
        [rest @ .., b'k' | b'K'] => (&value[..rest.len()], 1u64 << 10),
        [rest @ .., b'm' | b'M'] => (&value[..rest.len()], 1u64 << 20),
        [rest @ .., b'g' | b'G'] => (&value[..rest.len()], 1u64 << 30),
        _ => (value, 1),
    };
    let bytes = digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .ok_or_else(|| {
            CliFailure::from(
                "--max-rss value must be N bytes (k/K/m/M/g/G suffixes accepted), \
                 N% of physical memory, or 0 to disable",
            )
        })?;
    if bytes == 0 {
        Ok(crate::rss::MaxRss::Disabled)
    } else {
        Ok(crate::rss::MaxRss::Bytes(bytes))
    }
}

pub(crate) fn next_flag_value(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &'static str,
) -> Result<String, CliFailure> {
    arguments
        .next()
        .ok_or_else(|| CliFailure::from(format!("{flag} requires a value")))?
        .into_string()
        .map_err(|_| CliFailure::from(format!("{flag} value is not valid UTF-8")))
}

/// The next argv token as a filesystem path. Paths are OS strings: jq accepts a non-UTF-8 filename, and so does this
/// parser. Fail only when the flag is missing its value.
pub(crate) fn next_flag_path(
    arguments: &mut impl Iterator<Item = OsString>,
    flag: &'static str,
) -> Result<PathBuf, CliFailure> {
    arguments
        .next()
        .ok_or_else(|| CliFailure::from(format!("{flag} requires a value")))
        .map(PathBuf::from)
}

/// Resolves one format spelling against the acceptance table its flag family owns: input-side flags validate against
/// [`CliFormat:INPUT_FORMATS`], output-side flags against [`CliFormat:OUTPUT_FORMATS`]. The tables are not
/// interchangeable — the output set is a strict superset today, and an input-only format would silently break
/// advertise-equals-accept if an input flag validated against the output table.
pub(crate) fn parse_format(
    flag: &'static str,
    value: &str,
    table: &[(&'static str, CliFormat)],
) -> Result<CliFormat, CliFailure> {
    table
        .iter()
        .copied()
        .find(|(spelling, _)| *spelling == value)
        .map(|(_, format)| format)
        .ok_or_else(|| CliFailure::from(format!("unknown {flag} value: {value:?}")))
}

/// Whether two paths name one file: identical spellings always, and otherwise — when both resolve — by (device, inode),
/// so `./f`, a hard link, or a symlink to the same target is caught. A path that cannot be stated (missing, or an
/// inaccessible prefix) compares only as itself.
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if a == b {
        return true;
    }
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a_meta), Ok(b_meta)) => a_meta.dev() == b_meta.dev() && a_meta.ino() == b_meta.ino(),
        _ => false,
    }
}

/// The non-unix twin: no portable inode identity, so only spellings compare.
#[cfg(not(unix))]
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
}

/// Resolves one `--input-dialect` spelling against the single acceptance table. The unknown-value error names the flag
/// the way the caller wrote it.
fn parse_input_dialect(flag: &'static str, value: &str) -> Result<CliInputDialect, CliFailure> {
    CliInputDialect::ALL
        .iter()
        .copied()
        .find(|(spelling, _)| *spelling == value)
        .map(|(_, dialect)| dialect)
        .ok_or_else(|| CliFailure::from(format!("unknown {flag} value: {value:?}")))
}

/// Resolves one `--output-dialect` spelling against the single acceptance table.
fn parse_output_dialect(flag: &'static str, value: &str) -> Result<CliOutputDialect, CliFailure> {
    CliOutputDialect::ALL
        .iter()
        .copied()
        .find(|(spelling, _)| *spelling == value)
        .map(|(_, dialect)| dialect)
        .ok_or_else(|| CliFailure::from(format!("unknown {flag} value: {value:?}")))
}

/// Resolves the input pair, defaulting the dialect from the format.
///
/// An invalid pair is a usage error raised BEFORE stdin is consumed, so a mistyped dialect never half-reads a stream.
#[allow(
    clippy::too_many_lines,
    reason = "one resolution table for every accepted format/dialect pair; the arm count grows with the surface"
)]
pub(crate) fn resolve_input_selection(
    format: Option<CliFormat>,
    dialect: Option<CliInputDialect>,
    csv_delimiter: Option<u8>,
    csv_header: bool,
) -> Result<CliInputSelection, CliFailure> {
    let format = format.unwrap_or(CliFormat::Json);
    if format == CliFormat::Render {
        return Err("render is an output-only format; it registers no input".into());
    }
    // `--header` names the headered dialect on its own; an explicit ARRAY dialect of either family contradicts it (the
    // flag would silently flip the row shape the spelling names).
    if csv_header
        && format == CliFormat::Csv
        && matches!(dialect, Some(CliInputDialect::CsvRfc4180 | CliInputDialect::CsvUtf8))
    {
        let named = match dialect {
            Some(CliInputDialect::CsvUtf8) => "csv.utf8@1",
            _ => "csv.rfc4180@1",
        };
        return Err(format!(
            "--header contradicts --input-dialect {named} (the positional-array \
                    dialect); name one or the other"
        )
        .into());
    }
    if csv_header && format == CliFormat::Tsv && dialect == Some(CliInputDialect::TsvUtf8) {
        return Err("--header contradicts --input-dialect tsv.utf8@1 (the positional-array \
                    dialect); name one or the other"
            .into());
    }
    let dialect = dialect.unwrap_or(match format {
        CliFormat::Json => CliInputDialect::Rfc8259,
        // The trailing dialect is the JSONC default: the corpus the format exists to read permits a trailing comma.
        CliFormat::Jsonc => CliInputDialect::JsoncTrailing,
        CliFormat::Json5 => CliInputDialect::Json5Document,
        CliFormat::Ndjson => CliInputDialect::NdjsonStrict,
        CliFormat::JsonSeq => CliInputDialect::JsonSeqStrict,
        CliFormat::Toml => CliInputDialect::Toml10,
        // The array dialect stays the default: row 1 of a headerless file is data, and no content test tells a header
        // from data. `--header` is the one-word opt-in. The short format selects the Unicode-capable utf8 family ; the
        // frozen RFC alphabet is the explicit `--input-dialect csv.rfc4180@1`.
        CliFormat::Csv if csv_header => CliInputDialect::CsvUtf8Header,
        CliFormat::Csv => CliInputDialect::CsvUtf8,
        CliFormat::Tsv if csv_header => CliInputDialect::TsvUtf8Header,
        CliFormat::Tsv => CliInputDialect::TsvUtf8,
        CliFormat::Cbor => CliInputDialect::CborGeneric,
        CliFormat::CborSeq => CliInputDialect::CborSeqGeneric,
        CliFormat::Yaml => CliInputDialect::YamlCore,
        CliFormat::Jqft => CliInputDialect::JqftDocument,
        CliFormat::Jqfjson => CliInputDialect::JqfjsonDocument,
        CliFormat::Jqfb => CliInputDialect::JqfbDocument,
        CliFormat::Xml => CliInputDialect::XmlDocument,
        CliFormat::Html => CliInputDialect::HtmlDocument,
        CliFormat::Properties => CliInputDialect::PropertiesJdk,
        CliFormat::Ini => CliInputDialect::IniJqfStrict,
        CliFormat::Dotenv => CliInputDialect::DotenvJqfStrict,
        CliFormat::Messagepack => CliInputDialect::MessagepackUtf8,
        CliFormat::Render => unreachable!("render input is rejected above"),
    });
    let valid = matches!(
        (format, dialect),
        (CliFormat::Json, CliInputDialect::Rfc8259)
            | (
                CliFormat::Jsonc,
                CliInputDialect::JsoncTrailing | CliInputDialect::JsoncDefault
            )
            | (CliFormat::Json5, CliInputDialect::Json5Document)
            | (
                CliFormat::Ndjson,
                CliInputDialect::NdjsonStrict | CliInputDialect::NdjsonRecovering
            )
            | (CliFormat::JsonSeq, CliInputDialect::JsonSeqStrict)
            | (CliFormat::Toml, CliInputDialect::Toml10 | CliInputDialect::Toml11)
            | (
                CliFormat::Csv,
                CliInputDialect::CsvUtf8
                    | CliInputDialect::CsvUtf8Header
                    | CliInputDialect::CsvRfc4180
                    | CliInputDialect::CsvRfc4180Header
            )
            | (
                CliFormat::Tsv,
                CliInputDialect::TsvUtf8 | CliInputDialect::TsvUtf8Header
            )
            | (CliFormat::Cbor, CliInputDialect::CborGeneric)
            | (CliFormat::CborSeq, CliInputDialect::CborSeqGeneric)
            | (
                CliFormat::Yaml,
                CliInputDialect::YamlFailsafe | CliInputDialect::YamlJson | CliInputDialect::YamlCore
            )
            | (CliFormat::Jqft, CliInputDialect::JqftDocument)
            | (CliFormat::Jqfjson, CliInputDialect::JqfjsonDocument)
            | (CliFormat::Jqfb, CliInputDialect::JqfbDocument)
            | (CliFormat::Xml, CliInputDialect::XmlDocument)
            | (
                CliFormat::Html,
                CliInputDialect::HtmlDocument | CliInputDialect::HtmlFragment
            )
            | (CliFormat::Properties, CliInputDialect::PropertiesJdk)
            | (CliFormat::Ini, CliInputDialect::IniJqfStrict)
            | (CliFormat::Dotenv, CliInputDialect::DotenvJqfStrict)
            | (
                CliFormat::Messagepack,
                CliInputDialect::MessagepackUtf8 | CliInputDialect::MessagepackKeyEquivalence
            )
    );
    if !valid {
        return Err(format!("invalid input format/dialect pair: {}/{}", format.id(), dialect.id()).into());
    }
    // The delimiter dial is delimited-only, and the TSV grammar binds its own delimiter: a delimiter named for TSV is a
    // usage error, never a silently ignored flag.
    if csv_delimiter.is_some() && format != CliFormat::Csv {
        return Err("--csv-delimiter applies to CSV input only".into());
    }
    Ok(CliInputSelection {
        format,
        dialect,
        csv_delimiter,
    })
}

#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    clippy::too_many_lines,
    reason = "resolve_output_selection takes every output choice the flags express, and bundling \
              them would hide the flag surface the resolver exists to resolve; the TSV arms (134) \
              pushed it past the default line budget"
)]
pub(crate) fn resolve_output_selection(
    format: Option<CliFormat>,
    dialect: Option<CliOutputDialect>,
    terminator: Option<NdjsonTerminator>,
    indent: JsonIndent,
    render: RenderCliOptions,
    raw_strings: bool,
    sort_keys: bool,
    ascii_output: bool,
    no_newline: bool,
    raw_output_nul: bool,
    with_source: bool,
    csv_header: bool,
) -> Result<CliOutputSelection, CliFailure> {
    let format = format.unwrap_or(CliFormat::Json);
    // `--header` on output mirrors the input law: an explicit ARRAY dialect of either family contradicts it.
    if csv_header
        && format == CliFormat::Csv
        && matches!(
            dialect,
            Some(CliOutputDialect::CsvJqfRfc4180 | CliOutputDialect::CsvJqfUtf8)
        )
    {
        let named = match dialect {
            Some(CliOutputDialect::CsvJqfUtf8) => "csv.jqf-utf8@1",
            _ => "csv.jqf-rfc4180@1",
        };
        return Err(format!(
            "--header contradicts --output-dialect {named} (the \
                    positional-array dialect); name one or the other"
        )
        .into());
    }
    if csv_header && format == CliFormat::Tsv && dialect == Some(CliOutputDialect::TsvJqfLf) {
        return Err("--header contradicts --output-dialect tsv.jqf-lf@1 (the \
                    positional-array dialect); name one or the other"
            .into());
    }
    let dialect = dialect.unwrap_or(match format {
        CliFormat::Json => CliOutputDialect::Rfc8259,
        // The trailing output profile mirrors the trailing input default.
        CliFormat::Jsonc => CliOutputDialect::JsoncTrailingJqf,
        CliFormat::Json5 => CliOutputDialect::Json5Jqf,
        CliFormat::Ndjson => CliOutputDialect::NdjsonStrict,
        CliFormat::JsonSeq => CliOutputDialect::JsonSeqJqf,
        CliFormat::Toml => CliOutputDialect::TomlJqf10,
        // The encode mirror of the input opt-in: object rows write their header row once, so `--header` round-trips a
        // table. The short format selects the Unicode-capable utf8 family ; `csv.jqf-rfc4180@1` is the RFC-named
        // opt-in.
        CliFormat::Csv if csv_header => CliOutputDialect::CsvJqfUtf8Header,
        CliFormat::Csv => CliOutputDialect::CsvJqfUtf8,
        CliFormat::Tsv if csv_header => CliOutputDialect::TsvJqfLfHeader,
        CliFormat::Tsv => CliOutputDialect::TsvJqfLf,
        CliFormat::Cbor => CliOutputDialect::CborPreferred,
        CliFormat::CborSeq => CliOutputDialect::CborSeqJqf,
        CliFormat::Yaml => CliOutputDialect::YamlBlock,
        CliFormat::Jqft => CliOutputDialect::JqftCanonical,
        CliFormat::Jqfjson => CliOutputDialect::JqfjsonCanonical,
        CliFormat::Jqfb => CliOutputDialect::JqfbCanonical,
        CliFormat::Xml => CliOutputDialect::XmlDeterministic,
        CliFormat::Html => CliOutputDialect::HtmlDocumentSerialize,
        CliFormat::Properties => CliOutputDialect::PropertiesJqf10,
        CliFormat::Ini => CliOutputDialect::IniJqf10,
        CliFormat::Dotenv => CliOutputDialect::DotenvJqf10,
        CliFormat::Messagepack => CliOutputDialect::MessagepackDeterministic,
        CliFormat::Render => CliOutputDialect::RenderTree,
    });
    let valid = matches!(
        (format, dialect),
        (CliFormat::Json, CliOutputDialect::Rfc8259)
            | (
                CliFormat::Jsonc,
                CliOutputDialect::JsoncTrailingJqf | CliOutputDialect::JsoncDefaultJqf | CliOutputDialect::JsoncJqf
            )
            | (
                CliFormat::Json5,
                CliOutputDialect::Json5Jqf | CliOutputDialect::Json5Jqf10
            )
            | (CliFormat::Ndjson, CliOutputDialect::NdjsonStrict)
            | (CliFormat::JsonSeq, CliOutputDialect::JsonSeqJqf)
            | (
                CliFormat::Toml,
                CliOutputDialect::TomlJqf10 | CliOutputDialect::TomlJqf11
            )
            | (
                CliFormat::Csv,
                CliOutputDialect::CsvJqfUtf8
                    | CliOutputDialect::CsvJqfUtf8Header
                    | CliOutputDialect::CsvJqfRfc4180
                    | CliOutputDialect::CsvJqfRfc4180Header
            )
            | (
                CliFormat::Tsv,
                CliOutputDialect::TsvJqfLf | CliOutputDialect::TsvJqfLfHeader
            )
            | (
                CliFormat::Cbor,
                CliOutputDialect::CborSource
                    | CliOutputDialect::CborPreferred
                    | CliOutputDialect::CborCoreDeterministic
                    | CliOutputDialect::CborLengthFirst
            )
            | (CliFormat::CborSeq, CliOutputDialect::CborSeqJqf)
            | (
                CliFormat::Yaml,
                CliOutputDialect::YamlStreamCanonical
                    | CliOutputDialect::YamlSingleDocument
                    | CliOutputDialect::YamlBlock
                    | CliOutputDialect::YamlJqf
            )
            | (CliFormat::Jqft, CliOutputDialect::JqftCanonical)
            | (CliFormat::Jqfjson, CliOutputDialect::JqfjsonCanonical)
            | (CliFormat::Jqfb, CliOutputDialect::JqfbCanonical)
            | (
                CliFormat::Xml,
                CliOutputDialect::XmlSource | CliOutputDialect::XmlDeterministic
            )
            | (
                CliFormat::Html,
                CliOutputDialect::HtmlSource | CliOutputDialect::HtmlDocumentSerialize
            )
            | (CliFormat::Properties, CliOutputDialect::PropertiesJqf10)
            | (CliFormat::Ini, CliOutputDialect::IniJqf10)
            | (CliFormat::Dotenv, CliOutputDialect::DotenvJqf10)
            | (
                CliFormat::Messagepack,
                CliOutputDialect::MessagepackDeterministic | CliOutputDialect::MessagepackDeterministicFloat64
            )
            | (
                CliFormat::Render,
                CliOutputDialect::RenderPlain
                    | CliOutputDialect::RenderGfmTable
                    | CliOutputDialect::RenderHtmlTable
                    | CliOutputDialect::RenderGridTable
                    | CliOutputDialect::RenderTree
                    | CliOutputDialect::RenderTerminal
                    | CliOutputDialect::RenderShell
                    | CliOutputDialect::RenderHist,
            )
    );
    if !valid {
        return Err(format!("invalid output format/dialect pair: {}/{}", format.id(), dialect.id()).into());
    }
    if terminator.is_some() && format != CliFormat::Ndjson {
        return Err("--ndjson-terminator requires NDJSON output".into());
    }
    if !render.is_default() && format != CliFormat::Render {
        return Err("--render-header/--render-width/--render-shape/--render-max-width require render output".into());
    }
    Ok(CliOutputSelection {
        format,
        dialect,
        terminator: terminator.unwrap_or(NdjsonTerminator::Lf),
        indent,
        render,
        raw_strings,
        sort_keys,
        ascii_output,
        no_newline,
        raw_output_nul,
        with_source,
    })
}

/// A named path's filename selects a format when no `--input-format` / `--output-format` pinned one. The lookup reads
/// the codec catalog's own declaration; a name no registration claims falls back to `None` and the caller keeps JSON.
/// The FULL file name is matched — first by exact filename, then by filename glob, then by extension — so extensionless
/// names (`.env`, `Makefile`) are reachable beside `data.yaml`. Filename matching is case-insensitive so detection
/// agrees with the filesystems jqf runs on.
fn detect_format(
    path: impl AsRef<Path>,
    catalog: CodecCatalog<'_, '_>,
    table: &[(&'static str, CliFormat)],
    side: &str,
) -> Result<Option<CliFormat>, CliFailure> {
    let path = path.as_ref();
    let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let filename = filename.to_ascii_lowercase();
    match catalog.detect_by_filename(&filename) {
        Ok((format, _dialect)) => {
            let format = table
                .iter()
                .find(|(spelling, _)| *spelling == format.as_str())
                .map(|(_, format)| *format)
                .ok_or_else(|| {
                    CliFailure::from(format!(
                        "the {filename:?} file name resolves to {side} format {format:?}, \
                         which the CLI does not accept — a registration/CLI drift bug"
                    ))
                })?;
            Ok(Some(format))
        }
        Err(RegistryFailure::ExtensionUnavailable | RegistryFailure::FilenameUnavailable) => Ok(None),
        Err(failure) => Err(CliFailure::from(format!(
            "cannot detect the {side} format of {} from its file name: {failure}",
            path.display()
        ))),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one flat flag table; every arm is three lines of the same shape, and splitting it \
              would thread a dozen out-parameters through helpers"
)]
pub(crate) fn parse_arguments(catalog: Option<CodecCatalog<'_, '_>>) -> Result<CliCommand, CliFailure> {
    let mut program = None;
    let mut program_file = None;
    let mut null_input = false;
    let mut raw_input = false;
    let mut slurp = false;
    let mut raw_output = false;
    let mut bindings = Vec::new();
    let mut max_memory_bytes = None;
    let mut max_spill_bytes = None;
    let mut max_spill_disk_bytes = None;
    let mut max_iterations: Option<u64> = None;
    let mut max_rss = None;
    let mut input_format = None;
    let mut input_dialect = None;
    // Where the output format came from, when anything chose one: the `--edit` mismatch refusal names the origin so a
    // config-file preference or an --output-path extension never reads as a mystery flag.
    let mut output_format_source: Option<&'static str> = None;
    let mut csv_delimiter = None;
    let mut csv_header = false;
    let mut output_format = None;
    let mut output_dialect = None;
    let mut ndjson_terminator = None;
    let mut render_header = None;
    let mut render_width = None;
    let mut render_shape = None;
    let mut render_max_width = None;
    let mut parallel: Option<bool> = None;
    let mut workers = None;
    let mut mismatch_policy = None;
    let mut strictness = None;
    let mut json_facts = false;
    let mut json_facts_off = false;
    let mut types_as_strings = false;
    let mut diagnostics = false;
    let mut explain = false;
    let mut exit_status = false;
    let mut seed = None;
    let mut unbuffered = false;
    let mut stream = false;
    let mut stream_errors = false;
    let mut follow = false;
    let mut seq = false;
    // jq's color switches: presence is recorded, and `-M` wins over `-C` by jq's own argument law (applied last).
    let mut color = false;
    let mut monochrome = false;
    let mut sort_keys = false;
    let mut ascii_output = false;
    let mut join_output = false;
    let mut raw_output0 = false;
    let mut with_source = false;
    let mut positional_args: Vec<PositionalArg> = Vec::new();
    // jq's `--args`/`--jsonargs` mode switch: once set, every remaining positional (a file or value after this point)
    // is a `$ARGS.positional` value under the mode active when it was seen. Options still parse, so `--arg x 1` after
    // `--args` still binds; only positionals change.
    let mut further_args: Option<fn(String) -> PositionalArg> = None;
    let mut plan_out = None;
    let mut plan_file = None;
    let mut edit = false;
    let mut edit_check = false;
    let mut edit_expand_alias = false;
    let mut output_path = None;
    let mut in_place = false;
    let mut no_atomic = false;
    let mut split_exp = None;
    let mut split_exp_file = None;
    let mut diff_pair = None;
    let mut old_format = None;
    let mut new_format = None;
    let mut schema_file = None;
    let mut input_files = Vec::new();
    let mut library_paths = Vec::new();
    // jq lets the indent switches repeat and takes the last one, so these are deliberately not "may only be given once"
    // flags: `--tab -c` is compact and `-c --tab` is tabs, exactly as jq resolves them.
    let mut indent = DEFAULT_INDENT;
    let mut arguments: Box<dyn Iterator<Item = std::ffi::OsString>> = Box::new(std::env::args_os().skip(1));
    // jq's bare `--`: end of options. Everything after it is positional — the program or an input file, never a flag —
    // so a filter beginning with `-` survives `jqf -- "$filter"` (jq's own help lists `--`). A second `--` after the
    // first is positional too, exactly as jq's parser treats it; options parsed before the marker stand.
    let mut end_of_options = false;
    // config machinery: `--no-config` (and a non-empty JQF_NO_CONFIG) make the run hermetic, `--config PATH` names an
    // explicit file (replacing discovery and the global file), and `--show-config` prints the merged view. The config
    // is resolved AFTER the loop, when the argv-seen state is complete, and only fills flags argv left unset.
    let mut no_config = false;
    let mut config_path = None;
    let mut show_config = false;
    // Which Tier P flags argv set. The Option locals already say "unset", but the bool/indent locals default to a
    // value, so presence needs its own bit — the config merge must not overwrite an argv spelling.
    let mut indent_seen = false;
    let mut color_seen = false;
    let mut monochrome_seen = false;
    let mut diagnostics_seen = false;
    let mut explain_seen = false;
    let mut unbuffered_seen = false;
    // Which format flags argv set. Config may fill `output_format` (Tier P), so the --seq rewrite must ask whether ARGV
    // spoke, not whether the local is still unset — an argv `--seq` beats a config output-format preference (argv wins,
    // §5).
    let mut input_format_given = false;
    let mut output_format_given = false;
    let mut saw_run_option = false;
    while let Some(argument) = arguments.next() {
        if !end_of_options && argument == "--" {
            end_of_options = true;
            continue;
        }
        if !end_of_options && argument.as_encoded_bytes().first() == Some(&b'-') && argument != "-" {
            // One bit for the subcommand-prefix guard: any option token before a reserved keyword is a run option.
            // Commands that consume the rest of argv (--help, --version, discovery) return before the keyword check, so
            // they never trip it.
            saw_run_option = true;
        }
        if end_of_options {
            // item 1: `--` terminates option processing — everything after it is POSITIONAL (the program, a $ARGS
            // value, or an input file), never a flag. jq's law, `jq -- -n` compiles the program `-n`, and `jq. -- -n`
            // reads the file named `-n` — so a user-controlled filename beginning with `-` is passable and an
            // option-injection path is closed. A second `--` is positional too, and options before the marker stand.
            // The reserved-keyword check is deliberately NOT consulted: a keyword after `--` is ordinary jq text, the
            // documented non-keyword slot.
            if program_file.is_none() && program.is_none() {
                let argument = argument
                    .into_string()
                    .map_err(|_| CliFailure::from("program is not valid UTF-8"))?;
                program = Some(argument);
            } else if let Some(build) = further_args {
                let argument = argument
                    .into_string()
                    .map_err(|_| CliFailure::from("$ARGS value is not valid UTF-8"))?;
                positional_args.push(build(argument));
            } else {
                input_files.push(PathBuf::from(argument));
            }
            continue;
        }
        if argument == "-L" || argument == "--library-path" {
            let value = next_flag_path(&mut arguments, "-L")?;
            library_paths.push(value);
            continue;
        }
        // item 3: jq's ATTACHED `-Ldir` form, which appears in real shebang lines (`-L$HOME/.jq`): jq's option parser
        // takes the rest of the token as the value. `-L` exactly is handled above and takes the NEXT argument;
        // `--library-path` starts with `--`, so the `-L` prefix test cannot catch it.
        if argument.len() > 2 && std::ffi::OsStr::as_encoded_bytes(argument.as_os_str()).starts_with(b"-L") {
            let rest = argument.as_encoded_bytes()[2..].to_vec();
            // SAFETY: `argument` is a valid OsString whose encoded bytes start with the ASCII prefix `-L`; dropping
            // that prefix leaves a valid platform encoding of the attached path.
            let path = unsafe { OsString::from_encoded_bytes_unchecked(rest) };
            library_paths.push(PathBuf::from(path));
            continue;
        }
        if argument == "-h" {
            // item 4: `-h` is jq-pure — a help flag exits immediately and NEVER consumes the next argument, so `jqf
            // -h.` prints help and exits 0 exactly as `jq -h.` does (a wrapper that appends the filter after the flag
            // keeps working). The `--help <topic>` extension stays on the LONG form, below, where a following argument
            // is a topic by contract. The page is the one-screen summary; the long form keeps the full reference.
            return Ok(CliCommand::ShortHelp);
        }
        if argument == "--help" {
            // `--help <topic>`: a following argument is a help topic — a format/dialect spelling or one of the fixed
            // topics — and the page is generated from the same tables the parser reads. An unknown topic is a usage
            // error (exit 2), never a silent fallback to the full help.
            if let Some(topic_argument) = arguments.next() {
                let topic = topic_argument
                    .into_string()
                    .map_err(|_| CliFailure::from("help topic is not valid UTF-8"))?;
                return parse_help_topic(&topic).map(CliCommand::HelpTopic);
            }
            return Ok(CliCommand::Help);
        }
        if argument == "--version" || argument == "-V" {
            return Ok(CliCommand::Version);
        }
        // The discovery surface : every enumeration is generated from the registry that owns the fact, so what the CLI
        // prints and what the CLI accepts can never drift. These are COMMANDS — each exits without reading stdin, like
        // --help/--version — so they return before any request state exists.
        if argument == "--list-builtins" {
            return Ok(CliCommand::ListBuiltins);
        }
        if argument == "--list-formats" {
            return Ok(CliCommand::ListFormats);
        }
        if argument == "--help-format" {
            let value = next_flag_value(&mut arguments, "--help-format")?;
            let format = parse_format("--help-format", &value, &CliFormat::OUTPUT_FORMATS)?;
            return Ok(CliCommand::HelpFormat(format));
        }
        if argument == "--explain-code" {
            let value = next_flag_value(&mut arguments, "--explain-code")?;
            let id = value
                .parse::<u16>()
                .map_err(|_| format!("--explain-code takes a numeric diagnostic-code id, got {value:?}"))?;
            return Ok(CliCommand::ExplainCode(id));
        }
        // config switches. `--show-config` is a COMMAND (like --help): it prints the merged configuration and exits 0
        // without reading stdin, resolved after the loop so it sees the whole argv.
        if argument == "--no-config" {
            no_config = true;
            continue;
        }
        if argument == "--config" {
            if config_path.is_some() {
                return Err("--config may only be given once".into());
            }
            config_path = Some(next_flag_path(&mut arguments, "--config")?);
            continue;
        }
        if argument == "--show-config" {
            show_config = true;
            continue;
        }
        if argument == "-f" || argument == "--from-file" {
            if program_file.is_some() {
                return Err("-f may only be given once".into());
            }
            program_file = Some(next_flag_path(&mut arguments, "-f")?);
            continue;
        }
        if argument == "-n" || argument == "--null-input" {
            null_input = true;
            continue;
        }
        if argument == "-R" || argument == "--raw-input" {
            raw_input = true;
            continue;
        }
        if argument == "-s" || argument == "--slurp" {
            slurp = true;
            continue;
        }
        if argument == "-r" || argument == "--raw-output" {
            raw_output = true;
            continue;
        }
        if argument == "--arg" {
            let name = next_flag_value(&mut arguments, "--arg")?;
            let value = next_flag_value(&mut arguments, "--arg")?;
            bindings.push(CliBinding::Arg(name, value));
            continue;
        }
        if argument == "--argjson" {
            let name = next_flag_value(&mut arguments, "--argjson")?;
            let value = next_flag_value(&mut arguments, "--argjson")?;
            bindings.push(CliBinding::ArgJson(name, value));
            continue;
        }
        if argument == "--slurpfile" {
            let name = next_flag_value(&mut arguments, "--slurpfile")?;
            let file = next_flag_path(&mut arguments, "--slurpfile")?;
            bindings.push(CliBinding::SlurpFile(name, file));
            continue;
        }
        if argument == "--rawfile" {
            let name = next_flag_value(&mut arguments, "--rawfile")?;
            let file = next_flag_path(&mut arguments, "--rawfile")?;
            bindings.push(CliBinding::RawFile(name, file));
            continue;
        }
        if argument == "--schema" {
            if schema_file.is_some() {
                return Err("--schema may only be given once".into());
            }
            schema_file = Some(next_flag_path(&mut arguments, "--schema")?);
            continue;
        }
        if argument == "--args" {
            further_args = Some(PositionalArg::String);
            continue;
        }
        if argument == "--jsonargs" {
            further_args = Some(PositionalArg::Json);
            continue;
        }
        if argument == "-e" || argument == "--exit-status" {
            exit_status = true;
            continue;
        }
        if argument == "-b" || argument == "--binary" {
            // item 5: jq's `-b`/`--binary` is a no-op on Unix — its Windows-only capability is raw binary output — and
            // jq ACCEPTS it there, running normally. Accepted and ignored, exactly as jq does, so a cross-platform
            // script runs unchanged on both binaries. Decided and recorded in.
            continue;
        }
        if argument == "--build-configuration" {
            // item 5: jq's `--build-configuration` is a COMMAND — it prints the build facts and exits 0 without reading
            // stdin (jq prints its configure flags; bug-report templates capture it). jqf prints its OWN honest facts —
            // the same build kind, profile identity, allocator, and platform topology the `--diagnostics` provenance
            // line carries.
            return Ok(CliCommand::BuildConfiguration);
        }
        if argument == "--seed" {
            if seed.is_some() {
                return Err("--seed may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--seed")?;
            // `rand(seed)`'s own domain: any i64, bit-reinterpreted to u64 so the CLI and the in-language seed agree on
            // what one integer means (`--seed -1` primes the same state `rand(-1)` would seed from).
            let parsed = value
                .parse::<i64>()
                .map_err(|_| CliFailure::from("--seed value is not a valid integer"))?;
            seed = Some(u64::from_ne_bytes(parsed.to_ne_bytes()));
            continue;
        }
        if argument == "-S" || argument == "--sort-keys" {
            sort_keys = true;
            continue;
        }
        if argument == "-j" || argument == "--join-output" {
            join_output = true;
            continue;
        }
        if argument == "--raw-output0" {
            raw_output0 = true;
            continue;
        }
        if argument == "-a" || argument == "--ascii-output" {
            ascii_output = true;
            continue;
        }
        if argument == "--unbuffered" {
            unbuffered = true;
            unbuffered_seen = true;
            continue;
        }
        if argument == "--stream" {
            stream = true;
            continue;
        }
        // jq's `--stream-errors` (the help says it "implies --stream" — there is NO diagnostic for using it alone, it
        // just sets both parser flags).
        if argument == "--stream-errors" {
            stream = true;
            stream_errors = true;
            continue;
        }
        if argument == "--follow" {
            follow = true;
            continue;
        }
        // jq's `--seq`: the input is RS-framed json-seq read RECOVERING, and the output is RS-framed json-seq. An
        // explicit `--input-format`/ `--output-format` overrides the flag's side; the recovering profile itself is
        // flag-scoped (`json-seq.recover@1` stays reserved), so only `--seq` without an explicit input selection
        // selects it.
        if argument == "--seq" {
            seq = true;
            continue;
        }
        // jq's color switches: `-M` is applied LAST in jq's own argument law, so it wins over `-C` in either order (
        // `-C -M -C` and `-M -C -M` are both monochrome). The resolved request is recorded; the decision itself happens
        // in `run` where the destination's terminal-ness and NO_COLOR are known.
        if argument == "-M" || argument == "--monochrome-output" {
            monochrome = true;
            monochrome_seen = true;
            continue;
        }
        if argument == "-C" || argument == "--color-output" {
            color = true;
            color_seen = true;
            continue;
        }
        if argument == "--compact-output" || argument == "-c" {
            indent = JsonIndent::Compact;
            indent_seen = true;
            continue;
        }
        if argument == "--tab" {
            indent = JsonIndent::Tabs;
            indent_seen = true;
            continue;
        }
        if argument == "--indent" {
            let value = next_flag_value(&mut arguments, "--indent")?;
            indent = parse_indent(&value)?;
            indent_seen = true;
            continue;
        }
        if argument == "--max-memory-bytes" {
            if max_memory_bytes.is_some() {
                return Err("--max-memory-bytes may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--max-memory-bytes")?;
            max_memory_bytes = Some(
                value
                    .parse::<u64>()
                    .map_err(|_| CliFailure::from("--max-memory-bytes value is not a valid nonnegative integer"))?,
            );
            continue;
        }
        if argument == "--max-rss" {
            if max_rss.is_some() {
                return Err("--max-rss may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--max-rss")?;
            max_rss = Some(parse_max_rss(&value)?);
            continue;
        }
        if argument == "--max-spill-bytes" {
            if max_spill_bytes.is_some() {
                return Err("--max-spill-bytes may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--max-spill-bytes")?;
            max_spill_bytes = Some(
                value
                    .parse::<u64>()
                    .map_err(|_| CliFailure::from("--max-spill-bytes value is not a valid nonnegative integer"))?,
            );
            continue;
        }
        if argument == "--max-spill-disk-bytes" {
            if max_spill_disk_bytes.is_some() {
                return Err("--max-spill-disk-bytes may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--max-spill-disk-bytes")?;
            max_spill_disk_bytes =
                Some(value.parse::<u64>().map_err(|_| {
                    CliFailure::from("--max-spill-disk-bytes value is not a valid nonnegative integer")
                })?);
            continue;
        }
        if argument == "--max-iterations" {
            if max_iterations.is_some() {
                return Err("--max-iterations may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--max-iterations")?;
            let parsed = value
                .parse::<u64>()
                .map_err(|_| CliFailure::from("--max-iterations value is not a valid nonnegative integer"))?;
            // 0 is the documented spelling of unlimited: normalize so the rest of the tree sees only real ceilings.
            max_iterations = (parsed > 0).then_some(parsed);
            continue;
        }
        if argument == "--input-format" {
            if input_format.is_some() {
                return Err("--input-format may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--input-format")?;
            input_format = Some(parse_format("--input-format", &value, &CliFormat::INPUT_FORMATS)?);
            input_format_given = true;
            continue;
        }
        if argument == "--input-dialect" {
            if input_dialect.is_some() {
                return Err("--input-dialect may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--input-dialect")?;
            input_dialect = Some(parse_input_dialect("--input-dialect", &value)?);
            continue;
        }
        // The per-side `--diff` formats: each defaults to the input format, so the shared `--input-format` flag stays
        // the one-dial default and a cross-format diff names exactly the side that differs.
        if argument == "--old-format" {
            if old_format.is_some() {
                return Err("--old-format may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--old-format")?;
            old_format = Some(parse_format("--old-format", &value, &CliFormat::INPUT_FORMATS)?);
            continue;
        }
        if argument == "--new-format" {
            if new_format.is_some() {
                return Err("--new-format may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--new-format")?;
            new_format = Some(parse_format("--new-format", &value, &CliFormat::INPUT_FORMATS)?);
            continue;
        }
        if argument == "--header" {
            if csv_header {
                return Err("--header may only be given once".into());
            }
            csv_header = true;
            continue;
        }
        if argument == "--csv-delimiter" {
            if csv_delimiter.is_some() {
                return Err("--csv-delimiter may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--csv-delimiter")?;
            csv_delimiter = Some(parse_csv_delimiter(&value)?);
            continue;
        }
        if argument == "--output-format" {
            if output_format.is_some() {
                return Err("--output-format may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--output-format")?;
            output_format = Some(parse_format("--output-format", &value, &CliFormat::OUTPUT_FORMATS)?);
            output_format_source = Some("--output-format");
            output_format_given = true;
            continue;
        }
        //: the `-o FORMAT` short spelling of
        // `--output-format` (the long spelling stays canonical; `-o` is a jqf extension, jq has no output-format dial).
        if argument == "-o" {
            if output_format.is_some() {
                return Err("--output-format may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "-o")?;
            output_format = Some(parse_format("-o", &value, &CliFormat::OUTPUT_FORMATS)?);
            output_format_source = Some("--output-format");
            output_format_given = true;
            continue;
        }
        if argument == "--output-dialect" {
            if output_dialect.is_some() {
                return Err("--output-dialect may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--output-dialect")?;
            output_dialect = Some(parse_output_dialect("--output-dialect", &value)?);
            continue;
        }
        if argument == "--with-source" {
            if with_source {
                return Err("--with-source may only be given once".into());
            }
            with_source = true;
            continue;
        }
        if argument == "--render-header" {
            if render_header.is_some() {
                return Err("--render-header may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--render-header")?;
            render_header = Some(match value.as_str() {
                "present" => jqf_codec_render::HeaderPolicy::Present,
                "absent" => jqf_codec_render::HeaderPolicy::Absent,
                _ => return Err(format!("unknown --render-header value: {value:?}").into()),
            });
            continue;
        }
        if argument == "--render-width" {
            if render_width.is_some() {
                return Err("--render-width may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--render-width")?;
            render_width = Some(match value.as_str() {
                "western" => jqf_codec_render::WidthProfile::Western,
                "cjk" => jqf_codec_render::WidthProfile::Cjk,
                _ => return Err(format!("unknown --render-width value: {value:?}").into()),
            });
            continue;
        }
        if argument == "--render-shape" {
            if render_shape.is_some() {
                return Err("--render-shape may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--render-shape")?;
            render_shape = Some(match value.as_str() {
                "plain" => jqf_codec_render::TerminalShape::Plain,
                "table" => jqf_codec_render::TerminalShape::Table,
                "tree" => jqf_codec_render::TerminalShape::Tree,
                _ => return Err(format!("unknown --render-shape value: {value:?}").into()),
            });
            continue;
        }
        if argument == "--render-max-width" {
            if render_max_width.is_some() {
                return Err("--render-max-width may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--render-max-width")?;
            render_max_width = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| CliFailure::from("--render-max-width value is not a valid nonnegative integer"))?,
            );
            continue;
        }
        if argument == "--ndjson-terminator" {
            if ndjson_terminator.is_some() {
                return Err("--ndjson-terminator may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--ndjson-terminator")?;
            ndjson_terminator = Some(
                NdjsonTerminator::parse(&value)
                    .ok_or_else(|| CliFailure::from(format!("unknown --ndjson-terminator value: {value:?}")))?,
            );
            continue;
        }
        if argument == "--parallel" || argument == "--no-parallel" {
            let requested = argument == "--parallel";
            if parallel.is_some_and(|previous| previous != requested) {
                return Err("--parallel and --no-parallel are exclusive".into());
            }
            parallel = Some(requested);
            continue;
        }
        if argument == "--workers" {
            if workers.is_some() {
                return Err("--workers may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--workers")?;
            workers = Some(parse_workers(&value)?);
            continue;
        }
        if argument == "--mismatch-policy" {
            if mismatch_policy.is_some() {
                return Err("--mismatch-policy may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--mismatch-policy")?;
            mismatch_policy = Some(match value.as_str() {
                "lenient" => jqf_resource::policy::MismatchPolicy::Lenient,
                "warn" => jqf_resource::policy::MismatchPolicy::Warn,
                "strict" => jqf_resource::policy::MismatchPolicy::Strict,
                _ => {
                    return Err(format!(
                        "unknown --mismatch-policy value: {value:?} (expected lenient, warn, or strict)"
                    )
                    .into());
                }
            });
            continue;
        }
        if argument == "--strictness" {
            if strictness.is_some() {
                return Err("--strictness may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--strictness")?;
            strictness = Some(match value.as_str() {
                "error" => jqf_resource::policy::StrictnessPolicy::Error,
                "warn" => jqf_resource::policy::StrictnessPolicy::Warn,
                "strict" => jqf_resource::policy::StrictnessPolicy::Strict,
                "lenient" => jqf_resource::policy::StrictnessPolicy::Lenient,
                _ => {
                    return Err(format!(
                        "unknown --strictness value: {value:?} (expected error, warn, strict, or lenient)"
                    )
                    .into());
                }
            });
            continue;
        }
        if argument == "--diagnostics" {
            diagnostics = true;
            diagnostics_seen = true;
            continue;
        }
        if argument == "--explain" {
            explain = true;
            explain_seen = true;
            continue;
        }
        if argument == "--json-facts" {
            json_facts = true;
            continue;
        }
        if argument == "--types-as-strings" {
            types_as_strings = true;
            continue;
        }
        if argument == "--no-json-facts" {
            json_facts_off = true;
            continue;
        }
        if argument == "--plan-out" {
            if plan_out.is_some() {
                return Err("--plan-out may only be given once".into());
            }
            plan_out = Some(next_flag_path(&mut arguments, "--plan-out")?);
            continue;
        }
        if argument == "--plan-file" {
            if plan_file.is_some() {
                return Err("--plan-file may only be given once".into());
            }
            plan_file = Some(next_flag_path(&mut arguments, "--plan-file")?);
            continue;
        }
        if argument == "--edit" {
            edit = true;
            continue;
        }
        if argument == "--edit-expand-alias" {
            edit_expand_alias = true;
            continue;
        }
        if argument == "--check" {
            edit_check = true;
            continue;
        }
        if argument == "--output" {
            if output_path.is_some() {
                return Err("--output may only be given once".into());
            }
            output_path = Some(next_flag_path(&mut arguments, "--output")?);
            continue;
        }
        if argument == "--diff" {
            let old = next_flag_path(&mut arguments, "--diff")?;
            let new = next_flag_path(&mut arguments, "--diff")?;
            if diff_pair.is_some() {
                return Err("--diff may only be given once".into());
            }
            diff_pair = Some((old, new));
            continue;
        }
        if argument == "--in-place" {
            in_place = true;
            continue;
        }
        if argument == "--split-exp" {
            if split_exp.is_some() || split_exp_file.is_some() {
                return Err("--split-exp may only be given once".into());
            }
            let value = next_flag_value(&mut arguments, "--split-exp")?;
            split_exp = Some(value);
            continue;
        }
        if argument == "--split-exp-file" {
            if split_exp.is_some() || split_exp_file.is_some() {
                return Err("--split-exp-file may only be given once".into());
            }
            split_exp_file = Some(next_flag_path(&mut arguments, "--split-exp-file")?);
            continue;
        }
        if argument == "--no-atomic" {
            no_atomic = true;
            continue;
        }
        let Some(argument) = argument.to_str().map(str::to_owned) else {
            // The option-shape checks run on ENCODED BYTES so acceptance does not depend on the token's encoding: a
            // non-UTF-8 token spelled like an option is the same usage error its UTF-8 twin gets below, never a
            // silently accepted input file.
            let bytes = argument.as_encoded_bytes();
            let option_shaped = !end_of_options
                && match bytes {
                    [b'-', b'-', ..] => true,
                    [b'-', second, ..] => second.is_ascii_alphabetic(),
                    _ => false,
                };
            if option_shaped {
                return Err(format!("unknown option: {}", argument.to_string_lossy()).into());
            }
            if program_file.is_none() && program.is_none() {
                return Err("program is not valid UTF-8".into());
            }
            if further_args.is_some() {
                return Err("$ARGS value is not valid UTF-8".into());
            }
            input_files.push(PathBuf::from(argument));
            continue;
        };
        // A bare `--` was handled above; an argument after one is positional by construction, so it never reaches the
        // option rejections here.
        if !end_of_options && argument.starts_with("--") {
            return Err(format!("unknown option: {argument}").into());
        }
        // jq combines short flags (`-nR`, `-rs`, `-nc`): a single-dash argument whose every character is a KNOWN short
        // flag expands into them, each handled by the loop below. An unknown character keeps the whole argument a
        // program/input spelling (jq treats an unrecognized short flag the same way — it only splits what it knows).
        if !end_of_options
            && argument.len() > 2
            && argument.starts_with('-')
            && argument[1..].chars().all(|flag| {
                matches!(
                    flag,
                    'n' | 'r' | 'R' | 's' | 'c' | 'f' | 'h' | 'e' | 'S' | 'j' | 'a' | 'M' | 'C' | 'L' | 'b'
                )
            })
        {
            // The expansion pushes each flag back ONTO the iterator front, so iterating REVERSED makes the flags emerge
            // in AUTHORED order (`-nL` -> `-n` then `-L`). The forward iteration the code originally used reversed the
            // cluster (`-nL` came out as `-L` then `-n`), which broke a value-taking flag: `-nL dir` gave `-L` the
            // value `-n` and pushed `dir` into the program slot (item 3 found this; the same latent shape would have
            // broken `-nf FILE` the moment it was ever clustered).
            for flag in argument[1..].chars().rev() {
                arguments = Box::new(std::iter::once(std::ffi::OsString::from(format!("-{flag}"))).chain(arguments));
            }
            continue;
        }
        // jq rejects an unknown SINGLE-dash option with exit 2 rather than silently reusing it as program text: `-e`,
        // `-nan`, `-now`, `-pi` would otherwise parse as unary-minus programs and run. A single dash followed by an
        // ASCII letter is an option spelling — reject it. A dash followed by a digit (`-1`, `-.5`) or a bare `-` stays
        // a legitimate program/input spelling, and a known combined flag (`-nR`) was already expanded above.
        if !end_of_options
            && argument.starts_with('-')
            && argument.len() >= 2
            && argument[1..].chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        {
            return Err(format!("unknown option: {argument}").into());
        }
        if program_file.is_none() && program.is_none() {
            // The 045 W0 ruling: a first-positional argument that exactly equals a reserved subcommand keyword is a
            // subcommand — UNLESS a program-looking prefix established the program first. `--follow`'s positional IS
            // the program (the follow precedent: `jqf --follow 'serve'` must run the program `serve`), and `-f` names
            // the program file, so the keyword wins only in the plain first-positional slot. A keyword in any other
            // slot (an input file named `serve`, a program after `--`, the value of `--args`) is ordinary jq text.
            if !follow && let Some((_, keyword)) = RESERVED_KEYWORDS.iter().find(|(name, _)| *name == argument) {
                // The keyword is a subcommand only when nothing before it established a program; any pre-keyword run
                // option would be silently dropped into a daemon invocation that does not own it, so the combination is
                // a usage error rather than a half- parsed run request.
                if saw_run_option {
                    return Err("options cannot precede a subcommand keyword; put them after it".into());
                }
                return match keyword {
                    ReservedKeyword::Serve => parse_serve_subcommand(&mut arguments),
                };
            }
            program = Some(argument);
        } else if let Some(build) = further_args {
            // jq's `--args`/`--jsonargs`: from the flag on, every non-option positional is a `$ARGS.positional` value
            // under the active mode rather than an input file. The FIRST positional is still the program, exactly as
            // jq's loop assigns it.
            positional_args.push(build(argument));
        } else {
            // A positional input file, in argument order. Files are joined as one byte stream (jq's law), or — under
            // `--in-place` — each is the target of an independent edit.
            input_files.push(PathBuf::from(argument));
        }
    }
    //: resolve the config and merge it into the flags argv left
    // unset. Precedence, highest wins: argv, `--config PATH`, the nearest discovered `.jqf.toml`, the global file,
    // built-in defaults. With no config on disk (or under `--no-config` / JQF_NO_CONFIG) this block is a no-op, so the
    // no-config binary is byte-identical in every behavior. `--show-config` is a COMMAND: it renders the merged view
    // and returns before any stdin byte is read.
    let config_view = crate::config::resolve(no_config, config_path.as_deref())?;
    let mut report_entries: Vec<crate::config::ConfigEntry> = Vec::new();
    let mut report = |key: &'static str, value: String, argv_set: bool, from_config: bool| {
        report_entries.push(crate::config::ConfigEntry {
            key,
            value,
            origin: if argv_set {
                crate::config::ConfigOrigin::Argv
            } else if from_config {
                crate::config::ConfigOrigin::ConfigFile
            } else {
                crate::config::ConfigOrigin::BuiltIn
            },
        });
    };
    // Colour: argv's -C/-M win. Config `color=false` forces monochrome. Config `color=true` leaves Auto so TTY and
    // NO_COLOR still apply (`-C` is the only force-on, matching jq).
    if !color_seen && !monochrome_seen && config_view.color == Some(false) {
        monochrome = true;
    }
    report(
        "color",
        if monochrome {
            "false".to_owned()
        } else if color {
            "true".to_owned()
        } else {
            "auto".to_owned()
        },
        color_seen || monochrome_seen,
        config_view.color.is_some(),
    );
    // The indent family: argv's switches win; a config value fills the rest. Within one config file the family keys
    // resolve in ALPHABETICAL order (TOML tables iterate sorted here), so the alphabetically last of indent/tab/compact
    // speaks — see `ConfigView:indent`.
    if !indent_seen && let Some(config_indent) = config_view.indent {
        indent = config_indent;
    }
    let (indent_key, indent_value) = match indent {
        JsonIndent::Compact => ("compact", "true".to_owned()),
        JsonIndent::Tabs => ("tab", "true".to_owned()),
        JsonIndent::Spaces(width) => ("indent", width.to_string()),
    };
    report(indent_key, indent_value, indent_seen, config_view.indent.is_some());
    let argv_set = max_memory_bytes.is_some();
    if !argv_set {
        max_memory_bytes = config_view.max_memory_bytes;
    }
    report(
        "max-memory-bytes",
        match max_memory_bytes {
            Some(bytes) => bytes.to_string(),
            None => "unlimited".to_owned(),
        },
        argv_set,
        config_view.max_memory_bytes.is_some(),
    );
    let argv_set = max_rss.is_some();
    if !argv_set {
        max_rss = config_view.max_rss;
    }
    report(
        "max-rss",
        match max_rss {
            Some(crate::rss::MaxRss::Percent(percent)) => format!("{percent}%"),
            Some(crate::rss::MaxRss::Bytes(bytes)) => bytes.to_string(),
            Some(crate::rss::MaxRss::Disabled) => "0".to_owned(),
            None => format!("{}%", crate::rss::DEFAULT_CEILING_PERCENT),
        },
        argv_set,
        config_view.max_rss.is_some(),
    );
    let argv_set = max_spill_bytes.is_some();
    if !argv_set {
        max_spill_bytes = config_view.max_spill_bytes;
    }
    report(
        "max-spill-bytes",
        match max_spill_bytes {
            Some(bytes) => bytes.to_string(),
            None => "0".to_owned(),
        },
        argv_set,
        config_view.max_spill_bytes.is_some(),
    );
    let argv_set = max_spill_disk_bytes.is_some();
    if !argv_set {
        max_spill_disk_bytes = config_view.max_spill_disk_bytes;
    }
    report(
        "max-spill-disk-bytes",
        match max_spill_disk_bytes {
            Some(bytes) => bytes.to_string(),
            None => "0".to_owned(),
        },
        argv_set,
        config_view.max_spill_disk_bytes.is_some(),
    );
    if max_spill_disk_bytes.unwrap_or(0) > 0 && max_spill_bytes.unwrap_or(0) == 0 {
        return Err("--max-spill-disk-bytes requires --max-spill-bytes (the disk ceiling \
             only applies when a spill store is installed)"
            .into());
    }
    let parallel_from_argv = parallel.is_some();
    if !parallel_from_argv {
        parallel = config_view.parallel;
    }
    report(
        "parallel",
        parallel.unwrap_or(true).to_string(),
        parallel_from_argv,
        config_view.parallel.is_some(),
    );
    let workers_from_argv = workers.is_some();
    if !workers_from_argv {
        workers = config_view.workers;
    }
    report(
        "workers",
        match workers {
            Some(WorkerRequest::Explicit(width)) => width.to_string(),
            Some(WorkerRequest::Auto) | None => "auto".to_owned(),
        },
        workers_from_argv,
        config_view.workers.is_some(),
    );
    if !diagnostics_seen {
        diagnostics = config_view.diagnostics.unwrap_or(false);
    }
    report(
        "diagnostics",
        diagnostics.to_string(),
        diagnostics_seen,
        config_view.diagnostics.is_some(),
    );
    if !explain_seen {
        explain = config_view.explain.unwrap_or(false);
    }
    report(
        "explain",
        explain.to_string(),
        explain_seen,
        config_view.explain.is_some(),
    );
    if !unbuffered_seen {
        unbuffered = config_view.unbuffered.unwrap_or(false);
    }
    report(
        "unbuffered",
        unbuffered.to_string(),
        unbuffered_seen,
        config_view.unbuffered.is_some(),
    );
    let argv_set = mismatch_policy.is_some();
    if !argv_set {
        mismatch_policy = config_view.mismatch_policy;
    }
    report(
        "mismatch-policy",
        match mismatch_policy {
            Some(jqf_resource::policy::MismatchPolicy::Warn) => "warn".to_owned(),
            Some(jqf_resource::policy::MismatchPolicy::Strict) => "strict".to_owned(),
            Some(jqf_resource::policy::MismatchPolicy::Lenient) | None => "lenient".to_owned(),
        },
        argv_set,
        config_view.mismatch_policy.is_some(),
    );
    let argv_set = strictness.is_some();
    if !argv_set {
        strictness = config_view.strictness;
    }
    report(
        "strictness",
        match strictness {
            Some(jqf_resource::policy::StrictnessPolicy::Warn) => "warn".to_owned(),
            Some(jqf_resource::policy::StrictnessPolicy::Strict) => "strict".to_owned(),
            Some(jqf_resource::policy::StrictnessPolicy::Lenient) => "lenient".to_owned(),
            Some(jqf_resource::policy::StrictnessPolicy::Error) | None => "error".to_owned(),
        },
        argv_set,
        config_view.strictness.is_some(),
    );
    let argv_set = output_format.is_some();
    if !argv_set {
        output_format = config_view.output_format;
        if output_format.is_some() {
            // The origin rides the request: a later refusal that names the output format must not imply a flag the user
            // never typed.
            output_format_source = Some("the config file's output-format");
        }
    }
    report(
        "output-format",
        match output_format {
            Some(format) => format.id().to_owned(),
            None => "json".to_owned(),
        },
        argv_set,
        config_view.output_format.is_some(),
    );
    // The files whose values were merged, for the --diagnostics provenance line (the standalone --show-config command
    // carries its own report).
    let config_source = (!config_view.source_files.is_empty()).then(|| {
        config_view
            .source_files
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    });
    if show_config {
        return Ok(CliCommand::ShowConfig(crate::config::render_report(
            &report_entries,
            &config_view.source_files,
        )));
    }
    // The three input-model flags own the INPUT: `-n` replaces it with null, `-R` reads it as lines, `-s` collects it
    // into one array. Over a RECORD input each of the three now resolves exactly as it does over YAML's document stream
    // (D3 2026-08-04): `-s` collects every record into one array, `-n` runs once over null with `input`/`inputs`
    // reading the record stream, and `-R` rewrites the input into JSON string literals before any route decision (so
    // the record framing is gone and the named format stops describing the bytes — resolved in `main`).
    //
    // `--follow` is the one model still refused: it is a LIVE record route whose stream never ends, and each of the
    // three flags is defined by reading the input to completion first.
    //
    // `-n` is NOT refused here: an input-family program (one that pulls `inputs`/`input`) runs ONCE over the live
    // stream with the records served through the input family — the live-window shape — so `-n --follow 'ewma(0.2;
    // inputs|.ms)'` and the README's auto-detected spelling are the same drive. A non-input `-n --follow` (a program
    // that would run once over null and end the tail) is refused after compile, in `main`, where the program class is
    // known.
    if (raw_input || slurp) && follow {
        return Err(
            "-R/-s read the input to completion and cannot be combined with --follow's \
                    live record stream"
                .into(),
        );
    }
    // Implicit input-format detection for NAMED FILES. When no
    // `--input-format` pinned a format, the first positional file's extension selects it through the catalog's own
    // declaration; stdin keeps the json default (no extension to read, and content sniffing is genuinely ambiguous —
    // every JSON document is valid YAML). An unrecognized extension falls back to json, so no currently-working
    // invocation changes. When extension detection selects the input for `--in-place`, every file must agree before
    // any is opened for writing. The catalog-less pre-pass adopts a named dialect or CSV delimiter provisionally so
    // format-specific validation waits for the catalog pass. Runs before the `--follow` default (a `.csv` tail follows
    // as CSV, a `.ndjson` tail as NDJSON) and before the `--edit`/`--in-place` output mirror (a `.toml` file re-emits as
    // TOML); `--seq` still pins json-seq over a detection below.
    let seq_selects_input = seq && !input_format_given && input_dialect.is_none();
    let seq_selects_output = seq && !output_format_given && output_dialect.is_none();
    let extension_detection_pending =
        catalog.is_none() && input_format.is_none() && input_dialect.is_none() && !input_files.is_empty();
    if catalog.is_none() && input_format.is_none() && !input_files.is_empty() {
        input_format = input_dialect
            .map(CliInputDialect::format)
            .or_else(|| csv_delimiter.is_some().then_some(CliFormat::Csv));
    }
    if let Some(catalog) = catalog {
        if input_format.is_none() && !input_files.is_empty() && !seq_selects_input {
            let detected = detect_format(&input_files[0], catalog, &CliFormat::INPUT_FORMATS, "input")?;
            if in_place {
                let first = detected.unwrap_or(CliFormat::Json);
                for path in &input_files[1..] {
                    let next =
                        detect_format(path, catalog, &CliFormat::INPUT_FORMATS, "input")?.unwrap_or(CliFormat::Json);
                    if next != first {
                        return Err(format!(
                            "--in-place requires one input format per invocation: {} is {}, but {} is {}",
                            input_files[0].display(),
                            first.id(),
                            path.display(),
                            next.id()
                        )
                        .into());
                    }
                }
            }
            input_format = detected;
        }
        // A `--output PATH` whose extension a registration declares infers the output format. An explicit
        // `--output-format` wins; a config-only `output-format` does not beat the path extension (the path is the
        // request's destination, not a preference).
        if !output_format_given && let Some(path) = output_path.as_deref() {
            let detected = detect_format(path, catalog, &CliFormat::OUTPUT_FORMATS, "output")?;
            if detected.is_some() {
                output_format_source = Some("the --output path's extension");
            }
            output_format = detected;
        }
    }
    // `--follow`'s own fences: it is a live record route, so the document lanes and destinations it cannot serve are
    // rejected up front.
    if follow {
        if edit {
            return Err("--follow cannot be combined with --edit".into());
        }
        if diff_pair.is_some() {
            return Err("--follow cannot be combined with --diff".into());
        }
        if stream {
            return Err("--follow cannot be combined with --stream".into());
        }
        if in_place || output_path.is_some() {
            return Err("--follow streams results as records arrive; it cannot write an \
                        atomic file destination"
                .into());
        }
        if input_files.len() > 1 {
            return Err("--follow tails ONE input file; it cannot serve a list".into());
        }
        // Follow frames records at each input kind's EXACT cut: the newline for NDJSON, the RFC 4180 quote-state walk
        // for CSV (a record ends at a line feed only OUTSIDE a quoted field). NDJSON defaults to the recovering dialect
        // (a live tail's truncated record must not kill the stream); CSV has no recovering dialect, so its framing
        // faults stay terminal exactly as on the whole-input route.
        let format = input_format.unwrap_or(CliFormat::Ndjson);
        // Follow's dialect default is format-aware: an NDJSON tail defaults to the recovering dialect (a live tail's
        // truncated record must not kill the stream), a CSV tail to its array dialect; an explicit dialect stands. The
        // committed pair below must match what `resolve_input_selection` would resolve, or the format/dialect
        // validation right after this block would reject the follow default it just manufactured.
        let dialect = input_dialect.unwrap_or(match format {
            CliFormat::Csv => CliInputDialect::CsvRfc4180,
            _ => CliInputDialect::NdjsonRecovering,
        });
        match (format, dialect) {
            (
                CliFormat::Ndjson,
                CliInputDialect::NdjsonStrict | CliInputDialect::NdjsonRecovering,
            )
            // CSV is a record input under either dialect (array or headered); a format/dialect contradiction is
            // rejected by `resolve_input_selection` right after this block, so the check only needs to admit the
            // format.
            | (CliFormat::Csv, _) => {}
            _ => {
                return Err("--follow requires a newline-framed record input (ndjson) or \
                            RFC 4180 CSV; adjacent JSON, TOML, CBOR, cbor-seq, and YAML have \
                            no physical record framing"
                    .into());
            }
        }
        input_format = Some(format);
        input_dialect = Some(dialect);
    }
    // The edit lane's output subject is the document, which the three input models and raw output do not produce. `-j`
    // and `--raw-output0` both imply raw output, and `-S`/`-a` REWRITE bytes the edit lane is in business to preserve,
    // so all five are rejected the same way. The `-S`/`-a` ruling: STRUCTURAL, not incidental — the edit lane's
    // byte-preserving patch strategy keeps untouched spans' key order and escapes verbatim, which would silently ignore
    // a sort or ascii request, and honoring them means re-rendering the whole document, which requires the
    // codec-agnostic SDK to interpret JSON-specific render flags (`--edit -S '.b = 3'` patches in place and keeps the
    // original key order). Recorded in the plan with the reason.
    //
    // `--stream`/`--stream-errors` (which implies `--stream`) rewrite the input into `[path, leaf]` events, so the edit
    // program would run once per event against event arrays — accepted-but-useless, the same class as the input-model
    // rejections above (`--edit --stream '.a = 2'` errors `Cannot index array with string` per event, exit 5).
    if edit
        && (null_input
            || raw_input
            || slurp
            || raw_output
            || join_output
            || raw_output0
            || sort_keys
            || ascii_output
            || stream
            || stream_errors)
    {
        return Err("--edit cannot be combined with -n/-R/-s/-r/-j/-S/-a/--raw-output0/--stream".into());
    }
    // `--check` is the edit lane's verdict dial : a bare `--check` on a non-edit request is meaningless, so the
    // combination is a usage error before a byte is read.
    if edit_check && !edit {
        return Err("--check may only be used with --edit".into());
    }
    // `--edit-expand-alias` is the edit lane's alias escape hatch ( D8): on a request that never runs the edit lane it
    // is accepted-but- meaningless, so the combination is a usage error before a byte is read.
    if edit_expand_alias && !(edit || in_place) {
        return Err("--edit-expand-alias may only be used with --edit or --in-place".into());
    }
    // `-r` is a JSON-renderer flag; the class check lives after output selection is resolved so it asks the
    // registration, not a hand list. `-S`/`-a`/`-j` are JSON formatting flags: sort keys, ascii escape, and
    // back-to-back output all describe the JSON renderer. A non-JSON target neither honors nor ignores them safely, so
    // the combination is a usage error rather than a silent formatting loss.
    //
    // jq's `--seq` selects json-seq for BOTH input and output unless an explicit format/dialect already pinned one side
    // (jq itself has no format flags; this is jqf's explicit-wins extension). The recovering INPUT profile is
    // flag-scoped and travels separately — see `CliArguments:seq_recovering`. The flag-scoped recovering INPUT profile:
    // `--seq` alone (no explicit input format/dialect) selects it. An explicit json-seq selection is strict by
    // construction — the registered dialect is always `json-seq.strict@1`, and `json-seq.recover@1` stays reserved.
    // Computed BEFORE the block below pins the formats. The checks ask whether ARGV spoke (§5: argv beats config),
    // because a config output-format preference must not suppress `--seq`'s own json-seq selection.
    let seq_recovering = seq_selects_input;
    if seq {
        if !input_format_given && input_dialect.is_none() {
            input_format = Some(CliFormat::JsonSeq);
        }
        if seq_selects_output {
            output_format = Some(CliFormat::JsonSeq);
            output_dialect = Some(CliOutputDialect::JsonSeqJqf);
        }
    }
    // `--follow` is a LIVE newline- or quote-walk-framed tail; json-seq is RS-framed, so the incremental framing the
    // follow route owns would have to be re-framed. Rejected up front, before a byte is read.
    if seq && follow {
        return Err("--seq and --follow are incompatible: follow tails a newline-framed \
                    record stream, json-seq is RS-framed"
            .into());
    }
    // `--edit` never changes format: the document is re-emitted as it was read. Non-edit `--in-place` follows the input
    // unless argv explicitly opted into conversion with `--output-format`; a config presentation default must not
    // silently change a file's format. YAML/JSONC/JSON5 under `--edit` name the edit-render dialect the splice policy
    // lives on.
    if (in_place && !edit && !output_format_given && !seq_selects_output) || (edit && output_format.is_none()) {
        output_format = input_format;
    }
    if extension_detection_pending && (edit || in_place) && output_format.is_none() {
        output_format = output_dialect
            .map(CliOutputDialect::format)
            .or_else(|| ndjson_terminator.is_some().then_some(CliFormat::Ndjson))
            .or_else(|| {
                (render_header.is_some()
                    || render_width.is_some()
                    || render_shape.is_some()
                    || render_max_width.is_some())
                .then_some(CliFormat::Render)
            });
    }
    if edit && output_dialect.is_none() {
        match output_format {
            Some(CliFormat::Yaml) => output_dialect = Some(CliOutputDialect::YamlJqf),
            Some(CliFormat::Jsonc) => output_dialect = Some(CliOutputDialect::JsoncJqf),
            Some(CliFormat::Json5) => output_dialect = Some(CliOutputDialect::Json5Jqf10),
            _ => {}
        }
    }
    let input = resolve_input_selection(input_format, input_dialect, csv_delimiter, csv_header)?;
    // `--raw-output0`'s NUL item terminator is a facade/JSON-renderer feature, but the RECORD route's terminator is
    // codec-owned — NDJSON and CSV append their line terminator inside the encoder's own staging buffer, so the facade
    // cannot replace it with a NUL byte. Half-applying the flag leaves the LF terminator in place and silently diverges
    // from jq's NUL, so the combination is a usage error before a byte is read, the same model-rejection law `--follow`
    // applies. json-seq keeps its own pinned raw-output0 law (the codec owns the NUL there), so the check is NDJSON/CSV
    // input only.
    if raw_output0
        && input
            .dialect
            .record_kind()
            .is_some_and(|kind| kind != RecordInputKind::JsonSeq)
    {
        return Err("--raw-output0 cannot be combined with a record input: the record \
                    terminator is codec-owned, so the NUL item terminator cannot be honored"
            .into());
    }
    // The per-side `--diff` selections: each side defaults to the input selection, and an explicit per-side format
    // resolves like any input — the shared `--input-dialect`/`--csv-delimiter` dials apply when they are valid for the
    // side's format (the resolve law rejects an invalid pair above, so a mismatch is already a usage error).
    let diff_old = match old_format {
        Some(format) => resolve_input_selection(Some(format), input_dialect, csv_delimiter, csv_header)?,
        None => input,
    };
    let diff_new = match new_format {
        Some(format) => resolve_input_selection(Some(format), input_dialect, csv_delimiter, csv_header)?,
        None => input,
    };
    let render_options = RenderCliOptions {
        header: render_header,
        width: render_width,
        shape: render_shape,
        max_width: render_max_width,
    };
    let output = resolve_output_selection(
        output_format,
        output_dialect,
        ndjson_terminator,
        indent,
        render_options,
        // jq's `-j` is exactly `-r` plus no newline (main.c sets `RAW_OUTPUT | RAW_NO_LF`); `--raw-output0` is exactly
        // `-r` plus a NUL terminator. Both imply a joined request always raw-prints root strings.
        raw_output || join_output || raw_output0,
        sort_keys,
        ascii_output,
        join_output,
        raw_output0,
        with_source,
        csv_header,
    )?;
    // The delimited dials name a side that must exist: a `--header` with no delimited side at all is a mistake, never a
    // silently ignored flag (the `--csv-delimiter` law, widened to both sides because the header dial serves encode as
    // well as decode).
    if csv_header
        && !matches!(input.format, CliFormat::Csv | CliFormat::Tsv)
        && !matches!(output.format, CliFormat::Csv | CliFormat::Tsv)
    {
        return Err("--header applies to CSV input or output only".into());
    }
    // `-r`/`-S`/`-a`/`-j`/`--raw-output0` admit iff the output format is served by the JSON renderer (asked of the
    // registration, not a hand-listed subset). json-seq honors them on its payload encoder.
    if (raw_output || sort_keys || ascii_output || join_output || raw_output0) && !output.format.is_json_renderer() {
        return Err("-r/-S/-a/-j/--raw-output0 apply to JSON-family output only".into());
    }
    // The level-composition flags are jqft-family emission-surface requests
    //: naming them for any other target is a usage error, never
    // a silently ignored dial.
    if with_source && output.format != CliFormat::Jqft && output.format != CliFormat::Jqfb {
        return Err("--with-source applies to jqft/jqfb output only".into());
    }
    // `-e` reads the LAST OUTPUT VALUE's truthiness. The edit and diff lanes publish a DOCUMENT, not an
    // expression-output stream, so the value the flag would judge does not exist there; the combination is a usage
    // error rather than an invented rule.
    if exit_status && (edit || diff_pair.is_some()) {
        return Err("-e cannot be combined with --edit or --diff".into());
    }
    // The edit lane is a same-format lane: it patches the retained source and re-encodes only through the OUTPUT
    // format, so the input and output formats must match. The format's EDIT capability comes from the codec's own
    // `route_capabilities` declaration (the 039 drift-class fix,): a codec that binds retained source spans and
    // supplies the edit-render dialect and splice policy declares `RouteCapability:Edit` and is served by declaration —
    // never by a hand-written format list in the CLI. YAML output under edit names the edit-render dialect
    // `yaml.jqf-1.0@1`.
    if edit && input.format != output.format {
        // The catalog-less pre-pass defaults a pending named file to json. Failing here would refuse
        // `--edit --output-format yaml t.yaml` as "the input is json" before the catalog pass can see `.yaml`.
        if !extension_detection_pending {
            // Name WHERE the clashing output format came from: the refusal must not read as a flag the user never
            // typed (a config-file preference or an --output path extension are both invisible on the command line).
            let origin = output_format_source.unwrap_or("the built-in default");
            return Err(format!(
                "--edit requires matching input and output formats: the input is {}, \
                 but {} selected {}",
                input.format.id(),
                origin,
                output.format.id()
            )
            .into());
        }
    }
    if edit && let Some(catalog) = catalog {
        let format = jqf_data::FormatId::try_new(input.format.id()).map_err(|_| CliFailure::Message {
            class: crate::errors::ExitClass::Usage,
            message: format!("invalid built-in format identity: {}", input.format.id()),
        })?;
        let dialect = jqf_data::DialectId::try_new(input.dialect.id()).map_err(|_| CliFailure::Message {
            class: crate::errors::ExitClass::Usage,
            message: format!("invalid built-in dialect identity: {}", input.dialect.id()),
        })?;
        let edit_capable = catalog
            .route_capabilities(&format, &dialect)
            .map_err(|error| CliFailure::Message {
                class: crate::errors::ExitClass::Usage,
                message: format!("cannot resolve edit capability for {}: {error:?}", input.format.id()),
            })?
            .contains(&jqf_codec_core::RouteCapability::Edit);
        if !edit_capable {
            return Err("--edit requires an input format whose codec declares the edit \
                    capability (its parser binds retained source spans)"
                .into());
        }
    }
    if in_place && output_path.is_some() {
        return Err("--in-place and --output are two destinations; give one".into());
    }
    // The `--split-exp` destination: a THIRD destination model — one destination per published ITEM — mutually
    // exclusive with the two existing destinations and with the document-subject lanes, in the wording of the existing
    // two-destinations refusal. `--edit`/`--diff` join the exclusion because their whole contract is one output derived
    // from one input's bytes; a split request has no single subject.
    if split_exp.is_some() || split_exp_file.is_some() {
        if output_path.is_some() {
            return Err("--split-exp and --output are two destinations; give one".into());
        }
        if in_place {
            return Err("--split-exp and --in-place are two destinations; give one".into());
        }
        if edit {
            return Err("--split-exp cannot be combined with --edit".into());
        }
        if diff_pair.is_some() {
            return Err("--split-exp cannot be combined with --diff".into());
        }
        // D18: `$index` is the split expression's counter binding (yq compatibility). A user `--arg index …` (or any
        // binding-family spelling of the name) would shadow or be shadowed by it; the conflict is a usage error naming
        // the binding, never a silent shadow in either direction.
        if let Some(
            CliBinding::Arg(name, _)
            | CliBinding::ArgJson(name, _)
            | CliBinding::SlurpFile(name, _)
            | CliBinding::RawFile(name, _),
        ) = bindings.iter().find(|binding| {
            matches!(
                binding,
                CliBinding::Arg(name, _)
                    | CliBinding::ArgJson(name, _)
                    | CliBinding::SlurpFile(name, _)
                    | CliBinding::RawFile(name, _)
                    if name == "index"
            )
        }) {
            return Err(
                format!("--split-exp binds $index; the --arg-family binding --arg {name} would conflict").into(),
            );
        }
    }
    // The `--in-place` rulings: with files it edits each INDEPENDENTLY — one run per file, that file's bytes read and
    // its output written back — so it needs at least one positional file, and a run whose input is not the file's own
    // bytes (`-n` over null, `-s` across every file) has no coherent per-file subject and is refused rather than
    // silently overwriting every file with the same run's output.
    if in_place && null_input {
        return Err(
            "--in-place edits the positional files; -n runs the filter over null and \
             has no per-file subject"
                .into(),
        );
    }
    if in_place && slurp {
        return Err(
            "--in-place edits the positional files one at a time; -s slurps every file \
             into ONE array and has no coherent file to write back to"
                .into(),
        );
    }
    if in_place && input_files.is_empty() {
        return Err("--in-place requires at least one positional input file".into());
    }
    if no_atomic && !in_place && output_path.is_none() && split_exp.is_none() && split_exp_file.is_none() {
        return Err("--no-atomic requires --output, --in-place, or --split-exp".into());
    }
    // `--diff` owns both files and the whole run: no other destination, subject, or input model may share the request.
    // `--stream` rewrites the program to `tostream | P`, which over the diff lane silently produces NO diff output
    // (`--diff --stream '.'` over differing files exits 0 with nothing) — the same accepted-but-useless class as
    // `--edit --stream`, rejected the same way.
    if let Some((old, new)) = &diff_pair {
        if in_place || output_path.is_some() || edit || !input_files.is_empty() {
            return Err("--diff cannot be combined with --in-place/--output/--edit or a positional input file".into());
        }
        if stream || stream_errors {
            return Err("--diff cannot be combined with --stream/--stream-errors".into());
        }
        // Same file by spelling OR by identity: `f` and `./f`, a hard link, or a symlink to the same inode all diff
        // nothing. When both paths resolve, (device, inode) is the truth; an unstated path can only be itself.
        if same_file(old, new) {
            return Err("--diff needs two different files".into());
        }
    }
    // The per-side format dials name a `--diff` side; without `--diff` they have no file to read and are a usage error,
    // never a silently ignored dial (the same law the csv-delimiter dial keeps for a non-CSV input).
    if diff_pair.is_none() && (old_format.is_some() || new_format.is_some()) {
        return Err("--old-format/--new-format require --diff".into());
    }
    // A per-side format that contradicts the shared input dialect is the ordinary invalid-pair refusal: `--old-format
    // toml --input-dialect ndjson.strict@1` names a side TOML cannot serve.
    if let Some(format) = old_format {
        resolve_input_selection(Some(format), input_dialect, csv_delimiter, csv_header)?;
    }
    if let Some(format) = new_format {
        resolve_input_selection(Some(format), input_dialect, csv_delimiter, csv_header)?;
    }
    // `--schema` gates each INPUT VALUE against the value-schema document
    //. The modes with no per-input-value stream cannot be
    // gated: --stream yields tostream [path, leaf] items, not documents, and --edit/--diff/--in-place publish a
    // document subject, not a value stream. The combination is a usage error rather than an invented rule.
    if schema_file.is_some() && (stream || edit || in_place || diff_pair.is_some()) {
        return Err("--schema cannot be combined with --stream/--edit/--diff/--in-place".into());
    }
    // `--json-facts` projects facts into the JSON value; a non-JSON target would silently drop the projection's shape,
    // and the document-subject lanes (edit/diff/in-place) publish the source artifact, not a value stream. `--stream`
    // runs the program over tostream events, whose values carry no attached facts — the same accepted-but-useless class
    // as `--schema --stream`, rejected the same way.
    if json_facts && json_facts_off {
        return Err("--json-facts and --no-json-facts are exclusive".into());
    }
    if json_facts && output.format != CliFormat::Json {
        return Err("--json-facts applies to JSON output only".into());
    }
    if json_facts && (edit || in_place || diff_pair.is_some() || stream || stream_errors) {
        return Err("--json-facts cannot be combined with --edit/--in-place/--diff/--stream".into());
    }
    // MARKUP RENDERS ITS FACTS BY DEFAULT. XML and HTML carry element names and attributes as FACTS, not as members, so
    // the bare positional value model answers `<r a="1"><c>x</c><c>y</c></r>` as `[["x"],["y"]]`: correct under the
    // model, and useless as a first contact — every name the document was written to carry is gone from the answer.
    // Where the target can spell the projection (JSON, the value lanes), markup input therefore turns `--json-facts` on
    // for itself and answers the xq-style tree. `--no-json-facts` asks for the bare positional value back, and the fact
    // accessors (`.@name`, `.&attr`) address the same document either way — the dial is a rendering choice, not a model
    // change.
    let markup_input = matches!(input.format, CliFormat::Xml | CliFormat::Html);

    // jq's law: a run whose input would come from an interactive terminal shows the help instead of silently reading
    // nothing (`jq` with no arguments does exactly this). A program, `-f FILE`, `-n`, or named input files all make the
    // run well-defined without stdin, so only the genuinely stdin-bound shape falls here.
    if program.is_none()
        && program_file.is_none()
        && input_files.is_empty()
        && !null_input
        && std::io::stdin().is_terminal()
    {
        return Ok(CliCommand::Help);
    }
    let json_facts = json_facts
        || (markup_input
            && !json_facts_off
            && output.format == CliFormat::Json
            && !(edit || in_place || diff_pair.is_some() || stream || stream_errors));
    Ok(CliCommand::Run(CliArguments {
        program,
        program_file,
        null_input,
        raw_input,
        slurp,
        bindings,
        max_memory_bytes: max_memory_bytes.unwrap_or(DEFAULT_MAX_MEMORY_BYTES),
        max_spill_bytes: max_spill_bytes.unwrap_or(0),
        max_spill_disk_bytes: max_spill_disk_bytes.unwrap_or(0),
        max_iterations,
        max_rss,
        library_paths,
        input,
        output,
        parallel: resolve_parallel_selection(parallel, workers, parallel_from_argv, workers_from_argv)?,
        mismatch_policy,
        strictness,
        json_facts,
        types_as_strings,
        diagnostics,
        explain,
        exit_status,
        seed,
        unbuffered,
        colour: if monochrome {
            crate::colour::ColourRequest::ForceOff
        } else if color {
            crate::colour::ColourRequest::ForceOn
        } else {
            crate::colour::ColourRequest::Auto
        },
        stream,
        stream_errors,
        follow,
        seq_recovering,
        positional_args,
        plan_out,
        plan_file,
        edit,
        edit_check,
        edit_expand_alias,
        output_path,
        in_place,
        split_exp,
        split_exp_file,
        input_files,
        no_atomic,
        diff_pair,
        diff_old_selection: diff_old,
        diff_new_selection: diff_new,
        schema_file,
        config_source,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four help enumeration lines must be the acceptance tables, nothing more and nothing less: the whole point of
    /// the tables is that the prose help cannot silently skip (or invent) a format or dialect. This is the in-crate
    /// belt; the process-level witness lives in the capability gate.
    #[test]
    fn help_enumerations_are_exactly_the_acceptance_tables() {
        let help = help_text();
        for (spelling, _) in CliFormat::INPUT_FORMATS {
            assert!(help.contains(spelling), "--help omits input format {spelling}");
        }
        for (spelling, _) in CliFormat::OUTPUT_FORMATS {
            assert!(help.contains(spelling), "--help omits output format {spelling}");
        }
        for (spelling, _) in CliInputDialect::ALL {
            assert!(help.contains(spelling), "--help omits input dialect {spelling}");
        }
        for (spelling, _) in CliOutputDialect::ALL {
            assert!(help.contains(spelling), "--help omits output dialect {spelling}");
        }
        let input_formats_line = help
            .lines()
            .find(|line| line.starts_with("  --input-format "))
            .expect("--input-format help line");
        let input_formats_enum = input_formats_line.split_once("  --input-format ").unwrap().1;
        assert!(
            input_formats_enum
                .split('|')
                .all(|spelling| CliFormat::INPUT_FORMATS.iter().any(|(s, _)| *s == spelling)),
            "an input-format spelling is not in the acceptance table: {input_formats_line}"
        );
        let output_formats_line = help
            .lines()
            .find(|line| line.starts_with("  --output-format "))
            .expect("--output-format help line");
        let output_formats_enum = output_formats_line.split_once("  --output-format ").unwrap().1;
        assert!(
            output_formats_enum
                .split('|')
                .all(|spelling| CliFormat::OUTPUT_FORMATS.iter().any(|(s, _)| *s == spelling)),
            "an output-format spelling is not in the acceptance table: {output_formats_line}"
        );
        let input_dialects_line = help
            .lines()
            .find(|line| line.starts_with("  --input-dialect "))
            .expect("--input-dialect help line");
        let input_dialects_enum = input_dialects_line.split_once("  --input-dialect ").unwrap().1;
        assert!(
            input_dialects_enum
                .split('|')
                .all(|spelling| CliInputDialect::ALL.iter().any(|(s, _)| *s == spelling)),
            "an input-dialect spelling is not in the acceptance table: {input_dialects_line}"
        );
        let output_dialects_line = help
            .lines()
            .find(|line| line.starts_with("  --output-dialect "))
            .expect("--output-dialect help line");
        let output_dialects_enum = output_dialects_line.split_once("  --output-dialect ").unwrap().1;
        assert!(
            output_dialects_enum
                .split('|')
                .all(|spelling| CliOutputDialect::ALL.iter().any(|(s, _)| *s == spelling)),
            "an output-dialect spelling is not in the acceptance table: {output_dialects_line}"
        );
    }

    /// Every flag in the CLI surface carries a tier (§3). The enumeration source is the help template — the same table
    /// the parser documents — plus the parser-accepted flags the help deliberately does not list. An unclassified flag
    /// fails HERE with "you forgot to classify", never as "it defaulted to permitted": an unclassified flag is not
    /// config-readable by construction.
    #[test]
    fn every_flag_carries_a_tier() {
        let mut classified: std::collections::HashSet<&str> = FLAG_TIERS.iter().map(|(name, _)| *name).collect();
        // Only the Options section enumerates flags; the Configuration block at the end is prose that also names
        // `--config`.
        let mut in_options = false;
        for line in HELP_TEMPLATE.lines() {
            if line == "Options:" {
                in_options = true;
                continue;
            }
            if !in_options {
                continue;
            }
            if line == "Configuration:" {
                break;
            }
            let Some(rest) = line.strip_prefix("  ") else {
                continue;
            };
            // Continuation prose is indented deeper than an option line.
            if rest.starts_with(' ') {
                continue;
            }
            // The option line's long spelling is its first `--` token (` -n, --null-input` → `null-input`; ` --diff OLD
            // NEW` → `diff`).
            let Some(token) = rest.split_whitespace().find(|t| t.starts_with("--")) else {
                continue;
            };
            let name = &token[2..];
            assert!(
                classified.remove(name),
                "flag {name} is documented in --help but has no tier in \
                 FLAG_TIERS — classify it"
            );
        }
        // The config flags are documented now, so the Options-section loop already accounted for them; the reverse
        // direction — a tier on a flag outside the surface — is a stale row. (`--seq` gained its own option line in 091
        // §5, so the loop removes it, not an exception here.)
        assert!(
            classified.is_empty(),
            "FLAG_TIERS lists flags outside the surface: {classified:?}"
        );
    }

    /// The parser accepts exactly the table spellings: a spelling the table lists must parse, and a spelling the table
    /// does not list must be rejected. XML in the output surface is the 049 item-4 regression.
    #[test]
    fn parse_accepts_exactly_the_table_spellings() {
        for (spelling, _) in CliFormat::OUTPUT_FORMATS {
            assert_eq!(
                parse_format("--output-format", spelling, &CliFormat::OUTPUT_FORMATS).ok(),
                Some(
                    CliFormat::OUTPUT_FORMATS
                        .iter()
                        .find(|(s, _)| *s == spelling)
                        .unwrap()
                        .1
                ),
                "parse_format rejects accepted spelling {spelling}"
            );
        }
        for (spelling, _) in CliInputDialect::ALL {
            assert!(
                parse_input_dialect("--input-dialect", spelling).is_ok(),
                "parse_input_dialect rejects accepted spelling {spelling}"
            );
        }
        for (spelling, _) in CliOutputDialect::ALL {
            assert!(
                parse_output_dialect("--output-dialect", spelling).is_ok(),
                "parse_output_dialect rejects accepted spelling {spelling}"
            );
        }
        assert!(
            parse_format("--output-format", "not-a-format", &CliFormat::OUTPUT_FORMATS).is_err(),
            "parse_format accepts a spelling outside the table"
        );
        // The families validate against their OWN tables: render is output-only (the input table is a strict subset),
        // so an input-side flag naming it is a usage error even though --output-format takes it.
        assert!(
            parse_format("--input-format", "render", &CliFormat::INPUT_FORMATS).is_err(),
            "parse_format accepts an output-only spelling for --input-format"
        );
        assert!(
            parse_input_dialect("--input-dialect", "not-a-dialect").is_err(),
            "parse_input_dialect accepts a spelling outside the table"
        );
        assert!(
            parse_output_dialect("--output-dialect", "not-a-dialect").is_err(),
            "parse_output_dialect accepts a spelling outside the table"
        );
    }

    /// The `--help <topic>` surface is one enumeration derived from the acceptance tables: every format/dialect
    /// spelling, every fixed topic, and every builtin family's canonical name is a topic.
    #[test]
    fn help_topics_are_exactly_the_tables_plus_the_fixed_topics() {
        for (spelling, _) in CliFormat::INPUT_FORMATS {
            assert!(
                parse_help_topic(spelling).is_ok(),
                "input format {spelling} is not a help topic"
            );
        }
        for (spelling, _) in CliFormat::OUTPUT_FORMATS {
            assert!(
                parse_help_topic(spelling).is_ok(),
                "output format {spelling} is not a help topic"
            );
        }
        for (spelling, _) in CliInputDialect::ALL {
            assert!(
                parse_help_topic(spelling).is_ok(),
                "input dialect {spelling} is not a help topic"
            );
        }
        for (spelling, _) in CliOutputDialect::ALL {
            assert!(
                parse_help_topic(spelling).is_ok(),
                "output dialect {spelling} is not a help topic"
            );
        }
        for (spelling, _) in HELP_TOPICS {
            assert!(
                parse_help_topic(spelling).is_ok(),
                "fixed topic {spelling} is not a help topic"
            );
        }
        assert!(
            parse_help_topic("startswith").is_ok(),
            "a registered builtin family is a help topic"
        );
        assert!(
            parse_help_topic("no-such-topic").is_err(),
            "a spelling outside the tables and registry is not a topic"
        );
    }

    /// The HTML surface is complete: input format, output format, the document input dialect, and both output profiles
    /// must all be accepted AND named by the help.
    #[test]
    fn html_is_fully_surfaced() {
        let help = help_text();
        assert!(help.contains(
            "--input-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack"
        ));
        assert!(help.contains(
            "--output-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack|render"        ));
        assert!(help.contains("html.document@1"));
        assert!(help.contains("html.fragment@1"));
        assert!(help.contains("html.source@1"));
        assert!(help.contains("html.document-serialize@1"));
    }

    /// The XML surface is complete: input format, output format, the document input dialect, and both output profiles
    /// must all be accepted AND named by the help.
    #[test]
    fn xml_is_fully_surfaced() {
        let help = help_text();
        assert!(help.contains(
            "--input-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack"
        ));
        assert!(help.contains(
            "--output-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack|render"        ));
        assert!(help.contains("xml.document@1"));
        assert!(help.contains("xml.source@1"));
        assert!(help.contains("xml.jqf-deterministic@1"));
        assert_eq!(
            parse_format("--output-format", "xml", &CliFormat::OUTPUT_FORMATS).ok(),
            Some(CliFormat::Xml)
        );
        assert_eq!(
            parse_input_dialect("--input-dialect", "xml.document@1").ok(),
            Some(CliInputDialect::XmlDocument)
        );
        assert_eq!(
            parse_output_dialect("--output-dialect", "xml.source@1").ok(),
            Some(CliOutputDialect::XmlSource)
        );
        assert_eq!(
            parse_output_dialect("--output-dialect", "xml.jqf-deterministic@1").ok(),
            Some(CliOutputDialect::XmlDeterministic)
        );
    }

    /// The jqft-family surface is complete: both input formats, both output formats, both input dialects, and both
    /// output profiles must all be accepted AND named by the help (049 item 4 — the same surface law as xml).
    #[test]
    fn jqft_is_fully_surfaced() {
        let help = help_text();
        assert!(help.contains(
            "--input-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack"
        ));
        assert!(help.contains(
            "--output-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack|render"        ));
        assert!(help.contains("jqft.document@1"));
        assert!(help.contains("jqft.canonical@1"));
        assert!(help.contains("jqfjson.document@1"));
        assert!(help.contains("jqfjson.canonical@1"));
        assert_eq!(
            parse_format("--input-format", "jqft", &CliFormat::INPUT_FORMATS).ok(),
            Some(CliFormat::Jqft)
        );
        assert_eq!(
            parse_format("--output-format", "jqfjson", &CliFormat::OUTPUT_FORMATS).ok(),
            Some(CliFormat::Jqfjson)
        );
        assert_eq!(
            parse_input_dialect("--input-dialect", "jqft.document@1").ok(),
            Some(CliInputDialect::JqftDocument)
        );
        assert_eq!(
            parse_output_dialect("--output-dialect", "jqft.canonical@1").ok(),
            Some(CliOutputDialect::JqftCanonical)
        );
    }

    /// The json-seq surface is complete: input format, output format, the strict input dialect, and the jqf output
    /// dialect are all accepted AND named by the help; the reserved recovering identity is NOT a dialect.
    #[test]
    /// The JSONC surface : the format appears in BOTH directions of the help enumeration and in the acceptance tables,
    /// and the reserved source echo dialect stays out of the selectable set.
    fn jsonc_is_fully_surfaced() {
        let help = help_text();
        assert!(help.contains(
            "--input-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack"
        ));
        assert!(help.contains(
            "--output-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack|render"
        ));
        assert!(help.contains("jsonc.trailing@1"));
        assert!(help.contains("jsonc.default@1"));
        assert!(help.contains("jsonc.trailing-jqf@1"));
        assert!(help.contains("jsonc.default-jqf@1"));
        assert!(help.contains("jsonc.jqf-1.0@1"));
        assert_eq!(
            parse_format("--input-format", "jsonc", &CliFormat::INPUT_FORMATS).ok(),
            Some(CliFormat::Jsonc)
        );
        assert_eq!(
            parse_format("--output-format", "jsonc", &CliFormat::OUTPUT_FORMATS).ok(),
            Some(CliFormat::Jsonc)
        );
        assert_eq!(
            parse_input_dialect("--input-dialect", "jsonc.trailing@1").ok(),
            Some(CliInputDialect::JsoncTrailing)
        );
        assert_eq!(
            parse_input_dialect("--input-dialect", "jsonc.default@1").ok(),
            Some(CliInputDialect::JsoncDefault)
        );
        assert_eq!(
            parse_output_dialect("--output-dialect", "jsonc.trailing-jqf@1").ok(),
            Some(CliOutputDialect::JsoncTrailingJqf)
        );
        assert_eq!(
            parse_output_dialect("--output-dialect", "jsonc.jqf-1.0@1").ok(),
            Some(CliOutputDialect::JsoncJqf)
        );
        assert!(
            parse_output_dialect("--output-dialect", "jsonc.source@1").is_err(),
            "the reserved source echo identity must not be a selectable dialect"
        );
    }

    /// The JSON5 surface is complete: input format, output format, the document input dialect, and both output profiles
    /// must all be accepted AND named by the help (049 item 4 — the same surface law as jsonc; /D8).
    #[test]
    fn json5_is_fully_surfaced() {
        let help = help_text();
        assert!(help.contains(
            "--input-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack"
        ));
        assert!(help.contains(
            "--output-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack|render"
        ));
        assert!(help.contains("json5.document@1"));
        assert!(help.contains("json5.jqf@1"));
        assert!(help.contains("json5.jqf-1.0@1"));
        assert_eq!(
            parse_format("--input-format", "json5", &CliFormat::INPUT_FORMATS).ok(),
            Some(CliFormat::Json5)
        );
        assert_eq!(
            parse_format("--output-format", "json5", &CliFormat::OUTPUT_FORMATS).ok(),
            Some(CliFormat::Json5)
        );
        assert_eq!(
            parse_input_dialect("--input-dialect", "json5.document@1").ok(),
            Some(CliInputDialect::Json5Document)
        );
        assert_eq!(
            parse_output_dialect("--output-dialect", "json5.jqf@1").ok(),
            Some(CliOutputDialect::Json5Jqf)
        );
        assert_eq!(
            parse_output_dialect("--output-dialect", "json5.jqf-1.0@1").ok(),
            Some(CliOutputDialect::Json5Jqf10)
        );
        assert!(
            parse_output_dialect("--output-dialect", "json5.source@1").is_err(),
            "the reserved source echo identity must not be a selectable dialect"
        );
    }

    #[test]
    fn json_seq_is_fully_surfaced() {
        let help = help_text();
        assert!(help.contains(
            "--input-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack"
        ));
        assert!(help.contains(
            "--output-format json|jsonc|json5|ndjson|json-seq|toml|csv|tsv|cbor|cbor-seq|yaml|jqft|jqfjson|jqfb|xml|html|properties|ini|dotenv|messagepack|render"        ));
        assert!(help.contains("json-seq.strict@1"));
        assert!(help.contains("json-seq.jqf@1"));
        assert!(!help.contains("json-seq.recover@1"));
        assert_eq!(
            parse_format("--input-format", "json-seq", &CliFormat::INPUT_FORMATS).ok(),
            Some(CliFormat::JsonSeq)
        );
        assert_eq!(
            parse_format("--output-format", "json-seq", &CliFormat::OUTPUT_FORMATS).ok(),
            Some(CliFormat::JsonSeq)
        );
        assert_eq!(
            parse_input_dialect("--input-dialect", "json-seq.strict@1").ok(),
            Some(CliInputDialect::JsonSeqStrict)
        );
        assert_eq!(
            parse_output_dialect("--output-dialect", "json-seq.jqf@1").ok(),
            Some(CliOutputDialect::JsonSeqJqf)
        );
        assert!(
            parse_input_dialect("--input-dialect", "json-seq.recover@1").is_err(),
            "the reserved recovering identity must not be a selectable dialect"
        );
    }

    /// User-facing help must stay free of plan numbers, item codes, and a competitor version pin.
    /// `--plan-out`/`--plan-file` are flag names, not provenance.
    #[test]
    fn help_is_clean_of_provenance() {
        let help = help_text();
        let short = short_help_text();
        for (name, text) in [("help", help.as_str()), ("short-help", short.as_str())] {
            assert!(!text.contains("1.8.2"), "{name} pins a jq version: {text}");
            assert!(!text.contains("PITCH"), "{name} names PITCH");
            for line in text.lines() {
                if line.contains("--plan-out") || line.contains("--plan-file") {
                    continue;
                }
                if line.contains("the request's plan")
                    || line.contains("serialized")
                    || line.contains("routing-facts plan")
                    || line.contains("print the derived plan")
                    || line.contains("plans serial")
                    || line.contains("parallel plan")
                {
                    continue;
                }
                assert!(
                    !line.contains("plan 0")
                        && !line.contains("plan 1")
                        && !line.contains("plan-0")
                        && !line.contains("plan-1"),
                    "{name} line carries a plan number: {line}"
                );
                assert!(
                    !line.contains(" §") && !line.contains("P7-") && !line.contains(" D8"),
                    "{name} line carries an item code: {line}"
                );
            }
        }
    }
}
