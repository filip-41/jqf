//! One portable request pipeline and its publication boundary.

mod cursor;
mod edit;
mod encode;
mod input_sequence;
mod records;
mod sequence;
mod stream_events;
mod value;

#[allow(unused_imports)]
pub(crate) use cursor::*;
pub use edit::*;
pub use encode::*;
pub use input_sequence::*;
pub use records::*;
pub(crate) use sequence::*;
pub use stream_events::*;
pub use value::*;

use std::borrow::ToOwned;
use std::collections::{BTreeMap, HashMap};

use std::boxed::Box;

use std::format;

use std::rc::Rc;

use std::string::String;

use std::vec;

use std::vec::Vec;

use core::cell::{Cell, RefCell};

use crate::Diagnostics;

use crate::patch::{BytePatch, PatchError, PatchSet};

use jqf_codec_core::{
    AccessBindError, AccessReport, AccessRequirement, CodecDescriptor, CodecError, CodecRegistration, CodecRunContext,
    DecodeRequest, DiagnosticPolicy, DocumentProduct, EncodeItem, EncodeRequest, ErasedEncoderFactory, ErasedProvider,
    PhysicalRouteId, PhysicalRouteReceipt, PreservationReport, PreservationRequest, RecordBatch, RecordBatchLimit,
    RecordEntry, RecordIssueCode, RecordIssueSeverity, RecordPoll, ReusableAccessSession, ReusableEncoderSession,
    RouteCapability, RouteSlot,
};

use jqf_data::{
    Array, BatchLimit, DataError, DialectId, Document, FactPayloadView, FormatId, Integer, LocalOwnerRef, NodeId,
    Number, ObjectBuilder, ObjectKey, Value, ValueKind, ValueView,
};

use jqf_engine::{
    ArithFailure, CodecInputOutcome, CodecInputResult, CodecRequirementPolicy, CompiledProgram, EngineResult,
    EngineRun, EngineRunError, EngineRunStream, EventParser, FactDelta, InputSource, RunInput, RunPoll, StreamEvent,
    raised_body, raised_frame_note, try_lower_root_requirement, values_semantically_equal,
};

use jqf_resource::{ResourceContext, ResourceError, WorkAdmission};

use jqf_source::{LabelStyle, ResolvedSource, SourceId, SourceKind, SourceRef};

/// Pre-resolved `(format, dialect)` and format indices for a catalog.
///
/// Built once per inventory so a morsel does not rescan every registration.
/// The catalog borrows it; the owner (CLI request, leaked record catalog)
/// keeps it alive.
#[derive(Debug)]
pub struct CatalogIndex {
    by_pair: HashMap<String, HashMap<String, Vec<u16>>>,
    by_format: HashMap<String, Vec<u16>>,
}

impl CatalogIndex {
    /// Indexes `registrations` by format and by `(format, dialect)`.
    ///
    /// # Panics
    ///
    /// Panics if `registrations` has more than `u16::MAX` entries.
    #[must_use]
    pub fn build(registrations: &[&CodecRegistration<'_>]) -> Self {
        let mut by_pair: HashMap<String, HashMap<String, Vec<u16>>> = HashMap::new();
        let mut by_format: HashMap<String, Vec<u16>> = HashMap::new();
        for (index, registration) in registrations.iter().enumerate() {
            let index = u16::try_from(index).expect("codec catalog fits in u16");
            let descriptor = registration.descriptor();
            let format = descriptor.format().as_str();
            by_format.entry(format.to_owned()).or_default().push(index);
            for dialect in descriptor.dialects() {
                by_pair
                    .entry(format.to_owned())
                    .or_default()
                    .entry(dialect.as_str().to_owned())
                    .or_default()
                    .push(index);
            }
        }
        Self { by_pair, by_format }
    }
}

/// A caller-owned immutable inventory of validated concrete registrations.
#[derive(Clone, Copy)]
pub struct CodecCatalog<'catalog, 'registration> {
    registrations: &'catalog [&'catalog CodecRegistration<'registration>],
    index: Option<&'catalog CatalogIndex>,
}

impl core::fmt::Debug for CodecCatalog<'_, '_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CodecCatalog")
            .field("registrations", &self.registrations.len())
            .finish_non_exhaustive()
    }
}

impl<'catalog, 'registration> CodecCatalog<'catalog, 'registration> {
    /// Borrows an inventory. Duplicate matching operations are rejected during selection.
    #[must_use]
    pub const fn new(registrations: &'catalog [&'catalog CodecRegistration<'registration>]) -> Self {
        Self {
            registrations,
            index: None,
        }
    }

    /// Attaches a pre-resolved index. Lookups then hash instead of scanning.
    #[must_use]
    pub const fn with_index(self, index: &'catalog CatalogIndex) -> Self {
        Self {
            index: Some(index),
            ..self
        }
    }

    fn registrations_for_pair(&self, format: &FormatId, dialect: &DialectId) -> Option<&[u16]> {
        let index = self.index?;
        index
            .by_pair
            .get(format.as_str())?
            .get(dialect.as_str())
            .map(Vec::as_slice)
    }

    fn visit_pair(
        &self,
        format: &FormatId,
        dialect: &DialectId,
        mut visit: impl FnMut(&'catalog CodecRegistration<'registration>) -> Result<(), RegistryFailure>,
    ) -> Result<(), RegistryFailure> {
        if self.index.is_some() {
            if let Some(indices) = self.registrations_for_pair(format, dialect) {
                for &index in indices {
                    visit(self.registrations[index as usize])?;
                }
            }
            return Ok(());
        }
        for registration in self.registrations {
            let descriptor = registration.descriptor();
            if descriptor.format().as_str() == format.as_str()
                && descriptor
                    .dialects()
                    .iter()
                    .any(|candidate| candidate.as_str() == dialect.as_str())
            {
                visit(registration)?;
            }
        }
        Ok(())
    }

    fn decoder(
        &self,
        format: &FormatId,
        dialect: &DialectId,
    ) -> Result<jqf_codec_core::DecoderFactoryRecord, RegistryFailure> {
        let mut selected = None;
        self.visit_pair(format, dialect, |registration| {
            if let Some(decoder) = registration.decoder() {
                if selected.is_some() {
                    return Err(RegistryFailure::AmbiguousDecoder);
                }
                selected = Some(decoder);
            }
            Ok(())
        })?;
        selected.ok_or(RegistryFailure::DecoderUnavailable)
    }

    fn encoder(
        &self,
        format: &FormatId,
        dialect: &DialectId,
    ) -> Result<jqf_codec_core::EncoderFactoryRecord, RegistryFailure> {
        let mut selected = None;
        self.visit_pair(format, dialect, |registration| {
            if let Some(encoder) = registration.encoder() {
                if selected.is_some() {
                    return Err(RegistryFailure::AmbiguousEncoder);
                }
                selected = Some(encoder);
            }
            Ok(())
        })?;
        selected.ok_or(RegistryFailure::EncoderUnavailable)
    }

