//! The discovery surface : `--list-builtins`, `--list-formats`, `--help-format <fmt>`, and `--explain-code <id>`.
//!
//! The law this module exists to serve: **every enumeration the CLI prints is GENERATED from the registry that owns the
//! fact, never hand-written.** Each command reads the same tables the parser reads, so what the CLI advertises and what
//! the CLI accepts can never drift (the 049 item-4 defect class: `--output-format xml` accepted while `--help` omitted
//! it).
//!
//! - `--list-builtins` enumerates the builtin registry — the same two sources
//!   the `builtins` builtin enumerates (one law, two doors; pinned by the `discovery_surface` integration test).
//! - `--help <builtin>` prints one family's [`BuiltinFamilyRecord:summary`]
//!   and [`BuiltinFamilyRecord:detail`] from the same registry ([`jqf_engine:resolve_family`]).
//! - `--list-formats` and `--help-format <fmt>` read the acceptance tables in
//!   [`crate:args`] (`INPUT_FORMATS`/`OUTPUT_FORMATS`/`ALL` dialect tables) plus the dialect→format grouping each dialect
//!   owns.
//! - `--explain-code <id>` reads the generated diagnostic-code registry
//!   (codes.toml is the manifest; `tools/gates/jqf-diag-codes-gen.py` is the only writer of the table this command reads).

use std::fmt::Write as _;

use crate::args::{CliFormat, CliInputDialect, CliOutputDialect, HelpTopic, help_text, join_spellings};

/// `--list-builtins`: every registered `name/arity`, sorted, exactly as the `builtins` builtin answers it.
///
/// The one-law pin is the integration test `list_builtins_matches_the_builtin`: this output and `null | builtins` must
/// enumerate the same names, so the CLI surface and the language surface cannot drift apart.
pub(crate) fn list_builtins() -> String {
    let mut names: Vec<String> = jqf_engine::builtin_overloads()
        .iter()
        // the adopted `builtins` hides its underscore-prefixed internals (`_negate`, `_strindices` — answers 226
        // entries with none starting `_`), and jqf's truthful enumeration follows.
        .filter(|overload| !overload.canonical_name.starts_with('_'))
        .map(|overload| format!("{}/{}", overload.canonical_name, overload.arity))
        .collect();
    for (name, arity) in jqf_engine::PRELUDE_ENUMERATED {
        names.push(format!("{name}/{arity}"));
    }
    names.sort();
    let mut out = String::new();
    for name in names {
        let _ = writeln!(out, "{name}");
    }
    out
}

/// `--help <builtin>`: one family's summary and detail from the registry.
///
/// The spelling is the family's canonical name (`map`, not `map/1`). An empty [`jqf_engine:BuiltinFamilyRecord:detail`]
/// omits the detail line; the summary is always present because const validation requires it.
pub(crate) fn help_builtin(canonical_name: &str) -> Result<String, String> {
    let family = jqf_engine::resolve_family(canonical_name)
        .ok_or_else(|| format!("no builtin family {canonical_name:?} in the registry"))?;
    let mut out = String::new();
    let _ = writeln!(out, "name:     {}", family.canonical_name);
    let _ = writeln!(out, "category: {}", family.category);
    let _ = writeln!(out, "summary:  {}", family.summary);
    if !family.detail.is_empty() {
        let _ = writeln!(out, "detail:   {}", family.detail);
    }
    Ok(out)
}

