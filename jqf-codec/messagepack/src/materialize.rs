//! Materializing the span skeleton into a document.
//!
//! Every item builds the node its kind demands: integers → `Integer`, `bin` → `Bytes`, `str` → `String`,
//! float32/64 → `Float` (through the promoted core `widen_f32`), `nil`/`false`/`true` → core, arrays → `Array`,
//! maps → `Object` ONLY when every key is a `str` (jqf's own first-position/ final-value duplicate law) and otherwise
//! `UnsupportedRepresentation`. Every node's authored span is recorded (the edit lane's per-item span binding: the span
//! names the item's exact header-through-payload bytes).
//!
//! ## Extensions and the timestamp
//!
//! A non-timestamp extension projects to `Value::Tagged { tag: "msgpack:ext:<n>", payload: Bytes }` through the node's
//! ordinary intrinsic-tag slot (canonical signed decimal, codec-validated — `jqf-data` gains NO `MessagePack`
//! variant). Extension `-1` with a valid timestamp payload (projected by the scan) becomes UTC `OffsetDateTime` (offset
//! zero) retaining the intrinsic tag `"msgpack:ext:-1"` when the instant falls in the core year range, and the exact
//! `{seconds, nanoseconds}` tagged object otherwise. An INVALID reserved `-1` payload — a length or nanoseconds the
//! scan declined to project — is refused here with `UnsupportedRepresentation` — fail strict semantic decode.

use jqf_codec_core::CodecError;
use jqf_data::{
    AccountedDocumentBuilder, AccountedIntrinsicTag, AccountedOccurrenceKey, AccountedSemanticNode,
    AuthoritativeEmptyFamilies, BuilderCoverage, ContainerSpanKind, DataError, DiagnosticCoverage, Document,
    DocumentCapabilityFamily, DocumentSchemaRecipe, FractionalSecond, KnownUtcOffset, LocalDateTime, LocalOwnerRef,
    LocalTime, NodeId, OffsetDateTime, PreparedDocumentSchema, PreparedNodeKind, PreparedOccurrenceRole,
    PreparedSemanticNode, UtcOffset, ValueKind, civil_from_epoch,
};
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, Span};

use crate::error;
use crate::options::Dialect;
use crate::scan::{ItemKind, Skeleton};

/// The four node kinds of the `MessagePack` document.
const SCALAR_KIND: &str = "messagepack.scalar@1";
const ARRAY_KIND: &str = "messagepack.array@1";
const OBJECT_KIND: &str = "messagepack.object@1";
const TAG_KIND: &str = "messagepack.tag@1";
/// The three occurrence roles of the `MessagePack` document.
const ITEM_ROLE: &str = "messagepack.item@1";
const MEMBER_ROLE: &str = "messagepack.member@1";
const TAG_PAYLOAD_ROLE: &str = "messagepack.tag-payload@1";

/// The schema's node-kind identities, shared with every site that spells a `MessagePack` recipe (the negative-outcome
/// path included) so the identities live in exactly one place.
pub(crate) const NODE_KINDS: &[&str] = &[SCALAR_KIND, ARRAY_KIND, OBJECT_KIND, TAG_KIND];
/// The schema's occurrence-role identities, shared like [`NODE_KINDS`].
pub(crate) const OCCURRENCE_ROLES: &[&str] = &[ITEM_ROLE, MEMBER_ROLE, TAG_PAYLOAD_ROLE];

fn schema_recipe(dialect: Dialect) -> Result<DocumentSchemaRecipe<'static>, DataError> {
    DocumentSchemaRecipe::try_new(
        crate::FORMAT_ID,
        Some(dialect.id()),
        NODE_KINDS,
        OCCURRENCE_ROLES,
        &[],
        &[],
    )
}

/// Records one authored source span on the builder (the TOML `record_authored_span` shape): the semantic is stored
/// exactly as without the span; the span only names the authored bytes the edit lane echoes verbatim or replaces.
#[allow(
    unsafe_code,
    reason = "the late-sealing span contract is an unsafe fn by jqf-data's design; the spans were produced by this codec's own scan over the exact session-owned source authority"
)]
fn maybe_span(
    builder: &mut AccountedDocumentBuilder<'static>,
    node: NodeId,
    span: Span,
    record_spans: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    if record_spans {
        record_authored_span(builder, node, span, resources)?;
    }
    Ok(())
}

