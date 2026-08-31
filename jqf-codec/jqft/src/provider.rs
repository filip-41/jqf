//! jqft-family decoder provider and access session.
//!
//! Route inventory: two advertised slots for both text formats (`jqft`, `jqfjson`) — slot 0 Whole/`CompleteDocument`
//! and slot 1 Exact/`Located`. The binary `jqfb` image advertises the same table in `jqfb_decode.rs` (its Exact slot
//! is the node-table walk). Attribute demand is not Direct on the text Exact slot (markup `.&` is real). The
//! route-slot duty pins each format's inventory in BOTH the codec smoke and `jqf-sdk-smoke` in the same commit.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessGuarantees, AccessRequirement, AccessResultKind, CodecError, CodecFailureKind, DiagnosticPolicy,
    ErasedAccessSession, InputProvider, ProviderInput, RecycledSessionState, RouteDescription, RouteSlot,
};
use jqf_data::DataError;
use jqf_resource::ResourceContext;

/// Which format owns a decode request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JqftKind {
    /// The jqft text profile.
    Jqft,
    /// The jqfjson JSON envelope.
    Jqfjson,
}

impl JqftKind {
    pub(crate) fn is_jqft(self) -> bool {
        self == Self::Jqft
    }

    pub(crate) fn schema_prefix(self) -> &'static str {
        match self {
            Self::Jqft => "jqft",
            Self::Jqfjson => "jqfjson",
        }
    }
}

pub(crate) struct JqftProvider {
    routes: Vec<RouteDescription>,
    kind: JqftKind,
    /// Whether the request opted into the adjacent-value contract (JSON's precedented shape). Threaded into the
    /// whole-document route's session so the publish phase knows whether to report a `consumed_offset`.
    allow_adjacent_values: bool,
}

impl JqftProvider {
    pub(crate) fn try_new(
        diagnostics: DiagnosticPolicy,
        kind: JqftKind,
        allow_adjacent_values: bool,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::strict(diagnostics);
        let routes = RouteDescription::try_standard_document_table(guarantees, resources)?;
        Ok(Self {
            routes,
            kind,
            allow_adjacent_values,
        })
    }
}

impl InputProvider for JqftProvider {
    fn route_descriptions(&self) -> &[RouteDescription] {
        self.routes.as_slice()
    }

    fn open_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        requirement: &AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError> {
        let source = input.source();
        if slot == RouteSlot::new(0) {
            requirement.expect_whole(AccessResultKind::CompleteDocument)?;
            let coverage = jqf_codec_core::required_builder_coverage(requirement);
            let state = crate::parse::JqftParseState::try_new(source, self.kind, self.allow_adjacent_values, coverage);
            let route = match self.kind {
                JqftKind::Jqft => crate::JQFT_FULL_PHYSICAL_ROUTE_ID,
                JqftKind::Jqfjson => crate::JQFJSON_FULL_PHYSICAL_ROUTE_ID,
            };
            return ErasedAccessSession::try_new_source_with_route(source, route, || Ok(state));
        }
        if slot != RouteSlot::new(1) {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        let (path, origin) = requirement.expect_exact(AccessResultKind::Located)?;
        let coverage = jqf_codec_core::required_builder_coverage(requirement);
        let session = crate::scoped::NativeScopedSession::try_new(
            path.steps(),
            origin,
            self.kind,
            coverage,
            self.allow_adjacent_values,
            requirement.located_skeleton(),
        )?;
        let route = match self.kind {
            JqftKind::Jqft => crate::JQFT_LOCATED_PHYSICAL_ROUTE_ID,
            JqftKind::Jqfjson => crate::JQFJSON_LOCATED_PHYSICAL_ROUTE_ID,
        };
        ErasedAccessSession::try_new_source_with_route(source, route, || Ok(session))
    }

    fn try_reopen_route(
        &mut self,
        state: &mut RecycledSessionState<'_>,
        slot: RouteSlot,
        requirement: &AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        // Slot 0 decodes ONE whole document per adjacent value and resets in place. Slot 1 is never recycled; the next
        // `---` unit is a new session at `consumed_offset`.
        if slot != RouteSlot::new(0)
            || !requirement.footprint().is_whole()
            || !requirement.schedule().is_empty_complete()
            || requirement.result() != AccessResultKind::CompleteDocument
        {
            return Ok(false);
        }
        let Some(parse) = state.downcast_mut::<crate::parse::JqftParseState>() else {
            return Ok(false);
        };
        parse.set_coverage(jqf_codec_core::required_builder_coverage(requirement));
        parse.try_reset();
        Ok(true)
    }
}

/// The one schema recipe for the jqft family: node kinds and occurrence roles named after the format. jqfjson decodes
/// into the same schema authority, so both formats share the shape; only the identity prefix differs.
pub(crate) fn jqft_recipe(kind: JqftKind) -> Result<jqf_data::DocumentSchemaRecipe<'static>, DataError> {
    let (format, dialect, kinds, roles) = match kind {
        JqftKind::Jqft => (
            "jqft",
            Some(crate::JQFT_DOCUMENT_DIALECT_ID),
            JQFT_NODE_KINDS,
            JQFT_OCCURRENCE_ROLES,
        ),
        JqftKind::Jqfjson => (
            crate::JQFJSON_FORMAT_ID,
            Some(crate::JQFJSON_DOCUMENT_DIALECT_ID),
            JQFJSON_NODE_KINDS,
            JQFJSON_OCCURRENCE_ROLES,
        ),
    };
    jqf_data::DocumentSchemaRecipe::try_new(format, dialect, kinds, roles, FAMILY_FACT_KINDS, FAMILY_FACT_ROLES)
}

