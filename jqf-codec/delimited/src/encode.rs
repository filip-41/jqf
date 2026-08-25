//! Deterministic RFC 4180 CSV encoding.
//!
//! ## `csv.jqf-rfc4180@1` — the default, headerless
//!
//! Each encoded item is ONE row and no header is ever written. An item projects to an ARRAY of scalar strings — every
//! scalar rendered as its string form, exactly like the `@csv`-style projection — or to a FLAT OBJECT, whose member
//! VALUES project as the row's fields in source order. The object's keys are not published under this dialect; a caller
//! who wants them wants the headered one.
//!
//! ## `csv.jqf-rfc4180-header@1` — the encode mirror of the headered input
//!
//! The item is an OBJECT (one row) or an ARRAY OF OBJECTS (a whole table). The FIRST object's keys are published as a
//! header row before the first data row, and every later object must present the identical key SEQUENCE — a change is
//! `UnsupportedRepresentation`: a streaming encoder that has already published a header cannot unsay it, and unioning
//! every key would mean buffering the whole stream before the first byte. Column order is therefore the FIRST object's
//! key order. Any other item shape is unrepresentable: this dialect exists to publish header-keyed objects.
//!
//! The header fact is CROSS-ITEM state, which is why a headered output request declines both morsel lanes — see
//! `PlanDecision::StatefulOutput`. A worker holds its own factory, so a split stream would publish one header per
//! morsel.
//!
//! RFC 4180 quoting is exact under both dialects: a field is quoted when it contains the delimiter, a quote, or a
//! CR/LF, and an embedded quote is doubled. The record terminator is per-dialect (CRLF for the RFC-4180-named CSV
//! output dialects, LF for `tsv.jqf-lf@1`) and is appended INSIDE the encoder's staging buffer, so a row can never be
//! published without its terminator and a row that fails mid-encode can never publish one.
//!
//! # The edit splice policy
//!
//! [`EncoderFactoryImpl::render_edit_append`] is the `--edit` structural seam: a program that GREW a record's field
//! list has no authored span to patch, so this codec renders the addition in the dialect's local syntax at a position
//! it names, and the SDK splices it into the retained source and re-verifies by re-decode. A delimited splice is the
//! simplest in the portfolio — no nesting, no comments, a fixed column count per row — so the rulings are short.
//! All pinned by `tools/jqf-edit-differential.py`'s CSV and TSV arms (identity, survival, placement).
//!
//! 1. **A changed field re-quotes only when it must — or when it was.** [`EncoderFactoryImpl::render_leaf`] replaces
//!    the field's authored span (quotes included — the record's span law) with the new field text. A field whose
//!    authored bytes OPEN with a quote keeps its quoted style: the text renders inside a quote pair with interior
//!    quotes doubled. An authored-PLAIN field renders through the RFC 4180 quote-forcing law (`push_field`): quoted
//!    exactly when the new text contains the delimiter, a quote, a CR, or an LF, plain otherwise. Under the TSV
//!    no-quote grammar the quote pair never exists: a field containing the TAB delimiter, CR, or LF is
//!    `UnsupportedRepresentation` (declines to the floor, never silently emitted unquoted). The patched bytes must
//!    re-decode to the program's value, so a text the grammar cannot carry is never emitted as one.
//! 2. **A record that GREW gains fields before its terminator.** The record extent span covers the row's authored
//!    payload bytes, terminator excluded; a new field splices at the extent's END — the row's last authored byte —
//!    each preceded by the delimiter (a zero-field empty record appends bare fields). The record's own terminator
//!    closes the grown row, exactly as it closed the original: the splice never writes one. The headered dialect
//!    REFUSES a growth: a member set that disagrees with the header row is the ragged-row rejection, and a reshape is
//!    not a splice.
//! 3. **A record that SHRANK loses fields, each cut with one adjacent delimiter** — the preceding one, or the
//!    following one for the first field (the JSON member-removal law, minus whitespace: a CSV row has none). A record
//!    emptied of every field keeps its terminator — an empty row is valid RFC 4180. The headered dialect refuses a
//!    shrink for the same ragged-row reason.
//! 4. **The record terminator is the framer's byte, never the splice's.** A field splice never touches it; the
//!    record-edit drive preserves each record's OWN authored terminator bytes between payloads, so a CRLF file stays
//!    CRLF and an unterminated final record stays unterminated. A splice the codec cannot place — a span-less
//!    container, a non-record root, a headered growth or shrink — returns an empty insertion/cut set and the SDK
//!    falls back to the whole-record floor; the re-decode verification makes any wrong splice degrade the same way,
//!    never corrupt bytes.
//! 5. **Headered `--edit` publishes the authored header prefix, then splices data records.** The header row is
//!    stream-prefix schema, not a record; the record-edit drive writes `[0, first_data_start)` before the poll loop so
//!    identity keeps those bytes. Cell assignment uses [`EncoderFactoryImpl::render_leaf`] on a data-record field.
//!    Growth and shrink still refuse (`csv.headered-edit`): a member set that disagrees with the prefix is ragged, and
//!    renaming the header is not a splice. [`create_registered_factory`] stays open so `--header` query encode (without
//!    `--edit`) rebuilds a header from the first object's keys.

