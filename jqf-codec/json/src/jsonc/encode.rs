//! The commented-JSON encoder: the host crate's JSON encoder plus leading-comment re-emission and the trailing-comma
//! output bit, and the comment-aware edit splice. JSON5 encodes through the same factory and the same splice, differing
//! only in its comment fact role, its route identity, and never writing trailing commas.
//!
//! ## The splice policy
//!
//! A commented dialect implements `render_edit_append` and `render_edit_remove` — no declining — reusing the host
//! crate's splice machinery with a comment-aware container scan. The rulings:
//!
//! 1. **A grown OBJECT/ARRAY splices its new members after the last member, inside the closing delimiter**, reusing the
//!    source's own separator run — but the scan is COMMENT-AWARE: a trailing comment block after the last member is
//!    not part of the separator, so the new members land BEFORE it and the comment stays a trailer. An insertion that
//!    would orphan a comment above the following member is a wrong answer; the scan prevents it by construction.
//! 2. **A shrunk container cuts the member with one adjacent comma and its owned whitespace — extended up through the
//!    member's own leading comment block** (own-line `//` lines and blank lines): a removal takes its comments with it,
//!    so the surviving members never inherit a stranger's comment. A seam the scan cannot name (a block comment in the
//!    seam, an unterminated region) returns an empty cut set and the caller falls back to the whole-document floor —
//!    never wrong bytes, and the floor re-encodes comments from facts.
//! 3. **Every splice re-verifies by re-decode** (the SDK's law), so a splice the byte rules misclassify degrades to the
//!    floor, exactly as strict JSON's.
//!
//! ## The whole-document floor
//!
//! A container edit that takes the floor re-encodes the whole document through this encoder, which re-emits every
//! leading comment from the `jsonc.comment@1` facts — so the floor is not lossy for comments. Untouched regions of a
//! spliced file keep every byte.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{
    CodecError, CodecFailureKind, EditAppendMembers, EditInsertion, EditRemoval, EditRemoveMembers, EncodeItem,
    EncodeRequest, EncoderFactoryImpl, ErasedEncoderFactory, ErasedEncoderSession, FactEditPatch, PreservationRequest,
    RecycledSessionState,
};
use jqf_data::{Document, NodeId, Value};
use jqf_resource::ResourceContext;

use crate::ENCODE_PHYSICAL_ROUTE_ID;
use crate::byte_scan::is_json_ws;
use crate::edit::{
    build_comment_index, last_member_indent, member_key_start, member_text, render_value_bytes, value_extent,
};
use crate::encode::{JsonEncodeOptions, JsonEncoder};

use super::options::{JsoncEncodeOptions, JsoncEncodeProfile};

/// The commented-JSON encoder factory, shared by JSONC and JSON5: the host JSON encoder armed with a comment index. The
/// two dialects differ in exactly three values — the fact role their decode attached comments under, whether the
/// output writes trailing commas, and their route identity — so the rendering and the splice below are one
/// implementation.
pub(crate) struct CommentedEncoderFactory {
    style: JsonEncodeOptions,
    comment_role: &'static str,
    trailing_commas: bool,
    route: jqf_codec_core::PhysicalRouteId,
}

impl CommentedEncoderFactory {
    pub(crate) const fn new(
        style: JsonEncodeOptions,
        comment_role: &'static str,
        trailing_commas: bool,
        route: jqf_codec_core::PhysicalRouteId,
    ) -> Self {
        Self {
            style,
            comment_role,
            trailing_commas,
            route,
        }
    }

    /// Builds the comment index for a located item; owned items (and documents without attached-fact coverage) yield an
    /// empty index.
    fn comment_index(
        &self,
        item: EncodeItem<'_, '_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<BTreeMap<jqf_data::NodeHandle, Vec<String>>, CodecError> {
        match item {
            EncodeItem::Located { product, .. }
                if product
                    .document()
                    .coverage()
                    .contains(jqf_data::DocumentCapability::AttachedFacts)
                    && product.document().fact_count().map_err(crate::encode::data_error)? > 0 =>
            {
                build_comment_index(product.document(), self.comment_role, resources)
            }
            _ => Ok(BTreeMap::new()),
        }
    }
}

impl EncoderFactoryImpl for CommentedEncoderFactory {
    fn physical_encoder(&self) -> jqf_codec_core::PhysicalRouteId {
        self.route
    }