    /// The registered record-provider factory for a format (127 A3), selected
    /// by format identity alone: every record format registers exactly one
    /// provider for its whole dialect set (NDJSON's two profiles share it).
    pub fn record_provider(
        &self,
        format: &FormatId,
    ) -> Result<jqf_codec_core::RecordProviderFactoryRecord, RegistryFailure> {
        let mut selected = None;
        let mut visit = |registration: &CodecRegistration<'registration>| {
            if let Some(provider) = registration.record_provider() {
                if selected.is_some() {
                    return Err(RegistryFailure::AmbiguousDecoder);
                }
                selected = Some(provider);
            }
            Ok(())
        };
        if let Some(index) = self.index {
            if let Some(indices) = index.by_format.get(format.as_str()) {
                for &i in indices {
                    visit(self.registrations[i as usize])?;
                }
            }
        } else {
            for registration in self.registrations {
                if registration.descriptor().format().as_str() == format.as_str() {
                    visit(registration)?;
                }
            }
        }
        selected.ok_or(RegistryFailure::DecoderUnavailable)
    }

    /// The CLI-facing routes the exact `(format, dialect)` registration
    /// declares. The declaration lives on the registration, so
    /// this is a static lookup miss for a `(format, dialect)` pair no
    /// registration owns — the CLI reads its input-model and record-route
    /// facts from here instead of re-declaring them as `match` arms.
    pub fn route_capabilities(
        &self,
        format: &FormatId,
        dialect: &DialectId,
    ) -> Result<&'registration [RouteCapability], RegistryFailure> {
        let mut selected = None;
        self.visit_pair(format, dialect, |registration| {
            if selected.is_some() {
                return Err(RegistryFailure::AmbiguousDialect);
            }
            selected = Some(registration.descriptor().route_capabilities());
            Ok(())
        })?;
        selected.ok_or(RegistryFailure::DialectUnavailable)
    }

    /// The inter-item byte owner the exact `(format, dialect)` registration
    /// declares. One declaration per dialect, aligned on
    /// the registration — the CLI derives both its output-lane and its
    /// edit-lane facade suffixes from here instead of re-declaring per-format
    /// `match` arms. The `(format, dialect)` pair selects the same static
    /// row [`Self::route_capabilities`] selects, so a miss is the same
    /// [`RegistryFailure::DialectUnavailable`] and an ambiguity the same
    /// [`RegistryFailure::AmbiguousDialect`].
    pub fn item_byte_owner(
        &self,
        format: &FormatId,
        dialect: &DialectId,
    ) -> Result<jqf_codec_core::ItemByteOwner, RegistryFailure> {
        let mut selected = None;
        self.visit_pair(format, dialect, |registration| {
            let descriptor = registration.descriptor();
            let Some(dialect_index) = descriptor
                .dialects()
                .iter()
                .position(|candidate| candidate.as_str() == dialect.as_str())
            else {
                return Ok(());
            };
            if selected.is_some() {
                return Err(RegistryFailure::AmbiguousDialect);
            }
            selected = Some(descriptor.inter_item_byte()[dialect_index]);
            Ok(())
        })?;
        selected.ok_or(RegistryFailure::DialectUnavailable)
    }

    /// The insignificant inter-value whitespace the exact `(format, dialect)`
    /// registration declares for its adjacent-value stream. The CLI reads
    /// this onto the request's `DecodeRequest::value_separator` instead of
    /// re-declaring a per-format list: a codec whose adjacent-value contract
    /// assumes the drive skips its trivia declares the set beside the
    /// capability that requires it. A miss is the same
    /// [`RegistryFailure::DialectUnavailable`] the other descriptor reads
    /// produce.
    pub fn value_separators(
        &self,
        format: &FormatId,
        dialect: &DialectId,
    ) -> Result<&'registration [u8], RegistryFailure> {
        let mut selected = None;
        self.visit_pair(format, dialect, |registration| {
            if selected.is_some() {
                return Err(RegistryFailure::AmbiguousDialect);
            }
            selected = Some(registration.descriptor().value_separators());
            Ok(())
        })?;
        selected.ok_or(RegistryFailure::DialectUnavailable)
    }

    /// Resolves a filename extension to the format (and its default input
    /// dialect) whose registration declared it. Extensions choose
    /// a FORMAT, never a dialect: only the format's default-dialect
    /// registration declares them, so a second registration claiming the same
    /// extension is the same registration-bug class as every other ambiguous
    /// catalog lookup — reported as [`RegistryFailure::AmbiguousExtension`],
    /// never a silent winner. An extension no registration declared is
    /// [`RegistryFailure::ExtensionUnavailable`], which the CLI treats as the
    /// json fallback. Whole-file names with no extension (`.env`, `Makefile`)
    /// are claimed through the `filenames` fact beside extensions and
    /// resolved by [`Self::detect_by_filename`], which falls
    /// back to this extension lookup last.
    ///
    /// # Panics
    ///
    /// Panics only on host allocation failure while materializing the
    /// detected identities. The identities themselves are validated at
    /// registration construction (`FormatIdRef::from_static` asserts), so a
    /// registered descriptor cannot fail validation.
    pub fn detect_by_extension(&self, extension: &str) -> Result<(FormatId, DialectId), RegistryFailure> {
        let mut selected = None;
        for registration in self.registrations {
            let descriptor = registration.descriptor();
            if descriptor.extensions().contains(&extension) {
                if selected.is_some() {
                    return Err(RegistryFailure::AmbiguousExtension);
                }
                // Registered identities are validated at construction
                // (`FormatIdRef::from_static` asserts), so only the
                // allocation arm of `try_new` can fail — the same host-OOM
                // class every allocation in this crate treats as fatal.
                let format = FormatId::try_new(descriptor.format().as_str())
                    .expect("registered format identities are valid and allocatable");
                // A registered descriptor always owns at least one dialect
                // (`try_new` rejects an empty dialect set), and the first is
                // the format's default INPUT dialect.
                let dialect = DialectId::try_new(descriptor.dialects()[0].as_str())
                    .expect("registered dialect identities are valid and allocatable");
                selected = Some((format, dialect));
            }
        }
        selected.ok_or(RegistryFailure::ExtensionUnavailable)
    }

    /// Resolves a FULL filename to the format (and its default input
    /// dialect) whose registration claimed it. A filename is
    /// the whole file name — `.env` and `Makefile` have no extension, so
    /// extension matching alone can never reach them. Precedence is exact
    /// name, then glob, then extension: `.env.local` resolves by an exact
    /// `.env.local` claim, else by a `.env.*` glob claim, and only then by
    /// a `.local` extension claim. The first two steps read the
    /// registration's `filenames` fact; the extension fallback is exactly
    /// [`Self::detect_by_extension`], so a file whose name no registration
    /// claimed resolves byte-identically to the pre-filename law. An exact
    /// name always beats a glob, so a glob claim is never consulted while an
    /// exact claim exists. Two registrations claiming the same exact
    /// filename, or globs that both match the same filename, is the same
    /// registration-bug class as every other ambiguous catalog lookup —
    /// reported as [`RegistryFailure::AmbiguousFilename`], never a silent
    /// winner. A name no registration claimed is
    /// [`RegistryFailure::FilenameUnavailable`] when it has no extension and
    /// [`RegistryFailure::ExtensionUnavailable`] when its extension is
    /// unregistered; the CLI treats both as the json fallback.
    ///
    /// # Panics
    ///
    /// Panics only on host allocation failure while materializing the
    /// detected identities, exactly as in [`Self::detect_by_extension`].
    pub fn detect_by_filename(&self, filename: &str) -> Result<(FormatId, DialectId), RegistryFailure> {
        // Registered identities are validated at construction
        // (`FormatIdRef::from_static` asserts), so only the allocation arm
        // of `try_new` can fail — the same host-OOM class every allocation
        // in this crate treats as fatal. A registered descriptor always owns
        // at least one dialect (`try_new` rejects an empty dialect set), and
        // the first is the format's default INPUT dialect.
        let materialize = |descriptor: &CodecDescriptor<'registration>| {
            let format = FormatId::try_new(descriptor.format().as_str())
                .expect("registered format identities are valid and allocatable");
            let dialect = DialectId::try_new(descriptor.dialects()[0].as_str())
                .expect("registered dialect identities are valid and allocatable");
            (format, dialect)
        };

        // Step one: exact names. An exact claim wins outright over any glob.
        let mut selected = None;
        for registration in self.registrations {
            let descriptor = registration.descriptor();
            if descriptor.filenames().contains(&filename) {
                if selected.is_some() {
                    return Err(RegistryFailure::AmbiguousFilename);
                }
                selected = Some(materialize(descriptor));
            }
        }
        if let Some(selected) = selected {
            return Ok(selected);
        }

        // Step two: filename globs (a trailing-`*` pattern whose star
        // matches any suffix). Only consulted when no exact name claimed the
        // file, so an exact name always beats a glob.
        let mut selected = None;
        for registration in self.registrations {
            let descriptor = registration.descriptor();
            if descriptor
                .filenames()
                .iter()
                .any(|pattern| Self::filename_glob_matches(pattern, filename))
            {
                if selected.is_some() {
                    return Err(RegistryFailure::AmbiguousFilename);
                }
                selected = Some(materialize(descriptor));
            }
        }
        if let Some(selected) = selected {
            return Ok(selected);
        }

        // Step three: the extension fallback, byte-identical to the
        // pre-filename law. A name with no extension (`Makefile`) is
        // FilenameUnavailable, the CLI's json fallback.
        match std::path::Path::new(filename)
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some(extension) => self.detect_by_extension(extension),
            None => Err(RegistryFailure::FilenameUnavailable),
        }
    }

    /// Whether a registered filename pattern matches a filename. A pattern ending in `*` is a glob whose star
    /// matches any (possibly empty) suffix — `.env.*` matches `.env.local`
    /// but not `.env`; any other pattern is an exact name. A star elsewhere
    /// in a pattern is not a wildcard.
    fn filename_glob_matches(pattern: &str, filename: &str) -> bool {
        match pattern.strip_suffix('*') {
            Some(prefix) => filename.strip_prefix(prefix).is_some(),
            None => pattern == filename,
        }
    }

    /// The extensions the registrations of one format declare for implicit
    /// input-format detection, as declared. Only the format's
    /// default-dialect registration declares them, so a second registration
    /// declaring any extension is the same ambiguity bug
    /// [`Self::detect_by_extension`] reports — surfaced here too rather than
    /// silently choosing one. A registered format that declares none (json-seq,
    /// render) answers the empty list, not an error; a format no registration
    /// owns is [`RegistryFailure::DialectUnavailable`].
    pub fn extensions_for(&self, format: &FormatId) -> Result<&'registration [&'registration str], RegistryFailure> {
        let mut selected = None;
        let mut owns_format = false;
        for registration in self.registrations {
            let descriptor = registration.descriptor();
            if descriptor.format().as_str() != format.as_str() {
                continue;
            }
            owns_format = true;
            if !descriptor.extensions().is_empty() {
                if selected.is_some() {
                    return Err(RegistryFailure::AmbiguousExtension);
                }
                selected = Some(descriptor.extensions());
            }
        }
        if !owns_format {
            return Err(RegistryFailure::DialectUnavailable);
        }
        Ok(selected.unwrap_or(&[]))
    }
}

/// Registry selection failure before codec execution starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryFailure {
    /// No registration can decode the exact input format and dialect.
    DecoderUnavailable,
    /// More than one registration claims the exact decoder operation.
    AmbiguousDecoder,
    /// No registration can encode the exact output format and dialect.
    EncoderUnavailable,
    /// More than one registration claims the exact encoder operation.
    AmbiguousEncoder,
    /// No registration owns the exact format and dialect pair.
    DialectUnavailable,
    /// More than one registration owns the exact format and dialect pair.
    AmbiguousDialect,
    /// No registration declared the requested filename extension.
    ExtensionUnavailable,
    /// More than one registration claims the requested filename extension
    /// — a registration bug, never a silent winner.
    AmbiguousExtension,
    /// No registration claimed the requested full filename — no exact name,
    /// no matching glob, and the filename has no extension.
    FilenameUnavailable,
    /// More than one registration claims the requested full filename (plan
    /// 137 D3) — two exact names, or globs that both match — a registration
    /// bug, never a silent winner.
    AmbiguousFilename,
}

