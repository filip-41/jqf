//! Deterministic semantic YAML encoding (`yaml.stream-canonical@1` / `yaml.single-document@1`).
//!
//! The canonical renderer implements §4.8 exactly: UTF-8 without BOM, LF only, two-space indentation, a final LF; the
//! checked-continuation-indentation node renderer (sequence `[`/`,`/`]`, mapping `?`/`:` with `i+2`/`i+4`
//! continuation, exactly one trailing comma per item); every unwrapped core node carries exactly one explicit standard
//! tag; every scalar is double-quoted with the exact escape set; integers render minimal decimal; finite floats render
//! through `jqf.float64-ryu@1` (the engine's `format_binary64`); standard tags use the `!!` spelling, local tags keep
//! their exact `!suffix`, other URIs use `!<...>`.
//!
//! The tag-boundary law: one non-core tag layer per node with a direct String/sequence/mapping payload; a nested layer
//! or a tag directly around Null/Bool/Int/Float is unrepresentable (preflight, never a silent drop). Located block
//! encode replays authored `&name` / `*name` from document facts. Owned encode re-emits shared heap values at every
//! occurrence; a cyclic or deeply nested owned tree hits the nesting ceiling rather than looping.
//!
//! `yaml.stream-canonical@1` emits an empty byte stream for zero items and, for each item, exact concatenation
//! `---\n` + `render_document(root)` + `\n...\n`. `yaml.single-document@1` accepts exactly one item and emits
//! `render_document(root)` + `\n` with no document markers.
//!
//! A THIRD profile shares this session lifecycle: the human-readable `yaml.block@1`, which is the DEFAULT for YAML
//! output and renders in [`crate::block`] — see that module for its shape, its quoting rule and its composition with
//! the encode-projection policy. It emits `---\n` BETWEEN documents, no `...` terminator, and no trailing LF of its own
//! (the publication facade terminates each item). The profile is derived from the requested DIALECT rather than from
//! the options, so a named dialect always emits its own bytes.
//!
//! # The edit splice policy
//!
//! [`EncoderFactoryImpl::render_edit_append`] is the `--edit` structural seam, exactly as for TOML: a program that GREW
//! a container has no authored span to patch, so this codec renders the addition in YAML's local syntax at a position
//! it names, and the SDK splices it into the retained source and re-verifies by re-decode. The rulings:
//!
//! 1. **Spliced blocks adopt the splice site's indentation.** A new member of a BLOCK mapping renders `key: value` at
//!    the container's own indentation (the column of its first entry), after the line containing the last member's
//!    whole subtree; a new item of a BLOCK sequence renders `- item` at the container's dash column. A nested container
//!    value renders as a deeper block at indent+step, where **step is the FILE's own indent step** — the smallest
//!    positive indent delta between consecutive content lines of the retained source, defaulting to 2 (the block
//!    profile's shape) when the file has none. A spliced block therefore matches the file it lands in: a 4-space file
//!    grows 4-space blocks, a file with no consistent indent keeps the two-space default. The whole-document floor's
//!    step is always 2.
//! 2. **Flow collections stay flow.** A new member of `{...}` renders inside the closing `}` (`{ a: 1, b: 2 }` grows to
//!    `{ a: 1, b: 2, c: 3 }`); a new item of `[...]` splices before the closing `]`. The container is flow when its
//!    closing byte is `}`/`]` (the span convention: a flow collection's authored span ends at its opening delimiter).
//! 3. **A block-scalar edit replaces the whole scalar span.** The decoder binds a literal/folded scalar's span as the
//!    entire `|`/`>` block; the leaf seam patches that whole region when the value changes (its indentation and header
//!    are structure, not content), and unchanged block scalars survive verbatim.
//! 4. **Editing THROUGH an alias refuses with prose** (the document-level law): the decoder shares ONE document node
//!    across an anchor and every alias that references it, so a value write through either path would patch the
//!    anchor's authored span and silently change every other alias site. Alias-referenced nodes carry an `edit-refusal`
//!    attached fact (payload = the prose message); the SDK edit diff reads the role by identity and raises the message
//!    — never a patch, never a silent whole-document re-encode.
//! 5. **Multi-document sources ride the lane's existing adjacent-value drive**, exactly as strict JSON's adjacent texts
//!    do: one document per poll, each spliced and verified against its own retained segment.
//! 6. **The edited scalar keeps its authored style**. The SDK hands [`EncoderFactoryImpl::render_leaf`] the retained
//!    source bytes of the patch site; this codec classifies the style (plain / single-quoted / double-quoted / block)
//!    and renders the NEW value in it — a string in a single-quoted site stays single-quoted, in a plain site stays
//!    plain, everywhere else double-quoted. A plain render is emitted only when the new text re-resolves to the same
//!    string (the [`crate::block::plain_admits`] round-trip law): a value the plain spelling would re-decode as another
//!    kind (`true`, `123`) is double-quoted, because the patched bytes must re-decode to the program's value or the
//!    whole-document verification floors — a silent style change on a value the style cannot carry is worse than a
//!    visible quote change. Numbers, booleans and null stay plain. Block scalars keep ruling 3 (whole-span replace) and
//!    are out of this row.
//! 7. **A write to a MERGE-INHERITED key is a LOCAL OVERRIDE**. A YAML 1.1 `<<: *anchor` mapping splices the anchored
//!    mapping's entries into the host by reusing the source node ids, so a member like `.svc_a.timeout` is
//!    indistinguishable from a host member by its node alone — and patching it would rewrite the anchor's bytes and
//!    silently change every other merge site. The decoder records the merge provenance (the `merge-override` fact on
//!    each merged member's value node, payload = the host mapping's document node id) and the SDK's edit diff splices
//!    the WHOLE new member (`key: value`) into the HOST mapping at the host's own indentation, after the host's last
//!    member, via the ordinary append seam (ruling 1's placement and step). The `<<:` line, the anchor, and every other
//!    merge site stay byte-identical; a re-decode of the spliced source has the host's own member win the duplicate-key
//!    law, so the override is exactly the one site the user named. A write that reaches the same node THROUGH the
//!    anchor (`.defaults.timeout`) sees a payload naming a different container and stays under ruling 4's refusal,
//!    unchanged. A merged CONTAINER member whose write is a deep change (`.svc_a.config.db`) splices the whole new
//!    `config:` member — the override is per merge-inherited KEY, never a partial rewrite of the merged subtree. A host
//!    that is itself alias-shared stays under ruling 4 (no unambiguous span to append into).
//!
//! The anchors are the document's own authored spans: a flow collection's region, a block collection's
//! first-entry-to-last-subtree region, and the leaf spans of scalars. A splice the codec cannot place — a span-less
//! build, a corner the byte rules misclassify — returns an empty insertion set and the SDK falls back to the
//! whole-document floor; the re-decode verification makes any wrong splice degrade the same way, never corrupt bytes.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::str::from_utf8;

use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EditAppendMembers, EditInsertion, EditRemoval,
    EditRemoveMembers, EditRenameMembers, EditReplacement, EncodeItem, EncodeRequest, EncoderFactoryImpl,
    EncoderSession, ErasedEncoderFactory, ErasedEncoderSession, NativeSpellings, PreservationReport,
    PreservationRequest, RecycledSessionState, TrackedProjectionSink, classify_scalar, line_statement_cut,
};
use jqf_data::{Document, NodeId, Value, ValueView};
use jqf_resource::{ResourceContext, WorkAdmission};

use crate::options::{YamlProfile, YamlTargetSchema};

const OFFER_BYTES: usize = 16 * 1024;

/// The stable identity of the canonical YAML encoder factory.
pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    // Validate the target: one of the two canonical output profiles, or the block profile. The profile is derived from
    // the DIALECT rather than the options so a named dialect always emits its own bytes.
    if request.format.as_str() != crate::FORMAT_ID {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let profile = match request.dialect.as_str() {
        crate::YAML_STREAM_CANONICAL_DIALECT_ID => YamlProfile::StreamCanonical,
        crate::YAML_SINGLE_DOCUMENT_DIALECT_ID => YamlProfile::SingleDocument,
        crate::YAML_BLOCK_DIALECT_ID | crate::YAML_JQF_1_0_DIALECT_ID => YamlProfile::Block,
        _ => return Err(CodecError::new(CodecFailureKind::RequirementMismatch)),
    };
    // The target schema comes from the normalized options.
    let target_schema = YamlTargetSchema::from_request_options(request.options)?;
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || {
        Ok(YamlEncoderFactory {
            target_schema,
            profile,
            document_emitted: Cell::new(false),
        })
    })
}

struct YamlEncoderFactory {
    target_schema: YamlTargetSchema,
    profile: YamlProfile,
    /// Whether a document has already been emitted through this factory.
    ///
    /// The block profile writes `---` BETWEEN documents, not before each one, so the separator is a property of the
    /// STREAM rather than of any single item — and the factory is the only thing a stream's items share. The two
    /// canonical profiles ignore it: each of their documents opens the same way whether it is first or fiftieth.
    document_emitted: Cell<bool>,
}