fn record_authored_span(
    builder: &mut AccountedDocumentBuilder<'static>,
    node: NodeId,
    span: Span,
    resources: &mut ResourceContext<'_>,
) -> Result<(), CodecError> {
    // SAFETY: the span was produced by this codec's own scan over the exact
    // source authority the session binds to the builder, and its bytes are
    // the validated item's header-through-payload region — the `record_authored_span` contract.
    unsafe { builder.record_authored_span(node, span, resources) }.map_err(map_data)
}

/// Builds the complete document from the validated skeleton. Every node carries an authored span, so the document must
/// seal its source before finishing (`spans_committed` is always true here).
pub(crate) fn build_document(
    skeleton: &Skeleton,
    dialect: Dialect,
    source: ResolvedSource<'_>,
    lazy_frontier: Option<usize>,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    build_document_with_spans(skeleton, dialect, source, true, lazy_frontier, resources)
}

pub(crate) fn build_document_with_spans(
    skeleton: &Skeleton,
    dialect: Dialect,
    source: ResolvedSource<'_>,
    record_spans: bool,
    lazy_frontier: Option<usize>,
    resources: &mut ResourceContext<'_>,
) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
    let recipe = schema_recipe(dialect).map_err(map_data)?;
    let (mut builder, schema) = AccountedDocumentBuilder::try_new_prepared_with_coverage(
        &recipe,
        // Minimal semantic coverage: attached facts are not demanded, and the source binding and span recording are
        // unconditional because every document retains its source authority for the edit lane.
        BuilderCoverage::minimal_semantic(),
    )
    .map_err(map_data)?;
    builder.set_authoritative_empty_families(AuthoritativeEmptyFamilies::from_family(
        DocumentCapabilityFamily::Attributes,
    ));
    builder.set_diagnostic_coverage(DiagnosticCoverage::NotRequested);
    // A positive frontier binds the dialect's span materializer, so a deferred container re-decodes under the same
    // grammar that validated it.
    if lazy_frontier.is_some() {
        builder.bind_span_materializer(crate::lazy::span_materializer(dialect));
    }
    let handles = Handles::resolve(&schema)?;
    let root = build_value(
        &mut builder,
        &schema,
        handles,
        skeleton,
        source,
        skeleton.root,
        record_spans,
        lazy_frontier,
        0,
        resources,
    )?;
    Ok((builder, root))
}

/// Re-decodes one already-validated `MessagePack` item as a fresh document. Nested materialization is eager: the
/// frontier is a request-level knob and this path never sets it, so re-entry terminates.
pub(crate) fn decode_span_document(
    source: ResolvedSource<'_>,
    dialect: Dialect,
    resources: &mut ResourceContext<'_>,
) -> Result<Document<'static>, CodecError> {
    let mut run = jqf_codec_core::CodecRunContext::new(resources);
    run.set_cooperative_credits(4_096);
    let skeleton = crate::scan::scan(source, dialect, &mut run)?;
    let (builder, root) = build_document_with_spans(&skeleton, dialect, source, false, None, run.resources())?;
    builder.finish(root, run.resources()).map_err(map_data)
}

/// Commits one container as a span of the sealed source: the node names the validated extent and the subtree's nodes
/// are never built.
#[allow(
    unsafe_code,
    reason = "the bound-span contract is an unsafe fn by jqf-data's design; the span was produced by this codec's own scan over the exact session-owned source authority"
)]
fn defer_container(
    builder: &mut AccountedDocumentBuilder<'static>,
    schema: &PreparedDocumentSchema,
    kind: PreparedNodeKind,
    span: Span,
    container: ContainerSpanKind,
    resources: &ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    // SAFETY: the scan proved the span is one complete, already-validated
    // container item inside the same immutable source authority this session
    // retains through publication; admission proves containment against the seal.
    unsafe { builder.add_prepared_bound_container_span_node(schema, kind, span, container, resources) }
        .map_err(map_data)
}

fn defer_at(frontier: Option<usize>, depth: usize) -> bool {
    frontier.is_some_and(|threshold| depth >= threshold)
}

#[derive(Clone, Copy)]
struct Handles {
    scalar: PreparedNodeKind,
    array: PreparedNodeKind,
    object: PreparedNodeKind,
    item: PreparedOccurrenceRole,
    member: PreparedOccurrenceRole,
}