impl core::fmt::Display for RegistryFailure {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::DecoderUnavailable => "no registered codec decodes the requested format and dialect",
            Self::AmbiguousDecoder => "more than one registered codec claims the requested decoder",
            Self::EncoderUnavailable => "no registered codec encodes the requested format and dialect",
            Self::AmbiguousEncoder => "more than one registered codec claims the requested encoder",
            Self::DialectUnavailable => "no registered codec owns the requested format and dialect",
            Self::AmbiguousDialect => "more than one registered codec owns the requested format and dialect",
            Self::ExtensionUnavailable => "no registered codec declares the requested filename extension",
            Self::AmbiguousExtension => "more than one registered codec claims the requested filename extension",
            Self::FilenameUnavailable => "no registered codec claims the requested file name",
            Self::AmbiguousFilename => "more than one registered codec claims the requested file name",
        })
    }
}

/// Host sink receiving one ordered item's published bytes at a time.
pub trait ItemSink {
    /// Host-specific failure preserved by [`PipelineError`].
    type Error;

    /// Marks the start of one ordered item. An error means no item boundary was accepted.
    ///
    /// A per-value codec skip (the one admitted kind, `RawNulByte`) opens the
    /// boundary and then publishes nothing for it, so a sink must tolerate a
    /// `begin_item` that is never followed by [`Self::finish_item`]: the
    /// skipped value's boundary simply never completes, and the next item
    /// opens normally.
    fn begin_item(&mut self, index: u64) -> Result<(), Self::Error>;

    /// Marks the start of one ordered item whose destination NAME the split
    /// program produced: one destination per
    /// published item. The default delegates to [`Self::begin_item`], so every
    /// existing sink is untouched; a split sink overrides it to open the
    /// named destination. Called in place of [`Self::begin_item`] exactly when
    /// the encoding policy carries a split program.
    fn begin_item_named(&mut self, index: u64, _name: &str) -> Result<(), Self::Error> {
        self.begin_item(index)
    }

    /// Publishes a nonempty prefix and returns its exact length.
    ///
    /// Returning `Ok(0)` or a value larger than `bytes.len()` violates the sink
    /// contract. Returning `Err` guarantees that no prefix was published.
    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error>;

    /// Whether writes into this sink should pay the visible-progress
    /// control check. Worker-private buffers return false.
    fn observes_host_progress(&self) -> bool {
        true
    }

    /// Marks an item complete after its codec bytes and facade suffix are published.
    fn finish_item(&mut self, index: u64, report: EncodedItemReport) -> Result<(), Self::Error>;

    /// Reports one per-value runtime error as an adjacent-value sequence
    /// continues past it. the reference reports such an error to stderr and proceeds to the
    /// next value; only `execute_sequence` calls this (single-value `execute_value_document`
    /// aborts instead). The default drops the notification, so existing sinks are
    /// unaffected; a host mirroring the reference's stderr overrides it. Byte publication to
    /// the sink is never affected by this call.
    fn report_value_error(&mut self, error: SequenceValueError) -> Result<(), Self::Error> {
        let _ = error;
        Ok(())
    }

    /// Reports one record-stream issue in ordinal order as a RECORD sequence
    /// continues past it. Only `execute_record_sequence` calls this. The
    /// default drops the notification; a host mirroring the issue to stderr
    /// overrides it. Byte publication to the sink is never affected.
    fn report_record_issue(&mut self, issue: RecordIssueReport<'_>) -> Result<(), Self::Error> {
        let _ = issue;
        Ok(())
    }

    /// Flushes published bytes to their destination before the drive blocks on
    /// the host source again.
    ///
    /// The streaming drive (`execute_sequence_streaming`) calls this before
    /// every refill so a live tail's already-published items reach the
    /// consumer before the stream ends — a buffered host would otherwise hold
    /// them until EOF, which reads exactly like the hang the drive exists to
    /// fix. The default is a no-op: a host that does not buffer (or does not
    /// stream) is unaffected, and `--unbuffered`'s per-ITEM flush is a
    /// separate, stronger cadence the CLI sink owns.
    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// One record-stream issue, in ordinal order, as reported to a sink.
///
/// It is format-neutral by construction: an issue names a record ORDINAL, an
/// absolute source offset, a closed severity, and a closed code from the record
/// ABI. Rendering that into a message is the host's job, using the framing
/// codec's own text — the SDK never learns a format's spelling.
#[derive(Debug)]
pub struct RecordIssueReport<'report> {
    ordinal: u64,
    offset: u64,
    severity: RecordIssueSeverity,
    code: RecordIssueCode,
    cause: Option<&'report CodecError>,
}

impl<'report> RecordIssueReport<'report> {
    /// Physical ordinal this issue occupies.
    #[must_use]
    pub const fn ordinal(&self) -> u64 {
        self.ordinal
    }
    /// Absolute source offset of the fault.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }
    /// Whether this issue forces the request's failure class.
    #[must_use]
    pub const fn severity(&self) -> RecordIssueSeverity {
        self.severity
    }
    /// Stable issue classification.
    #[must_use]
    pub const fn code(&self) -> RecordIssueCode {
        self.code
    }
    /// The payload failure behind a [`RecordIssueCode::MalformedPayload`]
    /// issue, whose structured diagnostic the host renders exactly as it would
    /// a terminal decode failure.
    #[must_use]
    pub const fn cause(&self) -> Option<&'report CodecError> {
        self.cause
    }
}

/// The the reference class of a per-value runtime mismatch, distinguished because the reference renders
/// (and the CLI must render) two different message families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeMismatchClass {
    /// A `Key`/`Index` step addressed the wrong type ("Cannot index X with Y").
    Index,
    /// An `.[]` step iterated a non-iterable ("Cannot iterate over X").
    Iterate,
    /// An object construction dynamic key produced a non-string ("Cannot use X as
    /// object key"). Its `step_index` is not meaningful (a key is not a path step).
    ObjectKey,
    /// `length` over a value with no length (`"<type> has no length"`). Its
    /// `step_index` is not meaningful.
    NoLength,
    /// `keys` over a value with no keys (`"<type> has no keys"`). Its `step_index`
    /// is not meaningful.
    NoKeys,
    /// A binary operator failed on its operands ("X and Y cannot be added",
    /// divide-by-zero, or numeric range). The exact class is carried alongside as
    /// an [`ArithFailure`]; its `step_index`/`actual_type` are not meaningful.
    Arithmetic,
    /// A `.[a:b]` slice over an ARRAY or STRING carried a bound that is neither a
    /// number nor `null` ("Array/string slice indices must be integers"). the reference's
    /// message has no operand at all, so `step_index`/`actual_type` are not
    /// meaningful. A slice over a NON-sliceable input is
    /// [`RuntimeMismatchClass::Index`] instead — the input's type dispatches first.
    SliceIndices,
    /// A mismatch cell raised under the strict dial (`052` W3): a site where jq
    /// answers a VALUE became a raise. The message names the cell; the other
    /// fields are not meaningful.
    MismatchRaised,
    /// A `~generator` filter emitted two or more values on one pull ;
    /// the message names the phase. The other fields are not meaningful.
    EngineCardinality,
    /// A program-raised error VALUE (`error/0-1`) that no `try` caught. The value
    /// is carried by [`SequenceValueError::raised`]; the other fields are not
    /// meaningful.
    Raised,
    /// The ONE per-value CODEC failure the drive admits (`RawNulByte` under
    /// `--raw-output0`): jq skips the offending value and continues. Not a
    /// runtime mismatch; kept out of the message families above.
    Codec,
}

/// The error location of a single-run drive whose run never touched the
/// input: the reference renders `(at <unknown>)` (`-n '1|.b'`), so the SDK
/// marks the line with this sentinel and the CLI renders it without a
/// `` `file:line` `` frame. Distinct from 0, which the reference reports for a pull
/// that found the stream empty (the `break` raise).
pub const UNKNOWN_INPUT_LINE: u64 = u64::MAX;

/// One per-value runtime error surfaced to the sink as an adjacent-value
/// sequence continues past it (the reference's continue-on-error).
///
/// It grew a [`RuntimeMismatchClass::Raised`] class carrying an owned raised
/// value, so it is no longer `Copy`/`Eq` and its accessors borrow `self` (the
/// named public change of the try/catch vertical — `jqf_data::Value` implements
/// neither `Clone` nor `PartialEq`, the engine owning the one equality law).
#[derive(Debug)]
pub struct SequenceValueError {
    value_index: u64,
    input_line: u64,
    /// The source label of the file holding the failing value's last byte,
    /// when the drive can attribute one (a named input file); `None` means
    /// the host renders its own default (the CLI's `<stdin>`).
    filename: Option<String>,
    class: RuntimeMismatchClass,
    step_index: usize,
    actual_type: ValueKind,
    arith: Option<ArithFailure>,
    raised: Option<Value>,
    frame_note: &'static str,
    message: String,
}

/// THE one admission test for the per-value CODEC failure class: exactly
/// which codec failure kinds a drive continues past instead of treating as
/// terminal. Today exactly ONE kind qualifies — `RawNulByte` under
/// `--raw-output0`: jq skips the offending root string (nothing published for
/// it), prints one error line, and the stream continues with
/// last-value-decides. Every other codec failure stays terminal: no correct
/// output exists to continue past.
///
/// Every site that keys on this law reads this function — the stream drive's
/// per-item admission (`drive_run_stream`), the resident feed's per-value
/// classification (`jqf_runtime::feed::is_per_value_failure`), and the
/// per-value message rendering (`SequenceValueError::try_for_codec`) — so a
/// new admitted kind lands here and nowhere else.
#[must_use]
pub fn is_per_value_codec_kind(kind: jqf_codec_core::CodecFailureKind) -> bool {
    matches!(kind, jqf_codec_core::CodecFailureKind::RawNulByte)
}