impl EncoderFactoryImpl for YamlEncoderFactory {
    fn physical_encoder(&self) -> jqf_codec_core::PhysicalRouteId {
        crate::ENCODE_PHYSICAL_ROUTE_ID
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        _preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        let leading_separator = self.document_emitted.replace(true);
        ErasedEncoderSession::try_new(item, PreservationRequest::None, || {
            Ok(YamlEncoder {
                bytes: Vec::new(),
                target_schema: self.target_schema,
                profile: self.profile,
                leading_separator,
                root_done: false,
                state: EncodeState::Active,
            })
        })
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        _item: EncodeItem<'_, '_>,
        _preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let Some(encoder) = state.downcast_mut::<YamlEncoder>() else {
            return Ok(false);
        };
        // A recycled encoder must carry the stream-position fact a fresh `start` would have computed: the block profile
        // writes `---` BETWEEN documents, so the first recycled item after a completed one opens with the separator
        // exactly as a fresh start after a completed item would. The factory's `document_emitted` cell is the same
        // mutation a fresh `start` performs.
        encoder.reset(self.document_emitted.replace(true));
        Ok(true)
    }

    fn render_leaf(
        &self,
        _document: &Document<'_>,
        _node: NodeId,
        _path: &[String],
        _source: &[u8],
        value: &Value,
        authored: Option<&[u8]>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<u8>, CodecError> {
        // A leaf at a block-mapping value site: a patch replaces the scalar's authored span with these exact bytes.
        // Scalars YAML resolves by their own text render PLAIN (numbers, booleans, null), so an untouched site stays in
        // the same style. A string keeps its authored style when the new text is safe in it (ruling 6): a single-quoted
        // site renders single-quoted, a plain site renders plain only when the text re-resolves to the same string, and
        // every other site — double-quoted, a block scalar (ruling 3), or no authored span at all — renders
        // double-quoted through the §4.8 escape pass so arbitrary text can never be misparsed as another kind. The
        // patch replaces a scalar's own span, so the site's indentation and key survive.
        let mut bytes = Vec::new();
        match value {
            Value::String(text) => match authored_style(authored) {
                AuthoredStyle::Single if crate::block::single_quoted_admits(text) => {
                    bytes.push(b'\'');
                    for ch in text.chars() {
                        if ch == '\'' {
                            bytes.push(b'\'');
                        }
                        let mut buffer = [0u8; 4];
                        bytes.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                    }
                    bytes.push(b'\'');
                }
                AuthoredStyle::Plain if crate::block::plain_admits(text, crate::block::Resolution::Resolved) => {
                    bytes.extend_from_slice(text.as_bytes());
                }
                _ => {
                    bytes.extend_from_slice(b"\"");
                    write_escaped(&mut bytes, text.as_str());
                    bytes.extend_from_slice(b"\"");
                }
            },
            Value::Number(number) => {
                let text = number_text(number).ok_or_else(unrepresentable)?;
                bytes.extend_from_slice(text.as_bytes());
            }
            Value::Bool(true) => bytes.extend_from_slice(b"true"),
            Value::Bool(false) => bytes.extend_from_slice(b"false"),
            Value::Null => bytes.extend_from_slice(b"null"),
            // The diff never reaches a container leaf (a kind change is a structural decline, and a same-kind container
            // recurses to its members), so containers decline here defensively.
            _ => return Err(unrepresentable()),
        }
        Ok(bytes.as_slice().to_vec())
    }

    fn render_edit_append(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditAppendMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
        let node = document.node_handle(container).map_err(map_data)?;
        let view = document.value_view(node).map_err(map_data)?;
        // The container's retained span names its region (a flow collection's opening delimiter, a block collection's
        // first entry to last-member subtree). A span-less container cannot be placed; the caller falls back to the
        // whole-document floor.
        let Some(span) = document.node_source_span(container).map_err(map_data)? else {
            return Ok(alloc::vec::Vec::new());
        };
        if is_flow_collection(source, span) {
            return render_flow_append(self, view, span, source, members, resources);
        }
        match members {
            EditAppendMembers::Table(members) => render_block_mapping_append(view, span, source, members, resources),
            EditAppendMembers::Array(items) => render_block_sequence_append(view, span, source, items, resources),
        }
    }

    fn render_edit_remove(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditRemoveMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
        // A BLOCK collection's members each own whole lines, so a removal is the line cut. A FLOW collection's members
        // are comma-separated inside one delimiter pair: removing one is punctuation surgery this policy does not name,
        // so it declines to the whole-document floor.
        let Some(span) = document.node_source_span(container).map_err(map_data)? else {
            return Ok(alloc::vec::Vec::new());
        };
        if is_flow_collection(source, span) {
            return Ok(alloc::vec::Vec::new());
        }
        render_block_removals(document, source, members)
    }

    fn render_edit_rename(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditRenameMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditReplacement>, CodecError> {
        // A FLOW collection's members are comma-separated inside one delimiter pair: naming a member's key token needs
        // the comma walk this policy does not implement, so it declines to the whole-document floor, exactly as the
        // remove seam does. A BLOCK mapping's key token is otherwise found from its value's authored span: `key: value`
        // on one line.
        let Some(span) = document.node_source_span(container).map_err(map_data)? else {
            return Ok(alloc::vec::Vec::new());
        };
        if is_flow_collection(source, span) {
            return Ok(alloc::vec::Vec::new());
        }
        let node = document.node_handle(container).map_err(map_data)?;
        let view = document.value_view(node).map_err(map_data)?;
        let Some(object) = view.object().map_err(map_data)? else {
            return Ok(alloc::vec::Vec::new());
        };
        let EditRenameMembers(pairs) = members;
        let mut replacements = alloc::vec::Vec::new();
        for (old, new) in pairs {
            // The member's VALUE node anchors the statement: walk back from its authored span to the key token. A value
            // without a retained span (a span-less build), or a statement whose walk cannot name the key token (a
            // nested container value on its own lines, an anchored value), declines the whole rename.
            let Some(value) = object.get(old) else {
                return Ok(alloc::vec::Vec::new());
            };
            let Some(vspan) = document.node_source_span(value.node()).map_err(map_data)? else {
                return Ok(alloc::vec::Vec::new());
            };
            let Some((start, end)) = yaml_key_span(source, vspan.start() as usize) else {
                return Ok(alloc::vec::Vec::new());
            };
            let Some(bytes) = yaml_rename_bytes(&source[start..end], old, new) else {
                return Ok(alloc::vec::Vec::new());
            };
            replacements.push(EditReplacement {
                at: start,
                region_len: end - start,
                bytes,
            });
        }
        Ok(replacements)
    }
    fn render_fact_delta(
        &self,
        document: &Document<'_>,
        node: NodeId,
        source: &[u8],
        role: &str,
        _kind: &str,
        payload: &Value,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<alloc::vec::Vec<jqf_codec_core::FactEditPatch>>, CodecError> {
        // The four METADATA roles are this codec's own write surface (`.@style`, `.@tag`, `.@anchor`, `.@alias`). Any
        // other role — the comment positions, the markup attribute — stays with the seam's shared handling.
        if !matches!(role, "style" | "tag" | "anchor" | "alias") {
            return Ok(None);
        }
        let Some(span) = document.node_source_span(node).map_err(map_data)? else {
            return Err(unrepresentable());
        };
        let start = span.start() as usize;
        let end = span.end() as usize;
        let handle = document.node_handle(node).map_err(map_data)?;
        // The authored span convention: a YAML QUOTED scalar's span is its INNER content (between the quotes, the same
        // convention the leaf splice's unchanged test reads), while a PLAIN scalar's span is the complete value token.
        // A style or alias write replaces the WHOLE token (the quotes are part of the value's spelling), and a property
        // insert goes BEFORE the opening quote, so the full token region is computed here once.
        let (full_start, full_end) = full_scalar_span(source, start, end);
        // A PLAIN scalar's first byte can never be a property marker (`&`/ `!` would make it a property, not a scalar),
        // so a plain span starting with one names an existing property to collide with. A QUOTED span is inner content
        // and carries no property. An ALIAS-REFERENCED node (an anchor or an alias site) never reaches this seam — the
        // SDK refuses it on the `edit-refusal` fact before any patch — so the anchor collision is defense-in-depth; the
        // tag collision on a plain value is the real check (a quoted scalar with an explicit tag is refused later by
        // the verify's re-decode, whose failure is the codec's own prose).
        let plain_token = full_start == start;
        let first = plain_token.then(|| source.get(start)).flatten().copied();
        let value = document.materialize_node(handle, resources).map_err(map_data)?;
        match role {
            // `.@style`: re-render the scalar's text in the requested style. A style is a STRING-rendering fact, so a
            // non-string value is a refusal (a number has no quoting style that preserves its value). An EXPLICIT style
            // request that the text cannot carry refuses with prose (the plain spelling of a text that would re-resolve
            // to another kind, a single-quoted spelling with a control character) — the implicit C1 preservation law's
            // double-quote fallback applies to the leaf splice, not to a user who asked for a style by name. `literal`
            // refuses when the text cannot be a literal block or the site is inside a flow collection; `folded` is not
            // emitted by this encoder and refuses loudly rather than silently dropping the request.
            "style" => {
                let Value::String(text) = payload.untagged() else {
                    return Err(fact_refusal(
                        "a .@style write needs a string payload naming one of plain, single, \
                         double, or literal",
                    ));
                };
                let Value::String(value_text) = value.untagged() else {
                    return Err(fact_refusal(
                        "a .@style write applies to a string scalar; this node is not one",
                    ));
                };
                let replacement = match text.as_str() {
                    "plain" => {
                        if crate::block::plain_admits(value_text.as_str(), crate::block::Resolution::Resolved) {
                            value_text.as_str().as_bytes().to_vec()
                        } else {
                            return Err(fact_refusal(
                                "the text cannot be spelled plain (it would re-resolve to \
                                 another kind); use double or single",
                            ));
                        }
                    }
                    "single" => {
                        if crate::block::single_quoted_admits(value_text.as_str()) {
                            single_quoted(value_text.as_str())
                        } else {
                            return Err(fact_refusal(
                                "the text carries a control character and cannot be spelled \
                                 single-quoted; use double",
                            ));
                        }
                    }
                    "double" => double_quoted(value_text.as_str()),
                    "literal" => {
                        if source[..start]
                            .iter()
                            .rev()
                            .take_while(|byte| **byte != b'\n')
                            .any(|byte| matches!(byte, b'[' | b'{'))
                        {
                            return Err(fact_refusal(
                                "a literal block scalar cannot appear inside a flow collection; \
                                 use double, single, or plain",
                            ));
                        }
                        literal_block(
                            value_text.as_str(),
                            line_indent_width(source, start) + file_indent_step(source),
                        )
                        .ok_or_else(|| {
                            fact_refusal(
                                "the text cannot be spelled as a literal block scalar (a line \
                                 carries trailing whitespace or a control character); use double",
                            )
                        })?
                    }
                    other => {
                        return Err(fact_refusal(&format!(
                            "the yaml encoder cannot honor the .@style write \"{other}\" (it \
                             emits plain, single, double, and literal; folded block scalars are \
                             not emitted)"
                        )));
                    }
                };
                Ok(Some(alloc::vec![jqf_codec_core::FactEditPatch {
                    start: full_start,
                    end: full_end,
                    replacement,
                }]))
            }
            // `.@tag`: insert the tag spelling before the value. A tag write must satisfy the encode-or-report-a-loss
            // law: only a STRING scalar is served — a non-core tag around a non-string scalar is unrepresentable on
            // decode, so writing one would produce bytes that cannot round-trip, and a core tag whose category
            // mismatches the value would re-type it, which the edit lane's value-identity law forbids. A node that
            // already carries an explicit tag refuses (two tags are not spellable).
            "tag" => {
                let Value::String(text) = payload.untagged() else {
                    return Err(fact_refusal(
                        "a .@tag write needs a string payload naming the tag spelling",
                    ));
                };
                if !crate::tag::valid_spelling(text.as_str()) {
                    return Err(fact_refusal(&format!(
                        "the tag spelling \"{}\" is not a valid YAML tag",
                        text.as_str()
                    )));
                }
                if matches!(first, Some(b'!')) {
                    return Err(fact_refusal(
                        "the node already carries an explicit tag; a second tag is not spellable",
                    ));
                }
                if !tag_serves_value(text.as_str(), value.untagged()) {
                    return Err(fact_refusal(
                        "a .@tag write serves a string scalar (any tag) or a scalar whose kind \
                         matches a core tag (!!int/!!float on a number, !!bool on a boolean, \
                         !!null on null); this tag cannot honor this value without re-typing it",
                    ));
                }
                let mut replacement = crate::tag::emit_spelling(text.as_str()).into_bytes();
                replacement.push(b' ');
                Ok(Some(alloc::vec![jqf_codec_core::FactEditPatch {
                    start: full_start,
                    end: full_start,
                    replacement,
                }]))
            }
            // `.@anchor`: insert `&name ` before the value. A node that already carries an anchor refuses (two anchors
            // are not spellable).
            "anchor" => {
                let Value::String(text) = payload.untagged() else {
                    return Err(fact_refusal(
                        "a .@anchor write needs a string payload naming the anchor",
                    ));
                };
                if !valid_anchor_name(text.as_str()) {
                    return Err(fact_refusal(&format!(
                        "\"{}\" is not a valid YAML anchor name (no whitespace, flow \
                         indicators, or control characters, and not empty)",
                        text.as_str()
                    )));
                }
                if matches!(first, Some(b'&')) {
                    return Err(fact_refusal(
                        "the node already carries an anchor; a second anchor is not spellable",
                    ));
                }
                let mut replacement = b"&".to_vec();
                replacement.extend_from_slice(text.as_str().as_bytes());
                replacement.push(b' ');
                Ok(Some(alloc::vec![jqf_codec_core::FactEditPatch {
                    start: full_start,
                    end: full_start,
                    replacement,
                }]))
            }
            // `.@alias`: replace the value with `*name`. An anchor that already sits on the node refuses (an alias
            // cannot carry an anchor). The anchor NAME must exist somewhere in the document and its value must equal
            // this node's — the verify re-decode enforces the equality (a mismatch is a prose refusal there), and an
            // undefined name fails the re-decode with the codec's own alias error, so both wrong shapes are loud, never
            // silent.
            "alias" => {
                let Value::String(text) = payload.untagged() else {
                    return Err(fact_refusal("a .@alias write needs a string payload naming the anchor"));
                };
                if !valid_anchor_name(text.as_str()) {
                    return Err(fact_refusal(&format!(
                        "\"{}\" is not a valid YAML anchor name (no whitespace, flow \
                         indicators, or control characters, and not empty)",
                        text.as_str()
                    )));
                }
                if matches!(first, Some(b'&')) {
                    return Err(fact_refusal(
                        "the node already carries an anchor; an alias site cannot also be \
                         anchored",
                    ));
                }
                let mut replacement = b"*".to_vec();
                replacement.extend_from_slice(text.as_str().as_bytes());
                Ok(Some(alloc::vec![jqf_codec_core::FactEditPatch {
                    start: full_start,
                    end: full_end,
                    replacement,
                }]))
            }
            _ => Ok(None),
        }
    }
}

/// The authored key token's byte region of the block-mapping member whose VALUE starts at `value_start`: from the
/// line's first significant byte (after indentation and a block-sequence item's `- ` / `? ` marker) to the byte before
/// the `:` separator, skipping the value's own closing quote (the quoted-scalar span convention is INNER content) and
/// the separator's spacing. Returns `None` when the walk cannot name the key token — a nested container value on its
/// own lines, or an anchored/tagged value, whose walk does not reach the `:` on the key's line.
fn yaml_key_span(source: &[u8], value_start: usize) -> Option<(usize, usize)> {
    let mut at = value_start;
    while at > 0 && matches!(source[at - 1], b' ' | b'\t' | b'"' | b'\'') {
        at -= 1;
    }
    if at == 0 || source[at - 1] != b':' {
        return None;
    }
    at -= 1;
    while at > 0 && matches!(source[at - 1], b' ' | b'\t') {
        at -= 1;
    }
    let key_end = at;
    while at > 0 && source[at - 1] != b'\n' {
        at -= 1;
    }
    let mut start = at;
    while start < key_end && matches!(source[start], b' ' | b'\t') {
        start += 1;
    }
    // A mapping under a block-sequence item (`- name: x`) or an explicit-key indicator (`? key : value`) puts the
    // marker before the key token.
    if start < key_end && matches!(source[start], b'-' | b'?') {
        start += 1;
        while start < key_end && matches!(source[start], b' ' | b'\t') {
            start += 1;
        }
    }
    (start < key_end).then_some((start, key_end))
}

/// The replacement bytes for one renamed YAML key: the new key rendered in the OLD key token's own spelling when it
/// still fits (plain stays plain, single-quoted stays single-quoted, double-quoted stays double-quoted), and at ANY
/// byte length — the same-length half needs no move and the caller applies it as an in-place overwrite; a
/// DIFFERENT-length half splices the region at the new length and shifts the following bytes, keeping the entry (and
/// its comments, which follow the key) in place. A plain key whose new text no longer re-resolves to a string renders
/// single-quoted, or double-quoted through the §4.8 escape pass when the text cannot carry single-quoted. `None`
/// declines the rename: an escaped old key (the walk cannot verify the pairing), a complex key, or a region that is not
/// the old key's token at all.
fn yaml_rename_bytes(region: &[u8], old: &str, new: &str) -> Option<Vec<u8>> {
    if region.len() >= 2 && region[0] == b'\'' && region[region.len() - 1] == b'\'' {
        // A single-quoted key: `''` is the one escape. The new key's `'` doubles; the render is any length.
        let inner = &region[1..region.len() - 1];
        if from_utf8(inner).ok()?.replace("''", "'") != old {
            return None;
        }
        if !crate::block::single_quoted_admits(new) {
            return None;
        }
        let mut bytes = Vec::with_capacity(new.len() + 2);
        bytes.push(b'\'');
        for ch in new.chars() {
            if ch == '\'' {
                bytes.push(b'\'');
            }
            let mut buffer = [0u8; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
        }
        bytes.push(b'\'');
        Some(bytes)
    } else if region.len() >= 2 && region[0] == b'"' && region[region.len() - 1] == b'"' {
        // A double-quoted key: the region's inner text must BE the old key (an escaped spelling declines); the new key
        // renders double-quoted through the §4.8 escape pass, at any length.
        let inner = &region[1..region.len() - 1];
        if inner != old.as_bytes() {
            return None;
        }
        let mut bytes = Vec::with_capacity(new.len() + 2);
        bytes.push(b'"');
        write_escaped(&mut bytes, new);
        bytes.push(b'"');
        Some(bytes)
    } else {
        // A plain key: the region IS the old key. The new key stays plain only when it re-resolves to the same string
        // (a plain key that resolves to a boolean or number would change the mapping's semantics); otherwise it renders
        // single-quoted, or double-quoted through the escape pass when the text cannot carry single-quoted.
        if region != old.as_bytes() {
            return None;
        }
        if crate::block::plain_admits(new, crate::block::Resolution::Resolved) {
            Some(new.as_bytes().to_vec())
        } else if crate::block::single_quoted_admits(new) {
            let mut bytes = Vec::with_capacity(new.len() + 2);
            bytes.push(b'\'');
            for ch in new.chars() {
                if ch == '\'' {
                    bytes.push(b'\'');
                }
                let mut buffer = [0u8; 4];
                bytes.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
            }
            bytes.push(b'\'');
            Some(bytes)
        } else {
            let mut bytes = Vec::with_capacity(new.len() + 2);
            bytes.push(b'"');
            write_escaped(&mut bytes, new);
            bytes.push(b'"');
            Some(bytes)
        }
    }
}

/// Every removed member's cut, or a decline if ANY of them has no retained span or ends on a line the cut cannot claim
/// whole. A partial removal set would publish a document missing only some of the deleted members, so the policy is
/// all-or-nothing.
fn render_block_removals(
    document: &Document<'_>,
    source: &[u8],
    members: EditRemoveMembers<'_>,
) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
    let mut removals = alloc::vec::Vec::new();
    match members {
        EditRemoveMembers::Table(members) => {
            for (key, node) in members {
                let Some(span) = document.node_source_span(*node).map_err(map_data)? else {
                    return Ok(alloc::vec::Vec::new());
                };
                let Some(removal) = block_member_cut(source, span, key) else {
                    return Ok(alloc::vec::Vec::new());
                };
                removals.push(removal);
            }
        }
        EditRemoveMembers::Array(items) => {
            for (_, node) in items {
                let Some(span) = document.node_source_span(*node).map_err(map_data)? else {
                    return Ok(alloc::vec::Vec::new());
                };
                let Some(removal) = line_statement_cut(source, span) else {
                    return Ok(alloc::vec::Vec::new());
                };
                removals.push(removal);
            }
        }
    }
    Ok(removals)
}

/// A block-mapping member's whole removal: the KEY line (with its leading comment block) through the end of the value
/// subtree's last line. The member's VALUE node span starts at the value's first ENTRY, not at the key, so a line cut
/// over it would leave the key line behind and the patched bytes would fail re-verification — The key line is found by
/// scanning UP from the value's first line: the first non-comment, non-blank line whose text is `KEY:`. A scalar or
/// flow value sits on the key's OWN line, so when no `KEY:` line sits above the value the whole-line cut
/// (`line_statement_cut`) is the right shape and this helper defers to it. An unmatched key (a complex or
/// quoted-with-escapes spelling the scan cannot name) declines through that same fallback.
fn block_member_cut(source: &[u8], span: jqf_source::Span, key: &str) -> Option<EditRemoval> {
    let value_start = span.start() as usize;
    let first_line = yaml_line_start(source, value_start);
    // Scan UP from the line above the value's first line. A scalar or flow value is on the key's own line, so the scan
    // finds no matching key line above and falls back to the whole-line cut.
    let mut cursor = yaml_line_start(source, first_line.saturating_sub(1));
    let key_line = loop {
        let line_end = source[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(source.len(), |position| cursor + position);
        let trimmed = source[cursor..line_end].trim_ascii();
        if trimmed.is_empty() || trimmed.starts_with(b"#") {
            if cursor == 0 {
                return line_statement_cut(source, span);
            }
            cursor = yaml_line_start(source, cursor - 1);
            continue;
        }
        if is_key_line(trimmed, key) {
            break cursor;
        }
        return line_statement_cut(source, span);
    };
    // Walk up from the key line over its own comment/blank block, the same law the line cut's start keeps.
    let mut start = key_line;
    while start > 0 {
        let previous = yaml_line_start(source, start - 1);
        let line = &source[previous..start];
        if line.trim_ascii().is_empty() || line.trim_ascii_start().starts_with(b"#") {
            start = previous;
        } else {
            break;
        }
    }
    // The value subtree's last line, newline included; a trailing comment on the last entry is inside the removed
    // member and leaves with it.
    let value_end = span.end() as usize;
    let tail = source[value_end..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(source.len(), |position| value_end + position);
    let end = if tail < source.len() { tail + 1 } else { tail };
    Some(EditRemoval {
        start,
        end,
        replacement: alloc::vec::Vec::new(),
    })
}

/// Whether a trimmed line is this member's `KEY:` line: the text before the final `:` (with a trailing comment
/// stripped, and a matching quote pair unwrapped) equals the member's key. Anything the scan cannot name — a complex or
/// quoted-with-escapes key — is not a match.
fn is_key_line(trimmed: &[u8], key: &str) -> bool {
    let mut text = trimmed;
    if let Some(comment) = text.windows(2).position(|pair| pair == b" #") {
        text = &text[..comment];
    }
    let Some(prefix) = text.strip_suffix(b":") else {
        return false;
    };
    let prefix = prefix.trim_ascii();
    let prefix =
        if prefix.len() >= 2 && matches!(prefix.first(), Some(b'\'' | b'\"')) && prefix.first() == prefix.last() {
            &prefix[1..prefix.len() - 1]
        } else {
            prefix
        };
    prefix == key.as_bytes()
}

/// The start of the line containing `at`.
fn yaml_line_start(source: &[u8], at: usize) -> usize {
    source[..at]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EncodeState {
    Active,
    InputFinished,
}

struct YamlEncoder {
    bytes: Vec<u8>,
    target_schema: YamlTargetSchema,
    profile: YamlProfile,
    /// Whether this item follows another one in the same block stream, and so opens with a `---` separator.
    leading_separator: bool,
    root_done: bool,
    state: EncodeState,
}

impl YamlEncoder {
    /// Reinitializes one recycled encoder for one more ordered item: drops every byte and flag a previous item may have
    /// left behind — including one that aborted mid-offer, whose partial staging must never reach the next item —
    /// leaving exactly the state a fresh [`EncoderFactoryImpl::start`] would have produced.
    fn reset(&mut self, leading_separator: bool) {
        self.bytes.clear();
        self.state = EncodeState::Active;
        self.root_done = false;
        self.leading_separator = leading_separator;
    }

    /// Whether the target schema admits a value category (failsafe admits only String/sequence/mapping core nodes).
    fn schema_admits(&self, kind: jqf_data::ValueKind) -> bool {
        match self.target_schema {
            YamlTargetSchema::Failsafe => matches!(
                kind,
                jqf_data::ValueKind::String | jqf_data::ValueKind::Array | jqf_data::ValueKind::Object
            ),
            YamlTargetSchema::Json | YamlTargetSchema::Core => true,
        }
    }
}

/// YAML 1.2's core schema has no timestamp and no binary type, so both project.
///
/// The one thing YAML DOES spell natively is the tag itself, which is why no tag ever reaches the projection layer
/// here: `!money 12.5` round-trips as `!money 12.5`. `!!binary` exists in YAML, but this codec decodes it as a non-core
/// TAGGED value rather than as `Bytes`, so emitting bytes as `!!binary` would not round-trip to what was read —
/// base64url text is the honest spelling until a `!!binary` decode path exists.
const YAML_NATIVE: NativeSpellings = NativeSpellings::NONE;

impl YamlEncoder {
    /// Writes one scalar YAML has no native spelling for as a canonical `!!str` scalar, through the shared projection
    /// layer.
    fn push_projected_scalar(
        &mut self,
        scalar: &jqf_data::ScalarView<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let projection = classify_scalar(scalar, YAML_NATIVE, resources).ok_or_else(unrepresentable)?;
        // The canonical dialects spell every core node with one explicit standard tag, and a projection IS a string, so
        // it carries `!!str` exactly as an ordinary string does.
        self.push(b"!!str ");
        self.push(b"\"");
        // Projected text is escape-free by the sink's contract, so it goes between the quotes without the §4.8 escape
        // pass.
        projection.write(&mut TrackedProjectionSink::new(&mut self.bytes), resources)?;
        self.push(b"\"");
        Ok(())
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Encodes one item into the staging buffer.
    fn encode_item(&mut self, item: EncodeItem<'_, '_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        match item {
            EncodeItem::Owned(value) => self.encode_owned_item(value, resources),
            EncodeItem::Located { product, node } => {
                let view = product.document().value_view(node).map_err(map_data)?;
                self.encode_located_item(product, &view, resources)
            }
        }
    }

    fn encode_owned_item(&mut self, value: &Value, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        // Preflight: the tag-boundary law and the profile cardinality.
        let root = value;
        match self.profile {
            YamlProfile::StreamCanonical => {
                self.push(b"---\n");
                self.render_owned_node(root, 0, resources)?;
                self.push(b"\n...\n");
            }
            YamlProfile::SingleDocument => {
                self.render_owned_node(root, 0, resources)?;
                self.push(b"\n");
            }
            YamlProfile::Block => {
                self.push_block_separator();
                crate::block::render_owned_document(&mut self.bytes, root, resources)?;
            }
        }
        Ok(())
    }

    fn encode_located_item(
        &mut self,
        product: &jqf_codec_core::DocumentProduct<'_>,
        view: &ValueView<'_, '_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        match self.profile {
            YamlProfile::StreamCanonical => {
                self.push(b"---\n");
                self.render_located_node(product, view, 0, resources)?;
                self.push(b"\n...\n");
            }
            YamlProfile::SingleDocument => {
                self.render_located_node(product, view, 0, resources)?;
                self.push(b"\n");
            }
            YamlProfile::Block => {
                self.push_block_separator();
                crate::block::render_located_document(&mut self.bytes, product, view, resources)?;
            }
        }
        Ok(())
    }

    /// Opens a block document that FOLLOWS another one with `---`.
    ///
    /// Block stream shape: the first document has no marker, every later one is separated from its predecessor, and no
    /// document is terminated with `...`. A single document — the overwhelmingly common case — therefore carries no
    /// markers at all.
    fn push_block_separator(&mut self) {
        if self.leading_separator {
            self.push(b"---\n");
        }
    }

    /// Renders one owned value as a YAML node with continuation indent `i`.
    fn render_owned_node(
        &mut self,
        value: &Value,
        indent: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        // The tag-boundary law: unwrap ONE non-core tag layer.
        if let Value::Tagged { tag, payload } = value {
            if matches!(&**payload, Value::Tagged { .. }) {
                return Err(unrepresentable());
            }
            // The emitted spelling follows the §4.8 tag law: a standard tag emits `!!suffix`, a local tag its exact
            // `!suffix`, and another exact URI `!<...>`.
            let spelling = crate::tag::emit_spelling(tag.as_str());
            self.push(spelling.as_bytes());
            self.push(b" ");
            return self.render_owned_tagged_payload(payload, indent, resources);
        }
        if !self.schema_admits(value.kind()) {
            return Err(unrepresentable());
        }
        match value {
            Value::Null => {
                self.push(b"!!null ");
                self.push(b"\"null\"");
            }
            Value::Bool(true) => {
                self.push(b"!!bool \"true\"");
            }
            Value::Bool(false) => {
                self.push(b"!!bool \"false\"");
            }
            Value::Number(number) => {
                // The inline machine arm is an integer; the boxed arm answers through its retained representation.
                let tag = if number.category() == jqf_data::NumberCategory::Integer {
                    "!!int"
                } else {
                    "!!float"
                };
                self.push(tag.as_bytes());
                self.push(b" ");
                let text = number_text(number).ok_or_else(unrepresentable)?;
                self.push(b"\"");
                write_escaped(&mut self.bytes, &text);
                self.push(b"\"");
            }
            Value::String(text) => {
                self.push(b"!!str ");
                self.push(b"\"");
                write_escaped(&mut self.bytes, text.as_str());
                self.push(b"\"");
            }
            Value::Array(array) => {
                self.push(b"!!seq [");
                for item in array {
                    self.push(b"\n");
                    self.push_indent(indent + 2);
                    self.render_owned_node(item, indent + 2, resources)?;
                    // The item's ONE trailing comma occupies its own line.
                    self.push(b",");
                }
                self.push(b"\n");
                self.push_indent(indent);
                self.push(b"]");
            }
            Value::Object(object) => {
                self.push(b"!!map {");
                // Every entry carries exactly ONE trailing comma (the separator); there is no comma before an entry.
                for entry in object {
                    self.push(b"\n");
                    self.push_indent(indent + 2);
                    self.push(b"? ");
                    self.render_owned_node(
                        &Value::try_string(entry.key()).map_err(|_| unrepresentable())?,
                        indent + 4,
                        resources,
                    )?;
                    self.push(b"\n");
                    self.push_indent(indent + 2);
                    self.push(b": ");
                    self.render_owned_node(entry.value(), indent + 4, resources)?;
                    self.push(b",");
                }
                if !object.is_empty() {
                    self.push(b"\n");
                    self.push_indent(indent);
                }
                self.push(b"}");
            }
            Value::Bytes(_)
            | Value::LocalDate(_)
            | Value::LocalTime(_)
            | Value::LocalDateTime(_)
            | Value::OffsetDateTime(_) => {
                let scalar = jqf_data::ScalarView::from_value(value).ok_or_else(unrepresentable)?;
                self.push_projected_scalar(&scalar, resources)?;
            }
            Value::Tagged { .. } => unreachable!("tag layer unwrapped above"),
        }
        Ok(())
    }

    /// Renders the direct payload of a non-core tag: String, sequence, or mapping (the §4.8 one-tag-layer law; anything
    /// else is unrepresentable).
    fn render_owned_tagged_payload(
        &mut self,
        payload: &Value,
        indent: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // A NESTED non-core tag cannot share the one explicit-tag property this site emits. `untagged()` peels one
        // layer; a second layer is refused so it cannot vanish.
        if let Value::Tagged { payload: inner, .. } = payload
            && matches!(&**inner, Value::Tagged { .. })
        {
            return Err(unrepresentable());
        }
        match payload.untagged() {
            Value::String(text) => {
                self.push(b"\"");
                write_escaped(&mut self.bytes, text.as_str());
                self.push(b"\"");
                Ok(())
            }
            Value::Array(array) => {
                self.push(b"[");
                for item in array {
                    self.push(b"\n");
                    self.push_indent(indent + 2);
                    self.render_owned_node(item, indent + 2, resources)?;
                    // The item's ONE trailing comma occupies its own line.
                    self.push(b",");
                }
                self.push(b"\n");
                self.push_indent(indent);
                self.push(b"]");
                Ok(())
            }
            Value::Object(object) => {
                self.push(b"{");
                for entry in object {
                    self.push(b"\n");
                    self.push_indent(indent + 2);
                    self.push(b"? ");
                    self.render_owned_node(
                        &Value::try_string(entry.key()).map_err(|_| unrepresentable())?,
                        indent + 4,
                        resources,
                    )?;
                    self.push(b"\n");
                    self.push_indent(indent + 2);
                    self.push(b": ");
                    self.render_owned_node(entry.value(), indent + 4, resources)?;
                    self.push(b",");
                }
                if !object.is_empty() {
                    self.push(b"\n");
                    self.push_indent(indent);
                }
                self.push(b"}");
                Ok(())
            }
            _ => Err(unrepresentable()),
        }
    }

    /// Renders one located value as a YAML node with continuation indent `i`.
    fn render_located_node(
        &mut self,
        product: &jqf_codec_core::DocumentProduct<'_>,
        view: &ValueView<'_, '_>,
        indent: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // The nesting guard: rendering recurses once per container level (the 10000-level ceiling — the stack-depth
        // gate's YAML lane).
        let _depth = resources.enter_nesting_owned().map_err(CodecError::from)?;
        // A NON-CORE intrinsic tag wraps the payload with one explicit tag property, and its payload renders WITHOUT
        // its own standard tag (the one-tag-layer law). Materialize the payload and render it through the OWNED path,
        // which implements exactly that law; the located path's kind arms below always emit the standard tag.
        let non_core = view
            .tag_semantics()
            .map_err(map_data)?
            .is_some_and(|semantics| semantics == jqf_data::IntrinsicTagSemantics::Tagged);
        if non_core {
            let tag = view.tag().map_err(map_data)?.expect("non-core tag present");
            let handle = product.document().node_handle(view.node()).map_err(map_data)?;
            let payload = product
                .document()
                .materialize_node(handle, resources)
                .map_err(map_data)?;
            // The emitted spelling follows the §4.8 tag law.
            let spelling = crate::tag::emit_spelling(tag.as_str());
            self.push(spelling.as_bytes());
            self.push(b" ");
            return self.render_owned_tagged_payload(&payload, indent, resources);
        }
        match view.kind().map_err(map_data)? {
            jqf_data::ValueKind::Null => {
                self.push(b"!!null \"null\"");
            }
            jqf_data::ValueKind::Bool => {
                let scalar = view.scalar().map_err(map_data)?.ok_or_else(|| {
                    CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "YAML located bool without a scalar view",
                    })
                })?;
                let jqf_data::ScalarView::Bool(boolean) = scalar else {
                    return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "YAML located bool scalar",
                    }));
                };
                self.push(if boolean {
                    b"!!bool \"true\""
                } else {
                    b"!!bool \"false\""
                });
            }
            jqf_data::ValueKind::Number => {
                let scalar = view.scalar().map_err(map_data)?.ok_or_else(|| {
                    CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "YAML located number without a scalar view",
                    })
                })?;
                let jqf_data::ScalarView::Number(number) = scalar else {
                    return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "YAML located number scalar",
                    }));
                };
                let tag = match number {
                    jqf_data::NumberView::Integer(_) => "!!int",
                    _ => "!!float",
                };
                self.push(tag.as_bytes());
                self.push(b" ");
                let text = number_view_text(&number).ok_or_else(unrepresentable)?;
                self.push(b"\"");
                write_escaped(&mut self.bytes, &text);
                self.push(b"\"");
            }
            jqf_data::ValueKind::String => {
                let Some(jqf_data::ScalarView::String(text)) = view.scalar().map_err(map_data)? else {
                    return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "YAML located string without text",
                    }));
                };
                self.push(b"!!str ");
                self.push(b"\"");
                write_escaped(&mut self.bytes, text);
                self.push(b"\"");
            }
            jqf_data::ValueKind::Array => {
                let array = view.array().map_err(map_data)?.ok_or_else(|| {
                    CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "YAML located array without an array view",
                    })
                })?;
                self.push(b"!!seq [");
                let count = array.len();
                for index in 0..count {
                    self.push(b"\n");
                    self.push_indent(indent + 2);
                    let item = array.get(index).ok_or_else(|| {
                        CodecError::new(CodecFailureKind::InternalContractViolation {
                            contract: "YAML located array item",
                        })
                    })?;
                    self.render_located_node(product, &item, indent + 2, resources)?;
                    self.push(b",");
                }
                self.push(b"\n");
                self.push_indent(indent);
                self.push(b"]");
            }
            jqf_data::ValueKind::Object => {
                let object = view.object().map_err(map_data)?.ok_or_else(|| {
                    CodecError::new(CodecFailureKind::InternalContractViolation {
                        contract: "YAML located object without an object view",
                    })
                })?;
                self.push(b"!!map {");
                let count = object.len();
                for index in 0..count {
                    self.push(b"\n");
                    self.push_indent(indent + 2);
                    let entry = object.get_index(index).map_err(map_data)?.ok_or_else(|| {
                        CodecError::new(CodecFailureKind::InternalContractViolation {
                            contract: "YAML located object entry",
                        })
                    })?;
                    self.push(b"? ");
                    self.render_located_key(entry.key(), indent + 4);
                    self.push(b"\n");
                    self.push_indent(indent + 2);
                    self.push(b": ");
                    self.render_located_node(product, &entry.value(), indent + 4, resources)?;
                    self.push(b",");
                }
                if count > 0 {
                    self.push(b"\n");
                    self.push_indent(indent);
                }
                self.push(b"}");
            }
            jqf_data::ValueKind::Bytes
            | jqf_data::ValueKind::LocalDate
            | jqf_data::ValueKind::LocalTime
            | jqf_data::ValueKind::LocalDateTime
            | jqf_data::ValueKind::OffsetDateTime => {
                let scalar = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)?;
                self.push_projected_scalar(&scalar, resources)?;
            }
        }
        Ok(())
    }

    fn render_located_key(&mut self, key: &str, indent: usize) {
        self.push(b"!!str ");
        self.push(b"\"");
        write_escaped(&mut self.bytes, key);
        self.push(b"\"");
        let _ = indent;
    }

    fn push_indent(&mut self, indent: usize) {
        for _ in 0..indent {
            self.push(b" ");
        }
    }
}