impl Handles {
    fn resolve(schema: &PreparedDocumentSchema) -> Result<Self, CodecError> {
        // The tag-layer slots are resolved and discarded: resolving them IS the contract check that the prepared schema
        // carries them. Their builds stay on the dynamic API by identity (TAG_KIND / TAG_PAYLOAD_ROLE), so no handle
        // for them is stored.
        let _ = schema.node_kind(3).ok_or_else(data_contract)?;
        let _ = schema.occurrence_role(2).ok_or_else(data_contract)?;
        Ok(Self {
            scalar: schema.node_kind(0).ok_or_else(data_contract)?,
            array: schema.node_kind(1).ok_or_else(data_contract)?,
            object: schema.node_kind(2).ok_or_else(data_contract)?,
            item: schema.occurrence_role(0).ok_or_else(data_contract)?,
            member: schema.occurrence_role(1).ok_or_else(data_contract)?,
        })
    }
}

fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("MessagePack prepared schema handles")
}

#[allow(
    clippy::too_many_lines,
    reason = "one item-kind match building the document nodes; the arms are short and share the span-recording tail"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "one item walk: builder, schema, skeleton, source, and the span/frontier ledger are the whole shape"
)]
fn build_value(
    builder: &mut AccountedDocumentBuilder<'static>,
    schema: &PreparedDocumentSchema,
    handles: Handles,
    skeleton: &Skeleton,
    source: ResolvedSource<'_>,
    item_index: usize,
    record_spans: bool,
    lazy_frontier: Option<usize>,
    depth: usize,
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let item = &skeleton.items[item_index];
    match &item.kind {
        ItemKind::Null => {
            let node = builder
                .add_prepared_node(schema, handles.scalar, PreparedSemanticNode::Null, resources)
                .map_err(map_data)?;
            maybe_span(builder, node, item.span, record_spans, resources)?;
            Ok(node)
        }
        ItemKind::Bool(value) => {
            let node = builder
                .add_prepared_node(schema, handles.scalar, PreparedSemanticNode::Bool(*value), resources)
                .map_err(map_data)?;
            maybe_span(builder, node, item.span, record_spans, resources)?;
            Ok(node)
        }
        ItemKind::Integer(value) => {
            let integer = value.to_integer();
            let text = builder.store_text(integer.as_str(), resources).map_err(map_data)?;
            let node = builder
                .add_prepared_stored_integer_node(schema, handles.scalar, text, resources)
                .map_err(map_data)?;
            maybe_span(builder, node, item.span, record_spans, resources)?;
            Ok(node)
        }
        ItemKind::Float(value) => {
            let node = builder
                .add_prepared_node(
                    schema,
                    handles.scalar,
                    PreparedSemanticNode::Float(jqf_data::Float::new(*value)),
                    resources,
                )
                .map_err(map_data)?;
            maybe_span(builder, node, item.span, record_spans, resources)?;
            Ok(node)
        }
        ItemKind::Str(payload) => {
            let payload = payload_slice(source, *payload);
            let text = core::str::from_utf8(payload).map_err(|_| {
                error::unrepresentable(
                    source,
                    item.span.start() as usize,
                    "a str payload is not valid UTF-8 (messagepack.wire@1 has no semantic text document)",
                )
            })?;
            let text = builder.store_text(text, resources).map_err(map_data)?;
            let node = builder
                .add_prepared_stored_string_node(schema, handles.scalar, text, resources)
                .map_err(map_data)?;
            maybe_span(builder, node, item.span, record_spans, resources)?;
            Ok(node)
        }
        ItemKind::Bin(payload) => {
            let payload = payload_slice(source, *payload);
            let node = builder
                .add_node(SCALAR_KIND, AccountedSemanticNode::Bytes(payload), None, resources)
                .map_err(map_data)?;
            maybe_span(builder, node, item.span, record_spans, resources)?;
            Ok(node)
        }
        ItemKind::Array(children) => {
            if defer_at(lazy_frontier, depth) {
                return defer_container(
                    builder,
                    schema,
                    handles.array,
                    item.span,
                    ContainerSpanKind::Array,
                    resources,
                );
            }
            let node = builder
                .add_prepared_node(
                    schema,
                    handles.array,
                    PreparedSemanticNode::Array(handles.item),
                    resources,
                )
                .map_err(map_data)?;
            maybe_span(builder, node, item.span, record_spans, resources)?;
            for child in children {
                let child_node = build_value(
                    builder,
                    schema,
                    handles,
                    skeleton,
                    source,
                    *child,
                    record_spans,
                    lazy_frontier,
                    depth + 1,
                    resources,
                )?;
                builder
                    .add_prepared_occurrence(
                        schema,
                        LocalOwnerRef::Node(node),
                        handles.item,
                        None,
                        child_node,
                        resources,
                    )
                    .map_err(map_data)?;
            }
            Ok(node)
        }
        ItemKind::Map(pairs) => {
            if defer_at(lazy_frontier, depth) {
                return defer_container(
                    builder,
                    schema,
                    handles.object,
                    item.span,
                    ContainerSpanKind::Object,
                    resources,
                );
            }
            let node = builder
                .add_prepared_node(
                    schema,
                    handles.object,
                    PreparedSemanticNode::Object(handles.member),
                    resources,
                )
                .map_err(map_data)?;
            maybe_span(builder, node, item.span, record_spans, resources)?;
            for pair in pairs.chunks_exact(2) {
                let key_item = &skeleton.items[pair[0]];
                let ItemKind::Str(key_span) = &key_item.kind else {
                    return Err(error::unrepresentable(
                        source,
                        key_item.span.start() as usize,
                        "a map key that is not a str is unrepresentable",
                    ));
                };
                let key_bytes = payload_slice(source, *key_span);
                let key = core::str::from_utf8(key_bytes).map_err(|_| {
                    error::unrepresentable(
                        source,
                        key_item.span.start() as usize,
                        "a map key that is not valid UTF-8 is unrepresentable",
                    )
                })?;
                let value_node = build_value(
                    builder,
                    schema,
                    handles,
                    skeleton,
                    source,
                    pair[1],
                    record_spans,
                    lazy_frontier,
                    depth + 1,
                    resources,
                )?;
                builder
                    .add_prepared_occurrence(
                        schema,
                        LocalOwnerRef::Node(node),
                        handles.member,
                        Some(AccountedOccurrenceKey::Text(key)),
                        value_node,
                        resources,
                    )
                    .map_err(map_data)?;
            }
            Ok(node)
        }
        ItemKind::Ext { ty, payload } => {
            if *ty == -1 {
                return Err(error::unrepresentable(
                    source,
                    item.span.start() as usize,
                    "an invalid reserved extension -1 (timestamp) payload is unrepresentable",
                ));
            }
            build_tagged_ext(
                builder,
                item,
                *ty,
                payload_slice(source, *payload),
                record_spans,
                resources,
            )
        }
        ItemKind::Timestamp { seconds, nanoseconds } => {
            if let Some(datetime) = epoch_to_offset(*seconds, *nanoseconds) {
                // In-core-range: UTC OffsetDateTime retaining the intrinsic `msgpack:ext:-1` tag as a core intrinsic
                // fact (the tagged-values law's resolved-core-tag pattern).
                let node = builder
                    .add_node(
                        SCALAR_KIND,
                        AccountedSemanticNode::OffsetDateTime(&datetime),
                        Some(AccountedIntrinsicTag::Core {
                            tag: "msgpack:ext:-1",
                            kind: ValueKind::OffsetDateTime,
                        }),
                        resources,
                    )
                    .map_err(map_data)?;
                maybe_span(builder, node, item.span, record_spans, resources)?;
                Ok(node)
            } else {
                // Out-of-range: the exact `{seconds, nanoseconds}` object, tagged `msgpack:ext:-1` (never a silent
                // rounding).
                build_out_of_range_timestamp(builder, item, *seconds, *nanoseconds, record_spans, resources)
            }
        }
    }
}