/// `--list-formats`: the format/dialect inventory from the acceptance tables.
///
/// The first two lines echo the help template's four enumeration slots; the per-format blocks group each format's input
/// and output dialects through the `format` accessor each dialect owns. Nothing here is a literal: every spelling comes
/// from `INPUT_FORMATS`/`OUTPUT_FORMATS`/`ALL`, the same tables
/// `parse_format`/`parse_input_dialect`/`parse_output_dialect` read.
pub(crate) fn list_formats(catalog: jqf_sdk::CodecCatalog<'_, '_>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "input formats:  {}",
        join_spellings(&CliFormat::INPUT_FORMATS, " ")
    );
    let _ = writeln!(
        out,
        "output formats: {}",
        join_spellings(&CliFormat::OUTPUT_FORMATS, " ")
    );
    out.push('\n');
    for (spelling, format) in CliFormat::INPUT_FORMATS {
        push_format_block(&mut out, format, spelling, catalog);
    }
    for (spelling, format) in CliFormat::OUTPUT_FORMATS {
        if CliFormat::INPUT_FORMATS.iter().any(|(_, existing)| *existing == format) {
            continue;
        }
        push_format_block(&mut out, format, spelling, catalog);
    }
    out
}

fn push_format_block(out: &mut String, format: CliFormat, spelling: &str, catalog: jqf_sdk::CodecCatalog<'_, '_>) {
    let _ = writeln!(out, "{spelling}");
    let is_input = CliFormat::INPUT_FORMATS.iter().any(|(_, existing)| *existing == format);
    let is_output = CliFormat::OUTPUT_FORMATS
        .iter()
        .any(|(_, existing)| *existing == format);
    let direction = match (is_input, is_output) {
        (true, true) => "input, output",
        (true, false) => "input",
        (false, true) => "output",
        (false, false) => "none",
    };
    let _ = writeln!(out, "  direction: {direction}");
    if is_input {
        // The input model is a codec route capability, read through the catalog: the first input dialect's declaration
        // answers for the format (every dialect of one format declares the same model).
        let model = CliInputDialect::ALL
            .iter()
            .find(|(_, dialect)| dialect.format() == format)
            .and_then(|(_, dialect)| {
                let format_id = jqf_data::FormatId::try_new(dialect.format().id()).ok()?;
                let dialect_id = jqf_data::DialectId::try_new(dialect.id()).ok()?;
                catalog.route_capabilities(&format_id, &dialect_id).ok()
            })
            .map(|caps| {
                if caps.contains(&jqf_codec_core::RouteCapability::AdjacentValues) {
                    "a stream of values"
                } else {
                    "one document per source"
                }
            });
        match model {
            Some(model) => {
                let _ = writeln!(out, "  input model: {model}");
            }
            None => {
                let _ = writeln!(out, "  input model: (no registered input dialect)");
            }
        }
        let dialects: Vec<&str> = CliInputDialect::ALL
            .iter()
            .filter(|(_, dialect)| dialect.format() == format)
            .map(|(spelling, _)| *spelling)
            .collect();
        let _ = writeln!(out, "  input dialects:  {}", dialects.join(" "));
        //: the extensions this format's registration claims for
        // implicit input-format detection, read from the catalog's own declaration (the 039 law). A format that
        // declares none (json-seq, render) prints no line: nothing is claimed, nothing is advertised.
        if let Ok(format_id) = jqf_data::FormatId::try_new(format.id())
            && let Ok(extensions) = catalog.extensions_for(&format_id)
            && !extensions.is_empty()
        {
            let _ = writeln!(out, "  extensions:      {}", extensions.join(" "));
        }
    }
    let output_dialects: Vec<&str> = CliOutputDialect::ALL
        .iter()
        .filter(|(_, dialect)| dialect.format() == format)
        .map(|(spelling, _)| *spelling)
        .collect();
    let _ = writeln!(out, "  output dialects: {}", output_dialects.join(" "));
    // The B7 stability note, widened to the whole jqft family
    //: jqft, jqfjson, and jqfb are the jot/.jqfd family, which
    // is sequenced LAST — after the fact schemas freeze — and carries no committed spec and no reader outside this
    // binary. All three are equally exposed (public format ids, public dialects, `--with-source`, entries in the
    // generated help), so all three carry the marker: until the freeze lands they are INTERNAL, and nobody should
    // persist data in them expecting cross-version stability. One special case because the rest of the page is
    // generated from the registration tables; when the family freezes, this line moves into the spec.
    if matches!(format, CliFormat::Jqft | CliFormat::Jqfjson | CliFormat::Jqfb) {
        let _ = writeln!(
            out,
            "  stability:     INTERNAL/UNSTABLE — the jqft family is not frozen: no \
             committed spec, no reader outside jqf; do not persist data in this format"
        );
    }
}

