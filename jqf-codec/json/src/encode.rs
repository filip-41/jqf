//! JSON encoder: factories, the iterative session, indent/raw/ascii/sort, and the `--edit` splice seam.
//!
//! Splice rulings live on [`EncoderFactoryImpl::render_edit_append`].
//!
//! # The JSON edit splice policy
//!
//! [`EncoderFactoryImpl::render_edit_append`] is the `--edit` structural seam, exactly as for TOML and YAML: a program
//! that GREW a container has no authored span to patch, so this codec renders the addition in JSON's local syntax at a
//! position it names, and the SDK splices it into the retained source and re-verifies by re-decode. The rulings, all
//! pinned by `tools/jqf-edit-differential.py`'s placement receipts:
//!
//! 1. **A grown OBJECT splices its new members after the last member, inside the closing `}`**; a grown ARRAY splices
//!    before the closing `]`. The new members reuse the source's OWN separator run — its comma spelling, newline, and
//!    indentation — observed between the last two members (or, for a one-member container, the whitespace the last
//!    member's line is written at). An EMPTY container grows directly after its opening delimiter, with no separator.
//! 2. **The splice copies the FILE's observed style, never the CLI's render dials**: `-c`/`--indent`/`--tab` shape the
//!    whole-document floor, and a splice that obeyed them would produce a file in two styles. The inserted member
//!    renders with jqf's canonical spacing (`: ` when the container wraps, `:` when it is one-line); what is copied is
//!    the container's wrap — the line breaks, the indentation column, the comma — not the CLI options in force.
//! 3. **A shrunk OBJECT cuts the member and exactly one adjacent comma — the preceding one, or the following one for
//!    the first member — plus the whitespace run the removed member owned**: a member owns the separator bytes on its
//!    left (a later member, the run after the preceding comma; the first member, its leading line break and
//!    indentation), and a LONE member's cut also takes the wrap between its value and the closing delimiter, so no bare
//!    newline is stranded inside the emptied container. A removal leaves the surviving members' own punctuation and
//!    whitespace untouched, so `{"a":1,"b":2}` minus `a` is `{"b":2}` and a lone member minus itself is `{}`.
//! 4. **Decline — the whole-document floor — for any container the source spans do not fully cover**: a span-less
//!    container (an `OwnedRun` document), a value token the scan cannot name, a member whose surrounding comma the
//!    source contradicts. The splice is byte arithmetic over the codec's own round-tripped text; the SDK's re-decode
//!    verification makes any wrong splice degrade to the floor the same way, never corrupt bytes.
//!
//! The anchors are the document's own spans: a built container carries an out-of-band record of its OPENING delimiter
//! (parse.rs records it when the container opens, in node order), and the splice scans from it — a deterministic
//! depth-counting waiting for the matching close, string bodies skipped — to name the container's region and observe
//! its separator run. Leaf values carry their own authored token spans.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EditAppendMembers, EditInsertion, EditRemoval,
    EditRemoveMembers, EncodeItem, EncodeRequest, EncoderFactoryImpl, EncoderSession, ErasedEncoderFactory,
    ErasedEncoderSession, NativeSpellings, PreservationOutcome, PreservationReport, PreservationRequest,
    RecycledSessionState, TagLayer, TrackedProjectionSink, classify_scalar, project_tag, tag_layer,
};
use jqf_data::{
    DecimalText, Document, DocumentCapability, NodeHandle, NodeId, NumberView, ScalarView, Value, ValueView,
    format_binary64,
};
use jqf_resource::{OwnedDepthGuard, ResourceContext, WorkAdmission};

use crate::byte_scan::escape_prefix_len;

use crate::encode_cursor::{JsonEncodeCursor, JsonEncodeInput};

use crate::edit::{render_edit_append, render_edit_remove, render_value_bytes};

/// JSON spells NOTHING beyond the core scalars: no tag, no temporal, no byte string. Every one of them therefore
/// reaches the shared projection layer, which is the whole reason JSON was the format that could not print a TOML date
/// at all.
const JSON_NATIVE: NativeSpellings = NativeSpellings::NONE;

const OFFER_BYTES: usize = 16 * 1024;
const TEXT_QUANTUM: usize = 256;

/// Structural whitespace policy for encoded JSON, the normalized render options, and the fill runs live in codec-core:
/// the record drives carry the render style across threads without depending on this crate.
pub use crate::options::JsonEncodeOptions;

/// Reads this codec's encoder options out of a request, falling back to the codec defaults when the caller named none.
///
/// The registration's declared schema has already been matched against the request by the time a factory runs, so a
/// request that reaches here with options carries this codec's own type; a downcast that fails anyway is a caller
/// passing a mismatched value under a matching schema identity, which is a requirement mismatch rather than a silent
/// default.
pub(crate) fn encode_options(request: EncodeRequest<'_, '_>) -> Result<JsonEncodeOptions, CodecError> {
    let Some(options) = request.options else {
        return Ok(JsonEncodeOptions::default());
    };
    options
        .downcast_ref::<JsonEncodeOptions>()
        .copied()
        .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch))
}

pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    crate::validate_target(request)?;
    let options = encode_options(request)?;
    new_encoder_factory(
        request.preservation,
        b"",
        b"",
        crate::ENCODE_PHYSICAL_ROUTE_ID,
        options,
        resources,
    )
}

/// Creates a strict-JSON encoder factory that ignores any indent the request named and always writes compact bytes.
///
/// This is the record-stream seam's entry point: the framer owns which bytes terminate a record and that there is
/// exactly one of them, strict JSON owns the payload bytes, and both publish as one scope. A framed record's payload
/// must occupy exactly one line for the framer's terminator to mean what it says, so line breaks inside a record are
/// not a formatting preference the caller gets to express. `physical` is the framer's own encoder route, so a receipt
/// distinguishes framed output from bare JSON.
///
/// The framing codec validates its own target before calling, so this entry point deliberately does not apply strict
/// JSON's `json/rfc8259` check: the request names the FRAMING format.
pub(crate) fn create_compact_framed_factory(
    request: EncodeRequest<'_, '_>,
    framing: &'static [u8],
    physical: jqf_codec_core::PhysicalRouteId,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    new_encoder_factory(
        request.preservation,
        b"",
        framing,
        physical,
        // The record seam forces compact and disables the CLI formatting flags: a framed record's payload must occupy
        // exactly one line, and the flags are JSON-formatting surface the record target does not honor.
        JsonEncodeOptions::default(),
        resources,
    )
}

/// Creates a strict-JSON encoder factory whose sessions write `prefix` before every ROOT value (except a raw-printed
/// root string) and append `framing` to every completed item, both inside the encoder's own staging buffer.
///
/// This is the json-seq seam: the prefix is the RS (0x1E) the RFC 7464 encoder grammar places before every item, and
/// the suffix is the LF that follows it. The style travels from the caller, so `-c`/`-r`/`-S`/`-a`/`--raw-output0`
/// render exactly as they do on bare JSON output; unlike the NDJSON record seam, json-seq does NOT force compact,
/// because json-seq output pretty-prints by default. The prefix is suppressed for a root string the `-r` raw arm writes
/// verbatim, because json-seq raw arm writes root strings with no RS.
///
/// `prefix` and `framing` must be `'static`: both are codec constants, not per-item data, and `physical` is the
/// framer's own encoder route.
///
/// The framing codec validates its own target before calling, so this entry point deliberately does not apply strict
/// JSON's `json/rfc8259` check: the request names the FRAMING format.
pub(crate) fn create_prefixed_framed_factory(
    request: EncodeRequest<'_, '_>,
    prefix: &'static [u8],
    framing: &'static [u8],
    physical: jqf_codec_core::PhysicalRouteId,
    style: JsonEncodeOptions,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    new_encoder_factory(request.preservation, prefix, framing, physical, style, resources)
}