use alloc::vec::Vec;

use crate::byte_scan::{self, Csv};
use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EditAppendMembers, EditInsertion, EditRemoval,
    EditRemoveMembers, EncodeItem, EncodeRequest, EncoderFactoryImpl, EncoderSession, ErasedEncoderFactory,
    ErasedEncoderSession, NativeSpellings, PreservationOutcome, PreservationReport, PreservationRequest,
    RecycledSessionState, TagLayer, TrackedProjectionSink, classify_scalar, project_tag, view_tag_layer,
};
use jqf_data::{DataError, Document, NodeId, ScalarView, Value, ValueKind, ValueView};
use jqf_resource::{ResourceContext, WorkAdmission};

use crate::options::headered_delimited_edit_refusal;

/// A CSV field is TEXT and nothing else — the format has no scalar types at all — so every projectable scalar
/// projects, dates included, through the one shared canonical layer. A reader of the CSV cannot tell a date from a
/// string; that honesty is the format's, not this crate's.
const DELIMITED_NATIVE: NativeSpellings = NativeSpellings::NONE;

use crate::provider::ENCODE_PHYSICAL_ROUTE_ID;
use crate::{
    CsvEncodeOptions, FORMAT_ID, JQF_RFC4180_DIALECT_ID, JQF_RFC4180_HEADER_DIALECT_ID, JQF_UTF8_DIALECT_ID,
    JQF_UTF8_HEADER_DIALECT_ID, TSV_FORMAT_ID, TSV_JQF_LF_DIALECT_ID, TSV_JQF_LF_HEADER_DIALECT_ID,
};

const OFFER_BYTES: usize = 16 * 1024;

/// The header row's published names, shared across one factory's sessions.
///
/// `None` means the header has not been written yet; `Some` is the exact key sequence every later object must present.
/// A CSV encoder session encodes one ITEM, so this is the only state that outlives a session — and the reason a
/// headered output request may not be split across morsel workers.
type HeaderState = alloc::rc::Rc<core::cell::RefCell<Option<alloc::vec::Vec<alloc::string::String>>>>;

/// Registry entry point: reads the delimiter policy from the request's own option payload, defaulting to comma when
/// options are omitted.
pub(crate) fn create_registered_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    let options = match request.options {
        None => CsvEncodeOptions::default(),
        Some(payload) => *payload
            .downcast_ref::<CsvEncodeOptions>()
            .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch))?,
    };
    let (header, format) = match request.dialect.as_str() {
        // The two CSV output families share one encoder: the RFC-named profiles and the Unicode-capable
        // `csv.jqf-utf8@1` twins emit the same quoting/CRLF bytes; the ids differ so a consumer can pin the family by
        // name.
        JQF_RFC4180_DIALECT_ID | JQF_UTF8_DIALECT_ID => (false, FORMAT_ID),
        JQF_RFC4180_HEADER_DIALECT_ID | JQF_UTF8_HEADER_DIALECT_ID => (true, FORMAT_ID),
        TSV_JQF_LF_DIALECT_ID => (false, TSV_FORMAT_ID),
        TSV_JQF_LF_HEADER_DIALECT_ID => (true, TSV_FORMAT_ID),
        _ => return Err(CodecError::new(CodecFailureKind::RequirementMismatch)),
    };
    if request.format.as_str() != format {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || {
        Ok(CsvEncoderFactory {
            options,
            header: header.then(|| alloc::rc::Rc::new(core::cell::RefCell::new(None))),
        })
    })
}

struct CsvEncoderFactory {
    options: CsvEncodeOptions,
    /// The headered dialect's cross-item header state; `None` under the headerless dialect, which has no state to
    /// carry.
    header: Option<HeaderState>,
}