impl SequenceValueError {
    /// One raised-value error (`error/0-1`) at `value_index`, rendered in the reference's
    /// uncaught-raise regimes. Returns the allocation failure that rendering the
    /// value's compact JSON hit, if any.
    /// One per-value CODEC failure (the single admitted kind, `RawNulByte`
    /// under `--raw-output0`): rendered in the same per-value stderr shape
    /// as a runtime mismatch, with jq's own wording.
    fn try_for_codec(value_index: u64, input_line: u64, filename: Option<&str>, error: &CodecError) -> Self {
        let message = if is_per_value_codec_kind(error.kind()) {
            "Cannot dump a string containing NUL with --raw-output0 option".to_owned()
        } else {
            // Unreachable today (the admission test above admits only
            // RawNulByte); the diagnostic's own message when one is attached,
            // else a generic.
            error.diagnostic().map_or_else(
                || "the value cannot be represented by the target".to_owned(),
                |diagnostic| diagnostic.message().to_owned(),
            )
        };
        Self {
            value_index,
            input_line,
            filename: filename.map(std::string::ToString::to_string),
            class: RuntimeMismatchClass::Codec,
            step_index: 0,
            actual_type: ValueKind::Null,
            arith: None,
            raised: None,
            frame_note: "",
            message,
        }
    }

    fn try_for_raised(
        value_index: u64,
        input_line: u64,
        filename: Option<&str>,
        value: Value,
    ) -> Result<Self, CodecError> {
        let frame_note = raised_frame_note(&value);
        let message = raised_body(&value).map_err(|_| allocation_failure())?;
        Ok(Self {
            value_index,
            input_line,
            filename: filename.map(std::string::ToString::to_string),
            class: RuntimeMismatchClass::Raised,
            step_index: 0,
            actual_type: ValueKind::Null,
            arith: None,
            raised: Some(value),
            frame_note,
            message,
        })
    }

    /// Zero-based index of the failing value within the adjacent sequence.
    #[must_use]
    pub const fn value_index(&self) -> u64 {
        self.value_index
    }
    /// The source label of the file holding the failing value's last byte,
    /// when the drive could attribute one.
    #[must_use]
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }
    /// Whether the value failed on an index step or an iterate step.
    #[must_use]
    pub const fn class(&self) -> RuntimeMismatchClass {
        self.class
    }
    /// Zero-based global failing path step.
    #[must_use]
    pub const fn step_index(&self) -> usize {
        self.step_index
    }
    /// Payload-transparent type observed at the failing step.
    #[must_use]
    pub const fn actual_type(&self) -> ValueKind {
        self.actual_type
    }
    /// The exact binary-arithmetic failure class, present only when
    /// [`Self::class`] is [`RuntimeMismatchClass::Arithmetic`].
    #[must_use]
    pub const fn arith(&self) -> Option<ArithFailure> {
        self.arith
    }
    /// The owned raised value, present only when [`Self::class`] is
    /// [`RuntimeMismatchClass::Raised`].
    #[must_use]
    pub const fn raised(&self) -> Option<&Value> {
        self.raised.as_ref()
    }
    /// the reference's `<stdin>:N` line for the failing value: the number of input newlines
    /// the reference's line-at-a-time lexer had consumed when the value came out — every
    /// newline up to and including the one that ends the line the value's last
    /// byte sits on, or all of them when no newline follows.
    #[must_use]
    pub const fn input_line(&self) -> u64 {
        self.input_line
    }
    /// The clause the reference places between the location and the colon: empty for every
    /// class except an uncaught raised NON-STRING, which reads ` (not a string)`.
    #[must_use]
    pub const fn frame_note(&self) -> &'static str {
        self.frame_note
    }
    /// The error text after the frame's colon, byte-identical to the reference's — rendered
    /// by the ENGINE at the raise site, where the operands the reference interpolates
    /// (`number (1) and string ("a")`) still exist, and carried here unchanged.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// the reference's `<stdin>:N` counter over one input's bytes.
///
/// the reference feeds its parser one LINE at a time, so the location it reports for a
/// value is how many newlines it had consumed when that value came out. Probed
/// against the reference: `1\n2\n3` reports `1,2,2`; `1 2\n3` reports `1,1,1`
/// (three values, one line boundary); `5\n\n\n` reports `1`, not `3`. Neither
/// a value ordinal, a next-value-start scan, nor a whole-input count reproduces
/// all three.
///
/// Over a MULTI-FILE source (`files` present) the counter resets at each file
/// boundary and the reported line is the count within the value's ENDING file
/// — the reference's per-file lexer position: `h1='1\n2\n'` `h2='3\n4\n'` reports
/// `1,2,1,2`, and a value spanning a boundary (a file ending `2` followed by
/// one starting `3` makes `23`) is attributed to the file holding its last
/// byte. `at_value_end` returns both the file-local line and the ending file's
/// label (when the source carries ranges).
pub(crate) struct InputLines<'a> {
    counted: u64,
    scanned: usize,
    files: Option<&'a [jqf_source::SourceFileRange<'a>]>,
    file: usize,
}

impl<'a> InputLines<'a> {
    const fn new() -> Self {
        Self {
            counted: 0,
            scanned: 0,
            files: None,
            file: 0,
        }
    }

    fn with_files(files: &'a [jqf_source::SourceFileRange<'a>]) -> Self {
        Self {
            counted: 0,
            scanned: 0,
            files: Some(files),
            file: 0,
        }
    }

    fn with_files_or_new(files: Option<&'a [jqf_source::SourceFileRange<'a>]>) -> Self {
        match files {
            Some(files) => Self::with_files(files),
            None => Self::new(),
        }
    }

    /// The line to report for a value whose bytes end at `end`.
    ///
    /// Calls must be non-decreasing in `end` — which the ordered sequence loop
    /// guarantees — so one run scans the input at most once in total even when
    /// every adjacent value fails.
    fn at_value_end(&mut self, bytes: &[u8], end: usize) -> u64 {
        // A value's last byte is a digit, a letter, or a closing delimiter, so
        // trimming trailing whitespace can only give back separator bytes the
        // codec counted as consumed — never any of the value itself.
        let mut cut = end.min(bytes.len());
        while cut > 0 && bytes.get(cut - 1).is_some_and(u8::is_ascii_whitespace) {
            cut -= 1;
        }
        let (search_from, file_end) = match self.files {
            None => (cut, bytes.len()),
            Some(files) => {
                // The value's TRUE end decides its file; a value ending at a
                // file boundary (possibly after separator whitespace that was
                // counted as consumed) is attributed to the file holding its
                // last byte, exactly as the reference attributes it. Ranges are
                // contiguous, so advancing past a range's end moves into the
                // next file; each advance resets the counter to that file's
                // start, which is what makes the line FILE-LOCAL.
                let last = u64::try_from(cut.saturating_sub(1)).unwrap_or(u64::MAX);
                while self.file + 1 < files.len() && last >= files[self.file].end() {
                    self.file += 1;
                    self.counted = 0;
                    self.scanned = usize::try_from(files[self.file].start()).unwrap_or(0);
                }
                (cut, usize::try_from(files[self.file].end()).unwrap_or(usize::MAX))
            }
        };
        let stop = bytes
            .get(search_from..file_end)
            .unwrap_or_default()
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(file_end, |offset| search_from + offset + 1);
        if stop > self.scanned {
            #[expect(
                clippy::naive_bytecount,
                reason = "counts newlines over one value's span at value boundaries only; \
                          a bytecount dependency is not warranted for this cold path"
            )]
            let counted = bytes
                .get(self.scanned..stop)
                .unwrap_or_default()
                .iter()
                .filter(|byte| **byte == b'\n')
                .count();
            self.counted = self.counted.saturating_add(u64::try_from(counted).unwrap_or(u64::MAX));
            self.scanned = stop;
        }
        self.counted
    }

    /// The label of the file holding the most recently ended value, when the
    /// source carries per-file ranges.
    fn current_file_label(&self) -> Option<&'a str> {
        self.files.map(|files| files[self.file].label())
    }
}

/// Physical, preservation, byte-count, and exit-status receipt for one
/// completed ordered item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodedItemReport {
    physical_encoder: PhysicalRouteId,
    preservation: Option<PreservationReport>,
    codec_bytes: u64,
    framing_bytes: u64,
    /// The item's VALUE truthiness under the reference's one truth law (only `false` and
    /// `null` are falsy). This is what the CLI's `-e`/`--exit-status` reads:
    /// it is carried per item so every publication drive — sequence,
    /// record, single-document — reports the same fact, and the
    /// facade decides the process exit code from the LAST one. `None` means
    /// the publication path had no single value to judge (the edit lane).
    value_truthy: Option<bool>,
    /// Whether the item's VALUE is an empty array under the
    /// payload-transparent view. The CLI's `--diff` exit law reads it: the
    /// fixed diff program emits ONE array of change records, and an empty
    /// array is the "documents are semantically equal" verdict. `None` when
    /// the publication path had no single value to judge (the edit lane).
    value_empty_array: Option<bool>,
    /// Whether the item's ROOT value is a text-family scalar — a string, a
    /// byte string, or a temporal spelling (`jqf_engine::is_raw_text`). The
    /// CLI's colour rendering reads it to reproduce the reference's `-r` raw arm law
    /// verbatim: a ROOT text item prints its own bytes with no colour, where
    /// a raw-printed non-text root is still a JSON rendering and colours
    /// normally. `false` when the publication path had no single value to
    /// judge (the edit lane publishes a document).
    raw_text_root: bool,
}

impl EncodedItemReport {
    /// Core-sealed physical encoder used for this item.
    #[must_use]
    pub const fn physical_encoder(self) -> PhysicalRouteId {
        self.physical_encoder
    }
    /// Final codec preservation evidence when requested.
    #[must_use]
    pub const fn preservation(self) -> Option<PreservationReport> {
        self.preservation
    }
    /// Bytes published from encoder offers for this item.
    #[must_use]
    pub const fn codec_bytes(self) -> u64 {
        self.codec_bytes
    }
    /// Facade-selected framing bytes published after this item.
    #[must_use]
    pub const fn framing_bytes(self) -> u64 {
        self.framing_bytes
    }
    /// The item's value truthiness under the reference's truth law, when the publication
    /// path judged one.
    #[must_use]
    pub const fn value_truthy(self) -> Option<bool> {
        self.value_truthy
    }
    /// Whether the item's value was an empty array, when the publication
    /// path judged one.
    #[must_use]
    pub const fn value_empty_array(self) -> Option<bool> {
        self.value_empty_array
    }
    /// Whether the item's root value is a text-family scalar (the `-r` raw
    /// arm's verbatim subject).
    #[must_use]
    pub const fn raw_text_root(self) -> bool {
        self.raw_text_root
    }
}