/// Picks the cheapest factory carrier for the requested policy.
///
/// The common strict-JSON path -- compact output, no record framing -- is a zero-sized factory, so
/// [`ErasedEncoderFactory`] holds it without any heap allocation (the erased carrier's zero-sized fast path). Any
/// framing or indentation policy carries its fields on the heap instead; those callers are the CLI's pretty mode and
/// the record-stream seam, both of which create one factory per request rather than per item.
fn new_encoder_factory(
    preservation: PreservationRequest,
    prefix: &'static [u8],
    framing: &'static [u8],
    physical: jqf_codec_core::PhysicalRouteId,
    style: JsonEncodeOptions,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    if prefix.is_empty() && framing.is_empty() && style == JsonEncodeOptions::default() {
        ErasedEncoderFactory::try_new_factory(preservation, resources, || Ok(JsonEncoderFactory))
    } else {
        ErasedEncoderFactory::try_new_factory(preservation, resources, || {
            Ok(JsonFramedEncoderFactory {
                prefix,
                framing,
                physical,
                style,
            })
        })
    }
}

/// The common strict-JSON encoder factory: compact, unframed, fixed physical route. Zero-sized so the erased carrier
/// never allocates for it.
struct JsonEncoderFactory;

/// An encoder factory carrying prefix/framing and/or indentation policy.
struct JsonFramedEncoderFactory {
    /// Codec-owned bytes written before every root value (json-seq's RS), suppressed for a raw-printed root string
    /// exactly as json-seq raw arm suppresses it. Empty for every other framing codec.
    prefix: &'static [u8],
    framing: &'static [u8],
    physical: jqf_codec_core::PhysicalRouteId,
    style: JsonEncodeOptions,
}

/// The three edit/render methods every JSON factory carrier answers identically: they read only the value and the free
/// render helpers below, never the carrier's framing, prefix, or style state. A new carrier includes this block rather
/// than copying the glue.
macro_rules! shared_render_methods {
    () => {
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
        ) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
            render_edit_append(document, container, source, members, resources)
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
            render_edit_remove(document, container, source, members)
        }
    };
}

impl EncoderFactoryImpl for JsonEncoderFactory {
    fn physical_encoder(&self) -> jqf_codec_core::PhysicalRouteId {
        crate::ENCODE_PHYSICAL_ROUTE_ID
    }

    fn emits_canonical_form(&self) -> bool {
        true
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        encoder_start(item, preservation, b"", b"", JsonEncodeOptions::default(), resources)
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        item: EncodeItem<'_, '_>,
        _preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        encoder_restart(state, item, b"", b"", JsonEncodeOptions::default())
    }

    shared_render_methods!();
}

impl EncoderFactoryImpl for JsonFramedEncoderFactory {
    fn physical_encoder(&self) -> jqf_codec_core::PhysicalRouteId {
        self.physical
    }

    fn emits_canonical_form(&self) -> bool {
        self.prefix.is_empty() && self.framing.is_empty() && self.style.emits_canonical_form()
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        encoder_start(item, preservation, self.prefix, self.framing, self.style, resources)
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        item: EncodeItem<'_, '_>,
        _preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        encoder_restart(state, item, self.prefix, self.framing, self.style)
    }

    shared_render_methods!();
}

fn encoder_start<'item, 'source>(
    item: EncodeItem<'item, 'source>,
    preservation: PreservationRequest,
    prefix: &'static [u8],
    framing: &'static [u8],
    style: JsonEncodeOptions,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
    let encoder = JsonEncoder::try_new(item, prefix, framing, style, resources)?;
    ErasedEncoderSession::try_new(item, preservation, || Ok(encoder))
}

fn encoder_restart(
    state: &mut RecycledSessionState<'_>,
    item: EncodeItem<'_, '_>,
    prefix: &'static [u8],
    framing: &'static [u8],
    style: JsonEncodeOptions,
) -> Result<bool, CodecError> {
    let facts = fact_preservation(item)?;
    let Some(encoder) = state.downcast_mut::<JsonEncoder>() else {
        return Ok(false);
    };
    // A recycled encoder must carry the prefix, framing and style its own factory declared; a factory whose prefix or
    // framing differs owns a different concrete state and would have declined the downcast above only by luck, and a
    // recycled style would silently reformat this factory's output.
    if encoder.prefix != prefix || encoder.framing != framing || encoder.style != style {
        return Ok(false);
    }
    encoder.reset(facts);
    Ok(true)
}

/// How faithfully one item's attached facts survive JSON encoding.
///
/// Read once per item, by both the fresh and the recycled start, so a recycled encoder classifies exactly what a fresh
/// one would.
pub(crate) fn fact_preservation(item: EncodeItem<'_, '_>) -> Result<FactPreservation, CodecError> {
    Ok(match item {
        EncodeItem::Owned(_) => FactPreservation::Exact,
        // A document that did not retain attached-fact coverage cannot carry any fact, so the encoding preserves them
        // exactly (there are none).
        EncodeItem::Located { product, .. }
            if !product
                .document()
                .coverage()
                .contains(DocumentCapability::AttachedFacts)
                || product.document().fact_count().map_err(data_error)? == 0 =>
        {
            FactPreservation::Exact
        }
        EncodeItem::Located { product, node } if node == product.document().root_handle() => FactPreservation::Omitted,
        EncodeItem::Located { .. } => FactPreservation::Indeterminate,
    })
}

#[derive(Clone, Copy)]
enum FrameKind {
    Array,
    Object,
}

struct Frame {
    kind: FrameKind,
    /// Iteration position. For a sorted object frame this is the position in the SORTED order; `order` maps it to the
    /// document's member index.
    index: usize,
    len: usize,
    _depth: OwnedDepthGuard,
    /// A permutation from sorted position to document member index, present only for an object frame under
    /// `-S`/`--sort-keys`. `None` means emit members in document order.
    order: Option<Vec<u32>>,
}

#[derive(Clone, Copy)]
enum TextTarget {
    Key,
    Value,
}