/// The exact §4.8 double-quote escape set over any staging buffer.
///
/// Both dialects double-quote — the canonical one always, the block one only where its quoting rule requires it — and
/// both escape ONE way, so the escape set lives beside the buffer rather than inside either renderer.
pub(crate) fn write_escaped(bytes: &mut Vec<u8>, text: &str) {
    let mut push = |slice: &[u8]| bytes.extend_from_slice(slice);
    for ch in text.chars() {
        let code = u32::from(ch);
        match ch {
            '\\' => push(b"\\\\"),
            '"' => push(b"\\\""),
            '\0' => push(b"\\0"),
            '\u{07}' => push(b"\\a"),
            '\u{08}' => push(b"\\b"),
            '\t' => push(b"\\t"),
            '\n' => push(b"\\n"),
            '\u{0B}' => push(b"\\v"),
            '\u{0C}' => push(b"\\f"),
            '\r' => push(b"\\r"),
            '\u{1B}' => push(b"\\e"),
            '\u{2028}' => push(b"\\u2028"),
            '\u{2029}' => push(b"\\u2029"),
            // The exact §4.8 law: remaining C0 plus U+007F..U+009F use `\xNN`; U+2028/U+2029 use `\u2028`/`\u2029`;
            // another YAML-printable scalar is COPIED; any remaining admitted BMP scalar uses `\u` and a supplementary
            // scalar uses `\U`.
            _ if (0x00..=0x1F).contains(&code) || (0x7F..=0x9F).contains(&code) => {
                let mut hex = [0u8; 4];
                hex[0] = b'\\';
                hex[1] = b'x';
                hex[2] = hex_digit(code >> 4);
                hex[3] = hex_digit(code);
                push(&hex);
            }
            _ if (0x20..=0x7E).contains(&code)
                || (0xA0..=0xD7FF).contains(&code)
                || (0xE000..=0xFFFD).contains(&code) =>
            {
                // A YAML-printable scalar: copied verbatim.
                let mut buf = [0u8; 4];
                let encoded = ch.encode_utf8(&mut buf);
                push(encoded.as_bytes());
            }
            _ if code <= 0xFFFF => {
                let mut buf = [0u8; 6];
                buf[0] = b'\\';
                buf[1] = b'u';
                buf[2] = hex_digit(code >> 12);
                buf[3] = hex_digit(code >> 8);
                buf[4] = hex_digit(code >> 4);
                buf[5] = hex_digit(code);
                push(&buf);
            }
            _ => {
                let mut buf = [0u8; 10];
                buf[0] = b'\\';
                buf[1] = b'U';
                buf[2] = hex_digit(code >> 28);
                buf[3] = hex_digit(code >> 24);
                buf[4] = hex_digit(code >> 20);
                buf[5] = hex_digit(code >> 16);
                buf[6] = hex_digit(code >> 12);
                buf[7] = hex_digit(code >> 8);
                buf[8] = hex_digit(code >> 4);
                buf[9] = hex_digit(code);
                push(&buf);
            }
        }
    }
}