/// Facade-selected bytes appended to every completed item.
///
/// JSON CLI newline framing is one use; the SDK does not assign those bytes
/// universal document or codec semantics.
#[derive(Clone, Copy, Debug, Default)]
pub struct FacadeFraming<'framing> {
    item_suffix: &'framing [u8],
}

impl<'framing> FacadeFraming<'framing> {
    /// Uses this exact suffix after every encoded item.
    #[must_use]
    pub const fn item_suffix(item_suffix: &'framing [u8]) -> Self {
        Self { item_suffix }
    }
}

/// Construction and cooperative-resume policy for one pipeline request.
#[derive(Clone, Copy, Debug)]
pub struct PipelinePolicy<'options> {
    /// Decoder construction policy.
    pub decode: DecodeRequest<'options>,
    /// Encoder diagnostic policy.
    pub encode_diagnostics: DiagnosticPolicy,
    /// Requested per-item preservation evidence.
    pub preservation: PreservationRequest,
    /// Optional target-codec options; absence selects codec defaults.
    pub encode_options: Option<&'options (dyn core::any::Any + Send + Sync)>,
    /// Credits installed on every resumed cooperative entry.
    pub cooperative_credits: u32,
    /// The `--split-exp` program: a THIRD destination model —
    /// one destination per published ITEM, its name the split program's single
    /// string output evaluated over the item (design (a)). `None` for every
    /// ordinary request; when `Some`, [`ItemSink::begin_item_named`] is
    /// called instead of [`ItemSink::begin_item`] and the sink owns the
    /// per-item destination.
    pub split: Option<&'options CompiledProgram>,
    /// Opt-in per-run frame-transition ceiling (`--max-iterations`); `None`
    /// is unlimited, the default. Enforced by the engine beside every
    /// cooperative work admission; a crossing raises the machine resource
    /// refusal (exit class 5).
    pub max_iterations: Option<u64>,
}

impl<'options> PipelinePolicy<'options> {
    const fn encoding(self) -> OrderedEncodingPolicy<'options> {
        OrderedEncodingPolicy {
            diagnostics: self.encode_diagnostics,
            preservation: self.preservation,
            options: self.encode_options,
            cooperative_credits: self.cooperative_credits,
            split: self.split,
            flush_each_item: false,
        }
    }
}

/// Target construction and cooperative policy for an ordered engine result stream.
#[derive(Clone, Copy, Debug)]
pub struct OrderedEncodingPolicy<'options> {
    /// Encoder diagnostic policy.
    pub diagnostics: DiagnosticPolicy,
    /// Requested per-item preservation evidence.
    pub preservation: PreservationRequest,
    /// Optional target-codec options.
    pub options: Option<&'options (dyn core::any::Any + Send + Sync)>,
    /// Credits installed on every resumed cooperative entry.
    pub cooperative_credits: u32,
    /// The split-destination program (see [`PipelinePolicy::split`]): when
    /// `Some`, the item is handed to [`ItemSink::begin_item_named`] with the
    /// program's per-item string output instead of [`ItemSink::begin_item`].
    pub split: Option<&'options CompiledProgram>,
    /// Flush the sink after every published item. The live-tail lanes whose
    /// pulls are driven INSIDE one program run (`-n 'inputs | …'`, and nested
    /// `input`/`inputs` under the input-family drive) cannot flush before a
    /// blocking read the way a driver-pull loop does — the sink is loaned for
    /// the whole run — so they publish at this cadence instead: everything
    /// published so far is observable whenever the program blocks on input.
    /// Default off; every other lane keeps its own flush cadence.
    pub flush_each_item: bool,
}

/// One cooperative observation from an ordered engine result producer.
#[derive(Debug)]
pub enum OrderedResultPoll<'source> {
    /// The producer needs another cooperative entry before yielding a result.
    Pending,
    /// The next ordered semantic result.
    Item(EngineResult<'source>),
    /// Stable end of the ordered result stream.
    Complete,
}

/// Format-neutral producer consumed by SDK output orchestration.
///
/// Engine execution owns result generation; the SDK owns only ordered item
/// publication over this boundary. NO production drive publishes through
/// this trait today — the in-tree implementor and caller are both the
/// sdk-smoke receipt tool (`tools/smoke/jqf-sdk-smoke`), which pins ordered
/// publication's cooperative-credit, cancellation, and partial-sink laws.
/// Production publication loops drive `Publication` + `encode_one`
/// directly.
pub trait OrderedResultProducer<'source> {
    /// Polls the next ordered result without publishing host-visible output.
    fn poll_next(&mut self, context: &mut CodecRunContext<'_, '_>) -> Result<OrderedResultPoll<'source>, CodecError>;
}

/// Observable publication state retained on success and every failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationStatus {
    /// No ordered item boundary or byte prefix was accepted by the sink.
    NotStarted,
    /// One item is open or one or more completed items were already published.
    InProgress {
        /// Number of fully completed ordered items.
        completed_items: u64,
        /// Exact bytes reported written and committed so far.
        published_bytes: u64,
    },
    /// All ordered items and their facade framing completed.
    Complete {
        /// Number of ordered items, including zero.
        items: u64,
        /// Exact committed output bytes.
        published_bytes: u64,
    },
}

/// Successful physical and publication receipt for one pipeline request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PipelineReport {
    publication: PublicationStatus,
    disposition: PipelineDisposition,
    access: AccessReport,
}

/// Successful publication summary for a generic ordered result producer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrderedEncodingReport {
    publication: PublicationStatus,
}

impl OrderedEncodingReport {
    /// Completed ordered item count and exact committed bytes.
    #[must_use]
    pub const fn publication(self) -> PublicationStatus {
        self.publication
    }
}

impl PipelineReport {
    /// Final publication state.
    #[must_use]
    pub const fn publication(self) -> PublicationStatus {
        self.publication
    }
    /// Engine-visible disposition retained without converting path outcomes to failures.
    #[must_use]
    pub const fn disposition(self) -> PipelineDisposition {
        self.disposition
    }
    /// Complete fixed access report retained across the codec and engine boundaries.
    #[must_use]
    pub const fn access_report(self) -> AccessReport {
        self.access
    }
    /// Core-sealed physical access route that executed.
    ///
    /// This compatibility projection is always present on a successful pipeline report.
    ///
    /// # Panics
    ///
    /// Panics only if an invalid unsealed report crosses the SDK's checked construction
    /// boundary, which is an internal contract violation rather than caller input.
    #[must_use]
    pub const fn access_route(self) -> PhysicalRouteReceipt {
        match self.access.route() {
            Some(route) => route,
            None => panic!("successful pipeline access report has no sealed route"),
        }
    }
}

/// Result cardinality or typed static-path outcome before facade encoding.
///
/// A type mismatch is not a disposition: it aborts the request as a
/// [`PipelineFailure::TypeMismatch`] before any item is encoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipelineDisposition {
    /// The residual ran over a resolved located input value.
    Emitted,
    /// The residual ran over the owned `null` a pushed-down missing path or
    /// mismatch-on-null produced. Under the residual-flows law this no longer
    /// implies exactly one published null: an identity residual forwards one null
    /// (`{} | .a`), but an `.[]` residual iterates that null and either publishes
    /// zero items (`{} | .a[]?` suppresses) or aborts (`{} | .a[]` errors, so the
    /// run is a failure rather than a `Missing` disposition). The tag records the
    /// input's provenance; the publication count and success come from the
    /// residual stream.
    Missing,
    /// A per-component optional (`?`) suppressed a non-null type mismatch at its
    /// exact step; zero items are published (the reference prints nothing, exit 0).
    Suppressed,
}