/// The jqfb schema recipe (the family's machine profile).
pub(crate) fn jqfb_recipe() -> Result<jqf_data::DocumentSchemaRecipe<'static>, DataError> {
    jqf_data::DocumentSchemaRecipe::try_new(
        "jqfb",
        Some(crate::JQFB_DOCUMENT_DIALECT_ID),
        JQFB_NODE_KINDS,
        JQFB_OCCURRENCE_ROLES,
        FAMILY_FACT_KINDS,
        FAMILY_FACT_ROLES,
    )
}

/// The family's attached-fact names (frozen 2026-08-07): `.@name`/`.@attrs`/`.@content`/`.@comment`, the `.&name`
/// attribute role, and the image's provenance/source facts. One family-wide list: the three profiles share one schema
/// authority, so a fact written by jqft text is the same fact a jqfb image carries.
///
/// The comment surface is the flat sibling set: `jqft.comment@1` carries the LEADING list only, `jqft.comment_inline@1`
/// the inline list, and `jqft.comment_foot@1` an empty list (the `#` grammar has no foot position). The role-keyed map
/// — trailing/inner/detached plus the `{text, style}` payload the flat lists cannot express — lives on
/// [`JQFT_COMMENT_MAP_FACT`], which the encoder reads and no accessor serves (its semantic segment is `comment_map`).
const FAMILY_FACT_KINDS: &[&str] = &[
    JQFT_NAME_FACT,
    JQFT_ATTRS_FACT,
    JQFT_CONTENT_FACT,
    JQFT_COMMENT_FACT,
    JQFT_COMMENT_INLINE_FACT,
    JQFT_COMMENT_FOOT_FACT,
    JQFT_COMMENT_MAP_FACT,
    ATTRIBUTE_FACT,
    JQFT_PROVENANCE_FACT,
    JQFB_SOURCE_FACT,
];

const FAMILY_FACT_ROLES: &[&str] = &[
    JQFT_NAME_FACT,
    JQFT_ATTRS_FACT,
    JQFT_CONTENT_FACT,
    JQFT_COMMENT_FACT,
    JQFT_COMMENT_INLINE_FACT,
    JQFT_COMMENT_FOOT_FACT,
    JQFT_COMMENT_MAP_FACT,
    ATTRIBUTE_FACT,
    JQFT_PROVENANCE_FACT,
    JQFB_SOURCE_FACT,
];

const JQFT_ARRAY_ROLE: &str = "jqft.array.item@1";
const JQFT_OBJECT_ROLE: &str = "jqft.object.member@1";
const JQFT_TAG_PAYLOAD_ROLE: &str = "jqft.tag-payload@1";
/// The markup-child role (the angle form): a markup node is an ARRAY of its ordered children (the array-of-children
/// model), each child an occurrence under this role.
pub(crate) const JQFT_MARKUP_CHILD_ROLE: &str = "jqft.markup-child@1";
const JQFT_NODE_KINDS: &[&str] = &[
    "jqft.null@1",
    "jqft.bool@1",
    "jqft.number@1",
    "jqft.string@1",
    "jqft.bytes@1",
    "jqft.local-date@1",
    "jqft.local-time@1",
    "jqft.local-date-time@1",
    "jqft.offset-date-time@1",
    "jqft.tag-layer@1",
    "jqft.array@1",
    "jqft.object@1",
];
const JQFT_OCCURRENCE_ROLES: &[&str] = &[
    JQFT_ARRAY_ROLE,
    JQFT_OBJECT_ROLE,
    JQFT_TAG_PAYLOAD_ROLE,
    JQFT_MARKUP_CHILD_ROLE,
];