fn hex_digit(value: u32) -> u8 {
    match value & 0xF {
        0..=9 => b'0' + (value & 0xF) as u8,
        _ => b'a' + (value & 0xF) as u8 - 10,
    }
}

/// The canonical number text of a number value (the `jqf.float64-ryu@1` renderer for floats; minimal decimal for
/// integers; the decimal `to-scientific-string` rendering for exact decimals).
pub(crate) fn number_text(number: &jqf_data::Number) -> Option<String> {
    // The inline machine arm renders its canonical spelling on demand; the boxed arm borrows its retained one.
    if let Some(machine) = number.as_machine() {
        let integer = jqf_data::Integer::from_i64(machine);
        return Some(integer.as_str().to_owned());
    }
    if let Some(integer) = number.as_integer() {
        return Some(integer.as_str().to_owned());
    }
    if let Some(decimal) = number.as_decimal() {
        let rendered = decimal_render(decimal.coefficient().as_str(), decimal.scale())?;
        return Some(rendered);
    }
    let float = number.as_float()?;
    let bits = float.bits();
    if bits == crate::schema::POSITIVE_QUIET_NAN_BITS {
        return Some(".nan".to_owned());
    }
    if bits == f64::INFINITY.to_bits() {
        return Some(".inf".to_owned());
    }
    if bits == f64::NEG_INFINITY.to_bits() {
        return Some("-.inf".to_owned());
    }
    let out = jqf_data::format_binary64(float.get())?.as_str().to_owned();
    // The engine's renderer prints `null` for NaN; the canonical YAML renderer prints the fixed `.nan` instead (the
    // codec owns float text).
    if out == "null" {
        return Some(".nan".to_owned());
    }
    // A ryu float text with no decimal point or exponent (e.g. `450`) is an INTEGER spelling; a `!!float` node whose
    // payload renders so would not re-resolve as a float on decode. The YAML float grammar requires a decimal point or
    // exponent, so re-spell with an explicit `.0`.
    if !out.contains('.') && !out.contains('e') && !out.contains('E') {
        let mut spelled = String::with_capacity(out.len() + 2);
        spelled.push_str(&out);
        spelled.push_str(".0");
        return Some(spelled);
    }
    Some(out)
}