    fn emits_canonical_form(&self) -> bool {
        self.style.emits_canonical_form()
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        let comments = self.comment_index(item, resources)?;
        let encoder =
            JsonEncoder::try_new_jsonc(item, b"", b"", self.style, comments, self.trailing_commas, resources)?;
        ErasedEncoderSession::try_new(item, preservation, || Ok(encoder))
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        item: EncodeItem<'_, '_>,
        _preservation: PreservationRequest,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let facts = crate::encode::fact_preservation(item)?;
        let Some(encoder) = state.downcast_mut::<JsonEncoder>() else {
            return Ok(false);
        };
        if encoder.style != self.style {
            return Ok(false);
        }
        let comments = self.comment_index(item, resources)?;
        // Reset first: it clears the previous item's comment index, and this item's arming must survive it.
        encoder.reset(facts);
        encoder.comments = Some(comments);
        encoder.trailing_commas = self.trailing_commas;
        Ok(true)
    }

    fn render_leaf(
        &self,
        _document: &Document<'_>,
        _node: NodeId,
        _path: &[String],
        _source: &[u8],
        value: &Value,
        _authored: Option<&[u8]>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<u8>, CodecError> {
        // Both dialects' value-position grammar is JSON's: the compact canonical spelling the splice also inserts, so a
        // leaf patch and a structural member can never disagree. Leaving this to the default fallback would splice the
        // profile's pretty rendering into the middle of a line.
        render_value_bytes(value, resources)
    }

    fn render_edit_append(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditAppendMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<EditInsertion>, CodecError> {
        jsonc_render_edit_append(document, container, source, members, resources)
    }

    fn render_edit_remove(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditRemoveMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<EditRemoval>, CodecError> {
        jsonc_render_edit_remove(document, container, source, members)
    }

    fn render_fact_delta(
        &self,
        document: &Document<'_>,
        node: NodeId,
        source: &[u8],
        role: &str,
        _kind: &str,
        payload: &Value,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Option<Vec<FactEditPatch>>, CodecError> {
        // `.@comment` is the HEAD list: insert or replace `//` lines immediately before the member so the decode
        // attaches them to this node. Inline and foot positions stay declined — the decoder only exposes leading
        // comments through `.@comment`.
        if role != jqf_codec_core::comment::HEAD {
            return Ok(None);
        }
        render_comment_write(document, node, source, payload)
    }
}

/// Splices `.@comment` as `//` lines immediately before the member.
///
/// Compact `{ "a": 1 }` and pretty members share one law: the comment belongs to the first value whose span starts at
/// or after it, so the write inserts before the member (the key, or the value for an array item) rather than at the
/// start of the physical line. A line-start insert on a compact document would attach the comment to the root.
pub(crate) fn render_comment_write(
    document: &Document<'_>,
    node: NodeId,
    source: &[u8],
    payload: &Value,
) -> Result<Option<Vec<FactEditPatch>>, CodecError> {
    let Some(span) = document.node_source_span(node).map_err(crate::encode::data_error)? else {
        return Ok(None);
    };
    // The bound-source convention records a string's INNER content, so the raw span start is one byte PAST the token's
    // first byte for a string node. The key walk-back below needs the token boundary (the opening quote), or it finds
    // no `:` before the value and splices the comment INSIDE the string literal — the patched buffer then fails the
    // verification re-decode and the whole write refuses. The same adjustment `crate::edit::value_extent` applies
    // everywhere else in the edit lane applies here.
    let Some((token_start, _)) = crate::edit::value_extent(source, span) else {
        return Ok(None);
    };
    let value_start = token_start;
    if value_start > source.len() {
        return Ok(None);
    }
    let member_start = commented_member_start(source, value_start);
    let lines = comment_payload_lines(payload)?;
    let (block_start, insert_at, indent) = slash_comment_block(source, member_start);
    let replacement = slash_comment_text(indent, &lines);
    if block_start == insert_at && replacement.is_empty() {
        return Ok(Some(Vec::new()));
    }
    Ok(Some(alloc::vec![FactEditPatch {
        start: block_start,
        end: insert_at,
        replacement,
    }]))
}

fn comment_payload_lines(payload: &Value) -> Result<Vec<String>, CodecError> {
    match payload.untagged() {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => {
            let mut lines = Vec::new();
            for item in items {
                let Value::String(text) = item.untagged() else {
                    return Err(comment_payload_error());
                };
                lines.push(String::from(text.as_str()));
            }
            Ok(lines)
        }
        _ => Err(comment_payload_error()),
    }
}

fn comment_payload_error() -> CodecError {
    CodecError::new(CodecFailureKind::UnsupportedRepresentation)
}

/// The member's start in source: the object key when a `:` precedes the value, otherwise the value itself (array item
/// or root).
fn commented_member_start(source: &[u8], value_start: usize) -> usize {
    if let Some(key) = member_key_start(source, 0, value_start) {
        return key;
    }
    identifier_key_start(source, value_start).unwrap_or(value_start)
}

/// JSON5 unquoted or single-quoted key immediately before `value_start`.
fn identifier_key_start(source: &[u8], value_start: usize) -> Option<usize> {
    let close = crate::edit_support::rewind_to_key(source, value_start, 0)?;
    match source.get(close.wrapping_sub(1)) {
        Some(b'\'') => crate::edit_support::opening_quote_left(source, close, b'\'', 0),
        Some(&byte) if is_ident_continue(byte) => {
            let mut start = close - 1;
            while start > 0 && is_ident_continue(source[start - 1]) {
                start -= 1;
            }
            Some(start)
        }
        _ => None,
    }
}

const fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// The existing `//` / `/*` block directly above the member, or the insertion point when none is there. `indent` is the
/// member's own-line indent when the member starts a line; empty for a mid-line insert (compact `{` immediately before
/// the key).
fn slash_comment_block(source: &[u8], member_start: usize) -> (usize, usize, &[u8]) {
    let line = line_start_backward(source, member_start, 0);
    let prefix = &source[line..member_start];
    let at_line_start = prefix.iter().all(|&byte| is_json_ws(byte));
    let indent = if at_line_start { prefix } else { b"" };
    let insert_at = if at_line_start { line } else { member_start };
    let mut block = insert_at;
    while block > 0 {
        let prev = line_start_backward(source, block - 1, 0);
        let line_bytes = &source[prev..block];
        if line_bytes.trim_ascii().is_empty() || is_slash_comment_line(line_bytes) {
            block = prev;
        } else {
            break;
        }
    }
    (block, insert_at, indent)
}

/// The start of the line holding byte `at`, or `floor` when no line feed precedes it. The scan walks BACKWARD from `at`
/// to the nearest `\n`, so a walk-back loop over consecutive comment lines costs the run's length once, never a full
/// prefix re-scan per line.
fn line_start_backward(source: &[u8], at: usize, floor: usize) -> usize {
    let mut start = at.min(source.len());
    while start > 0 && source[start - 1] != b'\n' {
        start -= 1;
    }
    if start == 0 { floor } else { start }
}

fn is_slash_comment_line(line: &[u8]) -> bool {
    let trimmed = line.trim_ascii_start();
    trimmed.starts_with(b"//") || trimmed.starts_with(b"/*")
}

fn slash_comment_text(indent: &[u8], lines: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        // A payload text's own newlines become separate `//` lines, exactly as the whole-document floor re-emits them
        // (`write_comment_lines_before`) — the two write paths must agree on a multi-line payload.
        for segment in line.split('\n') {
            out.extend_from_slice(indent);
            out.extend_from_slice(b"//");
            if !segment.is_empty() {
                out.extend_from_slice(b" ");
                out.extend_from_slice(segment.as_bytes());
            }
            out.push(b'\n');
        }
    }
    out
}

/// Reads this codec's encoder options out of a request, falling back to the profile the request's DIALECT names when
/// the caller named none.
fn encode_options(request: EncodeRequest<'_, '_>) -> Result<JsoncEncodeOptions, CodecError> {
    let Some(options) = request.options else {
        return Ok(JsoncEncodeOptions {
            style: JsonEncodeOptions::default(),
            profile: dialect_profile(request.dialect.as_str()),
        });
    };
    options
        .downcast_ref::<JsoncEncodeOptions>()
        .copied()
        .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch))
}

/// The output profile the named dialect renders with: the trailing spellings write trailing commas; every other
/// accepted spelling (`default`, both `-jqf` identities, and the edit lane's `jqf-1.0`) keeps strict JSON's comma law.
/// A request that names a dialect without encoder options still gets bytes that dialect re-reads.
fn dialect_profile(dialect: &str) -> JsoncEncodeProfile {
    if dialect == super::TRAILING_DIALECT_ID || dialect == super::TRAILING_JQF_DIALECT_ID {
        JsoncEncodeProfile::Trailing
    } else {
        JsoncEncodeProfile::Default
    }
}

pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    // The decoder strictly checks its dialect; the factory guards the same format+dialect pair so both directions of
    // the registration decline symmetrically (an unknown dialect is a mismatch, never another profile's bytes). An
    // EXPLICIT options payload stays the override channel; absence falls back to the dialect's own law.
    request.expect_target(super::FORMAT_ID, &super::DIALECT_TEXTS)?;
    let style = encode_options(request)?;
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || {
        Ok(CommentedEncoderFactory::new(
            style.style,
            crate::parse::COMMENT_FACT,
            style.trailing_commas(),
            ENCODE_PHYSICAL_ROUTE_ID,
        ))
    })
}

