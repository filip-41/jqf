//! Re-read a deferred TOML value span into a value.
//!
//! Only contiguous value containers (arrays, inline tables) defer. Standard tables have no single source extent. The
//! span is re-parsed as `x = <span>` through [`crate::parse::parse_direct`] under the document's dialect.

use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, map_span_materialization_error};
use jqf_data::{BuilderCoverage, DataError, LazySpanMaterializer, Value};
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::locate::Located;
use crate::materialize;
use crate::parse::{self, map_data};
use crate::provider::DialectKind;

/// Synthetic source identity for the re-read of a deferred value region.
///
/// The span is valid by construction — the outer decode's validating scan already accepted these exact bytes — so
/// no diagnostic ever renders this identity or the label below.
const SPAN_SOURCE: SourceRef = SourceRef::new(SourceId::new(0), SourceKind::Input);

/// The TOML 1.0 reader for deferred container spans.
pub(crate) struct TomlSpanMaterializer {
    dialect: DialectKind,
}

/// The two installed readers, one per dialect.
///
/// Each is a unit-ish type with no configuration beyond the grammar it serves; a single `&'static` reference per
/// dialect serves every document the TOML decoder publishes under that dialect.
pub(crate) static TOML_10_SPAN_MATERIALIZER: TomlSpanMaterializer = TomlSpanMaterializer {
    dialect: DialectKind::Toml10,
};
pub(crate) static TOML_11_SPAN_MATERIALIZER: TomlSpanMaterializer = TomlSpanMaterializer {
    dialect: DialectKind::Toml11,
};

impl LazySpanMaterializer for TomlSpanMaterializer {
    fn materialize_span(&self, text: &str, resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
        materialize(text, self.dialect, resources).map_err(|error| map_span_materialization_error(&error))
    }
}

fn materialize(text: &str, dialect: DialectKind, resources: &mut ResourceContext<'_>) -> Result<Value, CodecError> {
    let mut wrapped = alloc::vec![b'x', b' ', b'='];
    wrapped.extend_from_slice(text.as_bytes());
    let source = ResolvedSource::new(SPAN_SOURCE, "container-span", &wrapped, 0);
    let doc = parse::parse_direct(source, dialect, resources)?;
    let value = parse::first_assignment_value(&doc)?.clone();
    let (builder, root) = materialize::build_located_document(
        &Located::Value(&value),
        doc.names(),
        &wrapped,
        BuilderCoverage::minimal_semantic(),
        resources,
    )?;
    let document = builder.finish(root, resources).map_err(map_data)?;
    document.materialize_root(resources).map_err(map_data)
}

// --------------------------------------------------------------------------- The byte-walk materialization family:
// each located answer of the walk is turned into a fresh document by re-parsing the validated source regions it names
// through parse-direct, with the ORDINARY grammar under the document's own dialect.

/// Parses one complete VALUE region — wrapped as `x = <span>` — and builds a fresh document whose root is that
/// value. The walk's carried comments attach to the built root: the LEADING set as `toml.comment@1` and the own-line
/// trailing set as `toml.comment_inline@1`.
#[expect(
    clippy::too_many_arguments,
    reason = "one wrapped-value rebuild: span, carried comments, dialect, and coverage"
)]
pub(crate) fn build_wrapped_value(
    bytes: &[u8],
    start: usize,
    end: usize,
    leading: &[String],
    inline: &[String],
    dialect: DialectKind,
    coverage: BuilderCoverage,
    resources: &mut ResourceContext<'_>,
) -> Result<(jqf_data::AccountedDocumentBuilder<'static>, jqf_data::NodeId), CodecError> {
    let mut wrapped = alloc::vec![b'x', b' ', b'='];
    wrapped.extend_from_slice(&bytes[start..end]);
    let source = ResolvedSource::new(SPAN_SOURCE, "container-span", &wrapped, 0);
    let doc = parse::parse_direct(source, dialect, resources)?;
    let value = parse::first_assignment_value(&doc)?.clone();
    let (mut builder, root) =
        materialize::build_located_document(&Located::Value(&value), doc.names(), &wrapped, coverage, resources)?;
    parse::attach_comments(
        &mut builder,
        leading,
        inline,
        root,
        coverage.attached_facts(),
        resources,
    )?;
    Ok((builder, root))
}