/// Exact cause retained by a failed pipeline.
#[derive(Debug)]
pub enum PipelineFailure<SinkError> {
    /// Registration inventory selection failed.
    Registry(RegistryFailure),
    /// No provider route satisfied the exact engine requirement.
    AccessBind(AccessBindError),
    /// Codec, resource, or cooperative-control execution failed.
    Codec(CodecError),
    /// The host sink rejected an item boundary or write without publishing bytes.
    Sink(SinkError),
    /// The host sink returned an impossible write length.
    SinkContract,
    /// The configured cooperative credit quantum is invalid.
    InvalidCooperativeCredits,
    /// A `Key`/`Index` step addressed the wrong semantic category.
    TypeMismatch {
        /// Zero-based failing path step.
        step_index: usize,
        /// Payload-transparent type observed at that step.
        actual_type: ValueKind,
    },
    /// An `.[]` step iterated a non-iterable value. A DISTINCT class from
    /// [`PipelineFailure::TypeMismatch`]: the reference renders "Cannot iterate over X"
    /// where indexing renders "Cannot index X with Y", and the CLI names the
    /// iteration accordingly.
    IterateMismatch {
        /// Zero-based failing iterate step.
        step_index: usize,
        /// Payload-transparent type observed at that step.
        actual_type: ValueKind,
    },
    /// An object construction dynamic key produced a non-string value. A THIRD
    /// class distinct from the index/iterate mismatches (the reference renders "Cannot use X
    /// as object key"); it has no path step (a key is not a path step).
    ObjectKeyMismatch {
        /// Payload-transparent type observed for the offending key.
        actual_type: ValueKind,
    },
    /// `length` over a value with no length (a boolean): the reference renders
    /// `"<type> (<value>) has no length"`. A builtin-domain error with no path step.
    NoLength {
        /// Payload-transparent type observed for the value.
        actual_type: ValueKind,
    },
    /// `keys` over a value with no keys (null/boolean/number/string): the reference renders
    /// `"<type> (<value>) has no keys"`. A builtin-domain error with no path step.
    NoKeys {
        /// Payload-transparent type observed for the value.
        actual_type: ValueKind,
    },
    /// A binary operator failed on its operands (an operand-type mismatch, a zero
    /// divisor, or a binary64 numeric-range overflow). The exact class is the
    /// carried [`ArithFailure`]; it has no path step.
    ArithmeticError(ArithFailure),
    /// A `.[a:b]` slice over an array or string carried a bound that is neither a
    /// number nor `null`: the reference renders the constant "Array/string slice indices must
    /// be integers". It has no path step and no operand.
    SliceIndices,
    /// A mismatch cell raised under the strict dial (`052` W3): a site where jq
    /// answers a VALUE became a raise. The cell is the frozen table's row
    /// index; the registry payload is the cell's name. Exit class 5, like the
    /// typed semantic arms.
    MismatchRaised {
        /// The frozen table's row index (`052` W0).
        cell: u16,
    },
    /// A `~generator`/`~rng` constructor filter emitted two or more values on
    /// one pull ; the constructor and phase that
    /// over-emitted. Exit class 5, like the typed semantic arms.
    EngineCardinality {
        /// The constructor that owns the excess (`"generator"` or `"rng"`).
        constructor: &'static str,
        /// Which filter over-emitted (`"init"`, `"update"`, `"extract"`,
        /// or `"seed"`).
        phase: &'static str,
    },
    /// A program-raised error VALUE (`error/0-1`) that no `try` caught: the owned
    /// value/rendering materialized at this boundary. The CLI prints a string value
    /// as-is and a non-string as `(not a string): <json>`.
    Raised(RaisedError),
    /// `halt`/`halt_error` terminated the run: the process exit status and the
    /// optional message value (`halt_error`'s current input), which the host
    /// prints compact to stderr before exiting with `status`.
    Halt {
        /// The exit status the reference's `jq_halt` asked for.
        status: u32,
        /// The message value, when the terminating call was `halt_error`.
        message: Option<Value>,
    },
    /// The edit lane's exactly-one-output law: a document whose program
    /// published this many results — zero, or more than one — cannot be edited
    /// as a whole document.
    EditOutputCount {
        /// Number of results the program published for one document.
        observed: u64,
    },
    /// The `--split-exp` destination refused one item's name:
    /// the split program produced no output, or its output was not a single
    /// string. A usage-class failure naming the item index and the produced
    /// kind — a well-formed request whose expression's result
    /// contract failed. The detail is prose the CLI renders directly.
    SplitName {
        /// Zero-based item whose name was refused.
        index: u64,
        /// What the split program produced instead of one string.
        detail: String,
    },
    /// A later item named a destination the first writer already occupies.
    /// The first item's bytes stand; the run fails.
    SplitCollision {
        /// The colliding destination name.
        name: String,
        /// Zero-based index of the first writer.
        first_index: u64,
        /// Zero-based index of the refused item.
        second_index: u64,
    },
}

impl<SinkError> PipelineFailure<SinkError> {
    /// Maps the host sink error; every other variant is re-constructed
    /// unchanged. The one-execute surface uses this to erase the host error
    /// to its Display text so [`crate::Failure`] stays non-generic and
    /// `Send + Sync`.
    pub(crate) fn map_sink<Mapped>(self, map: impl FnOnce(SinkError) -> Mapped) -> PipelineFailure<Mapped> {
        match self {
            PipelineFailure::Sink(error) => PipelineFailure::Sink(map(error)),
            PipelineFailure::Registry(error) => PipelineFailure::Registry(error),
            PipelineFailure::AccessBind(error) => PipelineFailure::AccessBind(error),
            PipelineFailure::Codec(error) => PipelineFailure::Codec(error),
            PipelineFailure::SinkContract => PipelineFailure::SinkContract,
            PipelineFailure::InvalidCooperativeCredits => PipelineFailure::InvalidCooperativeCredits,
            PipelineFailure::TypeMismatch {
                step_index,
                actual_type,
            } => PipelineFailure::TypeMismatch {
                step_index,
                actual_type,
            },
            PipelineFailure::IterateMismatch {
                step_index,
                actual_type,
            } => PipelineFailure::IterateMismatch {
                step_index,
                actual_type,
            },
            PipelineFailure::ObjectKeyMismatch { actual_type } => PipelineFailure::ObjectKeyMismatch { actual_type },
            PipelineFailure::NoLength { actual_type } => PipelineFailure::NoLength { actual_type },
            PipelineFailure::NoKeys { actual_type } => PipelineFailure::NoKeys { actual_type },
            PipelineFailure::ArithmeticError(error) => PipelineFailure::ArithmeticError(error),
            PipelineFailure::SliceIndices => PipelineFailure::SliceIndices,
            PipelineFailure::MismatchRaised { cell } => PipelineFailure::MismatchRaised { cell },
            PipelineFailure::EngineCardinality { constructor, phase } => {
                PipelineFailure::EngineCardinality { constructor, phase }
            }
            PipelineFailure::Raised(error) => PipelineFailure::Raised(error),
            PipelineFailure::Halt { status, message } => PipelineFailure::Halt { status, message },
            PipelineFailure::EditOutputCount { observed } => PipelineFailure::EditOutputCount { observed },
            PipelineFailure::SplitName { index, detail } => PipelineFailure::SplitName { index, detail },
            PipelineFailure::SplitCollision {
                name,
                first_index,
                second_index,
            } => PipelineFailure::SplitCollision {
                name,
                first_index,
                second_index,
            },
        }
    }
}

/// A program-raised error that escaped every `try` barrier, carrying the owned
/// raised value for the boundary to render.
#[derive(Debug)]
pub struct RaisedError {
    value: Value,
}

impl RaisedError {
    /// The owned raised value (any JSON value, not only a string).
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.value
    }
}

/// A per-value runtime mismatch, internal to the pipeline: a single-value
/// `execute_value_document` turns it into a [`PipelineFailure`], while `execute_sequence`
/// reports it and continues (the reference's continue-on-error). It keeps the two jq
/// classes (index versus iterate) distinct end to end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeMismatch {
    /// A `Key`/`Index` step addressed the wrong type.
    Index { step_index: usize, actual_type: ValueKind },
    /// An `.[]` step iterated a non-iterable.
    Iterate { step_index: usize, actual_type: ValueKind },
    /// An object construction dynamic key produced a non-string (no path step).
    ObjectKey { actual_type: ValueKind },
    /// `length` over a value with no length (no path step).
    NoLength { actual_type: ValueKind },
    /// `keys` over a value with no keys (no path step).
    NoKeys { actual_type: ValueKind },
    /// A binary operator failed on its operands (no path step).
    Arithmetic(ArithFailure),
    /// A slice over an array/string carried a non-numeric, non-null bound (no
    /// path step, no operand).
    SliceIndices,
    /// A mismatch cell raised under the strict dial (`052` W3); the row index
    /// is the cell's frozen-table identity.
    MismatchRaised { cell: u16 },
    /// A `~generator`/`~rng` constructor filter emitted two or more values on
    /// one pull ; the constructor and phase that
    /// over-emitted.
    EngineCardinality {
        /// The constructor that owns the excess (`"generator"` or `"rng"`).
        constructor: &'static str,
        /// Which filter emitted the excess.
        phase: &'static str,
    },
}

/// One per-value error in an adjacent sequence: a typed runtime mismatch (Copy)
/// or an uncaught program-raised value (owning, non-Copy).
pub(crate) enum SequenceError {
    /// A typed runtime mismatch (index/iterate/object-key/no-length/no-keys/arith).
    Mismatch(RuntimeMismatch),
    /// An uncaught program-raised error VALUE (`error/0-1`).
    Raised(Value),
    /// The one per-value CODEC failure (`RawNulByte`): the last value's class.
    Codec(CodecError),
    /// A `--split-exp` name refusal on one item; the sequence continues.
    SplitName { index: u64, detail: String },
}

/// One value's failure as the sequence loop observes it, before it is reported:
/// a typed mismatch carrying its rendered message, or an uncaught raised VALUE.
///
/// It differs from [`SequenceError`] — which records only the LAST value's exit
/// class — by owning the message string, which the report consumes.
pub(crate) enum ValueOutcome {
    /// A typed runtime mismatch and its reference-exact message.
    Mismatch(RuntimeError),
    /// An uncaught program-raised error VALUE (`error/0-1`).
    Raised(Value),
    /// The one per-value CODEC failure (`RawNulByte` under `--raw-output0`).
    Codec(CodecError),
    /// A `--split-exp` name refusal on one item.
    SplitName { index: u64, detail: String },
}

/// A typed runtime mismatch together with the reference-exact message the ENGINE
/// rendered for it at the raise site.
///
/// The message travels; it is never re-derived downstream. Only the raise site
/// still holds the operands the reference interpolates (`number (1) and string ("a")`), so
/// a facade rendering from the typed class alone could print no more than an
/// operand-less paraphrase.
pub(crate) struct RuntimeError {
    mismatch: RuntimeMismatch,
    message: String,
}

impl RuntimeError {
    /// The sink-facing report for one continued-past value.
    fn into_sequence_error(self, value_index: u64, input_line: u64, filename: Option<&str>) -> SequenceValueError {
        let mut error = self.mismatch.into_sequence_error(value_index, filename);
        error.input_line = input_line;
        error.message = self.message;
        error
    }
}