/// `--help-format <fmt>`: one per-format page from the same tables plus the codec's `route_capabilities` declaration.
pub(crate) fn help_format(format: CliFormat, catalog: jqf_sdk::CodecCatalog<'_, '_>) -> String {
    let mut out = String::new();
    push_format_block(&mut out, format, format.id(), catalog);
    out
}

/// `--explain-code <id>`: one diagnostic-code row from the generated registry.
pub(crate) fn explain_code(id: u16) -> Result<String, String> {
    let row =
        jqf_resource::diag::codes::describe(id).ok_or_else(|| format!("no diagnostic code {id} in the registry"))?;
    let mut out = String::new();
    let _ = writeln!(out, "code:       {}", row.name);
    let _ = writeln!(out, "id:         {id}");
    let _ = writeln!(out, "revision:   {}", row.revision);
    let _ = writeln!(out, "class:      {:?}", row.class);
    let _ = writeln!(out, "severity:   {:?}", row.severity);
    if row.reserved {
        let _ = writeln!(out, "reserved:   true");
    }
    let _ = writeln!(out, "meaning:    {}", row.description);
    Ok(out)
}

/// `--help <topic>`: one focused page. The fixed topics and the format/dialect topics all read the acceptance tables or
/// the help template [`help_text`] renders — nothing here is a hand-written enumeration. The `052` dial
/// (`--mismatch-policy lenient|warn|strict`) is a row of the same template, so it appears in the `flags` and `mismatch`
/// pages by construction.
pub(crate) fn help_topic(topic: HelpTopic, catalog: jqf_sdk::CodecCatalog<'_, '_>) -> Result<String, String> {
    match topic {
        HelpTopic::Builtin(name) => help_builtin(name),
        // The two summary topics point at the machine-readable surfaces; the enumerations themselves live there, never
        // duplicated in prose.
        HelpTopic::Builtins => Ok("\
jqf builtins
  Every registered builtin answers as name/arity.

  --list-builtins prints the full sorted enumeration — the same list the
  `builtins` builtin answers (one law, two doors).

  --help <builtin> prints one family's summary and detail from the
  registry (the family's canonical name, not name/arity).
"
        .to_owned()),
        HelpTopic::Codes => Ok("\
jqf diagnostic codes
  Every diagnostic the runtime can raise has a stable numeric code, the
  manifest is codes.toml, and the generated registry is the table the
  runtime and the CLI share.

  --explain-code <id> prints one row (id, name, class, severity, meaning);
  the --diagnostics stream names the code id of every record it emits.
"
        .to_owned()),
        HelpTopic::Flags => Ok(options_section()),
        HelpTopic::Mismatch => Ok(mismatch_section()),
        HelpTopic::Diff => Ok(diff_section()),
        HelpTopic::Generators => Ok(generators_section()),
        HelpTopic::Facts => Ok(facts_section()),
        HelpTopic::Format(format) => Ok(help_format(format, catalog)),
        HelpTopic::Dialect(spelling) => Ok(dialect_page(spelling)),
    }
}

/// The full flag table: the help template's Options section, filled from the acceptance tables exactly as [`help_text`]
/// fills it. `--help flags` shows this, so every flag — including the `052` mismatch dial — appears in the generated
/// output by construction.
fn options_section() -> String {
    let help = help_text();
    help.split_once("Options:\n")
        .expect("the help template has one Options section")
        .1
        .to_owned()
}

/// The `--mismatch-policy lenient|warn|strict` dial's own page ('s help half): the flag-table row, cut from the same
/// template [`help_text`] renders, so the dial's help cannot drift from its accepted positions.
fn mismatch_section() -> String {
    let help = help_text();
    let start = help
        .find("  --mismatch-policy ")
        .expect("the help template documents the mismatch dial");
    let rest = &help[start..];
    let end = rest.find("\n  --").map_or(rest.len(), |index| index + 1);
    rest[..end].to_owned()
}

/// The generators page: a raw string describing the `~` engine namespace. Unlike the flag-cut pages, this text is
/// authored here — it is not derived from the help template and can drift from `--help` if edited without a matching
/// check.
fn generators_section() -> String {
    r#"jqf generators: the `~` engine namespace
  Four first-class suspended generators, pulled one value at a time. A
  `~` constructor binds an engine-resident
  cursor; `~x.next` pulls the next value (empty when exhausted) and
  `~x.rest` emits all remaining values as a stream. The cursor keeps its
  state across pulls, so two cursors can be zipped or merged lazily.

  ~cursor(f)          a cursor over ANY generator f — the wrapping form
  ~generator(i;u;e)   a state machine built from init/update/extract
  ~inputs             the input sequence as a cursor (requires -n)
  ~rng(seed)          reproducible randomness (xoshiro256**; ~r.next ==
                      rand(seed), one exact integer seed)

  All four are listed by --list-builtins. A cursor cannot be returned as
  a value; pull it with ~x.next or ~x.rest.

  Zipping two generators lazily:

    echo '[[1,2,3],["a","b"]]' | jqf \
      '~cursor(.[0][]) as ~a | ~cursor(.[1][]) as ~b |
       [limit(2; repeat([~a.next, ~b.next]))]'
    # [[1,"a"],[2,"b"]]

  A state machine over null:

    echo null | jqf \
      '~generator(0; if . < 3 then .+1 else empty end; .) as ~x | [~x.rest]'
    # [1,2,3]

  Seeded randomness:

    echo null | jqf '~rng(7) as ~r | [limit(3; ~r.rest)]'
    # [0.7005764821796896, 0.2787512294737843, 0.8396274618764198]
"#
    .to_owned()
}

fn facts_section() -> String {
    r##"jqf facts: node and value facts across formats
  A document node carries facts that are NOT its value: the YAML tag that
  named it, the comment attached to it, the attributes of a markup
  element. Read them with the .@ node/value accessor and the .& markup
  attribute accessor — the same spellings --edit writes.

  .@name      one intrinsic fact: .@tag, .@comment, .@attrs, ...
  .&name      one markup attribute (XML/HTML)

  .@comment_head is a second spelling of .@comment — the three comment
  positions are named symmetrically (.@comment / .@comment_inline /
  .@comment_foot, with .@comment_head meaning the leading position),
  normalized at compile so both spellings are permanent and equal.

  The READ surface is universal (a fact that does not exist reads null).
  A fact assignment (PATH.@comment = RHS, PATH.@comment_inline = RHS, or
  PATH.@comment_foot = RHS) applies the fact in memory and encodes; --edit
  splices the same write into the retained source. Dynamic accessor writes
  (PATH.@(expr) = RHS, PATH.&($name) = RHS) compile too: the role resolves
  at run time against the closed writable vocabulary (comment positions;
  YAML style/tag/anchor/alias), and an unknown role raises rather than
  writing silently.

  Facts are PROVENANCE, not data: any operation that constructs a new
  value drops them, so a read over a computed value is null exactly like
  a missing fact — (.key + 0) | .@comment and {k: .key} | .@comment are
  null even when .key.@comment is ["top note"]. Only a value that IS the
  source node — reached by a path that constructs nothing — carries its
  facts, and the same law holds for .& attributes.

  Read a YAML tag:

    echo '!money 5' | jqf --input-format yaml '.@tag'
    # "!money"

  Read a TOML comment (the comment fact is a list of text lines):

    printf 'port = 8080 # main port\n' | jqf --input-format toml '.port.@comment'
    # ["main port"]

  Read an XML attribute:

    echo '<a href="https://x">y</a>' | jqf --input-format xml '.&href'
    # "https://x"

  Write a comment under --edit (TOML attaches the leading block to the
  statement's value):

    jqf --edit --input-format toml '.port.@comment = ["the main port"]' f.toml
    # turns 'port = 8080' into '# the main port\nport = 8080'

  --json-facts projects the facts into the JSON output automatically:
  markup elements become xq-style trees (element name as key, attributes
  as @attr, text as #text, repeated elements as arrays), and other facts
  use their accessor spelling as keys (@comment, @tag, @attrs, @name,
  @content, &attr). A fact-bearing scalar or array is wrapped as
  {"value": ...}; data keys win on collision. The projection is lossy.

  It is ON BY DEFAULT for xml/html input answered as JSON, because markup
  keeps its element names and attributes as facts and the bare value would
  drop every one of them. --no-json-facts asks for that bare value back.

    echo '<a href="https://x">y</a>' | jqf --input-format xml .
    # {"a":{"@href":"https://x","#text":"y"}}

    echo '<a href="https://x">y</a>' | jqf --input-format xml --no-json-facts -c .
    # ["y"]
"##
    .to_owned()
}

/// The `--diff OLD NEW` lane's own page: the flags that serve it and the one law first-time users hit — cross-format
/// sides compare VALUES, so a temporal on one side and the same text as a string on the other is `changed`. The flag
/// rows are cut from the same template [`help_text`] renders, so the page cannot advertise a spelling the parser
/// refuses.
fn diff_section() -> String {
    let help = help_text();
    let start = help
        .find("  --diff OLD NEW")
        .expect("the help template documents the diff flag");
    let rest = &help[start..];
    let end = rest.find("\n  --").map_or(rest.len(), |index| index + 1);
    let flags = rest[..end].to_owned();
    format!(
        "jqf --diff: the path-keyed semantic diff of two documents\n\n{flags}\n  \
--old-format F / --new-format F name one side's format; each defaults to\n  \
--input-format, so a same-format diff needs no dial and a cross-format\n  \
diff names exactly the side that differs. Both sides are decoded through\n  \
the codec catalog; a file holding more than one document is a usage error\n  \
naming the count.\n  \
The exit law: 0 when the two documents are semantically equal, 1 when
  \
they differ — the CI gate (`--diff OLD NEW` fails a check that drifted)
  \
— with usage and runtime failures keeping their own classes.
  \
The CROSS-KIND law: a TOML datetime and the same spelling as a YAML string\n  \
differ (`changed`) — temporal values are not strings. That is the SEMANTIC\n  \
in semantic diff, not a bug.\n"
    )
}

/// One dialect spelling's page: the spelling, the direction(s) it serves, and the format it selects. A spelling can
/// serve both input and output (`rfc8259` and `ndjson.strict@1` are rows of both tables); every fact is read from the
/// acceptance tables.
fn dialect_page(spelling: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{spelling}");
    let input = CliInputDialect::ALL.iter().find(|(s, _)| *s == spelling);
    let output = CliOutputDialect::ALL.iter().find(|(s, _)| *s == spelling);
    let mut directions = Vec::new();
    let mut formats = Vec::new();
    if let Some((_, dialect)) = input {
        directions.push("input");
        formats.push(dialect.format().id());
    }
    if let Some((_, dialect)) = output {
        directions.push("output");
        if !formats.contains(&dialect.format().id()) {
            formats.push(dialect.format().id());
        }
    }
    let _ = writeln!(out, "  direction: {}", directions.join(", "));
    let _ = writeln!(out, "  format: {}", formats.join(", "));
    if input.is_some_and(|(_, dialect)| dialect.has_record_route()) {
        let _ = writeln!(out, "  input model: a physical record stream");
    }
    out
}