/// The out-of-range timestamp: `Value::Tagged { tag: "msgpack:ext:-1", payload: {seconds, nanoseconds} }` — the exact
/// ordered Integer members in a tag-layer-wrapped object, the shape the encoder's `-1` reverse-mapping reads back.
fn build_out_of_range_timestamp(
    builder: &mut AccountedDocumentBuilder<'static>,
    item: &crate::scan::Item,
    seconds: i64,
    nanoseconds: u32,
    record_spans: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let seconds_int = jqf_data::Integer::from_i64(seconds);
    let nanos_int = jqf_data::Integer::from_i64(i64::from(nanoseconds));
    let object = builder
        .add_node(
            OBJECT_KIND,
            AccountedSemanticNode::Object {
                member_role: MEMBER_ROLE,
            },
            None,
            resources,
        )
        .map_err(map_data)?;
    let seconds_node = builder
        .add_node(
            SCALAR_KIND,
            AccountedSemanticNode::Integer(seconds_int.as_str()),
            None,
            resources,
        )
        .map_err(map_data)?;
    builder
        .add_occurrence(
            LocalOwnerRef::Node(object),
            MEMBER_ROLE,
            Some(AccountedOccurrenceKey::Text("seconds")),
            seconds_node,
            resources,
        )
        .map_err(map_data)?;
    let nanos_node = builder
        .add_node(
            SCALAR_KIND,
            AccountedSemanticNode::Integer(nanos_int.as_str()),
            None,
            resources,
        )
        .map_err(map_data)?;
    builder
        .add_occurrence(
            LocalOwnerRef::Node(object),
            MEMBER_ROLE,
            Some(AccountedOccurrenceKey::Text("nanoseconds")),
            nanos_node,
            resources,
        )
        .map_err(map_data)?;
    let layer = builder
        .add_node(
            TAG_KIND,
            AccountedSemanticNode::Unrepresentable,
            Some(AccountedIntrinsicTag::Tagged("msgpack:ext:-1")),
            resources,
        )
        .map_err(map_data)?;
    builder
        .add_occurrence(LocalOwnerRef::Node(layer), TAG_PAYLOAD_ROLE, None, object, resources)
        .map_err(map_data)?;
    maybe_span(builder, layer, item.span, record_spans, resources)?;
    Ok(layer)
}