/// The canonical number text of a NUMBER VIEW (the located-encode arm).
pub(crate) fn number_view_text(number: &jqf_data::NumberView<'_>) -> Option<String> {
    match number {
        jqf_data::NumberView::Number(number) => number_text(number),
        jqf_data::NumberView::Integer(text) => Some((*text).to_owned()),
        jqf_data::NumberView::Float(value) => number_text(&jqf_data::Number::float(*value)),
        jqf_data::NumberView::Decimal { coefficient, scale } => decimal_render(coefficient, *scale),
    }
}

/// Renders an exact decimal as its `to-scientific-string` text, re-spelling an integer-shaped rendering with an
/// explicit `.0` so a `!!float` node's payload re-resolves as a float on decode (the same law `number_text`'s float arm
/// applies to a ryu integer spelling). The ONE shared renderer lives in `jqf-codec-core`; TOML's encoder calls the same
/// function.
fn decimal_render(coefficient: &str, scale: i64) -> Option<String> {
    jqf_codec_core::decimal_render(coefficient, scale, true)
}

impl EncoderSession for YamlEncoder {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn encode(
        &mut self,
        item: EncodeItem<'_, '_>,
        sink: &mut dyn ByteSink,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<PreservationReport, CodecError> {
        loop {
            if self.state == EncodeState::InputFinished {
                if !self.bytes.is_empty() {
                    sink.write_all(&self.bytes, context.resources())?;
                    self.bytes.clear();
                }
                return Ok(Self::report());
            }
            if self.root_done {
                self.state = EncodeState::InputFinished;
                continue;
            }
            if self.bytes.len() >= OFFER_BYTES {
                sink.write_all(&self.bytes, context.resources())?;
                self.bytes.clear();
                continue;
            }
            let remaining = context.resources().remaining_work() as usize;
            match context.resources().admit_work_transitions(remaining.max(1))? {
                WorkAdmission::Pending => context.replenish_work()?,
                WorkAdmission::Granted(_granted) => {
                    self.encode_item(item, context.resources())?;
                    self.root_done = true;
                }
            }
        }
    }
}

pub(crate) fn map_data(error: jqf_data::DataError) -> CodecError {
    jqf_codec_core::map_data(error, "YAML encoder document read")
}

pub(crate) fn unrepresentable() -> CodecError {
    CodecError::new(CodecFailureKind::UnsupportedRepresentation)
}

impl YamlEncoder {
    fn report() -> jqf_codec_core::PreservationReport {
        jqf_codec_core::PreservationReport::new(
            jqf_codec_core::PreservationOutcome::Exact,
            jqf_codec_core::PreservationOutcome::Omitted,
            jqf_codec_core::PreservationOutcome::Exact,
            jqf_codec_core::PreservationOutcome::Normalized,
        )
    }
}

// ------------------------------------------------------------------------- The splice policy (`render_edit_append`),
// written as the module-doc rulings. The anchors are the container's retained authored span:
//
// - A FLOW collection's span ends at its opening delimiter (`{`/`[`), which is how a flow container is recognized; the
//   splice renders INSIDE the collection, before its closing delimiter.
// - A BLOCK collection's span runs from its first entry to its last member's whole subtree; the splice renders new
//   members at that span's indentation, after the line containing the subtree end.
//
// Every splice re-verifies by re-decode (the SDK's law), so a splice the byte rules misclassify — a container the
// source contradicts, a dash column the scan cannot name — returns an empty insertion set and the caller falls back to
// the whole-document floor, never wrong bytes.

/// Whether a container is a FLOW collection: its span ends at a `{`/`[` (the opening delimiter) and opens with the
/// delimiter or a property token. The second conjunct separates a real flow container from a BLOCK container whose last
/// member is an empty flow collection (`b: {}`), whose subtree end happens to sit at its own `{`.
fn is_flow_collection(source: &[u8], span: jqf_source::Span) -> bool {
    let start = span.start() as usize;
    let end = span.end() as usize;
    matches!(source.get(end.wrapping_sub(1)), Some(b'{' | b'['))
        && matches!(source.get(start), Some(b'{' | b'[' | b'&' | b'!'))
}

/// The authored scalar style at a leaf patch site, classified from the retained source bytes the SDK passed (the
/// quote-inclusive span for a quoted scalar, the bare token for a plain one). The first significant byte names the
/// style; a block scalar keeps ruling 3 (whole-span replace) and renders double-quoted here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthoredStyle {
    Plain,
    Single,
    Double,
    Block,
}