impl EncoderFactoryImpl for CsvEncoderFactory {
    fn physical_encoder(&self) -> jqf_codec_core::PhysicalRouteId {
        ENCODE_PHYSICAL_ROUTE_ID
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        ErasedEncoderSession::try_new(item, preservation, || {
            Ok(CsvEncoder {
                bytes: Vec::new(),
                options: self.options,
                header: self.header.clone(),
                row_done: false,
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
        let Some(encoder) = state.downcast_mut::<CsvEncoder>() else {
            return Ok(false);
        };
        // The headered dialect's shared header state is cross-item by design (a fresh `start` clones the same
        // factory-owned `Rc`), so the reset keeps the encoder's clone exactly as a fresh start would.
        encoder.reset();
        Ok(true)
    }

    fn render_leaf(
        &self,
        _document: &Document<'_>,
        _node: NodeId,
        _path: &[alloc::string::String],
        _source: &[u8],
        value: &Value,
        authored: Option<&[u8]>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<u8>, CodecError> {
        // The bare-field grammar: a leaf patch replaces a field's authored span with these exact bytes, so they must be
        // the field as it appears BETWEEN separators — no delimiter, no terminator. This is the same `push_field` the
        // row encoder writes after a delimiter, so a patched record re-decodes to the same document. Quoting style is
        // preserved per policy ruling 1: a field whose authored bytes OPEN with a quote keeps its quote pair; an
        // authored-plain field renders through the RFC 4180 quote-forcing law (a plain render means the text carries no
        // delimiter/quote/CR/LF, so wrapping it is safe — there is no interior quote to double). The TSV no-quote
        // grammar returns `push_field`'s raw bytes, refusing a TAB/CR/LF-bearing field to the floor. A headered
        // data-record field is the same between-separator text: the drive already wrote the authored header prefix, so
        // a leaf splice must not emit a terminator.
        let mut encoder = CsvEncoder {
            bytes: Vec::new(),
            options: self.options,
            header: None,
            row_done: false,
        };
        encoder.push_owned_field(value, resources)?;
        let rendered = encoder.bytes;
        if self.options.quote().is_some()
            && rendered.first() != Some(&b'"')
            && authored.is_some_and(|bytes| bytes.first() == Some(&b'"'))
        {
            let mut out = Vec::with_capacity(rendered.len().saturating_add(2));
            out.push(b'"');
            out.extend_from_slice(&rendered);
            out.push(b'"');
            Ok(out)
        } else {
            Ok(rendered)
        }
    }

    fn render_edit_append(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[alloc::string::String],
        _source: &[u8],
        members: EditAppendMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
        // Policy ruling 2: a grown record gains fields spliced before its terminator, at the extent's end. Only the
        // headerless ARRAY dialect can grow by splicing — a headered growth disagrees with the header prefix the
        // drive already published, and a non-record root has no authored row to grow. The headered dialect refuses the
        // reshape.
        if self.header.is_some() {
            return Err(headered_delimited_edit_refusal());
        }
        let node = document.node_handle(container).map_err(map_data)?;
        let view = document.value_view(node).map_err(map_data)?;
        if !matches!(view.kind().map_err(map_data)?, ValueKind::Array) {
            return Ok(alloc::vec::Vec::new());
        }
        let Some(span) = document.node_source_span(container).map_err(map_data)? else {
            return Ok(alloc::vec::Vec::new());
        };
        let mut encoder = CsvEncoder {
            bytes: Vec::new(),
            options: self.options,
            header: None,
            row_done: false,
        };
        let mut text = Vec::new();
        for (index, item) in match members {
            EditAppendMembers::Array(items) => items.iter().enumerate(),
            EditAppendMembers::Table(_) => return Ok(alloc::vec::Vec::new()),
        } {
            // The leading delimiter joins the new field to the row's last authored byte; a zero-field empty record
            // appends bare fields.
            if index > 0 || span.end() > span.start() {
                text.push(self.options.delimiter());
            }
            encoder.bytes.clear();
            encoder.push_owned_field(item, resources)?;
            text.extend_from_slice(&encoder.bytes);
        }
        Ok(alloc::vec![EditInsertion {
            at: span.end() as usize,
            bytes: text,
            replace: None,
        }])
    }

    fn render_edit_remove(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[alloc::string::String],
        source: &[u8],
        members: EditRemoveMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
        // Each removed field is cut with ONE adjacent delimiter — its preceding one when that byte is still
        // unclaimed, else its following one — so the surviving fields stay joined by their own commas. The headered
        // dialect refuses a shrink: it would disagree with the authored header prefix.
        if self.header.is_some() {
            return Err(headered_delimited_edit_refusal());
        }
        let node = document.node_handle(container).map_err(map_data)?;
        let view = document.value_view(node).map_err(map_data)?;
        if !matches!(view.kind().map_err(map_data)?, ValueKind::Array) {
            return Ok(alloc::vec::Vec::new());
        }
        let mut fields = alloc::vec::Vec::new();
        for node in members.nodes() {
            let Some(span) = document.node_source_span(node).map_err(map_data)? else {
                return Ok(alloc::vec::Vec::new());
            };
            fields.push((span.start() as usize, span.end() as usize));
        }
        Ok(removal_cuts(source, self.options.delimiter(), &fields))
    }
}

/// Assigns each removed field ONE adjacent delimiter, disjointly: a field takes its preceding delimiter unless an
/// earlier removal in this batch already consumed that byte, then its following one. Two removals claiming the same
/// delimiter would overlap and splice a corrupted row back together (removing the first two fields of `a,b,c` must
/// leave `c`, never `,c`).
fn removal_cuts(source: &[u8], delimiter: u8, fields: &[(usize, usize)]) -> alloc::vec::Vec<EditRemoval> {
    let mut removals = alloc::vec::Vec::with_capacity(fields.len());
    let mut consumed_until = 0usize;
    for &(start, end) in fields {
        let cut = if start > consumed_until && source.get(start - 1) == Some(&delimiter) {
            consumed_until = end;
            EditRemoval {
                start: start - 1,
                end,
                replacement: alloc::vec::Vec::new(),
            }
        } else if source.get(end) == Some(&delimiter) {
            consumed_until = end + 1;
            EditRemoval {
                start,
                end: end + 1,
                replacement: alloc::vec::Vec::new(),
            }
        } else {
            consumed_until = end;
            EditRemoval {
                start,
                end,
                replacement: alloc::vec::Vec::new(),
            }
        };
        removals.push(cut);
    }
    removals
}

struct CsvEncoder {
    bytes: Vec<u8>,
    options: CsvEncodeOptions,
    /// The factory's shared header state under the headered dialect; `None` under the headerless one.
    header: Option<HeaderState>,
    /// Whether this encoder has consumed its single item and may only flush.
    ///
    /// A CSV encoder session encodes exactly ONE ITEM (one `EncodeItem`) per `start()`, then finishes — exactly as
    /// TOML's encoder emits one document per session. Under the headered dialect one item may be a whole TABLE, so an
    /// item is not always one row.
    row_done: bool,
}

impl CsvEncoder {
    /// Reinitializes one recycled encoder for one more ordered item: drops every byte and flag a previous item may have
    /// left behind — including one that aborted mid-offer, whose partial staging must never reach the next item —
    /// leaving exactly the state a fresh [`EncoderFactoryImpl::start`] would have produced (the factory-owned header
    /// `Rc` included).
    fn reset(&mut self) {
        self.bytes.clear();
        self.row_done = false;
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn push_byte(&mut self, byte: u8) {
        self.bytes.push(byte);
    }

    /// Writes one exact decimal as the canonical number text (the D1 numbers slice: a CSV field is text, so the exact
    /// decimal's rendering is the field). No `.0` reparse suffix: a CSV field is opaque text and is never re-read as a
    /// typed number.
    fn push_decimal(&mut self, coefficient: &str, scale: i64) -> Result<(), CodecError> {
        jqf_codec_core::decimal_render_into(coefficient, scale, false, &mut self.bytes)
    }

    /// Encodes one row into the staging buffer.
    fn encode_row(&mut self, item: EncodeItem<'_, '_>, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        match item {
            EncodeItem::Owned(value) => self.encode_owned_row(value, resources),
            EncodeItem::Located { product, node } => {
                let view = product.document().value_view(node).map_err(map_data)?;
                self.encode_located_row(&view, resources)
            }
        }
    }

    fn encode_owned_row(&mut self, value: &Value, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        if self.header.is_some() {
            return self.encode_owned_table(value, resources);
        }
        match value {
            Value::Array(array) => {
                for (index, item) in array.iter().enumerate() {
                    if index > 0 {
                        self.push_byte(self.options.delimiter());
                    }
                    self.push_owned_field(item, resources)?;
                }
                self.push(self.options.terminator().bytes());
                Ok(())
            }
            Value::Object(object) => {
                // Flat object: the member VALUES are the row. This dialect publishes no header;
                // `csv.jqf-rfc4180-header@1` is the one that does.
                for (index, entry) in object.iter().enumerate() {
                    if index > 0 {
                        self.push_byte(self.options.delimiter());
                    }
                    self.push_owned_field(entry.value(), resources)?;
                }
                self.push(self.options.terminator().bytes());
                Ok(())
            }
            _ => {
                // A scalar root is a single-field row, mirroring the located arm (`-n '5' --output-format csv`
                // publishes the same `5` stdin `5` does).
                self.push_owned_field(value, resources)?;
                self.push(self.options.terminator().bytes());
                Ok(())
            }
        }
    }

    /// The headered dialect's item law: one object is one row, an array of objects is a whole table, and nothing else
    /// is representable.
    fn encode_owned_table(&mut self, value: &Value, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        match value {
            Value::Object(_) => self.encode_owned_headered_row(value, resources),
            Value::Array(array) => {
                for item in array {
                    self.encode_owned_headered_row(item, resources)?;
                }
                Ok(())
            }
            _ => Err(unrepresentable()),
        }
    }

    /// Publishes one object as a data row, writing the header row first when this is the factory's first row.
    fn encode_owned_headered_row(&mut self, value: &Value, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        let Value::Object(object) = value else {
            return Err(unrepresentable());
        };
        let Some(state) = self.header.clone() else {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "CSV headered row without header state",
            }));
        };
        if let Some(published) = state.borrow().as_ref() {
            if published.len() != object.len()
                || published.iter().enumerate().any(|(index, published)| {
                    object
                        .get_index(index)
                        .is_none_or(|entry| published.as_str() != entry.key())
                })
            {
                return Err(unrepresentable());
            }
        } else {
            let mut keys = alloc::vec::Vec::with_capacity(object.len());
            for (index, entry) in object.iter().enumerate() {
                if index > 0 {
                    self.push_byte(self.options.delimiter());
                }
                self.push_field(entry.key().as_bytes())?;
                keys.push(alloc::string::String::from(entry.key()));
            }
            self.push(self.options.terminator().bytes());
            *state.borrow_mut() = Some(keys);
        }
        for (index, entry) in object.iter().enumerate() {
            if index > 0 {
                self.push_byte(self.options.delimiter());
            }
            self.push_owned_field(entry.value(), resources)?;
        }
        self.push(self.options.terminator().bytes());
        Ok(())
    }

    fn push_owned_field(&mut self, value: &Value, resources: &ResourceContext<'_>) -> Result<(), CodecError> {
        match value {
            Value::Null => {
                self.push(b"");
                Ok(())
            }
            Value::String(text) => self.push_field(text.as_str().as_bytes()),
            Value::Number(number) => {
                // Render the canonical number text; any other scalar is unrepresentable (CSV fields are strings). The
                // inline machine arm renders its canonical spelling on demand; the boxed arm borrows its retained one.
                if let Some(machine) = number.as_machine() {
                    let integer = jqf_data::Integer::from_i64(machine);
                    self.push(integer.as_str().as_bytes());
                    Ok(())
                } else if let Some(integer) = number.as_integer() {
                    self.push(integer.as_str().as_bytes());
                    Ok(())
                } else if let Some(decimal) = number.as_decimal() {
                    self.push_decimal(decimal.coefficient().as_str(), decimal.scale())
                } else if let Some(float) = number.as_float() {
                    self.push_number_text(float)
                } else {
                    Err(unrepresentable())
                }
            }
            Value::Bool(boolean) => {
                self.push(if *boolean { b"true" } else { b"false" });
                Ok(())
            }
            Value::Tagged { payload, .. } => {
                project_tag(resources);
                self.push_owned_field(payload, resources)
            }
            Value::Bytes(_)
            | Value::LocalDate(_)
            | Value::LocalTime(_)
            | Value::LocalDateTime(_)
            | Value::OffsetDateTime(_) => {
                let scalar = ScalarView::from_value(value).ok_or_else(unrepresentable)?;
                self.push_projected_field(&scalar, resources)
            }
            Value::Array(_) | Value::Object(_) => Err(unrepresentable()),
        }
    }

    /// Writes one scalar CSV has no native spelling for as canonical text.
    ///
    /// No RFC 4180 quoting is needed: projected text is escape-free by the sink's contract and contains no delimiter,
    /// quote, CR, or LF.
    fn push_projected_field(
        &mut self,
        scalar: &ScalarView<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let projection = classify_scalar(scalar, DELIMITED_NATIVE, resources).ok_or_else(unrepresentable)?;
        projection.write(&mut TrackedProjectionSink::new(&mut self.bytes), resources)
    }

    fn encode_located_row(
        &mut self,
        view: &ValueView<'_, '_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if self.header.is_some() {
            return self.encode_located_table(view, resources);
        }
        if let Some(array) = view.array().map_err(map_data)? {
            for index in 0..array.len() {
                if index > 0 {
                    self.push_byte(self.options.delimiter());
                }
                let item = array.get(index).ok_or_else(unrepresentable)?;
                self.push_located_field(&item, resources)?;
            }
            self.push(self.options.terminator().bytes());
            return Ok(());
        }
        if let Some(object) = view.object().map_err(map_data)? {
            for index in 0..object.len() {
                if index > 0 {
                    self.push_byte(self.options.delimiter());
                }
                let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
                self.push_located_field(&entry.value(), resources)?;
            }
            self.push(self.options.terminator().bytes());
            return Ok(());
        }
        // A scalar root is a single-field row.
        self.push_located_field(view, resources)?;
        self.push(self.options.terminator().bytes());
        Ok(())
    }

    /// The headered dialect's item law over a BORROWED document, mirroring [`Self::encode_owned_table`] exactly.
    fn encode_located_table(
        &mut self,
        view: &ValueView<'_, '_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if view.object().map_err(map_data)?.is_some() {
            return self.encode_located_headered_row(view, resources);
        }
        if let Some(array) = view.array().map_err(map_data)? {
            for index in 0..array.len() {
                let item = array.get(index).ok_or_else(unrepresentable)?;
                self.encode_located_headered_row(&item, resources)?;
            }
            return Ok(());
        }
        Err(unrepresentable())
    }

    /// Publishes one borrowed object as a data row, header row first.
    fn encode_located_headered_row(
        &mut self,
        view: &ValueView<'_, '_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let object = view.object().map_err(map_data)?.ok_or_else(unrepresentable)?;
        let Some(state) = self.header.clone() else {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "CSV headered row without header state",
            }));
        };
        if let Some(published) = state.borrow().as_ref() {
            if published.len() != object.len() {
                return Err(unrepresentable());
            }
            for (index, header_key) in published.iter().enumerate() {
                let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
                if header_key.as_str() != entry.key() {
                    return Err(unrepresentable());
                }
                if index > 0 {
                    self.push_byte(self.options.delimiter());
                }
                self.push_located_field(&entry.value(), resources)?;
            }
            self.push(self.options.terminator().bytes());
            return Ok(());
        }
        let mut keys = alloc::vec::Vec::with_capacity(object.len());
        for index in 0..object.len() {
            let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
            if index > 0 {
                self.push_byte(self.options.delimiter());
            }
            self.push_field(entry.key().as_bytes())?;
            keys.push(alloc::string::String::from(entry.key()));
        }
        self.push(self.options.terminator().bytes());
        *state.borrow_mut() = Some(keys);
        for index in 0..object.len() {
            if index > 0 {
                self.push_byte(self.options.delimiter());
            }
            let entry = object.get_index(index).map_err(map_data)?.ok_or_else(unrepresentable)?;
            self.push_located_field(&entry.value(), resources)?;
        }
        self.push(self.options.terminator().bytes());
        Ok(())
    }