/// Builds a `Value::Tagged { tag: "msgpack:ext:<n>", payload: Bytes }` node pair: the ordinary intrinsic-tag slot wraps
/// the byte payload through the tag-payload role, exactly the shape the engine's tag-layer materialization reads back.
fn build_tagged_ext(
    builder: &mut AccountedDocumentBuilder<'static>,
    item: &crate::scan::Item,
    ty: i8,
    payload: &[u8],
    record_spans: bool,
    resources: &mut ResourceContext<'_>,
) -> Result<NodeId, CodecError> {
    let mut tag_buf = [0u8; 32];
    let tag = write_ext_tag(&mut tag_buf, ty);
    let payload_node = builder
        .add_node(SCALAR_KIND, AccountedSemanticNode::Bytes(payload), None, resources)
        .map_err(map_data)?;
    let layer = builder
        .add_node(
            TAG_KIND,
            AccountedSemanticNode::Unrepresentable,
            Some(AccountedIntrinsicTag::Tagged(tag)),
            resources,
        )
        .map_err(map_data)?;
    builder
        .add_occurrence(
            LocalOwnerRef::Node(layer),
            TAG_PAYLOAD_ROLE,
            None,
            payload_node,
            resources,
        )
        .map_err(map_data)?;
    // The tag layer's span is the whole item; the payload node's span is the same bytes (the layer reads back as one
    // Value::Tagged). The spans are recorded in NODE-CREATION order (payload first, then the layer that wraps it) —
    // the `record_authored_span` order law.
    maybe_span(builder, payload_node, item.span, record_spans, resources)?;
    maybe_span(builder, layer, item.span, record_spans, resources)?;
    Ok(layer)
}

fn payload_slice(source: ResolvedSource<'_>, payload: Span) -> &[u8] {
    &source.bytes()[payload.start() as usize..payload.end() as usize]
}

fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "MessagePack builder rejected document construction")
}

/// Writes `msgpack:ext:{n}` into `buf` without allocating.
fn write_ext_tag(buf: &mut [u8; 32], ty: i8) -> &str {
    const PREFIX: &[u8] = b"msgpack:ext:";
    buf[..PREFIX.len()].copy_from_slice(PREFIX);
    let mut offset = PREFIX.len();
    let magnitude = if ty < 0 {
        buf[offset] = b'-';
        offset += 1;
        ty.unsigned_abs() as u64
    } else {
        ty as u64
    };
    let mut digits = [0u8; 3];
    let mut start = digits.len();
    let mut value = magnitude;
    if value == 0 {
        start -= 1;
        digits[start] = b'0';
    } else {
        while value > 0 {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
        }
    }
    let written = digits.len() - start;
    buf[offset..offset + written].copy_from_slice(&digits[start..]);
    core::str::from_utf8(&buf[..offset + written]).unwrap_or("")
}

/// The first representable second: `0000-01-01T00:00:00Z`.
const MIN_SECONDS: i64 = -62_167_219_200;
/// The last representable second: `9999-12-31T23:59:59Z`.
const MAX_SECONDS: i64 = 253_402_300_799;