/// Builds the fresh object a dotted key's IMPLICIT table materializes: the target has no contiguous source region (its
/// members are entries of the ENCLOSING inline table written with longer dotted paths), so the walk's collected pieces
/// are rebuilt as one synthesized inline table `{ <rest> = <value>, ... }` and parsed with the ordinary grammar under
/// the document's own dialect.
pub(crate) fn build_implicit_table(
    bytes: &[u8],
    pieces: &[(Vec<String>, usize, usize)],
    dialect: DialectKind,
    coverage: BuilderCoverage,
    resources: &mut ResourceContext<'_>,
) -> Result<(jqf_data::AccountedDocumentBuilder<'static>, jqf_data::NodeId), CodecError> {
    let buffer = synthesize_implicit_table_region(bytes, pieces);
    let source = ResolvedSource::new(SPAN_SOURCE, "implicit-table", &buffer, 0);
    let doc = parse::parse_direct(source, dialect, resources)?;
    let value = parse::first_assignment_value(&doc)?.clone();
    materialize::build_located_document(&Located::Value(&value), doc.names(), &buffer, coverage, resources)
}

fn synthesize_implicit_table_region(bytes: &[u8], pieces: &[(Vec<String>, usize, usize)]) -> Vec<u8> {
    let mut buffer = alloc::vec![b'x', b' ', b'='];
    buffer.push(b'{');
    for (index, (path, start, end)) in pieces.iter().enumerate() {
        if index > 0 {
            buffer.push(b',');
        }
        for (component, text) in path.iter().enumerate() {
            if component > 0 {
                buffer.push(b'.');
            }
            push_key_component(&mut buffer, text);
        }
        buffer.extend_from_slice(b" = ");
        buffer.extend_from_slice(&bytes[*start..*end]);
    }
    buffer.push(b'}');
    buffer
}

fn push_key_component(out: &mut Vec<u8>, text: &str) {
    if crate::encode::is_bare_key(text) {
        out.extend_from_slice(text.as_bytes());
    } else {
        out.push(b'"');
        crate::encode::escape_basic_string(text, |bytes| -> Result<(), ()> {
            out.extend_from_slice(bytes);
            Ok(())
        })
        .expect("the infallible sink never fails");
        out.push(b'"');
    }
}

/// Concatenates one located table's subtree STATEMENT spans and parses them as a mini-document; selects the walk's
/// exact target and builds a fresh document whose root is that target. The walk's carried FOOT run attaches to the
/// built root as `toml.comment_foot@1`.
#[expect(
    clippy::too_many_arguments,
    reason = "one statement-table rebuild: spans, foot comments, path depth, dialect, and coverage"
)]
pub(crate) fn build_statement_table(
    bytes: &[u8],
    spans: &[jqf_source::Span],
    foot: &[String],
    key_depth: usize,
    element: bool,
    dialect: DialectKind,
    coverage: BuilderCoverage,
    resources: &mut ResourceContext<'_>,
) -> Result<(jqf_data::AccountedDocumentBuilder<'static>, jqf_data::NodeId), CodecError> {
    let mut buffer = Vec::new();
    for span in spans {
        buffer.extend_from_slice(&bytes[span.start() as usize..span.end() as usize]);
        buffer.push(b'\n');
    }
    let source = ResolvedSource::new(SPAN_SOURCE, "table-spans", &buffer, 0);
    let mut doc = parse::parse_direct(source, dialect, resources)?;
    let target = parse::target_ids_for_walk(&doc, key_depth, element)?;
    let (mut builder, root) = parse::build_located_from_doc(&mut doc, &target, &buffer, coverage, resources)?;
    parse::attach_foot_comments(&mut builder, foot, root, coverage.attached_facts(), resources)?;
    Ok((builder, root))
}

