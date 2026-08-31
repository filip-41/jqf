//! Single-column CSV access: the scoped exact-path route.
//!
//! ## The SCOPED route
//!
//! Locate ONE column of a record and materialize only it. The record drive decodes each record's byte range through the
//! access ladder. A program whose static path names one column needs only ONE field of the record. The whole-document
//! route splits the record and builds every field; this route splits the record, locates the demanded column, and
//! builds a document carrying ONLY that field, so retained memory and build cost are proportional to the selected
//! column, not the row width.
//!
//! ## Which step names a column is the DIALECT's answer
//!
//! Under `csv.rfc4180@1` a row is an ARRAY, so an INDEX (`.[1]`) names a column and a member name cannot: `.age` is the
//! floor's own `Cannot index array with string`. Under `csv.rfc4180-header@1` a row is an OBJECT, so a MEMBER (`.age`)
//! names a column — resolved against the header this session was opened with — and an index cannot: `.[1]` is
//! `Cannot index object with number`. The two arms are exclusive by construction, because the session holds the header
//! names exactly when the headered dialect is in force. Neither arm can be reached under the other dialect, which is
//! what makes this lane's law satisfiable at all: before the headered dialect existed, the member arm was unreachable
//! AND its documented byte-identity claim was unsatisfiable against an array row.
//!
//! The published [`AccessOutcome::Located`] carries the identical [`ExactSelectionRecord`] the
//! whole-decode-then-navigate path publishes, so SDK/CLI behaviour is byte-identical. That identity is CARRIED, not
//! merely claimed: the located field is decoded through [`crate::fields::decode_field_into`], the same unquoting the
//! whole split performs. A negative observation (index out of range, or a header with no such name) publishes the null
//! product, exactly the floor's own `null`.
//!
//! The path is ONE step: CSV records are flat rows. A longer path still binds this route (dispatch never falls back),
//! and after the first step lands on a field the residual is the floor's type mismatch — a CSV field is always a
//! string. Do not publish the first-step value as if the path ended there.

use jqf_codec_core::{
    AccessInput, AccessOutcome, AccessResult, AccessSession, CodecError, CodecFailureKind, CodecRunContext,
    DocumentProduct, ExactSelectionRecord, LocatedOutcome, PortableStep, SelectionOrigin,
};
use jqf_data::{AccountedDocumentBuilder, DataError, DocumentSchemaPrototype, NodeId, PreparedSemanticNode};
use jqf_resource::ResourceContext;

use crate::CsvDecodeOptions;
use crate::decode::{CsvHandles, HeaderNames, data_contract, prepare_document};

/// Native single-column session: one record, one column.
pub(crate) struct CsvColumnSession {
    options: CsvDecodeOptions,
    /// The header's names under the headered dialect, `None` under the array dialect. Which one it is decides which
    /// path step names a column.
    header: Option<HeaderNames>,
    /// Exclusive physical end of the header unit. Lockstep with `header`; named in the ragged-row diagnostic.
    header_end: Option<u64>,
    /// The request's immutable schema prototype (an `Arc` clone of the provider's), so every record's stand-in or
    /// located field starts from the cheap prototype path instead of re-binding the recipe.
    schema_prototype: DocumentSchemaPrototype,
    /// The owned single step: `Some((is_member, member_or_index))`.
    step: Option<OwnedStep>,
    /// The bound path named more than one step. A successful first-step resolve then answers the residual as a string
    /// type-mismatch at step 1 — never the first-step value alone.
    extra_steps: bool,
    origin: SelectionOrigin,
    finished: bool,
    /// Reused unescape scratch for a quoted located field.
    scratch: alloc::vec::Vec<u8>,
}

/// An owned copy of the single column step (`PortableStep` is not `Clone`).
pub(crate) enum OwnedStep {
    /// A signed array position.
    Index(i64),
    /// A header member name.
    Member(alloc::string::String),
    /// A range step (`.[a:b]`): names a SLICE of the row, which this single-column session cannot serve.
    Range,
}