    fn push_located_field(
        &mut self,
        view: &ValueView<'_, '_>,
        resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // CSV spells no tag, so a tagged field publishes its payload and says so; navigation is already
        // payload-transparent underneath.
        if let TagLayer::Tagged(_) = view_tag_layer(*view)? {
            project_tag(resources);
        }
        if matches!(view.kind().map_err(map_data)?, jqf_data::ValueKind::Null) {
            self.push(b"");
            return Ok(());
        }
        let scalar = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)?;
        match scalar {
            ScalarView::String(text) => self.push_field(text.as_bytes()),
            ScalarView::Bool(boolean) => {
                self.push(if boolean { b"true" } else { b"false" });
                Ok(())
            }
            ScalarView::Number(number) => match number {
                jqf_data::NumberView::Integer(text) => {
                    self.push(text.as_bytes());
                    Ok(())
                }
                jqf_data::NumberView::Number(number) => {
                    // The inline machine arm renders its canonical spelling on demand; the boxed arm borrows its
                    // retained one.
                    if let Some(machine) = number.as_machine() {
                        let integer = jqf_data::Integer::from_i64(machine);
                        self.push(integer.as_str().as_bytes());
                        Ok(())
                    } else if let Some(integer) = number.as_integer() {
                        self.push(integer.as_str().as_bytes());
                        Ok(())
                    } else if let Some(decimal) = number.as_decimal() {
                        self.push_decimal(decimal.coefficient().as_str(), decimal.scale())
                    } else if let Some(float) = number.as_float() {
                        self.push_number_text(float)
                    } else {
                        Err(unrepresentable())
                    }
                }
                jqf_data::NumberView::Decimal { coefficient, scale } => self.push_decimal(coefficient, scale),
                jqf_data::NumberView::Float(float) => self.push_number_text(float),
            },
            ScalarView::Null => {
                self.push(b"");
                Ok(())
            }
            ScalarView::Bytes(_)
            | ScalarView::LocalDate(_)
            | ScalarView::LocalTime(_)
            | ScalarView::LocalDateTime(_)
            | ScalarView::OffsetDateTime(_) => self.push_projected_field(&scalar, resources),
        }
    }

    /// Writes one float field under the `@csv` non-finite law: a NaN cell is EMPTY (`[1,nan,2] | @csv` is `1,,2`), and
    /// either infinity writes the clamped double `format_binary64` already renders. One shared helper for the owned and
    /// located float arms so the law cannot drift.
    fn push_number_text(&mut self, float: jqf_data::Float) -> Result<(), CodecError> {
        if float.get().is_nan() {
            return Ok(());
        }
        let rendered = jqf_data::format_binary64(float.get()).ok_or_else(unrepresentable)?;
        self.push(rendered.as_str().as_bytes());
        Ok(())
    }

    /// Writes one field with exact RFC 4180 quoting under the CSV grammar, or — under the TSV no-quote grammar —
    /// refuses a field the dialect cannot represent: a field containing the TAB delimiter, CR, or LF has no quote or
    /// escape to protect it, so it is `UnsupportedRepresentation` (never silently emitted unquoted). A `"` is ordinary
    /// data under TSV. RFC 4180 quote-forcing: the field needs quotes when it contains the delimiter, a quote, CR, or
    /// LF. The comma-delimited stop set is exactly that predicate; wide cells take the kernel, short and
    /// exotic-delimiter cells keep the scalar any.
    fn field_needs_quote(&self, bytes: &[u8]) -> bool {
        if bytes.len() >= crate::scan::WIDE_SCAN_MIN && self.options.delimiter() == b',' {
            return byte_scan::prefix_len::<Csv>(bytes) != bytes.len();
        }
        let delimiter = self.options.delimiter();
        bytes
            .iter()
            .any(|&byte| byte == delimiter || byte == b'"' || byte == b'\n' || byte == b'\r')
    }

    fn push_field(&mut self, bytes: &[u8]) -> Result<(), CodecError> {
        if self.options.quote().is_none() {
            if bytes
                .iter()
                .any(|&byte| byte == b'\t' || byte == b'\n' || byte == b'\r')
            {
                return Err(unrepresentable());
            }
            self.push(bytes);
            return Ok(());
        }
        let needs_quote = self.field_needs_quote(bytes);
        if !needs_quote {
            self.push(bytes);
            return Ok(());
        }
        // Pre-reserve the worst case (every byte a doubled quote, plus the two surrounding quotes) so the run pushes
        // below never allocate.
        self.bytes
            .try_reserve_exact(bytes.len().saturating_mul(2).saturating_add(2))
            .map_err(jqf_resource::ResourceError::from)?;
        self.push(b"\"");
        // Runs between quotes pass through verbatim; only `"` doubles. The delimiter, CR, and LF need no special
        // handling inside a quoted field, so they stay inside the runs.
        let mut run_start = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            if byte == b'"' {
                self.push(&bytes[run_start..index]);
                self.push(b"\"\"");
                run_start = index + 1;
            }
        }
        self.push(&bytes[run_start..]);
        self.push(b"\"");
        Ok(())
    }

    const fn report() -> PreservationReport {
        PreservationReport::new(
            PreservationOutcome::Normalized,
            PreservationOutcome::Omitted,
            PreservationOutcome::Exact,
            PreservationOutcome::Normalized,
        )
    }
}