enum Phase {
    Value,
    Text { target: TextTarget, cursor: usize },
    Number { cursor: usize },
    FinishValue,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EncodeState {
    Active,
    InputFinished,
}

pub(crate) struct JsonEncoder {
    frames: Vec<Frame>,
    bytes: Vec<u8>,
    phase: Phase,
    state: EncodeState,
    normalized_semantics: bool,
    /// This item dropped at least one non-core tag on publish (the shared layer's `TagLayer::Tagged` arm publishes the
    /// payload bare). The report law needs that event VISIBLE on the tags-and-facts axis: JSON spells no tag, so a bare
    /// payload is a canonicalization of the tagged value, not an exact preservation of it. Mirrors
    /// `normalized_semantics`'s flag-then-report shape; per item, cleared in [`Self::reset`].
    dropped_tag: bool,
    facts: FactPreservation,
    /// Codec-owned suffix appended once, atomically, when the item completes.
    framing: &'static [u8],
    /// Codec-owned bytes written before every ROOT value, except one the `-r` raw arm publishes raw (json-seq's RS,
    /// suppressed for a raw root string exactly as json-seq suppresses it). Empty for every other caller.
    prefix: &'static [u8],
    /// The render style — indentation, `-r` raw strings, `-S` sort keys, `-a` ascii output — fixed for this
    /// encoder's whole life so a recycled session cannot reformat mid-publication.
    pub(crate) style: JsonEncodeOptions,
    /// The leading-comment index (the `jsonc.comment@1` facts keyed by node handle) when this session renders JSONC
    /// output; `None` for strict JSON and every framing codec. When present, the root/member/element hooks re-emit each
    /// node's leading comments as `//` lines before it. The index is per ITEM: a recycled session rebuilds it for the
    /// new item.
    pub(crate) comments: Option<BTreeMap<NodeHandle, Vec<String>>>,
    /// Whether this session writes a trailing comma before every closing delimiter (the `jsonc.trailing-jqf@1` output
    /// profile). Always `false` for strict JSON.
    pub(crate) trailing_commas: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum FactPreservation {
    Exact,
    Omitted,
    Indeterminate,
}

impl JsonEncoder {
    /// Reinitializes this encoder for one more ordered item, keeping the retained frame stack and output staging buffer
    /// the previous item grew.
    ///
    /// Every byte, frame, and phase of the previous item is dropped, so the reset state is exactly [`Self::try_new`]'s
    /// except for the recycled capacities: an item that aborted mid-offer cannot leak staged bytes into the next one's
    /// output.
    pub(crate) fn reset(&mut self, facts: FactPreservation) {
        self.frames.clear();
        self.bytes.clear();
        self.phase = Phase::Value;
        self.state = EncodeState::Active;
        self.normalized_semantics = false;
        self.dropped_tag = false;
        self.facts = facts;
        // The commented dialects' two extras belong to the ITEM that armed them, not to the slot: a recycled encoder
        // that kept them would emit the previous item's `//` lines, and a trailing comma, into a document that never
        // asked for either.
        self.comments = None;
        self.trailing_commas = false;
    }

    pub(crate) fn try_new(
        item: EncodeItem<'_, '_>,
        prefix: &'static [u8],
        framing: &'static [u8],
        style: JsonEncodeOptions,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let facts = fact_preservation(item)?;
        let initial_depth = usize::try_from(resources.limits().max_nesting_depth())
            .map_err(|_| CodecError::new(CodecFailureKind::Overflow))?
            .min(16);
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(initial_depth)
            .map_err(jqf_resource::ResourceError::from)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(OFFER_BYTES)
            .map_err(jqf_resource::ResourceError::from)?;
        Ok(Self {
            frames,
            bytes,
            phase: Phase::Value,
            state: EncodeState::Active,
            normalized_semantics: false,
            dropped_tag: false,
            facts,
            framing,
            prefix,
            style,
            comments: None,
            trailing_commas: false,
        })
    }

    /// The JSONC constructor: the strict encoder plus a leading-comment index and the trailing-comma output bit. See
    /// [`Self::try_new`] for the base. The comment index is built by the caller from the item's document facts; an
    /// owned item carries no comments and passes an empty index.
    pub(crate) fn try_new_jsonc(
        item: EncodeItem<'_, '_>,
        prefix: &'static [u8],
        framing: &'static [u8],
        style: JsonEncodeOptions,
        comments: BTreeMap<NodeHandle, Vec<String>>,
        trailing_commas: bool,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let mut encoder = Self::try_new(item, prefix, framing, style, resources)?;
        encoder.comments = Some(comments);
        encoder.trailing_commas = trailing_commas;
        Ok(encoder)
    }

    /// Writes the line break and indentation that precede a value nested inside `depth` open containers. A no-op in
    /// compact mode.
    ///
    /// Callers that batch their own write should prefer [`Self::break_slices`] and splice its result, which keeps the
    /// common depth to a single admitted write.
    fn push_break(&mut self, depth: usize) -> Result<(), CodecError> {
        // Compact mode writes no element breaks at all: adjacent values separate purely positionally. (A COMMENT break
        // still ends its line in compact mode — see `push_comment_break`.)
        if self.style.indent.fill().is_none() {
            return Ok(());
        }
        self.push_newline_indent(depth)
    }

    /// The `\n` plus `depth` levels of indent, written in bounded chunks of the static fill. ONE loop serves both break
    /// forms — an element break and a comment break write exactly these bytes whenever they write any.
    fn push_newline_indent(&mut self, depth: usize) -> Result<(), CodecError> {
        self.push(b"\n");
        if let Some((fill, width)) = self.style.indent.fill() {
            let mut remaining = depth.checked_mul(width).ok_or_else(overflow)?;
            while remaining > 0 {
                let chunk = remaining.min(fill.len());
                self.push(fill.get(..chunk).ok_or_else(invalid_document)?);
                remaining -= chunk;
            }
        }
        Ok(())
    }

    /// The `[newline, indent]` slices that precede a value nested inside `depth` open containers, for a caller to
    /// splice into its own batched write — `extend_from_slice` skips empty slices, so compact mode costs only a
    /// length check and keeps its bytes exactly as before.
    ///
    /// `None` means the indent run is longer than the static fill and has to be written in chunks by
    /// [`Self::push_break`], which only happens past 64 levels of nesting at the default indent width.
    fn break_slices(&self, depth: usize) -> Result<Option<[&'static [u8]; 2]>, CodecError> {
        let Some((fill, width)) = self.style.indent.fill() else {
            return Ok(Some([b"".as_slice(), b"".as_slice()]));
        };
        let total = depth.checked_mul(width).ok_or_else(overflow)?;
        Ok(fill.get(..total).map(|pad| [b"\n".as_slice(), pad]))
    }

    fn step(
        &mut self,
        input: &mut JsonEncodeInput<'_, '_, '_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        match self.phase {
            Phase::Value => self.start_value(input, resources),
            Phase::Text { target, cursor } => self.write_text(input, target, cursor),
            Phase::Number { cursor } => self.write_number(input, cursor),
            Phase::FinishValue => self.finish_value(input),
        }
    }

    /// Whether a ROOT value prints verbatim — the raw-string arm, which `-a` PRECEDES: with both flags a root string
    /// is written quoted with non-ASCII escaped, so the raw spelling additionally demands no ascii output. Nested
    /// values never qualify.
    fn root_prints_raw(&self) -> bool {
        self.style.raw_strings && !self.style.ascii_output && self.frames.is_empty()
    }