fn authored_style(authored: Option<&[u8]>) -> AuthoredStyle {
    let Some(bytes) = authored else {
        return AuthoredStyle::Plain;
    };
    match bytes.iter().find(|byte| !matches!(byte, b' ' | b'\t')) {
        Some(b'\'') => AuthoredStyle::Single,
        Some(b'"') => AuthoredStyle::Double,
        Some(b'|' | b'>') => AuthoredStyle::Block,
        _ => AuthoredStyle::Plain,
    }
}

/// The column of `offset`'s line (the bytes since the previous line break).
fn offset_column(source: &[u8], offset: usize) -> usize {
    let line_start = source[..offset]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    offset - line_start
}

/// The file's OWN indent step: the smallest positive indent delta between consecutive CONTENT lines of the retained
/// source — blank lines and comment-only lines carry no structure and are skipped. Measured once per splice over bytes
/// the lane already holds; a file with no positive delta (flat, or a single content line) defaults to 2, the block
/// profile's declared shape, and a file with inconsistent indentation has no right answer anyway — the smallest delta
/// is the best reading it has. A tab counts as one column, matching `offset_column`.
fn file_indent_step(source: &[u8]) -> usize {
    let mut previous: Option<usize> = None;
    let mut step: Option<usize> = None;
    for line in source.split(|byte| *byte == b'\n') {
        let indent = line.iter().take_while(|byte| matches!(byte, b' ' | b'\t')).count();
        let Some(first) = line.get(indent).copied() else {
            continue;
        };
        if first == b'#' {
            continue;
        }
        if let Some(prev) = previous
            && indent > prev
        {
            let delta = indent - prev;
            step = Some(step.map_or(delta, |best| best.min(delta)));
        }
        previous = Some(indent);
    }
    step.unwrap_or(2)
}