// --------------------------------------------------------------------------- The comment-aware splice.
// ---------------------------------------------------------------------------

/// A JSONC-aware container scan: names the container's region and its last member's value end, skipping strings AND
/// comments (a strict-JSON scan would miscount a `{` or `"` inside a comment). Returns `(close, value_end, empty)`
/// where `value_end` is the first delimiter after the last top-level value (whitespace, comma, comment open, or close)
/// and `empty` says the container holds no value. A trailing comma after whitespace is not this delimiter —
/// [`last_member_comma`] walks from here to it. `None` when the region cannot be named (an unterminated string or
/// comment, a mismatched close) and the caller falls back to the floor.
fn jsonc_container_scan(source: &[u8], open: usize) -> Option<(usize, usize, bool)> {
    let closer = match source.get(open) {
        Some(b'{') => b'}',
        Some(b'[') => b']',
        _ => return None,
    };
    let mut depth = 0usize;
    let mut i = open;
    // One past the last non-trivia byte seen at depth 1 that closed a VALUE (a string quote, a nested container's
    // close, a bare word's end).
    let mut value_end = open;
    while i < source.len() {
        match source[i] {
            b'"' | b'\'' => {
                // Both quote spellings open a string here: JSONC proper uses only `"`, but the shared scan also names
                // regions in single-quoted sources, where a `]` or `}` inside a quoted member would otherwise truncate
                // the region at the quote's contents. An unterminated string leaves the region unnameable and the
                // caller falls back to the floor.
                let quote = source[i];
                i += 1;
                loop {
                    match source.get(i) {
                        Some(b'\\') => i = i.checked_add(2)?,
                        Some(&byte) if byte == quote => {
                            i += 1;
                            break;
                        }
                        Some(_) => i += 1,
                        None => return None,
                    }
                }
                if depth == 1 {
                    value_end = i;
                }
            }
            b'/' if source.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < source.len() && source[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if source.get(i + 1) == Some(&b'*') => {
                i += 2;
                loop {
                    match source.get(i) {
                        Some(b'*') if source.get(i + 1) == Some(&b'/') => {
                            i += 2;
                            break;
                        }
                        Some(_) => i += 1,
                        None => return None,
                    }
                }
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                i += 1;
                if depth == 0 {
                    if source.get(i - 1) != Some(&closer) {
                        return None;
                    }
                    return Some((i, value_end, value_end == open));
                }
                if depth == 1 {
                    value_end = i;
                }
            }
            // A comma at ANY depth advances: at depth 1 it separates members (the last value's end is already
            // recorded), deeper ones belong to nested containers and touch no outer bookkeeping.
            b',' => {
                i += 1;
            }
            // Whitespace and the member's key:value colon advance: neither carries a position of its own.
            byte if is_json_ws(byte) || byte == b':' => {
                i += 1;
            }
            _ => {
                // A bare word (true/false/null/number): advance to its delimiter. The value's end is the delimiter
                // position.
                let start = i;
                while i < source.len() && !delimiter_hit(source[i]) {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                if depth == 1 {
                    value_end = i;
                }
            }
        }
    }
    None
}

/// Whether `byte` ends a bare JSON word (`true`, `false`, `null`, a number): JSON whitespace, a comma or colon, any
/// container delimiter, a quote, or a comment open. Every stop is a byte the structured arms own, so the run never
/// swallows structurally significant input (a run entered from an unexpected byte stops before the next construct).
const fn delimiter_hit(byte: u8) -> bool {
    is_json_ws(byte) || matches!(byte, b',' | b':' | b'{' | b'[' | b'}' | b']' | b'"' | b'\'' | b'/')
}

/// The last member's trailing comma, or `ve` when the container has none. Walks from the last value's first delimiter
/// over whitespace and at most one comma. New members insert at that comma so it stays with the trailer (the last
/// member keeps its own trailing comma). A comment between the value and the comma is not skipped — that comma stays
/// in the suffix.
fn last_member_comma(source: &[u8], ve: usize, close: usize) -> usize {
    let end = close.saturating_sub(1);
    let mut i = ve;
    while i < end && is_json_ws(source[i]) {
        i += 1;
    }
    if source.get(i) == Some(&b',') { i } else { ve }
}

/// The position where trailing trivia after the last member begins: the first comment start after the last value and
/// its optional trailing comma, or the closing delimiter when the run to it holds none. Walks whitespace, then one
/// comma, then whitespace, then looks for `//` or `/*`. A comma after whitespace is the last member's own punctuation,
/// never a stop.
fn trailing_comment_start(source: &[u8], ve: usize, close: usize) -> usize {
    let end = close.saturating_sub(1);
    let mut i = ve;
    while i < end && is_json_ws(source[i]) {
        i += 1;
    }
    if source.get(i) == Some(&b',') {
        i += 1;
    }
    while i < end && is_json_ws(source[i]) {
        i += 1;
    }
    if i < end && source[i] == b'/' && matches!(source.get(i + 1), Some(b'/' | b'*')) {
        i
    } else {
        end
    }
}

/// The separator run a grown JSONC container reuses, comment-aware (ruling 1): the comma, then the whitespace between
/// the last member's value and the trailing comment block (or the closing delimiter), then the last member's
/// indentation. A trailing comma in the run belongs to the LAST member and is stripped; a trailing comment block is
/// never part of the separator, so the new member lands BEFORE it and the comment stays a trailer — the orphaning
/// law.
fn jsonc_member_separator(source: &[u8], open: usize, ve: usize, close: usize, empty: bool, wrapped: bool) -> Vec<u8> {
    if empty {
        return Vec::new();
    }
    let mut bytes = Vec::with_capacity(8);
    bytes.push(b',');
    if wrapped {
        let comment_start = trailing_comment_start(source, ve, close);
        let mut run = &source[ve..comment_start];
        // The trailing comma is the last member's own, not part of the separator (JSONC may hold one between the value
        // and the close).
        run = run.strip_prefix(b",").unwrap_or(run);
        if comment_start == close - 1 {
            // No trailing comment: the run is pure whitespace (the closing delimiter is never part of it). Only the run
            // through its final line break is the observed wrap — the bytes after it are the CLOSER's indentation,
            // whitespace of the parent level, and composing them with the member indent would over-indent a nested
            // container once per depth (see `member_separator`). The member indent is the separator's second half.
            bytes.extend_from_slice(crate::edit::through_final_line_break(run));
            bytes.extend_from_slice(&last_member_indent(source, open, ve));
        } else {
            // A trailing comment's run already carries its own column, so no indent is appended there.
            bytes.extend_from_slice(run);
        }
    }
    bytes
}

/// Renders the splice for a container the edit lane GREW (rulings 1-2): the new members land inside the closing
/// delimiter — after the last member's separator, reusing the observed separator run — or directly after the
/// opening delimiter for an empty container. Returns an empty insertion set (the floor) when the container's region
/// cannot be named.
pub(crate) fn jsonc_render_edit_append(
    document: &Document<'_>,
    container: NodeId,
    source: &[u8],
    members: EditAppendMembers<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<Vec<EditInsertion>, CodecError> {
    let Some(span) = document
        .node_source_span(container)
        .map_err(crate::encode::data_error)?
    else {
        return Ok(Vec::new());
    };
    let open = span.start() as usize;
    if !matches!(source.get(open), Some(b'{' | b'[')) {
        return Ok(Vec::new());
    }
    let Some((close, ve, empty)) = jsonc_container_scan(source, open) else {
        return Ok(Vec::new());
    };
    // Insert at the last member's trailing comma (or the value delimiter when there is none) so the comma stays with
    // the trailer. Wrapped still reads from the value delimiter: a newline before the comma is the file's wrap, not a
    // one-line container.
    let at = if empty {
        open + 1
    } else {
        last_member_comma(source, ve, close)
    };
    let wrapped = !empty
        && source[ve..close.saturating_sub(1)]
            .iter()
            .any(|byte| matches!(byte, b'\n' | b'\r'));
    let separator = jsonc_member_separator(source, open, at, close, empty, wrapped);
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
    Ok(alloc::vec![EditInsertion {
        at,
        bytes: text,
        replace: None,
    }])
}

/// One JSONC-aware backward trivia skip for the removal seam (ruling 2): walks `p` back over whitespace and whole
/// comment LINES, returning the new `p` (the position just after the last non-trivia byte). Each previous line whose
/// trimmed head is blank or `//` is comment/blank trivia and is skipped. A line whose trimmed tail is `*/` (a block
/// comment's close) is a seam the backward walk cannot name — `None`, and the caller falls back to the floor rather
/// than guess.
fn back_over_comment_lines(source: &[u8], mut p: usize, open: usize) -> Option<usize> {
    loop {
        while p > open && is_json_ws(source[p - 1]) {
            p -= 1;
        }
        let line_start = line_start_backward(source, p, open);
        if line_start >= p {
            return Some(p);
        }
        let line = &source[line_start..p];
        let trimmed = trim_ascii(line);
        if trimmed.is_empty() {
            p = line_start;
            continue;
        }
        if trimmed.starts_with(b"//") {
            p = line_start;
            continue;
        }
        if trimmed.ends_with(b"*/") {
            return None;
        }
        return Some(p);
    }
}

/// ASCII whitespace-trimmed view of a line.
fn trim_ascii(line: &[u8]) -> &[u8] {
    let start = line.iter().position(|byte| !is_json_ws(*byte)).unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|byte| !is_json_ws(*byte))
        .map_or(start, |position| position + 1);
    &line[start..end]
}

/// One removed member's cut (ruling 2): the member plus exactly one adjacent comma — the preceding one, or the
/// following one for the first member — plus the whitespace run the member owned (its line break, indentation, and
/// its own leading comment block). `preceded_by_key` selects the object walk (backtrack to the key) over the array walk
/// (the value IS the member).
fn jsonc_member_cut(
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
    // The preceding comma, skipping the comment lines between the member and it (a trailing-after-value comment is the
    // NEXT member's leading comment, so it leaves with this member — ruling 2).
    let c = back_over_comment_lines(source, token_start, open)?;
    if c > open && source.get(c - 1) == Some(&b',') {
        return Some(EditRemoval {
            start: c - 1,
            end: value_end,
            replacement: alloc::vec::Vec::new(),
        });
    }
    // First member: the line break before it (or the opening delimiter) is the member's owned whitespace, extended up
    // through its leading comment block; the cut runs through the FOLLOWING comma when one exists.
    let mut ls = token_start;
    while ls > open && source[ls - 1] != b'\n' {
        ls -= 1;
    }
    if let Some(block) = back_over_comment_lines(source, ls, open) {
        // Extend the owned whitespace up through the comment block: the walk returns the position just after the last
        // non-comment byte, which is the line start the block began at.
        ls = block;
    }
    let start = if ls > open && source[ls - 1] == b'\n' {
        ls - 1
    } else {
        open + 1
    };
    let mut e = value_end;
    while e < source.len() && is_json_ws(source[e]) {
        e += 1;
    }
    // The lone-member law, same as the strict twin `member_cut`: this branch runs only when no preceding comma exists,
    // so a closer directly after the member's own comma/whitespace means it is the container's ONLY member and the wrap
    // before the closer leaves with it.
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
        replacement: alloc::vec::Vec::new(),
    })
}