    /// Writes the codec-owned item prefix before a ROOT value, unless the item is a root string the `-r` raw arm prints
    /// verbatim.
    ///
    /// The reference's `--seq` raw arm writes root strings with NO RS prefix, in every ascii mode (`-r`, `-r -a`, `-j`,
    /// and `--raw-output0` all drop it); a projected-text root follows jqf's own raw arm, whose raw spelling
    /// additionally requires no ascii output.
    fn push_seq_prefix(&mut self, value: JsonRef<'_, '_>) -> Result<(), CodecError> {
        if self.prefix.is_empty() || !self.frames.is_empty() {
            return Ok(());
        }
        let scalar = value.scalar()?;
        let raw = match scalar {
            // The RS exception is the RS-prefix law and tracks the `-r` FLAG alone: a root string loses its RS prefix
            // even when `-a` forces the quoted rendering ([`Self::root_prints_raw`] is false).
            Some(ScalarView::String(_)) => self.style.raw_strings,
            Some(
                ScalarView::Bytes(_)
                | ScalarView::LocalDate(_)
                | ScalarView::LocalTime(_)
                | ScalarView::LocalDateTime(_)
                | ScalarView::OffsetDateTime(_),
            ) => self.root_prints_raw(),
            _ => false,
        };
        if !raw {
            self.push(self.prefix);
        }
        Ok(())
    }

    fn start_value(
        &mut self,
        input: &mut JsonEncodeInput<'_, '_, '_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // The root value's leading comments: a JSONC document whose ROOT node carries `jsonc.comment@1` facts re-emits
        // them before the root. Only the true root qualifies — nested values enter `start_value` with a frame on the
        // stack.
        if self.frames.is_empty()
            && let EncodeItem::Located { node, .. } = input.item()?
        {
            self.write_comment_lines_before(Some(node))?;
        }
        // The cross-format encode policy, reached through the ONE shared layer: JSON spells no tag and no non-core
        // scalar, so a tagged value publishes its payload and a temporal or byte string becomes canonical text. Both
        // record their event inside the shared layer.
        if let TagLayer::Tagged(_) = tag_layer(input.item()?)? {
            project_tag(resources);
            // The payload publishes bare, so this item's report must say the tag axis was normalized, not exact — see
            // `dropped_tag`.
            self.dropped_tag = true;
        }
        let value = JsonRef::from_item(input.item()?)?;
        // The codec-owned item prefix (json-seq's RS) joins the SAME staging buffer before the value, so prefix,
        // payload, and suffix publish as one atomic scope; a root string the `-r` raw arm prints verbatim gets no
        // prefix, exactly as the json-seq raw arm writes no RS.
        self.push_seq_prefix(value)?;
        match value.scalar()? {
            Some(ScalarView::Null) => {
                self.push(b"null");
                self.phase = Phase::FinishValue;
            }
            Some(ScalarView::Bool(value)) => {
                self.push(if value { b"true" } else { b"false" });
                self.phase = Phase::FinishValue;
            }
            Some(ScalarView::Number(number)) => {
                self.write_number_view(number, 0)?;
            }
            Some(ScalarView::String(text)) => {
                // The reference's `-r`/`--raw-output` verbatim arm: a ROOT string item is written with no quotes and no
                // escapes — its bytes exactly — so the escape scan is skipped too. Nested strings (frames
                // non-empty) keep the ordinary quoted spelling, which is the reference's own `-r` law (only the item
                // root is raw; `-r @json` still prints the quotes @json produced).
                //
                // The reference's `-a`/`--ascii-output` PRECEDES the raw arm for a root string: with both flags the
                // string is written quoted with non-ASCII escaped (`-ra` and `-ja` on `"h\u00e9llo"` both print the
                // six-line `"h\u00e9llo"`), because the reference's raw arm routes a root string through the ascii
                // renderer when `-a` is set.
                if self.root_prints_raw() {
                    // The reference's `--raw-output0` guard: a NUL byte inside the string would be indistinguishable
                    // from the facade's own NUL terminator once written, so this is rejected rather than emitted.
                    if self.style.raw_output_nul && text.as_bytes().contains(&0) {
                        return Err(CodecError::new(CodecFailureKind::RawNulByte));
                    }
                    self.push(text.as_bytes());
                    self.phase = Phase::FinishValue;
                    return Ok(());
                }
                if text.len() <= TEXT_QUANTUM {
                    let prefix = escape_prefix_len(text.as_bytes(), self.style.ascii_output);
                    if prefix == text.len() {
                        self.bytes.extend_from_slice(b"\"");
                        self.bytes.extend_from_slice(text.as_bytes());
                        self.bytes.extend_from_slice(b"\"");
                    } else {
                        self.push(b"\"");
                        push_escaped_text(&mut self.bytes, text, self.style.ascii_output)?;
                        self.push(b"\"");
                    }
                    self.phase = Phase::FinishValue;
                } else {
                    self.push(b"\"");
                    self.phase = Phase::Text {
                        target: TextTarget::Value,
                        cursor: 0,
                    };
                }
            }
            Some(
                scalar @ (ScalarView::Bytes(_)
                | ScalarView::LocalDate(_)
                | ScalarView::LocalTime(_)
                | ScalarView::LocalDateTime(_)
                | ScalarView::OffsetDateTime(_)),
            ) => {
                self.write_projected_scalar(&scalar, resources)?;
                self.phase = Phase::FinishValue;
            }
            None => {
                if let Some(len) = value.array_len()? {
                    if len == 0 {
                        let _depth = resources.enter_nesting()?;
                        self.push(b"[]");
                        self.phase = Phase::FinishValue;
                    } else {
                        self.push(b"[");
                        self.push_frame(FrameKind::Array, len, None, resources)?;
                        // The frame is on the stack, so the stack depth is now the depth the first element sits at.
                        self.push_break(self.frames.len())?;
                        // The first element's leading comments, after the break, before the element's bytes.
                        if self.comments.is_some() {
                            let node = Self::array_item_handle(input, 0)?;
                            self.write_comment_lines_before(node)?;
                        }
                        input.enter_array(0)?;
                    }
                } else if let Some(len) = value.object_len()? {
                    if len == 0 {
                        let _depth = resources.enter_nesting()?;
                        self.push(b"{}");
                        self.phase = Phase::FinishValue;
                    } else {
                        // `-S`: read every member key and sort the member indices by it BEFORE the frame lands, so the
                        // frame carries a stable permutation and every later member read maps through it.
                        let order = if self.style.sort_keys {
                            Some(Self::build_object_order(input, len)?)
                        } else {
                            None
                        };
                        self.push_frame(FrameKind::Object, len, order, resources)?;
                        self.start_object_key(input, b"{")?;
                    }
                } else {
                    return Err(invalid_document());
                }
            }
        }
        Ok(())
    }

    fn push_frame(
        &mut self,
        kind: FrameKind,
        len: usize,
        order: Option<Vec<u32>>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if self.frames.capacity() == 0 {
            let initial = usize::try_from(resources.limits().max_nesting_depth())
                .map_err(|_| CodecError::new(CodecFailureKind::Overflow))?
                .min(16);
            self.frames
                .try_reserve_exact(initial)
                .map_err(jqf_resource::ResourceError::from)?;
        } else if self.frames.len() == self.frames.capacity() {
            self.frames.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
        }
        let frame = Frame {
            kind,
            index: 0,
            len,
            _depth: resources.enter_nesting_owned()?,
            order,
        };
        self.frames.push(frame);
        self.phase = Phase::Value;
        Ok(())
    }

