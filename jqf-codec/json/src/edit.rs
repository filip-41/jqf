//! Source-preserving edit splice machinery; serves the SDK edit drive.
//!
//! The splice-policy rulings live on [`crate::encode`]'s module doc; this module holds their byte-level implementation.

// --------------------------------------------------------------------- The splice policy (`render_edit_append` /
// `render_edit_remove`), written as the module-doc rulings. The anchors are the document's own authored spans: every
// BUILT container carries an out-of-band record of its OPENING delimiter (recorded at open, in node order —
// parse.rs), and every leaf value carries its authored token. The splice scans from a container's opening delimiter to
// name its whole region and observe its separator run.
//
// Every splice re-verifies by re-decode (the SDK's law), so a splice the byte rules misclassify — a span the source
// contradicts, a member whose surrounding comma the scan cannot name — returns an empty insertion/cut set and the
// caller falls back to the whole-document floor, never wrong bytes.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::ControlFlow;

use jqf_codec_core::{
    CodecError, CodecRunContext, EditAppendMembers, EditInsertion, EditRemoval, EditRemoveMembers, EncodeItem,
    ErasedEncoderSession, PreservationRequest, VecByteSink, map_data,
};
use jqf_data::{Document, DocumentCapability, FactPayloadView, NodeHandle, NodeId, Value};
use jqf_resource::ResourceContext;

use crate::byte_scan::is_json_ws;
use crate::encode::{JsonEncodeOptions, JsonEncoder, data_error, push_escaped_text};