impl CsvColumnSession {
    pub(crate) fn new(
        options: CsvDecodeOptions,
        header: Option<HeaderNames>,
        header_end: Option<u64>,
        steps: &[PortableStep],
        origin: SelectionOrigin,
        schema_prototype: DocumentSchemaPrototype,
    ) -> Self {
        // CSV records are flat: the first step names the column. Extra steps cannot land on a field (always a string);
        // decode answers the floor's type mismatch instead of truncating the path.
        let extra_steps = steps.len() > 1;
        let step = steps.first().map(|step| match step {
            PortableStep::SemanticIndex(index) => OwnedStep::Index(*index),
            PortableStep::SemanticMember(member) => OwnedStep::Member(alloc::string::String::from(member.as_str())),
            PortableStep::SemanticRange { .. } => OwnedStep::Range,
        });
        Self {
            options,
            header,
            header_end,
            schema_prototype,
            step,
            extra_steps,
            origin,
            finished: false,
            scratch: alloc::vec::Vec::new(),
        }
    }

    /// Column index known without a field count: a header member or a non-negative array index. `None` means the walk
    /// must count first (negative index) or the step is a type mismatch / missing name.
    fn known_column(&self) -> Option<usize> {
        match (&self.step, &self.header) {
            (Some(OwnedStep::Member(name)), Some(header)) => header.iter().position(|candidate| candidate == name),
            (Some(OwnedStep::Index(index)), None) if *index >= 0 => usize::try_from(*index).ok(),
            _ => None,
        }
    }

    /// Resolves the single step to a column index, or a negative observation.
    ///
    /// The dialect decides which step KIND names a column, and the other kind is the floor's own type mismatch against
    /// the row's kind — never a second lookup rule. A range step names a slice this single-column session cannot serve;
    /// it declines the located route so the whole-document floor can answer.
    fn resolve_column(&self, field_count: usize) -> Result<Option<usize>, ColumnObservation> {
        let Some(step) = &self.step else {
            // No step: the root selection. A zero-step Located requirement never binds these routes (the root
            // classifies whole-document), so this arm is defensive.
            return Ok(Some(0));
        };
        match (step, &self.header) {
            (OwnedStep::Index(index), None) => {
                // The shared checked law (`i64::MIN` cannot be negated, so the magnitude is subtracted): an
                // out-of-range or past-the-start index is `Missing`, never wrapping arithmetic.
                jqf_data::resolve_index(field_count, *index)
                    .map(Some)
                    .ok_or(ColumnObservation::Missing { step: 0 })
            }
            (OwnedStep::Member(name), Some(header)) => header
                .iter()
                .position(|candidate| candidate == name)
                .map(Some)
                .ok_or(ColumnObservation::Missing { step: 0 }),
            // A member name against an ARRAY row and an index against an OBJECT row are both the floor's typed index
            // mismatch, named by the row kind this dialect publishes.
            (OwnedStep::Member(_), None) => Err(ColumnObservation::TypeMismatch {
                step: 0,
                actual: jqf_data::ValueKind::Array,
            }),
            (OwnedStep::Index(_), Some(_)) => Err(ColumnObservation::TypeMismatch {
                step: 0,
                actual: jqf_data::ValueKind::Object,
            }),
            (OwnedStep::Range, _) => Err(ColumnObservation::RangeDeclined),
        }
    }

    /// Copies one owned field string into the builder's stored text and commits it as the record's scalar node (the
    /// prepared-path equivalent of `AccountedSemanticNode::String`).
    fn build_field_scalar(
        builder: &mut AccountedDocumentBuilder<'static>,
        schema: &jqf_data::PreparedDocumentSchema,
        handles: CsvHandles,
        field: &str,
        resources: &mut ResourceContext<'_>,
    ) -> Result<NodeId, CodecError> {
        let text = builder.store_text(field, resources).map_err(map_data)?;
        builder
            .add_prepared_stored_string_node(schema, handles.scalar, text, resources)
            .map_err(map_data)
    }