/// The splice that lands new statement text on its own line after `anchor`'s line: the position is the end of the line
/// containing `anchor` (so a trailing comment on that line survives), the text gets a leading newline only when that
/// line is not already terminated, and a trailing newline only when the document itself is line-terminated (the edit
/// lane never changes the source's own trailing state — a newline-less source stays newline-less). The TOML splice
/// policy's same shape, minus the unconditional trailing newline.
fn yaml_line_splice(segment: &[u8], anchor: usize, text: &[u8]) -> EditInsertion {
    let line_end = segment[anchor..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(segment.len(), |position| anchor + position);
    let at = if line_end < segment.len() {
        line_end + 1
    } else {
        line_end
    };
    let mut bytes = Vec::with_capacity(text.len() + 2);
    if at == segment.len() && !segment.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(text);
    if segment.ends_with(b"\n") && !text.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    EditInsertion {
        at,
        bytes,
        replace: None,
    }
}

/// A BLOCK mapping grew: each new member renders at the container's own indentation (the column of its first entry —
/// the span start), one line per member, spliced after the line containing the last member's subtree end (the span
/// end). A nested container value renders as a deeper block at indent+step, where step is the FILE's own — the ruled
/// "spliced blocks adopt the splice site's indentation".
#[allow(clippy::too_many_arguments)]
fn render_block_mapping_append(
    view: ValueView<'_, '_>,
    span: jqf_source::Span,
    source: &[u8],
    members: &[(&str, &Value)],
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    let _ = view;
    let indent = offset_column(source, span.start() as usize);
    let step = file_indent_step(source);
    let anchor = span.end() as usize;
    let mut text: Vec<u8> = Vec::new();
    for (index, (key, value)) in members.iter().enumerate() {
        if index > 0 {
            text.push(b'\n');
        }
        let mut member = Vec::new();
        crate::block::render_owned_member(&mut member, key, value, indent, step, resources)?;
        text.extend_from_slice(member.as_slice());
    }
    // The members join with newlines; the final line break is the splice's decision (it mirrors the source's own
    // trailing state).
    Ok(alloc::vec![yaml_line_splice(source, anchor, &text)])
}

/// A BLOCK sequence grew: each new item renders at the container's dash column (the first non-blank byte of the first
/// item's line), spliced after the line containing the last item's subtree end. A dash column the source cannot name
/// (an item on its own line under a bare `-`) declines to the floor.
fn render_block_sequence_append(
    view: ValueView<'_, '_>,
    span: jqf_source::Span,
    source: &[u8],
    items: &[&Value],
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    let _ = view;
    let start = span.start() as usize;
    let line_start = source[..start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let mut dash_col = None;
    for (index, byte) in source[line_start..start].iter().enumerate() {
        if *byte == b'-' {
            dash_col = Some(index);
            break;
        }
        if !matches!(byte, b' ' | b'\t') {
            break;
        }
    }
    let Some(dash_col) = dash_col else {
        return Ok(alloc::vec::Vec::new());
    };
    let step = file_indent_step(source);
    let anchor = span.end() as usize;
    let mut text: Vec<u8> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            text.push(b'\n');
        }
        let mut member = Vec::new();
        crate::block::render_owned_item(&mut member, item, dash_col, step, resources)?;
        text.extend_from_slice(member.as_slice());
    }
    // The final line break is the splice's decision (see the mapping arm).
    Ok(alloc::vec![yaml_line_splice(source, anchor, &text)])
}