/// Scans forward from a container's opening delimiter to the byte just past its matching close, skipping string bodies
/// (with escapes) and counting nesting. The splice operates on the document's RETAINED AUTHORED bytes — byte
/// preservation of everything the edit did not touch IS this vertical, not a codec round trip; only each patch's result
/// is verified by re-decode. The walk is deterministic over authored JSON; a run it cannot close returns `None` and the
/// caller declines.
fn container_close(source: &[u8], open: usize) -> Option<usize> {
    let closer = match source.get(open) {
        Some(b'{') => b'}',
        Some(b'[') => b']',
        _ => return None,
    };
    let mut depth = 0usize;
    let mut i = open;
    while i < source.len() {
        match source[i] {
            b'"' => {
                i += 1;
                loop {
                    match source.get(i) {
                        Some(b'\\') => i = i.checked_add(2)?,
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(_) => i += 1,
                        None => return None,
                    }
                }
            }
            b'{' | b'[' => {
                depth = depth.checked_add(1)?;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                i += 1;
                if depth == 0 {
                    return (source.get(i - 1) == Some(&closer)).then_some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// One past a non-empty container's last member VALUE, and whether the container is EMPTY: scanning back from the
/// closing delimiter over whitespace, the first non-whitespace byte reached is the opener itself for an empty container
/// (`{}`/`{ }`) and the last value's final byte otherwise.
fn last_value_end(source: &[u8], open: usize, close: usize) -> Option<(usize, bool)> {
    let closer = source.get(close.checked_sub(1)?).copied()?;
    let opener = match closer {
        b'}' => b'{',
        b']' => b'[',
        _ => return None,
    };
    let mut ve = close - 1;
    while ve > open && is_json_ws(source[ve - 1]) {
        ve -= 1;
    }
    let empty = source.get(ve - 1) == Some(&opener);
    Some((ve, empty))
}

/// The indentation (leading spaces/tabs) of the line holding the container's LAST member — the column the observed
/// separator indents new members to. A one-line container's member sits right after the opening delimiter, so its
/// indent is empty.
pub(crate) fn last_member_indent(source: &[u8], open: usize, ve: usize) -> Vec<u8> {
    let mut line_start = ve.saturating_sub(1);
    while line_start > open && source[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    source[line_start..]
        .iter()
        .copied()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .collect()
}

/// Builds the leading-comment index for one located document (`jsonc.comment@1`: a list of texts on the value node),
/// keyed by node handle. A document from owned values, or one whose decode attached no facts, yields an empty index
/// (the encode then emits no comment lines). Mirrors TOML's `build_comment_index`; called by the JSONC encoder factory
/// once per item.
pub(crate) fn build_comment_index(
    document: &Document<'_>,
    role: &'static str,
    resources: &mut ResourceContext<'_>,
) -> Result<BTreeMap<NodeHandle, Vec<String>>, CodecError> {
    let mut index = BTreeMap::new();
    let mut reader = match document.fact_reader(resources) {
        Ok(reader) => reader,
        Err(jqf_data::DataError::CapabilityUnavailable {
            capability: DocumentCapability::AttachedFacts,
        }) => return Ok(index),
        // Resource refusals and control stops are REACHABLE here (a memory-limited request can refuse the reader's
        // work), so they map through the shared carrier instead of the contract class — a ceiling hit must read as a
        // ceiling, never as an engine defect. The carrier keeps THIS site's contract name: genuinely impossible local
        // states collapse to "JSONC comment index over a valid document", never to another site's contract.
        Err(error) => {
            return Err(map_data(error, "JSONC comment index over a valid document"));
        }
    };
    match reader
        .drain(resources, |fact| {
            let jqf_data::LocalOwnerRef::Node(node) = fact.owner() else {
                return ControlFlow::Continue(());
            };
            if fact.role().as_str() != role {
                return ControlFlow::Continue(());
            }
            let FactPayloadView::List(texts) = fact.payload() else {
                return ControlFlow::Continue(());
            };
            let mut out = Vec::new();
            for entry in texts.iter() {
                if let FactPayloadView::Text(text) = entry {
                    out.push(String::from(text));
                }
            }
            if out.is_empty() {
                return ControlFlow::Continue(());
            }
            match document.node_handle(node).map_err(data_error) {
                Ok(handle) => {
                    index.insert(handle, out);
                    ControlFlow::Continue(())
                }
                Err(error) => ControlFlow::Break(error),
            }
        })
        .map_err(|error| map_data(error, "JSONC comment index read"))?
    {
        ControlFlow::Break(error) => Err(error),
        ControlFlow::Continue(()) => Ok(index),
    }
}

/// Renders one owned value in JSON's VALUE-position grammar — the bytes a splice inserts after a `:` or `,` — as a
/// compact canonical item with no trailing newline. This is the codec's `render_leaf` body too, and the one place the
/// leaf seam and the splice share a value render, so a leaf patch and a structural member can never disagree about a
/// value's bytes.
pub(crate) fn render_value_bytes(value: &Value, resources: &mut ResourceContext<'_>) -> Result<Vec<u8>, CodecError> {
    let encoder = JsonEncoder::try_new(
        EncodeItem::Owned(value),
        b"",
        b"",
        JsonEncodeOptions::default(),
        resources,
    )?;
    let mut session =
        ErasedEncoderSession::try_new(EncodeItem::Owned(value), PreservationRequest::Report, || Ok(encoder))?;
    let mut bytes = Vec::new();
    let mut sink = VecByteSink::new(&mut bytes);
    let mut run = CodecRunContext::new(resources);
    // The codec's internal straight-line convention (provider.rs): a nested one-shot encode replenishes its own
    // cooperative budget, exactly as the SDK's `encode_edit_value` does with the request's credit count.
    run.set_cooperative_credits(4_096);
    session.encode(&mut sink, &mut run)?;
    Ok(bytes)
}

/// Renders one added OBJECT member: a double-quoted (escaped) key, the canonical key-value separator (`: ` when the
/// container wraps, `:` when it is one-line), and the value's compact canonical bytes.
pub(crate) fn member_text(
    key: &str,
    value: &Value,
    wrapped: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<Vec<u8>, CodecError> {
    let mut bytes = Vec::with_capacity(key.len() + value_bytes_hint(value) + 8);
    bytes.push(b'"');
    push_escaped_text(&mut bytes, key, false)?;
    bytes.push(b'"');
    if wrapped {
        bytes.extend_from_slice(b": ");
    } else {
        bytes.push(b':');
    }
    bytes.extend_from_slice(&render_value_bytes(value, resources)?);
    Ok(bytes)
}

fn value_bytes_hint(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len() + 2,
        Value::Number(_) => 24,
        _ => 32,
    }
}

/// The run's bytes up to and including its final line break (`\r\n` counts as one), or an empty slice when the run
/// holds none.
pub(crate) fn through_final_line_break(run: &[u8]) -> &[u8] {
    match run.iter().rposition(|&byte| matches!(byte, b'\n' | b'\r')) {
        Some(position) => &run[..=position],
        None => &[],
    }
}

/// One added member's separator, in the container's observed style: the comma and, when the container wraps, the
/// whitespace run between the last member's value and the closing delimiter plus the last member's indentation (the
/// observed newline plus the column it indents to).
pub(crate) fn member_separator(
    source: &[u8],
    open: usize,
    ve: usize,
    close: usize,
    empty: bool,
    wrapped: bool,
) -> Vec<u8> {
    if empty {
        return Vec::new();
    }
    let mut bytes = Vec::with_capacity(8);
    bytes.push(b',');
    if wrapped {
        // The separator reproduces how the container's own members are separated: the wrap plus the last member's
        // indentation. The run between the last value and the closer ENDS in the closer's own indentation —
        // whitespace belonging to the PARENT level — so composing the whole run with the member indent would land the
        // new member at closer-indent + member-indent, over-indented once per nesting depth (a root container hides
        // this because its closer run is a bare newline). Only the run through its final line break is the observed
        // wrap; the bytes after it are the closer's column and are replaced by the member indent.
        bytes.extend_from_slice(through_final_line_break(&source[ve..close - 1]));
        bytes.extend_from_slice(&last_member_indent(source, open, ve));
    }
    bytes
}

/// Renders the splice for a container the edit lane GREW (rulings 1-2): the new members land inside the closing `}`/`]`
/// — after the last member's separator, reusing the observed separator run — or directly after the opening
/// delimiter for an empty container. Returns an empty insertion set (the floor) when the container's region cannot be
/// named.
pub(crate) fn render_edit_append(
    document: &Document<'_>,
    container: NodeId,
    source: &[u8],
    members: EditAppendMembers<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    let Some(span) = document.node_source_span(container).map_err(data_error)? else {
        return Ok(alloc::vec::Vec::new());
    };
    let open = span.start() as usize;
    if !matches!(source.get(open), Some(b'{' | b'[')) {
        return Ok(alloc::vec::Vec::new());
    }
    let Some(close) = container_close(source, open) else {
        return Ok(alloc::vec::Vec::new());
    };
    let Some((ve, empty)) = last_value_end(source, open, close) else {
        return Ok(alloc::vec::Vec::new());
    };
    let wrapped = !empty
        && source[ve..close.saturating_sub(1)]
            .iter()
            .any(|byte| matches!(byte, b'\n' | b'\r'));
    let separator = member_separator(source, open, ve, close, empty, wrapped);
    let mut text = Vec::new();
    match members {
        EditAppendMembers::Table(members) => {
            for (index, (key, value)) in members.iter().enumerate() {
                if index > 0 || !empty {
                    text.extend_from_slice(&separator);
                }
                text.extend_from_slice(&member_text(key, value, wrapped, resources)?);
            }
        }
        EditAppendMembers::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                if index > 0 || !empty {
                    text.extend_from_slice(&separator);
                }
                text.extend_from_slice(&render_value_bytes(item, resources)?);
            }
        }
    }
    let at = if empty { open + 1 } else { ve };
    Ok(alloc::vec![EditInsertion {
        at,
        bytes: text,
        replace: None,
    }])
}

/// The extent of one removed member's VALUE token: its first byte (the opening quote for a string, the opening
/// delimiter for a container) and one past its last byte (past the closing quote / delimiter). A built container's
/// recorded anchor names only the opening delimiter, so its extent comes from the scan; the bound-source convention
/// records a string's INNER content, so the token extends across the quote pair.
///
/// The span alone cannot say which convention produced it — a one-byte span is either a built container's anchor or a
/// one-character string's inner content — so this infers it from the NEIGHBORING bytes: an opening delimiter not
/// preceded by a quote is the anchor form, and a quote pair hugging the span marks inner content to widen across.
/// EITHER quote spelling is string evidence (`json5.document@1` writes single-quoted strings with the same inner-span
/// convention), and the widening pair must be the SAME character on both sides — the neighbors are structural bytes
/// the member itself owns, never member text, so the inference cannot misread an escaped quote inside the value.
pub(crate) fn value_extent(source: &[u8], span: jqf_source::Span) -> Option<(usize, usize)> {
    let mut start = span.start() as usize;
    let mut end = span.end() as usize;
    let quoted_at = |at: usize| matches!(source.get(at), Some(b'"' | b'\''));
    if span.end() - span.start() == 1
        && matches!(source.get(start), Some(b'{' | b'['))
        && (start == 0 || !quoted_at(start - 1))
    {
        // A built container's one-char anchor (parse.rs records the opening delimiter); a string value whose inner
        // content opens a brace is excluded by the quote check on the byte before it.
        return container_close(source, start).map(|close| (start, close));
    }
    if start > 0 && quoted_at(start - 1) && source.get(end) == source.get(start - 1) {
        start -= 1;
        end += 1;
    }
    Some((start, end))
}

/// The start of a removed object member's KEY, walking back from the value token over its whitespace, the `:`, and the
/// quoted key string. Walking backward over a quoted key, a quote byte is the opening quote iff an EVEN run of
/// backslashes precedes it — the parity handle makes an escaped quote stay inside the string. Either quote spelling
/// closes a key (JSON5 single-quoted keys ride the same convention); an identifier tail still declines. `None` for a
/// token the source does not back up to a key (an array item, an unparseable value) and the caller declines.
pub(crate) fn member_key_start(source: &[u8], open: usize, token_start: usize) -> Option<usize> {
    let close = crate::edit_support::rewind_to_key(source, token_start, open)?;
    // The walk may legally land ON `open` after the whitespace run; the key must start inside the container, so that
    // case declines.
    if close == open {
        return None;
    }
    let quote = source[close - 1];
    if !matches!(quote, b'"' | b'\'') {
        return None;
    }
    // The quote walk's floor is the container: for any source the parser accepted, the key's opening quote sits
    // strictly ABOVE `open`, so a well-formed walk reaches it first. If caller-contract drift ever breaks that premise,
    // the walk declines at `open` — never an opening quote at or below the container, and never a walk outside the
    // object.
    crate::edit_support::opening_quote_left(source, close, quote, open)
}

/// One removed member's cut (ruling 3): the member plus exactly one adjacent comma — the preceding one, or the
/// following one for the first member — plus the whitespace run the member owned (its line break and indentation).
/// `preceded_by_key` selects the object walk (backtrack to the key) over the array walk (the value IS the member).
fn member_cut(
    source: &[u8],
    open: usize,
    token_start: usize,
    value_end: usize,
    preceded_by_key: bool,
) -> Option<EditRemoval> {
    let token_start = if preceded_by_key {
        member_key_start(source, open, token_start)?
    } else {
        token_start
    };
    // The preceding comma, if the member is not first.
    let mut c = token_start;
    while c > open && is_json_ws(source[c - 1]) {
        c -= 1;
    }
    if c > open && source.get(c - 1) == Some(&b',') {
        return Some(EditRemoval {
            start: c - 1,
            end: value_end,
            replacement: Vec::new(),
        });
    }
    // First member: the line break before it (or the opening delimiter) is the member's owned whitespace; the cut runs
    // through the FOLLOWING comma when one exists.
    let mut ls = token_start;
    while ls > open && source[ls - 1] != b'\n' {
        ls -= 1;
    }
    let start = if ls > open && source[ls - 1] == b'\n' {
        // A CRLF source keeps its `\r`: the cut begins AT the `\n`, so the authored carriage return survives ahead of
        // the splice (e.g. a first member removed from `{\r\n "a":1\r\n}` leaves `{\r}`). Valid JSON, cosmetically odd
        // — and deliberate. This lane deletes the bytes a removal owns; it never normalizes bytes the author wrote,
        // so the `\r` is preserved as source-faithful.
        ls - 1
    } else {
        open + 1
    };
    let mut e = value_end;
    while e < source.len() && is_json_ws(source[e]) {
        e += 1;
    }
    // This branch runs only when NO PRECEDING comma exists, so a closer directly after the member's own
    // comma/whitespace means the member is the container's ONLY one: its owned whitespace includes the wrap before the
    // closer, and leaving it strands a bare newline inside an empty container (`{\n}` where the law says `{}`).
    let end = if source.get(e) == Some(&b',') {
        let mut f = e + 1;
        while f < source.len() && is_json_ws(source[f]) {
            f += 1;
        }
        if matches!(source.get(f), Some(b'}' | b']')) {
            f
        } else {
            e + 1
        }
    } else if matches!(source.get(e), Some(b'}' | b']')) {
        e
    } else {
        value_end
    };
    Some(EditRemoval {
        start,
        end,
        replacement: Vec::new(),
    })
}

/// Renders the cuts for a container the edit lane SHRANK (ruling 3): each removed member's value token is cut with one
/// adjacent comma and its owned whitespace. ANY span the source does not fully cover — a span-less value, a token
/// that does not back up to its member — declines the whole removal to the floor (ruling 4).
pub(crate) fn render_edit_remove(
    document: &Document<'_>,
    container: NodeId,
    source: &[u8],
    members: EditRemoveMembers<'_>,
) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
    let Some(span) = document.node_source_span(container).map_err(data_error)? else {
        return Ok(alloc::vec::Vec::new());
    };
    let open = span.start() as usize;
    if !matches!(source.get(open), Some(b'{' | b'[')) {
        return Ok(alloc::vec::Vec::new());
    }
    let mut removals = alloc::vec::Vec::new();
    for (token_start, value_end, preceded_by_key) in member_values(document, source, members)? {
        let Some(cut) = member_cut(source, open, token_start, value_end, preceded_by_key) else {
            return Ok(alloc::vec::Vec::new());
        };
        removals.push(cut);
    }
    Ok(removals)
}

/// The removed members' authored value-token extents, in the order the removed list names them, each with whether the
/// member is an OBJECT member (backtracks to its key) or an ARRAY item. Any removed member without a fully-covered
/// value span declines the whole removal.
fn member_values(
    document: &Document<'_>,
    source: &[u8],
    members: EditRemoveMembers<'_>,
) -> Result<alloc::vec::Vec<(usize, usize, bool)>, CodecError> {
    let mut extents = alloc::vec::Vec::new();
    match members {
        EditRemoveMembers::Table(members) => {
            for (_, node) in members {
                let Some(span) = document.node_source_span(*node).map_err(data_error)? else {
                    return Ok(alloc::vec::Vec::new());
                };
                let Some((start, end)) = value_extent(source, span) else {
                    return Ok(alloc::vec::Vec::new());
                };
                extents.push((start, end, true));
            }
        }
        EditRemoveMembers::Array(items) => {
            for (_, node) in items {
                let Some(span) = document.node_source_span(*node).map_err(data_error)? else {
                    return Ok(alloc::vec::Vec::new());
                };
                let Some((start, end)) = value_extent(source, span) else {
                    return Ok(alloc::vec::Vec::new());
                };
                extents.push((start, end, false));
            }
        }
    }
    Ok(extents)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grown nested container's new member lands at the last member's indentation, not at closer-indent +
    /// member-indent: the run between the last value and the closer ends in the CLOSER's column, and only its final
    /// line break is the observed wrap.
    #[test]
    fn nested_append_separator_lands_at_member_indent() {
        let source = b"{\n  \"outer\": {\n    \"a\": 1\n  }\n}";
        // The inner container: `{` at 13, `1` ends at 24, closer `}` at 28.
        let separator = member_separator(source, 13, 25, 29, false, true);
        assert_eq!(separator, b",\n    ", "nested object append: {separator:?}");

        let source = b"[\n  1,\n  [\n    2\n  ]\n]";
        // The inner array: `[` at 7, `2` ends at 15, closer `]` at 19.
        let separator = member_separator(source, 7, 16, 20, false, true);
        assert_eq!(separator, b",\n    ", "nested array append: {separator:?}");
    }

    /// A single-quoted JSON5 string widens across its quote pair exactly like a double-quoted one: parse.rs records
    /// inner content for BOTH spellings, so the token extent must cover both quotes.
    #[test]
    fn value_extent_widens_across_a_single_quote_pair() {
        // `{a: 'x', b: 2}` — the value's inner span is `x` at [5, 6).
        let source = b"{a: 'x', b: 2}";
        assert_eq!(value_extent(source, jqf_source::Span::new(5, 6)), Some((4, 7)));
        // The double-quoted twin keeps its pinned answer.
        let source = b"{a: \"x\", b: 2}";
        assert_eq!(value_extent(source, jqf_source::Span::new(5, 6)), Some((4, 7)));
        // An escaped quote inside the body cannot confuse the pair check: the neighbors hug the recorded inner span by
        // convention.
        let source = b"['it\\'s']";
        assert_eq!(value_extent(source, jqf_source::Span::new(2, 7)), Some((1, 8)));
    }

    /// A one-char SINGLE-quoted string whose content opens a container is not a built-container anchor: the anchor
    /// inference treats both quote spellings as string evidence, so the extent widens across the pair instead of
    /// scanning a delimiter inside a string literal.
    #[test]
    fn value_extent_single_quoted_delimiter_content_is_not_an_anchor() {
        // `['[']` — the inner span is `[` at [2, 3).
        let source = b"['[']";
        assert_eq!(value_extent(source, jqf_source::Span::new(2, 3)), Some((1, 4)));
        // `['{']` — same law for the object delimiter.
        let source = b"['{']";
        assert_eq!(value_extent(source, jqf_source::Span::new(2, 3)), Some((1, 4)));
    }

    /// The key walk accepts a single-quoted JSON5 key: the rewind cursor names the closing `'`, and the escape-parity
    /// walk finds its opener.
    #[test]
    fn member_key_start_walks_back_over_a_single_quoted_key() {
        // `{'a': 1}` — the value token `1` sits at [6, 7).
        let source = b"{'a': 1}";
        assert_eq!(member_key_start(source, 0, 6), Some(1));
        // The double-quoted twin keeps its pinned answer.
        let source = b"{\"a\": 1}";
        assert_eq!(member_key_start(source, 0, 6), Some(1));
    }

    /// Root containers keep their pinned shape: a bare-newline closer run composes with the member indent exactly as
    /// before.
    #[test]
    fn root_append_separator_is_unchanged() {
        let source = b"{\n  \"a\": 1\n}";
        // `{` at 0, `1` ends at 9, closer `}` at 11 (exclusive close 12).
        let separator = member_separator(source, 0, 10, 12, false, true);
        assert_eq!(separator, b",\n  ");

        // Blank lines between the last member and the closer survive.
        let source = b"{\n  \"a\": 1\n\n\n}";
        let separator = member_separator(source, 0, 10, 14, false, true);
        assert_eq!(separator, b",\n\n\n  ");
    }
}