/// Projects a timestamp `(seconds, nanoseconds)` to a zero-offset date-time, or `None` when the instant falls outside
/// the core year range. The fraction is normalized to the format-neutral rule: the nine-digit zero-padded decimal of
/// the nanoseconds (zero is the empty fraction).
pub(crate) fn epoch_to_offset(seconds: i64, nanoseconds: u32) -> Option<OffsetDateTime> {
    if !(MIN_SECONDS..=MAX_SECONDS).contains(&seconds) {
        return None;
    }
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400) as u32;
    let date = civil_from_epoch(days)?;
    let fraction = if nanoseconds == 0 {
        FractionalSecond::parse("").ok()?
    } else {
        FractionalSecond::parse(&alloc::format!("{nanoseconds:09}")).ok()?
    };
    let time = LocalTime::new(
        (second_of_day / 3600) as u8,
        ((second_of_day % 3600) / 60) as u8,
        (second_of_day % 60) as u8,
        fraction,
    )?;
    Some(OffsetDateTime {
        local: LocalDateTime { date, time },
        offset: UtcOffset::KnownSeconds(KnownUtcOffset::new(0)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use jqf_codec_core::{
        AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecFailureKind, CodecRunContext,
        DecodeRequest, DemandClause, DiagnosticPolicy, ValidationMode,
    };
    use jqf_data::{DialectId, Value};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    fn decode(bytes: &[u8]) -> Result<Value, CodecFailureKind> {
        let mut resources = crate::test_support::resources();
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(97), SourceKind::Input),
            "materialize.test",
            bytes,
            0,
        );
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source,
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::MESSAGEPACK_UTF8_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .map_err(|e| e.kind())?;
        let mut demand = CodecDemand::try_new(&resources);
        demand.try_insert(&DemandClause::SemanticRoot).expect("root");
        demand.try_insert(&DemandClause::ValueShape).expect("shape");
        let requirement = AccessRequirement::try_whole(
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).map_err(|e| e.kind())?;
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("full document")
        };
        Ok(product
            .document()
            .materialize_root(&mut resources)
            .expect("materialize"))
    }

    /// The core year range is inclusive: the first and last representable seconds project to `OffsetDateTime`; one
    /// second outside does not.
    #[test]
    fn epoch_to_offset_projects_only_inside_the_core_year_range() {
        assert!(epoch_to_offset(MIN_SECONDS, 0).is_some());
        assert!(epoch_to_offset(MAX_SECONDS, 0).is_some());
        assert!(epoch_to_offset(MIN_SECONDS - 1, 0).is_none());
        assert!(epoch_to_offset(MAX_SECONDS + 1, 0).is_none());
    }

    #[test]
    fn an_in_range_timestamp_materializes_as_offset_datetime() {
        // timestamp32 for epoch second 1.
        let value = decode(&[0xd6, 0xff, 0x00, 0x00, 0x00, 0x01]).expect("in-range");
        assert!(matches!(value, Value::OffsetDateTime(_)));
    }

    #[test]
    fn an_out_of_range_timestamp_materializes_as_the_tagged_seconds_object() {
        // timestamp96: seconds = MAX_SECONDS + 1, nanoseconds = 0.
        let seconds = MAX_SECONDS + 1;
        let mut bytes = vec![0xc7, 0x0c, 0xff, 0, 0, 0, 0];
        bytes.extend_from_slice(&seconds.to_be_bytes());
        let value = decode(&bytes).expect("out-of-range decodes");
        let Value::Tagged { tag, payload } = value else {
            panic!("expected a tag layer, got {value:?}");
        };
        assert_eq!(tag.as_str(), "msgpack:ext:-1");
        let Value::Object(object) = &*payload else {
            panic!("expected the seconds object");
        };
        let mut entries = object.iter();
        let first = entries.next().expect("seconds");
        let second = entries.next().expect("nanoseconds");
        assert_eq!(first.key(), "seconds");
        assert_eq!(second.key(), "nanoseconds");
    }

    #[test]
    fn an_unknown_ext_materializes_as_tagged_bytes() {
        let value = decode(&[0xc7, 0x02, 0x07, 0xaa, 0xbb]).expect("ext");
        let Value::Tagged { tag, payload } = value else {
            panic!("expected a tag layer, got {value:?}");
        };
        assert_eq!(tag.as_str(), "msgpack:ext:7");
        let Value::Bytes(bytes) = &*payload else {
            panic!("expected bytes");
        };
        assert_eq!(bytes.as_ref(), &[0xaa, 0xbb]);
    }

    #[test]
    fn an_invalid_reserved_timestamp_payload_is_refused() {
        // ext -1 with a 3-byte payload: scan accepts, materialize refuses.
        let error = decode(&[0xc7, 0x03, 0xff, 0, 0, 0]).expect_err("invalid -1");
        assert_eq!(error, CodecFailureKind::UnsupportedRepresentation);
    }
}