const JQFJSON_ARRAY_ROLE: &str = "jqfjson.array.item@1";
const JQFJSON_OBJECT_ROLE: &str = "jqfjson.object.member@1";
const JQFJSON_TAG_PAYLOAD_ROLE: &str = "jqfjson.tag-payload@1";
const JQFJSON_NODE_KINDS: &[&str] = &[
    "jqfjson.null@1",
    "jqfjson.bool@1",
    "jqfjson.number@1",
    "jqfjson.string@1",
    "jqfjson.bytes@1",
    "jqfjson.local-date@1",
    "jqfjson.local-time@1",
    "jqfjson.local-date-time@1",
    "jqfjson.offset-date-time@1",
    "jqfjson.tag-layer@1",
    "jqfjson.array@1",
    "jqfjson.object@1",
];
const JQFJSON_OCCURRENCE_ROLES: &[&str] = &[JQFJSON_ARRAY_ROLE, JQFJSON_OBJECT_ROLE, JQFJSON_TAG_PAYLOAD_ROLE];

/// The jqfb profile's node kinds.
const JQFB_NODE_KINDS: &[&str] = &[
    "jqfb.null@1",
    "jqfb.bool@1",
    "jqfb.number@1",
    "jqfb.string@1",
    "jqfb.bytes@1",
    "jqfb.local-date@1",
    "jqfb.local-time@1",
    "jqfb.local-date-time@1",
    "jqfb.offset-date-time@1",
    "jqfb.tag-layer@1",
    "jqfb.array@1",
    "jqfb.object@1",
];
const JQFB_ARRAY_ROLE: &str = "jqfb.array.item@1";
const JQFB_OBJECT_ROLE: &str = "jqfb.object.member@1";
const JQFB_TAG_PAYLOAD_ROLE: &str = "jqfb.tag-payload@1";
const JQFB_OCCURRENCE_ROLES: &[&str] = &[JQFB_ARRAY_ROLE, JQFB_OBJECT_ROLE, JQFB_TAG_PAYLOAD_ROLE];

/// The family's attached-fact names (frozen 2026-08-07).
pub(crate) const JQFT_NAME_FACT: &str = "jqft.name@1";
pub(crate) const JQFT_ATTRS_FACT: &str = "jqft.attrs@1";
pub(crate) const JQFT_CONTENT_FACT: &str = "jqft.content@1";
pub(crate) const JQFT_COMMENT_FACT: &str = "jqft.comment@1";
/// The inline-comment sibling fact: the node's inline list, so `.@comment_inline` serves it. Attached only when
/// non-empty.
pub(crate) const JQFT_COMMENT_INLINE_FACT: &str = "jqft.comment_inline@1";
/// The foot-comment sibling fact: always an EMPTY list — the `#` line grammar has no foot position, so the fact exists
/// only to make the surface complete and every node answers `[]`.
pub(crate) const JQFT_COMMENT_FOOT_FACT: &str = "jqft.comment_foot@1";
/// The role-keyed comment map: the parse output and authority — leading/inline/trailing/inner/detached, each a list of
/// `{text, style}` entries. No accessor serves it (semantic segment `comment_map`); the canonical encoder reads it to
/// re-spell comments in place.
pub(crate) const JQFT_COMMENT_MAP_FACT: &str = "jqft.comment_map@1";
pub(crate) const ATTRIBUTE_FACT: &str = "attribute";
pub(crate) const JQFT_PROVENANCE_FACT: &str = "jqft.provenance@1";
pub(crate) const JQFB_SOURCE_FACT: &str = "jqfb.source@1";