/// The channel one engine run error reports on, with a typed class's reference-exact
/// message already rendered out of the operands the raise site still held.
pub(crate) enum RunError {
    /// A machine failure (control, ledger, internal contract) — not catchable.
    Machine(CodecError),
    /// An uncaught program-raised error VALUE (`error/0-1`).
    Raised(Value),
    /// `halt`/`halt_error` terminated the run: the process exit status and the
    /// optional message value (`halt_error`'s current input, printed compact to
    /// stderr; `halt` has none).
    Halt { status: u32, message: Option<Value> },
    /// A typed reference-semantic runtime error and its message.
    Runtime(RuntimeError),
}

impl RuntimeMismatch {
    /// The typed class of one engine run error, or `None` for the machine and
    /// raised channels (which [`split_run_error`] routes separately).
    const fn of(error: &EngineRunError) -> Option<Self> {
        Some(match error {
            EngineRunError::Codec(_) | EngineRunError::Raised(_) | EngineRunError::Halt { .. } => {
                return None;
            }
            EngineRunError::TypeMismatch {
                step_index,
                actual_type,
                ..
            } => Self::Index {
                step_index: *step_index,
                actual_type: *actual_type,
            },
            EngineRunError::IterateMismatch {
                step_index,
                actual_type,
                ..
            } => Self::Iterate {
                step_index: *step_index,
                actual_type: *actual_type,
            },
            EngineRunError::ObjectKeyMismatch { actual_type, .. } => Self::ObjectKey {
                actual_type: *actual_type,
            },
            EngineRunError::NoLength { actual_type, .. } => Self::NoLength {
                actual_type: *actual_type,
            },
            EngineRunError::NoKeys { actual_type, .. } => Self::NoKeys {
                actual_type: *actual_type,
            },
            EngineRunError::Arithmetic { failure, .. } => Self::Arithmetic(*failure),
            EngineRunError::SliceIndices => Self::SliceIndices,
            EngineRunError::MismatchRaised { cell } => Self::MismatchRaised { cell: *cell },
            EngineRunError::EngineCardinality { constructor, phase } => Self::EngineCardinality { constructor, phase },
        })
    }

    /// The [`PipelineFailure`] a single-value pipeline aborts with.
    fn into_failure<E>(self) -> PipelineFailure<E> {
        match self {
            Self::Index {
                step_index,
                actual_type,
            } => PipelineFailure::TypeMismatch {
                step_index,
                actual_type,
            },
            Self::Iterate {
                step_index,
                actual_type,
            } => PipelineFailure::IterateMismatch {
                step_index,
                actual_type,
            },
            Self::ObjectKey { actual_type } => PipelineFailure::ObjectKeyMismatch { actual_type },
            Self::NoLength { actual_type } => PipelineFailure::NoLength { actual_type },
            Self::NoKeys { actual_type } => PipelineFailure::NoKeys { actual_type },
            Self::Arithmetic(failure) => PipelineFailure::ArithmeticError(failure),
            Self::SliceIndices => PipelineFailure::SliceIndices,
            Self::MismatchRaised { cell } => PipelineFailure::MismatchRaised { cell },
            Self::EngineCardinality { constructor, phase } => PipelineFailure::EngineCardinality { constructor, phase },
        }
    }

    /// The sink-facing report for one continued-past value.
    fn into_sequence_error(self, value_index: u64, filename: Option<&str>) -> SequenceValueError {
        let (class, step_index, actual_type, arith) = match self {
            Self::Index {
                step_index,
                actual_type,
            } => (RuntimeMismatchClass::Index, step_index, actual_type, None),
            Self::Iterate {
                step_index,
                actual_type,
            } => (RuntimeMismatchClass::Iterate, step_index, actual_type, None),
            // A key/builtin-domain error is not a path step, so `step_index` is 0.
            Self::ObjectKey { actual_type } => (RuntimeMismatchClass::ObjectKey, 0, actual_type, None),
            Self::NoLength { actual_type } => (RuntimeMismatchClass::NoLength, 0, actual_type, None),
            Self::NoKeys { actual_type } => (RuntimeMismatchClass::NoKeys, 0, actual_type, None),
            // Arithmetic carries its exact class; the path-step and type fields
            // are not meaningful for it (the facade reads `arith`).
            Self::Arithmetic(failure) => (RuntimeMismatchClass::Arithmetic, 0, ValueKind::Null, Some(failure)),
            // The slice-bound class carries no operand at all — the reference's message is
            // one constant sentence.
            Self::SliceIndices => (RuntimeMismatchClass::SliceIndices, 0, ValueKind::Null, None),
            // The strict-dial raise names its cell; step/type fields are not
            // meaningful (the facade reads the message).
            Self::MismatchRaised { .. } => (RuntimeMismatchClass::MismatchRaised, 0, ValueKind::Null, None),
            // The engine-cardinality raise names its phase; step/type fields
            // are not meaningful (the facade reads the message).
            Self::EngineCardinality { .. } => (RuntimeMismatchClass::EngineCardinality, 0, ValueKind::Null, None),
        };
        SequenceValueError {
            value_index,
            input_line: 0,
            filename: filename.map(std::string::ToString::to_string),
            class,
            step_index,
            actual_type,
            arith,
            raised: None,
            frame_note: "",
            message: String::new(),
        }
    }
}

/// Pipeline failure together with exact partial-publication status.
#[derive(Debug)]
pub struct PipelineError<SinkError> {
    failure: PipelineFailure<SinkError>,
    publication: PublicationStatus,
}

impl<SinkError> PipelineError<SinkError> {
    /// Exact failure cause.
    #[must_use]
    pub const fn failure(&self) -> &PipelineFailure<SinkError> {
        &self.failure
    }
    /// Publication state at the failure boundary.
    #[must_use]
    pub const fn publication(&self) -> PublicationStatus {
        self.publication
    }
}

/// Erases the host sink error to another type; the one-execute surface maps
/// it to its Display text so [`crate::Failure`] stays non-generic.
pub(crate) fn erase_sink<SinkError, Mapped>(
    error: PipelineError<SinkError>,
    map: impl FnOnce(SinkError) -> Mapped,
) -> PipelineError<Mapped> {
    PipelineError {
        failure: error.failure.map_sink(map),
        publication: error.publication,
    }
}

impl<SinkError: std::fmt::Display> std::fmt::Display for PipelineFailure<SinkError> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineFailure::Registry(failure) => {
                write!(formatter, "codec selection failed: {failure}")
            }
            PipelineFailure::AccessBind(failure) => {
                write!(formatter, "codec route bind failed: {failure}")
            }
            PipelineFailure::Codec(failure) => write!(formatter, "{failure}"),
            PipelineFailure::Sink(failure) => write!(formatter, "sink failed: {failure}"),
            PipelineFailure::SinkContract => write!(formatter, "sink violated the write contract"),
            PipelineFailure::InvalidCooperativeCredits => {
                write!(formatter, "invalid cooperative work quantum")
            }
            PipelineFailure::TypeMismatch {
                step_index,
                actual_type,
            } => write!(formatter, "cannot index {actual_type:?} at path step {step_index}"),
            PipelineFailure::IterateMismatch {
                step_index,
                actual_type,
            } => write!(formatter, "cannot iterate {actual_type:?} at path step {step_index}"),
            PipelineFailure::ObjectKeyMismatch { actual_type } => {
                write!(formatter, "cannot use {actual_type:?} as object key")
            }
            PipelineFailure::NoLength { actual_type } => {
                write!(formatter, "{actual_type:?} has no length")
            }
            PipelineFailure::NoKeys { actual_type } => {
                write!(formatter, "{actual_type:?} has no keys")
            }
            PipelineFailure::ArithmeticError(failure) => write!(formatter, "{failure:?}"),
            PipelineFailure::SliceIndices => {
                write!(formatter, "array/string slice indices must be integers")
            }
            PipelineFailure::MismatchRaised { cell } => {
                write!(formatter, "mismatch under strict policy: cell {cell}")
            }
            PipelineFailure::EngineCardinality { constructor, phase } => {
                write!(formatter, "{constructor} {phase} filter emitted multiple values")
            }
            PipelineFailure::Raised(_) => write!(formatter, "uncaught program-raised value"),
            PipelineFailure::Halt { status, .. } => write!(formatter, "halt with status {status}"),
            PipelineFailure::EditOutputCount { observed } => write!(
                formatter,
                "edit mode requires exactly one output per document; the program produced {observed}"
            ),
            PipelineFailure::SplitName { index, detail } => {
                write!(formatter, "the split expression produced {detail} at item {index}")
            }
            PipelineFailure::SplitCollision {
                name,
                first_index,
                second_index,
            } => write!(
                formatter,
                "split destination {name:?} already written by item {first_index}; item {second_index} refused"
            ),
        }
    }
}

impl<SinkError: std::fmt::Display> std::fmt::Display for PipelineError<SinkError> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.failure.fmt(formatter)
    }
}

impl<SinkError> std::error::Error for PipelineFailure<SinkError> where SinkError: std::error::Error + 'static {}

impl<SinkError> std::error::Error for PipelineError<SinkError>
where
    SinkError: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.failure)
    }
}

pub(crate) struct Publication {
    completed_items: u64,
    published_bytes: u64,
    item_open: bool,
    /// The output format cannot express MORE than one document (no
    /// `RouteCapability::AdjacentValues`): a second published item is
    /// refused. Default false (multi-document is allowed) — only the
    /// multi-item drives set it from the output format's own registration,
    /// so the single-document edit lane (whose many encoder sessions are all
    /// patches of ONE document) is untouched by construction.
    single_document_output: bool,
    /// First-writer-wins destinations for `--split-exp`. A second item
    /// naming an already-written destination is refused; the first item's
    /// bytes stand.
    split_destinations: BTreeMap<String, u64>,
}

impl Publication {
    const fn new() -> Self {
        Self {
            completed_items: 0,
            published_bytes: 0,
            item_open: false,
            single_document_output: false,
            split_destinations: BTreeMap::new(),
        }
    }