impl EncoderSession for CsvEncoder {
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
            if self.row_done {
                if self.bytes.is_empty() {
                    return Ok(Self::report());
                }
                sink.write_all(&self.bytes, context.resources())?;
                self.bytes.clear();
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
                WorkAdmission::Granted(granted) => {
                    for _ in 0..granted {
                        if self.row_done || self.bytes.len() >= OFFER_BYTES {
                            break;
                        }
                        self.encode_row(item, context.resources())?;
                        self.row_done = true;
                    }
                }
            }
        }
    }
}

fn unrepresentable() -> CodecError {
    CodecError::new(CodecFailureKind::UnsupportedRepresentation)
}

fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "CSV encoder document access")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_codec_core::{DiagnosticPolicy, PreservationRequest};
    use jqf_data::{Array, DialectId, Float, FormatId, Number, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn encoder() -> CsvEncoder {
        CsvEncoder {
            bytes: Vec::new(),
            options: CsvEncodeOptions::default(),
            header: None,
            row_done: false,
        }
    }

    fn owned(values: &[Value]) -> Value {
        let mut owned = alloc::vec::Vec::new();
        for value in values {
            owned.push(value.clone());
        }
        Value::Array(Array::try_from_vec(owned).expect("array"))
    }

    #[test]
    fn an_owned_scalar_root_is_a_single_field_row() {
        // Mirrors the located arm: `-n '5' --output-format csv` publishes the same `5\r\n` stdin `5` does (CSV output
        // is CRLF).
        let mut encoder = encoder();
        let resources = resources();
        encoder
            .encode_owned_row(
                &Value::Number(Number::integer(jqf_data::Integer::from_i64(5))),
                &resources,
            )
            .expect("encodes");
        assert_eq!(encoder.bytes.as_slice(), b"5\r\n");
    }

    #[test]
    fn non_finite_fields_match_at_csv() {
        // `[1,nan,2] | @csv` is `1,,2`, and `[Infinity,-Infinity] | @csv` is the clamped double pair. The encoder's
        // bytes are the same law.
        let mut encoder = encoder();
        let resources = resources();
        encoder
            .encode_owned_row(
                &owned(&[
                    Value::Number(Number::integer(jqf_data::Integer::from_i64(1))),
                    Value::Number(Number::float(Float::new(f64::NAN))),
                    Value::Number(Number::float(Float::new(f64::INFINITY))),
                    Value::Number(Number::float(Float::new(f64::NEG_INFINITY))),
                ]),
                &resources,
            )
            .expect("encodes");
        assert_eq!(
            encoder.bytes.as_slice(),
            b"1,,1.7976931348623157e+308,-1.7976931348623157e+308\r\n"
        );
    }

    #[test]
    fn quoted_field_round_trips_with_doubled_quotes() {
        // The run-based push_field rewrites the same bytes the byte-at-a-time loop did: quotes double, delimiter and
        // newline pass through.
        let mut encoder = encoder();
        encoder.push_field(b"a\"b,c\nd").expect("encodes");
        assert_eq!(encoder.bytes.as_slice(), b"\"a\"\"b,c\nd\"");
    }

    #[test]
    fn a_tsv_row_joins_with_tabs_keeps_lf_and_refuses_unquoted_breaks() {
        // The TSV output dialect: TAB joins, LF terminates, and a field containing TAB/CR/LF is unrepresentable (no
        // quote to protect it).
        let mut encoder = CsvEncoder {
            bytes: Vec::new(),
            options: CsvEncodeOptions::try_new_tsv().expect("tsv options"),
            header: None,
            row_done: false,
        };
        let resources = resources();
        encoder
            .encode_owned_row(
                &owned(&[
                    Value::String(jqf_data::Shared::<str>::try_from_str("a").expect("s")),
                    Value::String(jqf_data::Shared::<str>::try_from_str("b\"c").expect("s")),
                ]),
                &resources,
            )
            .expect("encodes");
        // A quote is ordinary data under TSV; the terminator is LF.
        assert_eq!(encoder.bytes.as_slice(), b"a\tb\"c\n");
        // A field containing TAB, CR, or LF is refused, never emitted unquoted.
        for forbidden in [b"a\tb", b"a\nb", b"a\rb"] {
            let mut encoder = CsvEncoder {
                bytes: Vec::new(),
                options: CsvEncodeOptions::try_new_tsv().expect("tsv options"),
                header: None,
                row_done: false,
            };
            assert!(
                encoder
                    .push_field(forbidden)
                    .expect_err("must refuse a TSV-unrepresentable field")
                    .kind()
                    == CodecFailureKind::UnsupportedRepresentation
            );
        }
    }

    #[test]
    fn create_registered_factory_still_opens_the_headered_dialect() {
        let mut resources = resources();
        let format = FormatId::try_new(FORMAT_ID).expect("format");
        let dialect = DialectId::try_new(JQF_RFC4180_HEADER_DIALECT_ID).expect("dialect");
        create_registered_factory(
            EncodeRequest {
                format: &format,
                dialect: &dialect,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::None,
                options: None,
            },
            &mut resources,
        )
        .expect("headered catalog encode stays open for query output");
    }

    #[test]
    fn create_registered_factory_still_encodes_a_headered_row() {
        let mut encoder = CsvEncoder {
            bytes: Vec::new(),
            options: CsvEncodeOptions::default(),
            header: Some(alloc::rc::Rc::new(core::cell::RefCell::new(None))),
            row_done: false,
        };
        let resources = resources();
        let mut builder = jqf_data::ObjectBuilder::new();
        builder
            .try_insert_last(
                jqf_data::ObjectKey::try_from_str("name").expect("k"),
                Value::String(jqf_data::Shared::<str>::try_from_str("ada").expect("v")),
            )
            .expect("insert");
        let row = Value::Object(builder.try_finish().expect("object"));
        encoder.encode_owned_headered_row(&row, &resources).expect("encodes");
        assert_eq!(encoder.bytes.as_slice(), b"name\r\nada\r\n");
    }

    /// The disjoint-delimiter law of [`removal_cuts`]: adjacent removed fields must never claim the same comma. The
    /// load-bearing row is the leading pair — field 0 has no preceding delimiter, so under the old
    /// both-take-the-following/preceding split the two cuts overlapped on the first comma and spliced `,c` back instead
    /// of `c`.
    #[test]
    fn adjacent_removals_cut_disjoint_delimiters() {
        let source = b"a,b,c";
        // Fields 0+1 removed: [0,2) takes the following comma, so field 1's preceding comma is consumed and it takes
        // the following one instead.
        let cuts = removal_cuts(source, b',', &[(0, 1), (2, 3)]);
        assert_eq!(cuts[0].start..cuts[0].end, 0..2);
        assert_eq!(cuts[1].start..cuts[1].end, 2..4);
        // Middle pair: each cut takes its own preceding comma.
        let cuts = removal_cuts(source, b',', &[(2, 3), (4, 5)]);
        assert_eq!(cuts[0].start..cuts[0].end, 1..3);
        assert_eq!(cuts[1].start..cuts[1].end, 3..5);
        // Non-adjacent pair keeps the survivor's delimiters whole.
        let cuts = removal_cuts(source, b',', &[(0, 1), (4, 5)]);
        assert_eq!(cuts[0].start..cuts[0].end, 0..2);
        assert_eq!(cuts[1].start..cuts[1].end, 3..5);
        // Splicing each batch back reproduces the expected survivors.
        let splice = |cuts: &[EditRemoval]| {
            let mut out = alloc::vec::Vec::new();
            let mut cursor = 0usize;
            for cut in cuts {
                out.extend_from_slice(&source[cursor..cut.start]);
                cursor = cut.end;
            }
            out.extend_from_slice(&source[cursor..]);
            out
        };
        assert_eq!(splice(&removal_cuts(source, b',', &[(0, 1), (2, 3)])), b"c");
        assert_eq!(splice(&removal_cuts(source, b',', &[(2, 3), (4, 5)])), b"a");
        assert_eq!(splice(&removal_cuts(source, b',', &[(0, 1), (4, 5)])), b"b");
        // A last-field removal takes its preceding comma; a single-field row has no delimiter to take and cuts bare.
        let cuts = removal_cuts(source, b',', &[(4, 5)]);
        assert_eq!(cuts[0].start..cuts[0].end, 3..5);
        assert_eq!(&splice(&cuts), b"a,b");
        let cuts = removal_cuts(b"abc", b',', &[(0, 3)]);
        assert_eq!(cuts[0].start..cuts[0].end, 0..3);
    }
}