/// The kind text a semantic node projects to under the jqft schema.
#[must_use]
pub(crate) fn kind_for(prefix: &str, semantic: &jqf_data::AccountedSemanticNode<'_>) -> &'static str {
    let kind = match semantic {
        jqf_data::AccountedSemanticNode::Null => "null",
        jqf_data::AccountedSemanticNode::Bool(_) => "bool",
        jqf_data::AccountedSemanticNode::Integer(_)
        | jqf_data::AccountedSemanticNode::Decimal { .. }
        | jqf_data::AccountedSemanticNode::Float(_) => "number",
        jqf_data::AccountedSemanticNode::String(_) | jqf_data::AccountedSemanticNode::SourceString(_) => "string",
        jqf_data::AccountedSemanticNode::Bytes(_) => "bytes",
        jqf_data::AccountedSemanticNode::LocalDate(_) => "local-date",
        jqf_data::AccountedSemanticNode::LocalTime(_) => "local-time",
        jqf_data::AccountedSemanticNode::LocalDateTime(_) => "local-date-time",
        jqf_data::AccountedSemanticNode::OffsetDateTime(_) => "offset-date-time",
        jqf_data::AccountedSemanticNode::Array { .. } => "array",
        jqf_data::AccountedSemanticNode::Object { .. } => "object",
        jqf_data::AccountedSemanticNode::Unrepresentable => "tag-layer",
    };
    match prefix {
        "jqft" => match kind {
            "null" => "jqft.null@1",
            "bool" => "jqft.bool@1",
            "number" => "jqft.number@1",
            "string" => "jqft.string@1",
            "bytes" => "jqft.bytes@1",
            "local-date" => "jqft.local-date@1",
            "local-time" => "jqft.local-time@1",
            "local-date-time" => "jqft.local-date-time@1",
            "offset-date-time" => "jqft.offset-date-time@1",
            "tag-layer" => "jqft.tag-layer@1",
            "array" => "jqft.array@1",
            "object" => "jqft.object@1",
            _ => unreachable!("jqft kind table is total"),
        },
        "jqfjson" => match kind {
            "null" => "jqfjson.null@1",
            "bool" => "jqfjson.bool@1",
            "number" => "jqfjson.number@1",
            "string" => "jqfjson.string@1",
            "bytes" => "jqfjson.bytes@1",
            "local-date" => "jqfjson.local-date@1",
            "local-time" => "jqfjson.local-time@1",
            "local-date-time" => "jqfjson.local-date-time@1",
            "offset-date-time" => "jqfjson.offset-date-time@1",
            "tag-layer" => "jqfjson.tag-layer@1",
            "array" => "jqfjson.array@1",
            "object" => "jqfjson.object@1",
            _ => unreachable!("jqfjson kind table is total"),
        },
        _ => match kind {
            "null" => "jqfb.null@1",
            "bool" => "jqfb.bool@1",
            "number" => "jqfb.number@1",
            "string" => "jqfb.string@1",
            "bytes" => "jqfb.bytes@1",
            "local-date" => "jqfb.local-date@1",
            "local-time" => "jqfb.local-time@1",
            "local-date-time" => "jqfb.local-date-time@1",
            "offset-date-time" => "jqfb.offset-date-time@1",
            "tag-layer" => "jqfb.tag-layer@1",
            "array" => "jqfb.array@1",
            "object" => "jqfb.object@1",
            _ => unreachable!("jqfb kind table is total"),
        },
    }
}

/// The occurrence role text a container or tag-payload occurrence projects to under the jqft schema.
#[must_use]
pub(crate) fn role_for(prefix: &str, container: &str) -> &'static str {
    match (prefix, container) {
        ("jqft", "array") => JQFT_ARRAY_ROLE,
        ("jqft", "object") => JQFT_OBJECT_ROLE,
        ("jqft", "tag-payload") => JQFT_TAG_PAYLOAD_ROLE,
        ("jqft", "markup") => JQFT_MARKUP_CHILD_ROLE,
        ("jqfjson", "array") => JQFJSON_ARRAY_ROLE,
        ("jqfjson", "object") => JQFJSON_OBJECT_ROLE,
        ("jqfjson", "tag-payload") => JQFJSON_TAG_PAYLOAD_ROLE,
        (_, "array") => JQFB_ARRAY_ROLE,
        (_, "object") => JQFB_OBJECT_ROLE,
        (_, "tag-payload") => JQFB_TAG_PAYLOAD_ROLE,
        _ => unreachable!("jqft role table is total"),
    }
}