    /// Builds a one-field document: the located field's scalar.
    fn build_single_field(
        prototype: &DocumentSchemaPrototype,
        field: &str,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
        let (mut builder, schema, handles) = prepare_document(prototype, resources)?;
        // The located value for a column path is the SCALAR field itself, exactly as the floor's `.[1]` navigation
        // produces — not a one-element array. The root is the scalar.
        let root = Self::build_field_scalar(&mut builder, &schema, handles, field, resources)?;
        Ok((builder, root))
    }

    /// Builds the null product of a negative observation.
    fn build_null(
        prototype: &DocumentSchemaPrototype,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(AccountedDocumentBuilder<'static>, NodeId), CodecError> {
        let (mut builder, schema, handles) = prepare_document(prototype, resources)?;
        let root = builder
            .add_prepared_node(&schema, handles.scalar, PreparedSemanticNode::Null, resources)
            .map_err(map_data)?;
        Ok((builder, root))
    }

    /// A residual step after a successful column resolve: the field is a string, so the floor's answer is a type
    /// mismatch at step 1.
    fn publish_string_residual<'source>(
        &self,
        resources: &mut ResourceContext<'_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let (builder, root) = Self::build_null(&self.schema_prototype, resources)?;
        let document = builder.finish(root, resources).map_err(map_data)?;
        let product = DocumentProduct::try_new(document, resources)?;
        Self::publish(
            &product,
            ExactSelectionRecord::TypeMismatch {
                step_index: 1,
                actual_type: jqf_data::ValueKind::String,
                origin: self.origin,
                hint: None,
            },
        )
    }

    fn publish<'source>(
        product: &DocumentProduct<'source>,
        selection: ExactSelectionRecord,
    ) -> Result<AccessResult<'source>, CodecError> {
        let outcome = LocatedOutcome::try_new(product, selection)?;
        Ok(AccessResult::from_outcome(AccessOutcome::Located(outcome)))
    }

    /// Re-seeds this session for a new record range (record-drive reuse).
    pub(crate) fn reset(&mut self) {
        self.finished = false;
    }
}

enum ColumnObservation {
    Missing { step: usize },
    TypeMismatch { step: usize, actual: jqf_data::ValueKind },
    RangeDeclined,
}