/// Renders the cuts for a container the edit lane SHRANK: each removed member's value token is cut with one adjacent
/// comma, its owned whitespace, and its leading comment block. ANY span the source does not fully cover — a span-less
/// value, a token that does not back up to its member, a block-comment seam — declines the whole removal to the
/// whole-document floor. The JSONC/JSON5 shared removal seam: the JSON5 encoder calls this verbatim — the shared scan
/// names both quote spellings, and a region it cannot name declines to the floor (never wrong bytes; the re-decode
/// law).
pub(crate) fn jsonc_render_edit_remove(
    document: &Document<'_>,
    container: NodeId,
    source: &[u8],
    members: EditRemoveMembers<'_>,
) -> Result<Vec<EditRemoval>, CodecError> {
    let Some(span) = document
        .node_source_span(container)
        .map_err(crate::encode::data_error)?
    else {
        return Ok(Vec::new());
    };
    let open = span.start() as usize;
    if !matches!(source.get(open), Some(b'{' | b'[')) {
        return Ok(Vec::new());
    }
    let mut removals = Vec::new();
    for (token_start, value_end, preceded_by_key) in jsonc_member_values(document, source, members)? {
        let Some(cut) = jsonc_member_cut(source, open, token_start, value_end, preceded_by_key) else {
            return Ok(Vec::new());
        };
        removals.push(cut);
    }
    Ok(removals)
}

/// The removed members' authored value-token extents, in the order the removed list names them, each with whether the
/// member is an OBJECT member (backtracks to its key) or an ARRAY item. Any removed member without a fully-covered
/// value span declines the whole removal.
fn jsonc_member_values(
    document: &Document<'_>,
    source: &[u8],
    members: EditRemoveMembers<'_>,
) -> Result<Vec<(usize, usize, bool)>, CodecError> {
    let mut extents = Vec::new();
    match members {
        EditRemoveMembers::Table(members) => {
            for (_, node) in members {
                let Some(span) = document.node_source_span(*node).map_err(crate::encode::data_error)? else {
                    return Ok(Vec::new());
                };
                let Some((start, end)) = value_extent(source, span) else {
                    return Ok(Vec::new());
                };
                extents.push((start, end, true));
            }
        }
        EditRemoveMembers::Array(items) => {
            for (_, node) in items {
                let Some(span) = document.node_source_span(*node).map_err(crate::encode::data_error)? else {
                    return Ok(Vec::new());
                };
                let Some((start, end)) = value_extent(source, span) else {
                    return Ok(Vec::new());
                };
                extents.push((start, end, false));
            }
        }
    }
    Ok(extents)
}