    /// The document member index at the top frame's current position, mapping through the frame's `-S` permutation when
    /// the object is being sorted.
    ///
    /// A sorted object frame's `index` counts positions in the SORTED order, so every member read — the key lookup
    /// and the descent to the value — must consult the permutation; an unsorted frame's index is the document index
    /// directly, so the mapping is free there.
    fn member_index(&self) -> Result<usize, CodecError> {
        let frame = self.frames.as_slice().last().ok_or_else(invalid_document)?;
        match &frame.order {
            Some(order) => usize::try_from(*order.as_slice().get(frame.index).ok_or_else(invalid_document)?)
                .map_err(|_| invalid_document()),
            None => Ok(frame.index),
        }
    }

    /// Builds a stable permutation sorting this object's member indices by their keys' bytes (the sorted-key law).
    ///
    /// The keys borrow the ITEM, not the cursor, so every key is read once up front into a buffer the sort consults.
    fn build_object_order(input: &mut JsonEncodeInput<'_, '_, '_>, len: usize) -> Result<Vec<u32>, CodecError> {
        let mut keys = Vec::new();
        keys.try_reserve_exact(len).map_err(jqf_resource::ResourceError::from)?;
        for index in 0..len {
            keys.push(input.object_key(index)?);
        }
        let mut order = Vec::new();
        order
            .try_reserve_exact(len)
            .map_err(jqf_resource::ResourceError::from)?;
        for index in 0..len {
            let member = u32::try_from(index).map_err(|_| overflow())?;
            order.push(member);
        }
        // Stable: equal keys keep their document order, exactly as the reference's own sort over the parsed object
        // does.
        let keys = keys.as_slice();
        order
            .as_mut_slice()
            .sort_by(|&a, &b| keys[a as usize].cmp(keys[b as usize]));
        Ok(order)
    }

    fn write_text(
        &mut self,
        input: &mut JsonEncodeInput<'_, '_, '_>,
        target: TextTarget,
        cursor: usize,
    ) -> Result<(), CodecError> {
        let text = match target {
            TextTarget::Key => {
                let member = self.member_index()?;
                input.object_key(member)?
            }
            TextTarget::Value => JsonRef::from_item(input.item()?)?
                .string()?
                .ok_or_else(invalid_document)?,
        };
        let remaining = text.get(cursor..).ok_or_else(invalid_document)?;
        let mut consumed = remaining.len().min(TEXT_QUANTUM);
        while !remaining.is_char_boundary(consumed) {
            consumed -= 1;
        }
        let segment = remaining.get(..consumed).ok_or_else(invalid_document)?;
        push_escaped_text(&mut self.bytes, segment, self.style.ascii_output)?;
        let next = cursor.checked_add(segment.len()).ok_or_else(overflow)?;
        if next == text.len() {
            self.push(b"\"");
            match target {
                TextTarget::Key => {
                    self.push(self.style.indent.key_separator());
                    let member = self.member_index()?;
                    input.enter_object(member)?;
                    self.phase = Phase::Value;
                }
                TextTarget::Value => self.phase = Phase::FinishValue,
            }
        } else {
            self.phase = Phase::Text { target, cursor: next };
        }
        Ok(())
    }

    fn write_number(&mut self, input: &mut JsonEncodeInput<'_, '_, '_>, cursor: usize) -> Result<(), CodecError> {
        let number = JsonRef::from_item(input.item()?)?
            .number()?
            .ok_or_else(invalid_document)?;
        self.write_number_view(number, cursor)
    }

    fn write_number_view(&mut self, number: NumberView<'_>, cursor: usize) -> Result<(), CodecError> {
        match number {
            NumberView::Integer(value) => {
                self.write_verbatim_number(value, cursor)?;
            }
            NumberView::Decimal { coefficient, scale } => {
                self.write_decimal_scientific(coefficient, scale, cursor)?;
            }
            NumberView::Number(value) => {
                // The inline machine arm renders its canonical spelling on demand; the boxed arm borrows its retained
                // one.
                if let Some(machine) = value.as_machine() {
                    let integer = jqf_data::Integer::from_i64(machine);
                    self.write_verbatim_number(integer.as_str(), cursor)?;
                } else if let Some(integer) = value.as_integer() {
                    self.write_verbatim_number(integer.as_str(), cursor)?;
                } else if let Some(decimal) = value.as_decimal() {
                    self.write_decimal_scientific(decimal.coefficient().as_str(), decimal.scale(), cursor)?;
                } else {
                    self.write_float(value.as_float().ok_or_else(invalid_document)?)?;
                }
            }
            NumberView::Float(value) => self.write_float(value)?,
        }
        Ok(())
    }

    /// Writes one object key preceded by its container delimiter (`{` for the first field, `,` for every later field)
    /// and, when indenting, the line break between the two, in as few writes as the key's shape allows: a short,
    /// unescaped key batches delimiter + break + opening quote + key bytes + closing quote/separator into one step.
    ///
    /// The break slices are empty when compact, and `extend_from_slice` skips empty slices, so compact output keeps
    /// exactly the writes it always had.
    fn start_object_key(
        &mut self,
        input: &mut JsonEncodeInput<'_, '_, '_>,
        delimiter: &[u8],
    ) -> Result<(), CodecError> {
        let index = self.member_index()?;
        let key = input.object_key(index)?;
        let separator = self.style.indent.key_separator();
        // Nesting past the static fill cannot be spliced, so that case writes the delimiter and its break on their own
        // and leaves the batched writes below with nothing to splice.
        let mut delimiter = delimiter;
        let [newline, pad] = if let Some(slices) = self.break_slices(self.frames.len())? {
            slices
        } else {
            self.push(delimiter);
            self.push_break(self.frames.len())?;
            delimiter = b"";
            [b"".as_slice(), b"".as_slice()]
        };
        if key.len() <= TEXT_QUANTUM && escape_prefix_len(key.as_bytes(), self.style.ascii_output) == key.len() {
            self.bytes.extend_from_slice(delimiter);
            self.bytes.extend_from_slice(newline);
            self.bytes.extend_from_slice(pad);
            self.write_member_comments(input, index)?;
            self.bytes.extend_from_slice(b"\"");
            self.bytes.extend_from_slice(key.as_bytes());
            self.bytes.extend_from_slice(b"\"");
            self.bytes.extend_from_slice(separator);
            input.enter_object(index)?;
            self.phase = Phase::Value;
            return Ok(());
        }
        self.bytes.extend_from_slice(delimiter);
        self.bytes.extend_from_slice(newline);
        self.bytes.extend_from_slice(pad);
        self.write_member_comments(input, index)?;
        self.bytes.extend_from_slice(b"\"");
        if key.len() <= TEXT_QUANTUM {
            push_escaped_text(&mut self.bytes, key, self.style.ascii_output)?;
            self.bytes.extend_from_slice(b"\"");
            self.bytes.extend_from_slice(separator);
            input.enter_object(index)?;
            self.phase = Phase::Value;
        } else {
            self.phase = Phase::Text {
                target: TextTarget::Key,
                cursor: 0,
            };
        }
        Ok(())
    }

    /// Writes an integer number, whose canonical spelling is already its exact rendered form, resuming from `cursor`
    /// one `TEXT_QUANTUM` at a time.
    fn write_verbatim_number(&mut self, text: &str, cursor: usize) -> Result<(), CodecError> {
        let end = cursor.saturating_add(TEXT_QUANTUM).min(text.len());
        self.push(text.get(cursor..end).ok_or_else(invalid_document)?.as_bytes());
        if end == text.len() {
            self.phase = Phase::FinishValue;
        } else {
            self.phase = Phase::Number { cursor: end };
        }
        Ok(())
    }