impl AccessSession for CsvColumnSession {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn decode<'source>(
        &mut self,
        input: AccessInput<'_, 'source>,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<AccessResult<'source>, CodecError> {
        let source = match input {
            AccessInput::Source(source) => source,
            AccessInput::Document(_) => {
                return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
            }
        };
        if self.finished {
            return Err(data_contract());
        }
        self.finished = true;
        let payload = source.bytes();
        let delimiter = self.options.delimiter();
        let quote = self.options.quote();
        let prototype = &self.schema_prototype;
        // A known non-negative / header column locates during the validate walk. Extra residual steps and negative
        // indices only need the count; the latter keeps the second locate walk.
        let locate = if self.extra_steps { None } else { self.known_column() };
        let walk = crate::scan::count_and_locate(payload, delimiter, quote, locate)?;
        // A headered row whose width disagrees with the header is ragged, and the floor rejects it before any
        // navigation. The fast lane must reject it at the same point or it would answer where the floor errors.
        if let Some(names) = &self.header
            && names.len() != walk.count
        {
            return Err(crate::error::ragged_row(
                names.len(),
                walk.count,
                self.header_end.unwrap_or(0),
            ));
        }
        match self.resolve_column(walk.count) {
            Ok(Some(_)) if self.extra_steps => self.publish_string_residual(context.resources()),
            Ok(Some(column)) => {
                // Materialize ONLY the located column: the fused walk names the range when the column was known; a
                // negative index takes the second locate. Decode shares the whole split's unquoting AND its TEXTDATA
                // freeze — the byte-identity law. A clean (unquoted) field is the payload subslice; no per-record
                // heap copy.
                let range = match walk.range {
                    Some(range) => range,
                    None => crate::scan::field_bytes(payload, delimiter, column, quote)?
                        .ok_or_else(|| CodecError::new(CodecFailureKind::InvalidInput))?,
                };
                let raw = &payload[range.0..range.1];
                let textdata = self.options.textdata();
                let (builder, root) = if quote.is_none() || !raw.contains(&b'"') {
                    // The unquoted field: under the freeze every byte must be TEXTDATA, exactly as `finish_clean_field`
                    // enforces on the whole split.
                    if textdata && quote.is_some() && !raw.iter().all(|&byte| crate::fields::is_textdata(byte)) {
                        return Err(CodecError::new(CodecFailureKind::InvalidInput));
                    }
                    let field =
                        core::str::from_utf8(raw).map_err(|_| CodecError::new(CodecFailureKind::InvalidInput))?;
                    Self::build_single_field(prototype, field, context.resources())?
                } else {
                    let field = crate::fields::decode_field_into(raw, quote, textdata, &mut self.scratch)?;
                    Self::build_single_field(prototype, &field, context.resources())?
                };
                let document = builder.finish(root, context.resources()).map_err(map_data)?;
                let product = DocumentProduct::try_new(document, context.resources())?;
                Self::publish(
                    &product,
                    ExactSelectionRecord::Node {
                        node: product.document().root_handle(),
                        origin: self.origin,
                    },
                )
            }
            Err(ColumnObservation::Missing { step }) => {
                let (builder, root) = Self::build_null(prototype, context.resources())?;
                let document = builder.finish(root, context.resources()).map_err(map_data)?;
                let product = DocumentProduct::try_new(document, context.resources())?;
                Self::publish(
                    &product,
                    ExactSelectionRecord::Missing {
                        step_index: step,
                        origin: self.origin,
                    },
                )
            }
            Err(ColumnObservation::TypeMismatch { step, actual }) => {
                let (builder, root) = Self::build_null(prototype, context.resources())?;
                let document = builder.finish(root, context.resources()).map_err(map_data)?;
                let product = DocumentProduct::try_new(document, context.resources())?;
                Self::publish(
                    &product,
                    ExactSelectionRecord::TypeMismatch {
                        step_index: step,
                        actual_type: actual,
                        origin: self.origin,
                        hint: None,
                    },
                )
            }
            Err(ColumnObservation::RangeDeclined) => Err(CodecError::new(CodecFailureKind::RequirementMismatch)),
            Ok(None) => Err(crate::decode::data_contract()),
        }
    }
}

fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "CSV single-column builder rejected document construction")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::csv_schema_recipe;
    use jqf_codec_core::{CodecRunContext, SelectionOrigin};
    use jqf_data::DocumentSchemaPrototype;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "row.csv", bytes, 0)
    }

    /// A range step on the single-column session declines the located route so the whole-document floor can serve the
    /// slice.
    #[test]
    fn a_range_step_declines_to_the_floor() {
        let options = crate::CsvDecodeOptions::try_new_rfc4180(None, None, 1 << 20, false).expect("options");
        let recipe = csv_schema_recipe(&options).expect("recipe");
        let schema_prototype = DocumentSchemaPrototype::try_new(&recipe).expect("prototype");
        let mut session = CsvColumnSession::new(
            options,
            None,
            None,
            &[PortableStep::SemanticRange {
                start: Some(0),
                end: Some(1),
            }],
            SelectionOrigin::new(0),
            schema_prototype,
        );
        let mut resources = resources();
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        let error = session
            .decode(jqf_codec_core::AccessInput::Source(source(b"a,b,c")), &mut context)
            .expect_err("range declines the column session");
        assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
    }
}