    const fn status(&self) -> PublicationStatus {
        if self.item_open || self.completed_items != 0 || self.published_bytes != 0 {
            PublicationStatus::InProgress {
                completed_items: self.completed_items,
                published_bytes: self.published_bytes,
            }
        } else {
            PublicationStatus::NotStarted
        }
    }

    fn fail<E>(&self, failure: PipelineFailure<E>) -> PipelineError<E> {
        PipelineError {
            failure,
            publication: self.status(),
        }
    }
}

/// Successful publication receipt for a sequence of adjacent decoded values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SequenceReport {
    publication: PublicationStatus,
    items: u64,
    /// How many per-value CODEC failures (the one admitted kind,
    /// `RawNulByte`) the sequence continued past. Under `--strictness strict`
    /// any nonzero count forces the failure class at run end.
    codec_value_errors: u64,
}

impl SequenceReport {
    /// Final publication state across every item and its facade framing.
    #[must_use]
    pub const fn publication(self) -> PublicationStatus {
        self.publication
    }

    /// How many per-value codec failures the sequence continued past.
    #[must_use]
    pub const fn codec_value_errors(self) -> u64 {
        self.codec_value_errors
    }
    /// Number of ordered items published across every decoded value, including
    /// zero. A value whose program suppresses a mismatch (`?`) publishes none, so
    /// this counts published items, not decoded values.
    #[must_use]
    pub const fn items(self) -> u64 {
        self.items
    }
}

/// A streaming adjacent-value drive's failure, at the boundary that raised it.
///
/// The streaming drive  reads its input through a caller-supplied
/// closure, so the caller's read failure is a distinct arm from the pipeline's
/// own failure — the host renders each with its own frame.
#[derive(Debug)]
pub enum StreamingSequenceError<SinkError, ReadError> {
    /// The caller's byte source failed to produce input.
    Read(ReadError),
    /// The pipeline's own failure, unchanged.
    Pipeline(PipelineError<SinkError>),
}

#[cfg(test)]
pub(crate) mod direct_json_identity_tests {
    //! The value-direct eager decode's identity pin: the SDK carries the
    //! json-codec identities as text (no dependency), so drift is caught
    //! here against the codec's own constants.

    #[test]
    fn direct_json_identities_match_the_json_codec() {
        assert_eq!(super::DIRECT_JSON_FORMAT, jqf_codec_json::FORMAT_ID);
        assert_eq!(super::DIRECT_JSON_DIALECT, jqf_codec_json::RFC8259_DIALECT_ID);
    }
}

#[cfg(test)]
pub(crate) mod catalog_detection_tests {
    //! The exact-filename and filename-glob registration fact:
    //! `.env`-shaped names with no extension resolve, precedence is exact
    //! name → glob → extension, and the ambiguity law fires.

    use super::{CodecCatalog, RegistryFailure};
    use jqf_codec_core::{CodecDescriptor, CodecOperations, CodecRegistration, ItemByteOwner};
    use jqf_data::{DialectId, DialectIdRef, FormatId, FormatIdRef};

    // One shared dialect identity: detection tests never select by dialect,
    // and `CodecRegistration::try_new` validates dialect uniqueness only
    // within one descriptor, so a static shared array keeps the descriptor
    // `'static` without a per-call allocation.
    static TEST_DIALECTS: [DialectIdRef<'static>; 1] = [DialectIdRef::from_static("test.dialect@1")];

    fn registration(
        format: &'static str,
        filenames: &'static [&'static str],
        extensions: &'static [&'static str],
    ) -> CodecRegistration<'static> {
        CodecRegistration::try_new(
            CodecDescriptor::new(
                FormatIdRef::from_static(format),
                &TEST_DIALECTS,
                CodecOperations::new(false, false, false),
                &[],
                extensions,
                // Detection tests never consult the framing declaration; the
                // single test dialect takes the facade row.
                &[ItemByteOwner::Facade],
                filenames,
                &[],
            ),
            None,
            None,
            None,
            None,
        )
        .expect("valid registration")
    }

    fn catalog<'a>(registrations: &'a [&'a CodecRegistration<'static>]) -> CodecCatalog<'a, 'static> {
        CodecCatalog::new(registrations)
    }

    #[test]
    fn hashed_lookup_agrees_with_a_linear_scan() {
        let json = jqf_codec_json::registration().expect("json registration");
        let registrations = [&json];
        let linear = CodecCatalog::new(&registrations);
        let index = super::CatalogIndex::build(&registrations);
        let hashed = CodecCatalog::new(&registrations).with_index(&index);
        let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format");
        let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect");
        assert_eq!(
            linear.route_capabilities(&format, &dialect).unwrap(),
            hashed.route_capabilities(&format, &dialect).unwrap()
        );
        assert_eq!(
            linear.item_byte_owner(&format, &dialect).unwrap(),
            hashed.item_byte_owner(&format, &dialect).unwrap()
        );
    }

    #[test]
    fn exact_filename_resolves() {
        let env = registration("env-test", &[".env"], &[]);
        let registrations = [&env];
        let catalog = catalog(&registrations);
        let (format, _dialect) = catalog
            .detect_by_filename(".env")
            .expect("the exact .env claim must resolve");
        assert_eq!(format.as_str(), "env-test");
    }

    #[test]
    fn exact_name_beats_glob() {
        let exact = registration("exact-test", &[".env.local"], &[]);
        let glob = registration("glob-test", &[".env.*"], &[]);
        let registrations = [&exact, &glob];
        let catalog = catalog(&registrations);
        let (format, _dialect) = catalog
            .detect_by_filename(".env.local")
            .expect("the exact name must beat the glob");
        assert_eq!(format.as_str(), "exact-test");
    }

    #[test]
    fn glob_beats_extension_claim() {
        // `.env.local` must resolve by the `.env.*` glob before a `.local`
        // extension claim is consulted, and an unclaimed name still falls
        // back to the extension law unchanged.
        let env_glob = registration("env-test", &[".env.*"], &[]);
        let local_ext = registration("local-test", &[], &["local"]);
        let registrations = [&env_glob, &local_ext];
        let catalog = catalog(&registrations);
        let (format, _dialect) = catalog
            .detect_by_filename(".env.local")
            .expect("the .env.* glob must resolve before the .local extension");
        assert_eq!(format.as_str(), "env-test");
        let (format, _dialect) = catalog
            .detect_by_filename("data.local")
            .expect("an unclaimed name still resolves by extension");
        assert_eq!(format.as_str(), "local-test");
    }

    #[test]
    fn ambiguous_exact_filenames_are_rejected() {
        let a = registration("a-test", &[".env"], &[]);
        let b = registration("b-test", &[".env"], &[]);
        let registrations = [&a, &b];
        let catalog = catalog(&registrations);
        let error = catalog
            .detect_by_filename(".env")
            .expect_err("two exact .env claims must be ambiguous");
        assert!(matches!(error, RegistryFailure::AmbiguousFilename));
    }

    #[test]
    fn ambiguous_globs_are_rejected() {
        let a = registration("a-test", &[".env.*"], &[]);
        let b = registration("b-test", &[".env.*"], &[]);
        let registrations = [&a, &b];
        let catalog = catalog(&registrations);
        let error = catalog
            .detect_by_filename(".env.local")
            .expect_err("two matching globs must be ambiguous");
        assert!(matches!(error, RegistryFailure::AmbiguousFilename));
    }

    #[test]
    fn unclaimed_filename_is_unavailable() {
        let env = registration("env-test", &[".env"], &[]);
        let registrations = [&env];
        let catalog = catalog(&registrations);
        let error = catalog
            .detect_by_filename("Makefile")
            .expect_err("an unclaimed extensionless name must be unavailable");
        assert!(matches!(error, RegistryFailure::FilenameUnavailable));
        let error = catalog
            .detect_by_filename("data.xyz")
            .expect_err("an unclaimed extension stays extension-unavailable");
        assert!(matches!(error, RegistryFailure::ExtensionUnavailable));
    }
}

#[cfg(test)]
pub(crate) mod guard_tests {
    use super::{PipelineFailure, Publication, require_forward_progress};

    #[test]
    fn forward_progress_accepts_nonzero_consumed_offset() {
        let publication = Publication::new();
        let consumed = require_forward_progress::<&'static str>(Some(3), &publication)
            .expect("nonzero consumed offset must be accepted");
        assert_eq!(consumed, 3);
    }

    #[test]
    fn forward_progress_rejects_zero_consumed_offset() {
        let publication = Publication::new();
        let error = require_forward_progress::<&'static str>(Some(0), &publication)
            .expect_err("a zero consumed offset must not permit forward progress");
        assert!(matches!(error.failure(), PipelineFailure::Codec(_)));
    }

    #[test]
    fn forward_progress_rejects_missing_consumed_offset() {
        let publication = Publication::new();
        let error = require_forward_progress::<&'static str>(None, &publication)
            .expect_err("a missing consumed offset must not permit forward progress");
        assert!(matches!(error.failure(), PipelineFailure::Codec(_)));
    }
}

/// Hard combined entry ceiling for one record poll.
///
/// Batching is what amortizes the erased record dispatch across records; the
/// the record-route autopsy priced per-record dispatch as 8.6 %
/// of the donor route's deficit. The bound is on ENTRIES, not bytes, because
/// the batch holds ranges, not payloads.
///
/// It is public because it is also the record CONCURRENCY WINDOW: a worker
/// substrate sizes one worker's memory envelope from this bound, never from
/// the record stream's `max_record_bytes` option, which defaults to the whole
/// input ceiling .
pub const RECORD_BATCH_ENTRIES: u32 = 256;

/// Cooperative payload-byte target for one record poll.
///
/// Public for the same reason as [`RECORD_BATCH_ENTRIES`]: it is the byte half
/// of the record concurrency window a worker envelope is sized from.
pub const RECORD_BATCH_TARGET_BYTES: u64 = 256 * 1024;