/// A FLOW collection grew: the members render inside the collection, before its closing delimiter — `{ a: 1, b: 2 }`
/// grows to `{ a: 1, b: 2, c: 3 }`. The closing delimiter is found by a depth scan from the opening one (quoted scalars
/// skipped; the document already validated, so the close exists). An empty collection grows right after its opening
/// delimiter.
#[allow(clippy::too_many_arguments)]
fn render_flow_append(
    factory: &YamlEncoderFactory,
    view: ValueView<'_, '_>,
    span: jqf_source::Span,
    source: &[u8],
    members: EditAppendMembers<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    let open = span.end() as usize - 1;
    let empty = match members {
        EditAppendMembers::Table(_) => view
            .object()
            .map_err(map_data)?
            .ok_or_else(|| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "YAML flow append table view",
                })
            })?
            .is_empty(),
        EditAppendMembers::Array(_) => view
            .array()
            .map_err(map_data)?
            .ok_or_else(|| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "YAML flow append array view",
                })
            })?
            .is_empty(),
    };
    let Some(close) = flow_close(source, open) else {
        return Ok(alloc::vec::Vec::new());
    };
    if empty {
        // Right after the opening delimiter: `{}` grows to `{k: v}`.
        let mut bytes = Vec::new();
        render_flow_members(factory, &mut bytes, members, resources)?;
        return Ok(alloc::vec![EditInsertion {
            at: open + 1,
            bytes: bytes.as_slice().to_vec(),
            replace: None,
        }]);
    }
    // Before the closing delimiter (after any trailing whitespace), so `{ a: 1, b: 2 }` reads like a member list.
    let mut at = close;
    while at > open && matches!(source[at - 1], b' ' | b'\t' | b'\n' | b'\r') {
        at -= 1;
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b", ");
    render_flow_members(factory, &mut bytes, members, resources)?;
    Ok(alloc::vec![EditInsertion {
        at,
        bytes: bytes.as_slice().to_vec(),
        replace: None,
    }])
}

/// Renders a flow collection's new members: `k: v` pairs or items, joined with `, `, in flow syntax (nested containers
/// flow).
fn render_flow_members(
    factory: &YamlEncoderFactory,
    bytes: &mut Vec<u8>,
    members: EditAppendMembers<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    let _ = factory;
    match members {
        EditAppendMembers::Table(members) => {
            for (index, (key, value)) in members.iter().enumerate() {
                if index > 0 {
                    bytes.extend_from_slice(b", ");
                }
                crate::block::push_flow_member(bytes, key, value, resources)?;
            }
        }
        EditAppendMembers::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    bytes.extend_from_slice(b", ");
                }
                crate::block::push_flow_item(bytes, item, resources)?;
            }
        }
    }
    Ok(())
}

/// The position of a flow collection's closing delimiter, found by a depth scan from the opening one. Quoted scalars
/// (`"..."` with escapes, `'...'` with doubled quotes) are skipped so their brackets never count; the document already
/// validated, so the close always exists inside the segment. `None` bounds the scan defensively.
fn flow_close(source: &[u8], open: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut index = open + 1;
    while index < source.len() {
        match source[index] {
            b'{' | b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
                index += 1;
            }
            b'"' => {
                index += 1;
                while index < source.len() {
                    if source[index] == b'"' {
                        break;
                    }
                    if source[index] == b'\\' {
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                index += 1;
            }
            b'\'' => {
                index += 1;
                while index < source.len() {
                    if source[index] == b'\'' {
                        if source.get(index + 1) == Some(&b'\'') {
                            index += 2;
                        } else {
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

fn fact_refusal(message: &str) -> CodecError {
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(
        jqf_source::Namespace::new("yaml-fact").code("fact-write"),
        jqf_source::Severity::Error,
        message,
    ) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

/// Renders a text as a DOUBLE-QUOTED scalar through the codec's own §4.8 escape set, the one spelling every text is
/// safe in.
fn double_quoted(text: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.push(b'"');
    write_escaped(&mut bytes, text);
    bytes.push(b'"');
    bytes
}

/// Renders a text as a SINGLE-QUOTED scalar (`''` escapes a literal quote).
fn single_quoted(text: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.push(b'\'');
    for ch in text.chars() {
        if ch == '\'' {
            bytes.extend_from_slice(b"''");
        } else {
            let mut buffer = [0u8; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
        }
    }
    bytes.push(b'\'');
    bytes
}

/// Renders a text as a LITERAL block scalar (`|` keeps the single trailing newline, `|-` strips it), with every content
/// line at `indent` — the caller passes the node's line indentation plus the file's own step, so the block matches the
/// file it lands in. `None` when the text cannot be a literal block (a line carrying trailing whitespace or a control
/// character, or a double trailing newline), mirroring the block encoder's `literal_form` law.
fn literal_block(text: &str, indent: usize) -> Option<Vec<u8>> {
    let (body, header) = match text.strip_suffix('\n') {
        Some(body) => (body, b"|".as_slice()),
        None => (text, b"|-".as_slice()),
    };
    if body.is_empty() || body.ends_with('\n') {
        return None;
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(header);
    for line in body.split('\n') {
        if line.starts_with([' ', '\t']) || line.ends_with([' ', '\t']) {
            return None;
        }
        if line.chars().any(char::is_control) {
            return None;
        }
        bytes.push(b'\n');
        bytes.extend(core::iter::repeat_n(b' ', indent));
        bytes.extend_from_slice(line.as_bytes());
    }
    Some(bytes)
}

/// Whether a WRITTEN tag spelling can honor a node's value without changing it: any well-formed tag serves a STRING (a
/// non-core tag around a string re-types it, and the verification strips the wrapper), while a non-string scalar is
/// served only by the CORE tag of its own KIND — an int by its int tag, a float by its float tag — because a
/// kind-mismatched core tag would patch bytes that fail resolution when re-decoded (`!!int` on `1.5`), which the edit
/// lane's value-identity law forbids.
fn tag_serves_value(spelling: &str, value: &Value) -> bool {
    match value.untagged() {
        Value::String(_) => true,
        Value::Number(number) => {
            if number.to_integer().is_some() {
                matches!(spelling, "!!int" | "tag:yaml.org,2002:int")
            } else {
                matches!(spelling, "!!float" | "tag:yaml.org,2002:float")
            }
        }
        Value::Bool(_) => matches!(spelling, "!!bool" | "tag:yaml.org,2002:bool"),
        Value::Null => matches!(spelling, "!!null" | "tag:yaml.org,2002:null"),
        _ => false,
    }
}

/// Whether a WRITTEN anchor name is spellable: the spec's ns-anchor-char law — non-empty, no whitespace or line breaks,
/// and none of the flow indicators `,[]{}` (the scanner's `is_anchor_char`).
fn valid_anchor_name(text: &str) -> bool {
    !text.is_empty()
        && !text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, ',' | '[' | ']' | '{' | '}'))
}

/// The FULL authored token of a scalar node whose recorded span is the INNER content of a quoted scalar: a matching
/// quote pair on both edges extends the region over the quotes. A plain scalar's span is already the complete token and
/// passes through unchanged. This is the same convention the leaf splice's unchanged test reads.
fn full_scalar_span(source: &[u8], start: usize, end: usize) -> (usize, usize) {
    let quote = source.get(start.wrapping_sub(1)).copied();
    if quote.is_some_and(|q| q == b'"' || q == b'\'') && source.get(end) == quote.as_ref() {
        (start - 1, end + 1)
    } else {
        (start, end)
    }
}

/// The whitespace indent WIDTH of `offset`'s line (the leading blanks the line opens with).
fn line_indent_width(source: &[u8], offset: usize) -> usize {
    let line_start = source[..offset]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    source[line_start..]
        .iter()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count()
}

#[cfg(test)]
mod splice_tests {
    use super::flow_close;

    /// The flow-close scan skips quoted scalars, so a `}` or `]` inside a string never closes the collection — the
    /// bracket the splice lands before is the collection's own.
    #[test]
    fn flow_close_skips_quoted_scalars_and_nests() {
        assert_eq!(flow_close(b"{a: 1, b: 2}", 0), Some(11));
        assert_eq!(flow_close(b"{a: {b: 1}}", 0), Some(10));
        assert_eq!(flow_close(b"[1, [2, 3], 4]", 0), Some(13));
        assert_eq!(flow_close(b"{a: \"x} y[\", b: 'it''s'}", 0), Some(23));
        assert_eq!(flow_close(b"{a: 1", 0), None);
        assert_eq!(flow_close(b"{}", 0), Some(1));
    }
}