    /// Writes a decimal number in the reference's exact `scientific-string form` form (`coefficient * 10^-scale`),
    /// reproducing the decimal output byte for byte: `0.1`, `10.250`, `1E+2`, `0.01`, `0.00`, `0E+5`, `1E-323`.
    ///
    /// The law itself lives in [`DecimalText`], which the engine's error-message renderer shares; the encoder owns only
    /// the resumption. Its pieces are the synthesized prefix (sign, plus `0.` and leading zeroes for a pure fraction),
    /// the coefficient digits (with an interior `.` for the point or scientific mantissa), and the `E±exponent`
    /// suffix. Only the coefficient run is unbounded, so the whole stream resumes from a single logical `cursor` one
    /// `TEXT_QUANTUM` at a time.
    fn write_decimal_scientific(&mut self, coefficient: &str, scale: i64, cursor: usize) -> Result<(), CodecError> {
        let text = DecimalText::new(coefficient, scale).ok_or_else(invalid_document)?;
        let pieces = text.pieces();
        let total: usize = pieces.iter().map(|piece| piece.len()).sum();
        let mut position = cursor;
        let mut budget = TEXT_QUANTUM;
        let mut base = 0;
        for piece in pieces {
            let end = base + piece.len();
            if budget > 0 && position < end {
                let within = position - base;
                let take = budget.min(piece.len() - within);
                self.push(&piece[within..within + take]);
                position += take;
                budget -= take;
            }
            base = end;
        }
        if position >= total {
            self.phase = Phase::FinishValue;
        } else {
            self.phase = Phase::Number { cursor: position };
        }
        Ok(())
    }

    /// Writes a computed binary64 value in the scientific-string float form. This is the COMPUTED-`Float` path: JSON
    /// literals decode to `Integer`/`Decimal` (rendered verbatim / by `write_decimal_scientific`), so a
    /// `NumberView::Float` here is always an arithmetic result. The placement law lives in [`format_binary64`], shared
    /// with the engine's error-message renderer, and it covers the non-finite spellings the number pipeline publishes:
    /// a NaN writes the `null` literal and an infinity writes the widest finite binary64 (`±1.7976931348623157e+308`).
    /// The value stays a number — `type` answers `"number"` for it — only its bytes are clamped, exactly as this
    /// encode law renders `nan` and `infinite`.
    fn write_float(&mut self, value: jqf_data::Float) -> Result<(), CodecError> {
        self.normalized_semantics = true;
        let rendered = format_binary64(value.get()).ok_or_else(|| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "JSON float formatting",
            })
        })?;
        self.push(rendered.as_str().as_bytes());
        self.phase = Phase::FinishValue;
        Ok(())
    }

    fn finish_value(&mut self, input: &mut JsonEncodeInput<'_, '_, '_>) -> Result<(), CodecError> {
        let Some(frame) = self.frames.as_mut_slice().last_mut() else {
            // The root value is complete. The codec-owned record terminator joins the SAME staging buffer here, before
            // any further offer, so payload and terminator publish as one atomic scope.
            if !self.framing.is_empty() {
                let framing = self.framing;
                self.push(framing);
            }
            self.state = EncodeState::InputFinished;
            return Ok(());
        };
        input.exit()?;
        frame.index = frame.index.checked_add(1).ok_or_else(overflow)?;
        let has_next = frame.index < frame.len;
        let kind = frame.kind;
        let index = frame.index;
        if has_next {
            match kind {
                FrameKind::Array => {
                    self.push(b",");
                    self.push_break(self.frames.len())?;
                    // The next element's leading comments, after the break, before the element's bytes.
                    if self.comments.is_some() {
                        let node = Self::array_item_handle(input, index)?;
                        self.write_comment_lines_before(node)?;
                    }
                    input.enter_array(index)?;
                    self.phase = Phase::Value;
                }
                FrameKind::Object => {
                    self.start_object_key(input, b",")?;
                }
            }
            return Ok(());
        }
        // The trailing-comma output arm: the `jsonc.trailing-jqf@1` profile writes a comma after the LAST member,
        // before the closing delimiter; every other profile writes the same bytes it always did.
        if self.trailing_commas {
            self.push(b",");
        }
        let frame = self.frames.pop().ok_or_else(invalid_document)?;
        // Popping first puts the stack at the PARENT's depth, which is where a closing bracket belongs: it lines up
        // with whatever opened it.
        self.push_break(self.frames.len())?;
        self.push(match frame.kind {
            FrameKind::Array => b"]",
            FrameKind::Object => b"}",
        });
        self.phase = Phase::FinishValue;
        Ok(())
    }

    /// Writes one scalar JSON has no native spelling for as canonical text, through the shared projection layer that
    /// also records the event.
    ///
    /// Projected text is escape-free by the sink's contract, so it goes between bare quotes with no escape scan. A ROOT
    /// projected value under `-r` follows the reference's raw arm for a string, because the projection IS the string
    /// JSON would otherwise have printed.
    ///
    /// That raw spelling needs no NUL guard, unlike the root-STRING arm's `--raw-output0` check: [`classify_scalar`]
    /// admits exactly two projections here — Bytes render as base64url (RFC 4648 §5 alphabet) and temporals render
    /// as ISO-8601 text — and neither alphabet contains a control byte, so the facade's NUL terminator can never
    /// collide with projected bytes. The debug assertion below pins that domain argument.
    fn write_projected_scalar(
        &mut self,
        scalar: &ScalarView<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let projection = classify_scalar(scalar, JSON_NATIVE, resources).ok_or_else(|| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "JSON declares no native spelling for a projectable scalar",
            })
        })?;
        let quoted = !self.root_prints_raw();
        if quoted {
            self.push(b"\"");
        }
        let written_from = self.bytes.len();
        projection.write(&mut TrackedProjectionSink::new(&mut self.bytes), resources)?;
        // Debug-only: the projection alphabets are NUL-free by construction, so this scan never fires; if it does, the
        // guard-free raw spelling above has lost its safety argument.
        debug_assert!(
            !self.bytes[written_from..].contains(&0),
            "projected scalar text carried a NUL byte",
        );
        if quoted {
            self.push(b"\"");
        }
        Ok(())
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// One object member's VALUE node handle, resolving without disturbing the encode cursor: descend, read, return.
    /// The member cache keeps the caller's later `enter_object` free.
    fn object_value_handle(
        input: &mut JsonEncodeInput<'_, '_, '_>,
        index: usize,
    ) -> Result<Option<NodeHandle>, CodecError> {
        input.enter_object(index)?;
        let node = match input.item()? {
            EncodeItem::Located { node, .. } => Some(node),
            EncodeItem::Owned(_) => None,
        };
        input.exit()?;
        Ok(node)
    }

    /// One array element's node handle, resolving without disturbing the encode cursor (the array analogue of
    /// [`Self::object_value_handle`]).
    fn array_item_handle(
        input: &mut JsonEncodeInput<'_, '_, '_>,
        index: usize,
    ) -> Result<Option<NodeHandle>, CodecError> {
        input.enter_array(index)?;
        let node = match input.item()? {
            EncodeItem::Located { node, .. } => Some(node),
            EncodeItem::Owned(_) => None,
        };
        input.exit()?;
        Ok(node)
    }

    /// One object member's leading comments: emitted between the member's line break and its key, as `// text` lines at
    /// the member's own depth.
    fn write_member_comments(
        &mut self,
        input: &mut JsonEncodeInput<'_, '_, '_>,
        index: usize,
    ) -> Result<(), CodecError> {
        if self.comments.is_none() {
            return Ok(());
        }
        let node = Self::object_value_handle(input, index)?;
        self.write_comment_lines_before(node)
    }

    /// Writes `node`'s leading comment lines as `// text` runs, each followed by a line break + indent at the current
    /// depth. The caller has already written the member/element's line break; the hook writes the comment lines plus a
    /// fresh break for the key/element that follows.
    ///
    /// A comment fact carries TEXT, not the form it was written in: a block comment's body arrives with its `/*` and
    /// `*/` already stripped, so a multi-line one is a single text holding line breaks. Emitting that text after one
    /// `// ` put every line after the first OUTSIDE the comment — a plain `.` over a file with a license header
    /// produced bytes this codec could not re-read. Each line of the text therefore gets its own `//`, which is the one
    /// form that cannot be broken by its own content.
    fn write_comment_lines_before(&mut self, node: Option<NodeHandle>) -> Result<(), CodecError> {
        let Some(node) = node else {
            return Ok(());
        };
        if self.comments.is_none() {
            return Ok(());
        }
        // Take the map so the line loop can push into staging without cloning the comment texts. Restored even if a
        // later write fails.
        let comments = self.comments.take();
        let result = (|| {
            let Some(ref map) = comments else {
                return Ok(());
            };
            let Some(texts) = map.get(&node) else {
                return Ok(());
            };
            for text in texts {
                for line in text.split('\n') {
                    self.push(b"//");
                    if !line.is_empty() {
                        self.push(b" ");
                        self.push(line.as_bytes());
                    }
                    self.push_comment_break(self.frames.len())?;
                }
            }
            Ok(())
        })();
        self.comments = comments;
        result
    }

    /// The line break a comment line occupies: `\n` plus the current depth's indent — written even in compact mode,
    /// because a comment line MUST end at a line break or it swallows the following key/element. In indented mode this
    /// is exactly [`Self::push_break`]'s bytes; the shared chunk loop in [`Self::push_newline_indent`] guarantees the
    /// two can never drift apart.
    fn push_comment_break(&mut self, depth: usize) -> Result<(), CodecError> {
        self.push_newline_indent(depth)
    }
}