/// Builds the fresh array a range over an array VALUE materializes: the in-range region is wrapped as `x = [<region>]`
/// (or `x = []` for a degenerate range) and parsed with the ordinary grammar.
pub(crate) fn build_range_value(
    bytes: &[u8],
    start: usize,
    end: usize,
    empty: bool,
    dialect: DialectKind,
    coverage: BuilderCoverage,
    resources: &mut ResourceContext<'_>,
) -> Result<(jqf_data::AccountedDocumentBuilder<'static>, jqf_data::NodeId), CodecError> {
    let mut wrapped = alloc::vec![b'x', b' ', b'='];
    wrapped.push(b'[');
    if !empty {
        wrapped.extend_from_slice(&bytes[start..end]);
    }
    wrapped.push(b']');
    let source = ResolvedSource::new(SPAN_SOURCE, "range-spans", &wrapped, 0);
    let doc = parse::parse_direct(source, dialect, resources)?;
    let value = parse::first_assignment_value(&doc)?.clone();
    materialize::build_located_document(&Located::Value(&value), doc.names(), &wrapped, coverage, resources)
}

/// Builds the fresh array a range over an array-of-tables materializes: each in-range element's subtree is concatenated
/// and parsed separately (its spans index its OWN buffer), the element's span-backed text is resolved against that
/// buffer, and the collected element tables become the items.
pub(crate) fn build_range_of_tables(
    bytes: &[u8],
    element_spans: &[Vec<jqf_source::Span>],
    dialect: DialectKind,
    coverage: BuilderCoverage,
    resources: &mut ResourceContext<'_>,
) -> Result<(jqf_data::AccountedDocumentBuilder<'static>, jqf_data::NodeId), CodecError> {
    let mut tables: Vec<crate::grammar::TableTree> = Vec::new();
    let mut names = Vec::new();
    for spans in element_spans {
        let mut buffer = Vec::new();
        for span in spans {
            buffer.extend_from_slice(&bytes[span.start() as usize..span.end() as usize]);
            buffer.push(b'\n');
        }
        let source = ResolvedSource::new(SPAN_SOURCE, "element-spans", &buffer, 0);
        let mut doc = parse::parse_direct(source, dialect, resources)?;
        let mut element = parse::single_aot_element_subtree(&mut doc)?;
        resolve_table_spans(&mut element, &buffer)?;
        let offset = u32::try_from(names.len()).expect("interned name count");
        crate::grammar::offset_table_key_ids(&mut element, offset);
        names.extend(doc.names().iter().cloned());
        tables.push(element);
    }
    materialize::build_located_document(&Located::ArrayOfTables(&tables), &names, bytes, coverage, resources)
}

/// Resolves every span-backed string in a parsed TABLE subtree against the buffer it was parsed from, into owned text:
/// the final build reads ONE source buffer, but each element tree's spans index its OWN concatenation.
fn resolve_table_spans(table: &mut crate::grammar::TableTree, buffer: &[u8]) -> Result<(), CodecError> {
    for (_, value) in &mut table.assignments {
        resolve_tree_spans(value, buffer)?;
    }
    for (_, child) in &mut table.children {
        match child {
            crate::grammar::ChildKind::Table(table) => resolve_table_spans(table, buffer)?,
            crate::grammar::ChildKind::ArrayOfTables(elements) => {
                for element in elements {
                    resolve_table_spans(element, buffer)?;
                }
            }
        }
    }
    Ok(())
}

fn resolve_tree_spans(tree: &mut crate::grammar::Tree, buffer: &[u8]) -> Result<(), CodecError> {
    match tree {
        crate::grammar::Tree::String(source) => {
            if let crate::grammar::TextSource::Span(span) = source {
                let start = span.start() as usize;
                let end = span.end() as usize;
                *source = crate::grammar::TextSource::Copied(
                    String::from_utf8(buffer[start..end].to_vec()).expect("validated UTF-8"),
                );
            }
            Ok(())
        }
        crate::grammar::Tree::Array { items, .. } => {
            for item in items {
                resolve_tree_spans(item, buffer)?;
            }
            Ok(())
        }
        crate::grammar::Tree::InlineTable { entries, .. } => {
            for (_, entry) in entries {
                resolve_tree_spans(entry, buffer)?;
            }
            Ok(())
        }
        crate::grammar::Tree::Commented { value, .. } => resolve_tree_spans(value, buffer),
        _ => Ok(()),
    }
}