impl EncoderSession for JsonEncoder {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn encode(
        &mut self,
        item: EncodeItem<'_, '_>,
        sink: &mut dyn ByteSink,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<PreservationReport, CodecError> {
        let mut cursor = JsonEncodeCursor::try_new(item);
        let mut json_input = JsonEncodeInput::new(&mut cursor);
        loop {
            if self.state == EncodeState::InputFinished {
                // Fold the facade suffix into this last staging write so payload and terminator publish as one hop.
                let trailer = context.item_trailer();
                if !trailer.is_empty() {
                    self.push(trailer);
                    context.consume_item_trailer();
                }
                if !self.bytes.is_empty() {
                    sink.write_all(&self.bytes, context.resources())?;
                    self.bytes.clear();
                }
                return Ok(self.report());
            }
            if self.bytes.len() >= OFFER_BYTES {
                sink.write_all(&self.bytes, context.resources())?;
                self.bytes.clear();
                continue;
            }
            let remaining = context.resources().remaining_work() as usize;
            match context.resources().admit_work_transitions(remaining.max(1))? {
                WorkAdmission::Pending => context.replenish_work()?,
                WorkAdmission::Granted(granted) => {
                    let mut used = 0usize;
                    for _ in 0..granted {
                        if matches!(self.state, EncodeState::InputFinished) || self.bytes.len() >= OFFER_BYTES {
                            break;
                        }
                        self.step(&mut json_input, context.resources())?;
                        used += 1;
                    }
                    if used < granted {
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "the grant never exceeds the remaining credit count, which is a u32, so the unused remainder fits"
                        )]
                        let unused = (granted - used) as u32;
                        context.resources().refund_work(unused);
                    }
                }
            }
        }
    }
}

impl JsonEncoder {
    const fn report(&self) -> PreservationReport {
        PreservationReport::new(
            if self.normalized_semantics {
                PreservationOutcome::Normalized
            } else {
                PreservationOutcome::Exact
            },
            // A dropped tag replaces only the EXACT reading: publishing a tagged payload bare is a canonicalization
            // (the value's meaning survives, the tag identity does not), so Exact would claim a preservation that did
            // not happen. `Omitted`/`Indeterminate` already state stronger non-preservation and stay.
            match self.facts {
                FactPreservation::Exact if self.dropped_tag => PreservationOutcome::Normalized,
                FactPreservation::Exact => PreservationOutcome::Exact,
                FactPreservation::Omitted => PreservationOutcome::Omitted,
                FactPreservation::Indeterminate => PreservationOutcome::Indeterminate,
            },
            PreservationOutcome::Exact,
            PreservationOutcome::Normalized,
        )
    }
}

#[derive(Clone, Copy)]
enum JsonRef<'item, 'source> {
    Owned(&'item Value),
    Located(ValueView<'item, 'source>),
}

impl<'item, 'source> JsonRef<'item, 'source> {
    fn from_item(item: EncodeItem<'item, 'source>) -> Result<Self, CodecError> {
        // Every tag layer is stripped FIRST, in both authorities: JSON spells no tag, so what it encodes is the payload
        // the publish law names.
        match item.untagged()? {
            EncodeItem::Owned(value) => Ok(Self::Owned(value)),
            EncodeItem::Located { product, node } => product
                .document()
                .value_view(node)
                .map(Self::Located)
                .map_err(data_error),
        }
    }
    fn scalar(self) -> Result<Option<ScalarView<'item>>, CodecError> {
        match self {
            Self::Owned(value) => Ok(ScalarView::from_value(value)),
            Self::Located(value) => value.scalar().map_err(data_error),
        }
    }
    fn string(self) -> Result<Option<&'item str>, CodecError> {
        Ok(match self.scalar()? {
            Some(ScalarView::String(value)) => Some(value),
            _ => None,
        })
    }
    fn number(self) -> Result<Option<NumberView<'item>>, CodecError> {
        Ok(match self.scalar()? {
            Some(ScalarView::Number(value)) => Some(value),
            _ => None,
        })
    }
    fn array_len(self) -> Result<Option<usize>, CodecError> {
        match self {
            Self::Owned(Value::Array(value)) => Ok(Some(value.len())),
            Self::Owned(_) => Ok(None),
            Self::Located(value) => Ok(value.array().map_err(data_error)?.map(jqf_data::ArrayView::len)),
        }
    }
    fn object_len(self) -> Result<Option<usize>, CodecError> {
        match self {
            Self::Owned(Value::Object(value)) => Ok(Some(value.len())),
            Self::Owned(_) => Ok(None),
            Self::Located(value) => Ok(value.object().map_err(data_error)?.map(jqf_data::ObjectView::len)),
        }
    }
}

/// Whether one byte of a JSON string body must be written as an escape.
///
/// The ASCII set is [`crate::json_escape_byte`]. Under `ascii`, every codepoint at or above `0x80` is also escaped
/// (`\uXXXX` or a surrogate pair).
fn push_escaped(bytes: &mut Vec<u8>, character: char, ascii: bool) {
    if character.is_ascii() {
        let (len, escape) = crate::json_escape_byte(character as u8);
        if len > 0 {
            bytes.extend_from_slice(&escape[..len as usize]);
            return;
        }
    }
    if ascii && (character as u32) >= 0x80 {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        // Lowercase hex; a supplementary character is its surrogate pair (`"😀"` → `"\ud83d\ude00"`).
        let codepoint = character as u32;
        if codepoint <= 0xFFFF {
            let value = codepoint;
            bytes.extend_from_slice(&[
                b'\\',
                b'u',
                HEX[((value >> 12) & 15) as usize],
                HEX[((value >> 8) & 15) as usize],
                HEX[((value >> 4) & 15) as usize],
                HEX[(value & 15) as usize],
            ]);
            return;
        }
        let value = codepoint - 0x10000;
        // A supplementary character is its surrogate PAIR, each half spelled by the same six-byte escape.
        let push_u16_hex = |bytes: &mut Vec<u8>, value: u16| {
            bytes.extend_from_slice(&[
                b'\\',
                b'u',
                HEX[((value >> 12) & 15) as usize],
                HEX[((value >> 8) & 15) as usize],
                HEX[((value >> 4) & 15) as usize],
                HEX[(value & 15) as usize],
            ]);
        };
        // Both halves fit u16 by construction (`value <= 0xFFFFF`).
        push_u16_hex(bytes, u16::try_from(0xD800 + (value >> 10)).expect("surrogate high"));
        push_u16_hex(bytes, u16::try_from(0xDC00 + (value & 0x3FF)).expect("surrogate low"));
        return;
    }
    let mut encoded = [0_u8; 4];
    bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
}

pub(crate) fn push_escaped_text(bytes: &mut Vec<u8>, text: &str, ascii: bool) -> Result<(), CodecError> {
    let source = text.as_bytes();
    let mut start = 0;
    while start < source.len() {
        let run = escape_prefix_len(&source[start..], ascii);
        if run == source.len() - start {
            // The whole remainder is plain: emit it and stop. This is also the all-plain fast path when `start == 0`,
            // exactly as the `TEXT_QUANTUM` arms used to spell it.
            bytes.extend_from_slice(&source[start..]);
            break;
        }
        if run > 0 {
            bytes.extend_from_slice(&source[start..start + run]);
        }
        // The next byte is either a `needs_escape` ASCII byte (single byte, its own char) or — only reachable under
        // ascii mode, because the default escape set is all-ASCII — the leading byte of a non-ASCII codepoint, which
        // must be decoded WHOLE and advanced past its full byte width.
        if source[start + run] < 0x80 {
            push_escaped(bytes, char::from(source[start + run]), ascii);
            start += run + 1;
        } else {
            let rest = &text[start + run..];
            let character = rest.chars().next().ok_or_else(invalid_document)?;
            push_escaped(bytes, character, ascii);
            start += run + character.len_utf8();
        }
    }
    Ok(())
}

pub(crate) fn data_error(_: jqf_data::DataError) -> CodecError {
    invalid_document()
}
fn invalid_document() -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation {
        contract: "JSON encoder authoritative value",
    })
}
fn overflow() -> CodecError {
    CodecError::new(CodecFailureKind::Overflow)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use jqf_codec_core::{
        CodecRunContext, EncodeItem, ErasedEncoderSession, PreservationOutcome, PreservationRequest, VecByteSink,
    };
    use jqf_data::{Float, Number, ObjectBuilder, ObjectKey, TagId, Value};

    use super::{JsonEncodeOptions, JsonEncoder};
    use crate::test_support;

    /// Encodes one owned value through the full session path (the same drive `edit.rs`'s `render_value_bytes` uses) and
    /// returns the published bytes with the item's preservation report.
    fn encode_owned(
        value: &Value,
        style: JsonEncodeOptions,
    ) -> (alloc::vec::Vec<u8>, jqf_codec_core::PreservationReport) {
        let mut resources = test_support::resources();
        let encoder = JsonEncoder::try_new(EncodeItem::Owned(value), b"", b"", style, &resources).expect("encoder");
        let mut session =
            ErasedEncoderSession::try_new(EncodeItem::Owned(value), PreservationRequest::Report, || Ok(encoder))
                .expect("session");
        let mut bytes = Vec::new();
        let mut sink = VecByteSink::new(&mut bytes);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let report = session.encode(&mut sink, &mut run).expect("encode");
        (bytes, report)
    }

    /// A tagged value publishes its payload bare, so the report must say the tag axis was NORMALIZED — Exact would
    /// claim a tag preservation that did not happen. The semantic axis stays Exact: the payload's meaning is untouched.
    #[test]
    fn tagged_object_publish_reports_the_dropped_tag_normalized() {
        let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(
                ObjectKey::try_from_str("amount").expect("key"),
                Value::try_string("1").expect("value"),
            )
            .expect("insert");
        let payload = Value::Object(builder.try_finish().expect("object"));
        let tag = TagId::try_new_unaccounted("!money").expect("tag");
        let value = Value::try_tagged(tag, payload).expect("tagged");
        let (bytes, report) = encode_owned(&value, JsonEncodeOptions::default());
        assert_eq!(bytes, br#"{"amount":"1"}"#);
        assert_eq!(report.semantic_values(), PreservationOutcome::Exact);
        assert_eq!(report.tags_and_facts(), PreservationOutcome::Normalized);
    }

    /// A non-finite float has no JSON literal; rendering it clamps the BYTES (`null` for NaN), so the semantic axis
    /// reads Normalized while the tag axis stays Exact.
    #[test]
    fn non_finite_float_render_reports_semantic_values_normalized() {
        let value = Value::Number(Number::float(Float::new(f64::NAN)));
        let (bytes, report) = encode_owned(&value, JsonEncodeOptions::default());
        assert_eq!(bytes, b"null");
        assert_eq!(report.semantic_values(), PreservationOutcome::Normalized);
        assert_eq!(report.tags_and_facts(), PreservationOutcome::Exact);
    }

    /// A plain publish drops and normalizes nothing: both axes read Exact.
    #[test]
    fn plain_publish_reports_both_axes_exact() {
        let value = Value::try_string("plain").expect("string");
        let (bytes, report) = encode_owned(&value, JsonEncodeOptions::default());
        assert_eq!(bytes, br#""plain""#);
        assert_eq!(report.semantic_values(), PreservationOutcome::Exact);
        assert_eq!(report.tags_and_facts(), PreservationOutcome::Exact);
    }

    /// The `-a` ascii arm spells a supplementary character as its SURROGATE PAIR — U+1F600 renders as the twelve
    /// bytes `\ud83d\ude00`, lowercase hex, high half first. This pins the pair branch end to end.
    #[test]
    fn ascii_output_escapes_an_astral_character_as_its_surrogate_pair() {
        let value = Value::try_string("\u{1F600}").expect("string");
        let style = JsonEncodeOptions {
            ascii_output: true,
            ..JsonEncodeOptions::default()
        };
        let (bytes, _) = encode_owned(&value, style);
        assert_eq!(bytes, br#""\ud83d\ude00""#);
    }
}
