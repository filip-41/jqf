//! Source text a document's spans borrow, plus interned decoded strings.
//!
//! [`DocumentSourceBindingStage`] seals source bytes into a [`DocumentSourceBinding`]. [`DocumentTextStorage`] holds
//! interned text behind generation-guarded ids. A stored token resolves only under the key and generation it was minted
//! with.

use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::{ResolvedSource, SourceRef, Span};

use super::{DataError, DocumentId};

static NEXT_BUILDER_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The next monotonic builder generation; `ArithmeticOverflow` once the counter leaves `1..=u64::MAX - 1`.
pub(crate) fn fresh_builder_generation() -> Result<u64, DataError> {
    NEXT_BUILDER_GENERATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |generation| {
            (generation != 0 && generation != u64::MAX).then_some(generation + 1)
        })
        .map_err(|_| DataError::ArithmeticOverflow)
}

/// Where a span's bytes live: in the retained source text (`Source`) or in the decoded-text arena behind the generation
/// guard (`Stored`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum TextRef {
    Source(Span),
    Stored(Span),
}

/// Fingerprint of one exact immutable source segment: source identity, base offset, and byte length.
///
/// There is deliberately no content digest: every production codec seals without hashing, nothing ever verifies a
/// stored digest against bytes, and the safety contract rests on metadata equality plus the caller's documented
/// ownership of the exact immutable segment.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SourceSnapshotSeal {
    source: SourceRef,
    base_offset: u64,
    byte_length: u64,
}

impl SourceSnapshotSeal {
    fn from_resolved(source: ResolvedSource<'_>) -> Result<Self, DataError> {
        let byte_length = u64::try_from(source.bytes().len()).map_err(|_| DataError::ArithmeticOverflow)?;
        source
            .base_offset()
            .checked_add(byte_length)
            .ok_or(DataError::ArithmeticOverflow)?;
        Ok(Self {
            source: source.source(),
            base_offset: source.base_offset(),
            byte_length,
        })
    }

    pub(crate) fn metadata_matches(self, source: ResolvedSource<'_>) -> bool {
        self.source == source.source()
            && self.base_offset == source.base_offset()
            && self.byte_length == source.bytes().len() as u64
    }

    /// The sealed source identity, read only by the benchmark layout receipt.
    #[cfg(feature = "benchmark-internals")]
    pub(crate) const fn source(self) -> SourceRef {
        self.source
    }
    pub(crate) const fn byte_length(self) -> u64 {
        self.byte_length
    }

    /// Bounds-checks a span against this seal's byte length and returns the segment-local byte range; `None` when the
    /// span exceeds the seal.
    pub(crate) fn local_range(self, span: Span) -> Option<core::ops::Range<usize>> {
        let start = u64::from(span.start());
        let end = u64::from(span.end());
        if start > end || end > self.byte_length {
            return None;
        }
        Some(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
    }
}

/// Opaque integrity binding for one exact immutable source segment.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DocumentSourceBinding {
    seal: SourceSnapshotSeal,
}

impl DocumentSourceBinding {
    /// Binds one source segment synchronously.
    ///
    /// This compatibility constructor performs linear unaccounted work. An accounted codec must use
    /// [`DocumentSourceBindingStage`] instead.
    pub fn from_resolved(source: ResolvedSource<'_>) -> Result<Self, DataError> {
        Ok(Self {
            seal: SourceSnapshotSeal::from_resolved(source)?,
        })
    }

    /// Creates a token without rehashing after an owning session has proved that `source` is the same immutable
    /// authority used to construct this binding.
    ///
    /// # Safety
    ///
    /// The caller must retain one immutable source authority across binding construction and every call; matching
    /// metadata alone is insufficient.
    #[doc(hidden)]
    pub unsafe fn text_from_bound_authority(
        self,
        source: ResolvedSource<'_>,
        span: Span,
    ) -> Result<DocumentSourceText, DataError> {
        if !self.seal.metadata_matches(source) {
            return Err(DataError::InvalidDocument);
        }
        self.text_metadata_checked(source, span)
    }

    fn text_metadata_checked(self, source: ResolvedSource<'_>, span: Span) -> Result<DocumentSourceText, DataError> {
        let range = self.seal.local_range(span).ok_or(DataError::InvalidDocument)?;
        core::str::from_utf8(source.bytes().get(range).ok_or(DataError::InvalidDocument)?)
            .map_err(|_| DataError::InvalidDocument)?;
        Ok(DocumentSourceText { seal: self.seal, span })
    }

    pub(crate) const fn seal(self) -> SourceSnapshotSeal {
        self.seal
    }
}

/// Result of polling cooperative source binding.
pub enum DocumentSourceBindingPoll {
    /// The seal cursor retained its continuation.
    Pending,
    /// The complete source segment has been sealed.
    Ready(DocumentSourceBinding),
}

/// Cooperative source-seal construction.
pub struct DocumentSourceBindingStage {
    source: SourceRef,
    base_offset: u64,
    byte_length: u64,
    cursor: usize,
    complete: bool,
    failed: bool,
}

impl DocumentSourceBindingStage {
    /// Starts binding one exact source segment.
    ///
    /// The stage walks the segment cooperatively (work-credited quanta) and seals its metadata; it computes no content
    /// digest — see [`SourceSnapshotSeal`] for why.
    pub fn new(source: ResolvedSource<'_>) -> Result<Self, DataError> {
        source
            .base_offset()
            .checked_add(source.bytes().len() as u64)
            .ok_or(DataError::ArithmeticOverflow)?;
        Ok(Self {
            source: source.source(),
            base_offset: source.base_offset(),
            byte_length: u64::try_from(source.bytes().len()).map_err(|_| DataError::ArithmeticOverflow)?,
            cursor: 0,
            complete: false,
            failed: false,
        })
    }

    /// Returns the source prefix already covered by the seal cursor.
    ///
    /// Test-only: no production reader needs the cursor, so the accessor stays out of the public surface.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn sealed_prefix_len(&self) -> usize {
        self.cursor
    }

    /// Draws every byte quantum this cooperative entry can still pay for, returning the end of the contiguous run to
    /// admit.
    ///
    /// The seal is a linear pass over one segment, so an entry that admitted a single byte quantum and yielded would
    /// spend one credit of the many it holds and re-enter through the provider, the parse loop and a fresh cooperative
    /// entry to spend the next. Draining the entry costs exactly the same credits per byte and still yields the instant
    /// the meter is empty.
    ///
    /// Returning the run's END keeps one admission per poll: a caller that re-entered per quantum would pay the
    /// provider and parse-loop round trip for nothing while leaving every other property of this stage intact.
    ///
    /// A control error can only accompany a `Pending` admission, so no granted quantum is lost when this returns one.
    fn admit_run(&self, source: ResolvedSource<'_>, resources: &mut ResourceContext<'_>) -> Result<usize, DataError> {
        let mut end = self.cursor;
        while end < source.bytes().len() {
            match resources.admit_work_bytes(source.bytes().len() - end)? {
                WorkAdmission::Granted(granted) => {
                    end = end.checked_add(granted).ok_or(DataError::ArithmeticOverflow)?;
                }
                WorkAdmission::Pending => break,
            }
        }
        Ok(end)
    }

    /// Advances one or more admitted byte quanta without replay.
    /// # Safety
    ///
    /// Every call must receive the same immutable source authority used at construction. Metadata equality alone is not
    /// sufficient continuity.
    pub unsafe fn poll(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<DocumentSourceBindingPoll, DataError> {
        if self.complete || self.failed {
            return Err(DataError::ReaderFailed);
        }
        if source.source() != self.source
            || source.base_offset() != self.base_offset
            || source.bytes().len() as u64 != self.byte_length
        {
            self.failed = true;
            return Err(DataError::InvalidDocument);
        }
        if self.cursor < source.bytes().len() {
            let end = match self.admit_run(source, resources) {
                Ok(end) => end,
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            };
            self.cursor = end;
            if self.cursor < source.bytes().len() {
                return Ok(DocumentSourceBindingPoll::Pending);
            }
        }
        if let Err(error) = resources.check_control() {
            self.failed = true;
            return Err(error.into());
        }
        self.complete = true;
        Ok(DocumentSourceBindingPoll::Ready(DocumentSourceBinding {
            seal: SourceSnapshotSeal {
                source: self.source,
                base_offset: self.base_offset,
                byte_length: self.byte_length,
            },
        }))
    }
}

/// Origin-bound token for one completed span in a builder's decoded arena.
#[derive(Debug)]
pub struct DocumentTextId {
    span: Span,
    key: DocumentId,
    generation: u64,
}

impl DocumentTextId {
    /// Creates a stored-text token under the given key and generation; resolution requires the same key and generation.
    pub(crate) fn new_accounted(span: Span, key: DocumentId, generation: u64) -> Self {
        Self { span, key, generation }
    }

    /// Resolves this token to `TextRef::Stored` only when the key and generation match this document's; otherwise
    /// `InvalidDocument`.
    pub(crate) fn resolve_accounted(&self, key: DocumentId, generation: u64) -> Result<TextRef, DataError> {
        if self.key != key || self.generation != generation {
            return Err(DataError::InvalidDocument);
        }
        Ok(TextRef::Stored(self.span))
    }
}

/// Validated UTF-8 span in one exact immutable source snapshot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DocumentSourceText {
    seal: SourceSnapshotSeal,
    span: Span,
}

impl DocumentSourceText {
    pub(crate) const fn seal(self) -> SourceSnapshotSeal {
        self.seal
    }
    pub(crate) const fn span(self) -> Span {
        self.span
    }
}

/// Retained text representation counts for physical-route evidence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DocumentTextStorageStats {
    /// Borrowed backing was installed through a codec-core-owned immutable session after the codec had already proved
    /// the canonical source seal.
    pub trusted_session_source_attachment: bool,
    /// Semantic string values resolved directly from retained source.
    pub source_string_values: usize,
    /// Semantic string values stored in the decoded-text arena.
    pub stored_string_values: usize,
    /// Object keys resolved directly from retained source.
    pub source_keys: usize,
    /// Object keys stored in the decoded-text arena.
    pub stored_keys: usize,
    /// Canonical integer text referenced by a semantic node, wherever it lives. Counts the source-backed and
    /// arena-stored arms together, which is what it has always counted — before the source-backed arm existed, every
    /// integer was arena-stored. [`Self::source_integer_values`] splits out the arm that costs the arena nothing.
    pub stored_integer_refs: usize,
    /// The subset of [`Self::stored_integer_refs`] whose canonical spelling is a span of retained source rather than
    /// arena bytes, because the input already spelled the number canonically.
    pub source_integer_values: usize,
    /// Canonical decimal coefficients referenced by a semantic node, wherever they live. Counts the source-backed and
    /// arena-stored arms together, which is what it has always counted — before the source-backed arm existed, every
    /// coefficient was arena-stored.
    pub stored_decimal_coefficient_refs: usize,
    /// Bytes logically retained by the decoded-text arena.
    pub decoded_arena_len: usize,
    /// Bytes allocated by the decoded-text arena.
    pub decoded_arena_capacity: usize,
}

/// The decoded-text arena: one owned string behind generation-guarded text ids.
pub(crate) struct DecodedTextArena {
    pub(crate) bytes: String,
}

/// Bytes already proven to match the seal, handed to `resolve` as a no-copy override when the codec holds the canonical
/// source itself.
#[derive(Clone, Copy)]
pub(crate) struct ValidatedSourceBacking<'source> {
    bytes: &'source [u8],
}

impl<'source> ValidatedSourceBacking<'source> {
    pub(crate) const fn new(bytes: &'source [u8]) -> Self {
        Self { bytes }
    }
    pub(crate) const fn bytes(self) -> &'source [u8] {
        self.bytes
    }
}

/// One document's text state: the optional seal and the decoded arena.
pub(crate) struct DocumentTextStorage {
    pub(crate) seal: Option<SourceSnapshotSeal>,
    pub(crate) decoded: DecodedTextArena,
}

impl DocumentTextStorage {
    /// Assembles storage from the optional seal and the decoded arena.
    ///
    /// A live source is attached only by the finalizer's `poll_with_source` path after publication.
    pub(crate) fn new(binding: Option<DocumentSourceBinding>, decoded: String) -> Self {
        Self {
            seal: binding.map(DocumentSourceBinding::seal),
            decoded: DecodedTextArena { bytes: decoded },
        }
    }

    /// Resolves a text reference to its `&str`: stored spans read the arena, source spans read the validated override
    /// within the seal's bounds, under the module doc's prior-UTF-8-validation invariant.
    pub(crate) fn resolve<'a>(
        &'a self,
        text: TextRef,
        override_bytes: Option<ValidatedSourceBacking<'a>>,
    ) -> Option<&'a str> {
        match text {
            TextRef::Stored(span) => self
                .decoded
                .bytes
                .as_str()
                .get(span.start() as usize..span.end() as usize),
            TextRef::Source(span) => {
                let seal = self.seal?;
                let bytes = override_bytes.map(ValidatedSourceBacking::bytes)?;
                let bytes = bytes.get(seal.local_range(span)?)?;
                // SAFETY: nothing on this path validates UTF-8. What is checked is
                // containment and metadata only: the span is bounds-checked against
                // the seal (`local_range`), and the backing bytes come from a source
                // whose identity, base offset, and length matched the seal at
                // attachment (`metadata_matches`). No encoding re-validation of the bytes occurs before this cast.
                Some(unsafe { core::str::from_utf8_unchecked(bytes) })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};
    use jqf_source::{SourceId, SourceKind};

    fn limits() -> ResourceLimits {
        ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX)
    }

    #[test]
    fn binding_stage_yields_without_replay_and_poison_is_terminal() {
        let bytes = [b'x'; 600];
        let source_ref = SourceRef::new(SourceId::new(71), SourceKind::Input);
        let source = ResolvedSource::new(source_ref, "fixture", &bytes, 17);
        let control = ContinueControl;
        let account = RequestAccount::try_new(limits()).expect("account");
        let work = WorkMeter::try_new_v1(1).expect("work");
        let mut resources = ResourceContext::new(account, &control, work).expect("context");
        let mut stage = DocumentSourceBindingStage::new(source).expect("stage");

        // SAFETY: `source` is one immutable stack allocation for the test.
        assert!(matches!(
            unsafe { stage.poll(source, &mut resources) },
            Ok(DocumentSourceBindingPoll::Pending)
        ));
        assert_eq!(stage.sealed_prefix_len(), 256);
        assert!(resources.try_begin_next_cooperative_entry(1).expect("replenish"));

        let foreign_ref = SourceRef::new(SourceId::new(72), SourceKind::Input);
        let foreign = ResolvedSource::new(foreign_ref, "fixture", &bytes, 17);
        // SAFETY: this deliberately violates metadata, which must poison before reading.
        assert!(matches!(
            unsafe { stage.poll(foreign, &mut resources) },
            Err(DataError::InvalidDocument)
        ));
        // SAFETY: a poisoned stage never reads the supplied source again.
        assert!(matches!(
            unsafe { stage.poll(source, &mut resources) },
            Err(DataError::ReaderFailed)
        ));
        assert_eq!(stage.sealed_prefix_len(), 256);
    }

    /// One poll must spend the whole cooperative entry, not one quantum of it.
    ///
    /// Two credits cover 512 bytes of a 600-byte segment, so the stage seals exactly 512 and yields on the empty meter
    /// — a stage that yielded after the first quantum would stop at 256 with a credit still in hand, which is what
    /// made a large source re-enter the codec once per 256 bytes.
    #[test]
    fn binding_stage_seals_every_credit_its_entry_holds() {
        let bytes = [b'x'; 600];
        let source_ref = SourceRef::new(SourceId::new(73), SourceKind::Input);
        let source = ResolvedSource::new(source_ref, "fixture", &bytes, 0);
        let control = ContinueControl;
        let account = RequestAccount::try_new(limits()).expect("account");
        let work = WorkMeter::try_new_v1(2).expect("work");
        let mut resources = ResourceContext::new(account, &control, work).expect("context");
        let mut stage = DocumentSourceBindingStage::new(source).expect("stage");

        // SAFETY: `source` is one immutable stack allocation for the test.
        assert!(matches!(
            unsafe { stage.poll(source, &mut resources) },
            Ok(DocumentSourceBindingPoll::Pending)
        ));
        assert_eq!(stage.sealed_prefix_len(), 512);
        assert_eq!(resources.remaining_work(), 0);
    }

    /// A seal RESUMED across cooperative entries names the same segment as the one-shot binding of it.
    ///
    /// The drained entry is the interesting half of that: the first entry ends mid-segment, and the second must
    /// continue from the retained cursor rather than replay or skip any of it. A two-entry seal is the shape every
    /// source larger than one entry's credits takes, so the completed seal is compared against a one-shot binding of
    /// the whole extent, not only the first entry's prefix. The drained-entry sibling pins the same identity law at a
    /// yield boundary: `drained_seal_matches_the_one_shot_seal_at_the_same_credit_price`.
    #[test]
    fn a_seal_resumed_across_two_entries_matches_the_one_shot_seal() {
        // A varying pattern rather than a constant one: a resumed digest that replayed or skipped bytes would still
        // match a uniform fixture.
        let bytes: [u8; 600] = core::array::from_fn(|index| u8::try_from(index % 251).unwrap_or(0));
        let source_ref = SourceRef::new(SourceId::new(75), SourceKind::Input);
        let source = ResolvedSource::new(source_ref, "fixture", &bytes, 11);
        let control = ContinueControl;
        let account = RequestAccount::try_new(limits()).expect("account");
        let work = WorkMeter::try_new_v1(2).expect("work");
        let mut resources = ResourceContext::new(account, &control, work).expect("context");
        let mut stage = DocumentSourceBindingStage::new(source).expect("stage");

        // First entry: two credits buy 512 of the 600 bytes and no more.
        // SAFETY: `source` is one immutable stack allocation for the test.
        assert!(matches!(
            unsafe { stage.poll(source, &mut resources) },
            Ok(DocumentSourceBindingPoll::Pending)
        ));
        assert_eq!(stage.sealed_prefix_len(), 512);
        assert_eq!(resources.remaining_work(), 0);

        // Second entry: the remaining 88 bytes are one quantum, so one of the two fresh credits pays for them and the
        // seal completes.
        assert!(resources.try_begin_next_cooperative_entry(2).expect("replenish"));
        // SAFETY: the same immutable allocation as above.
        let binding = match unsafe { stage.poll(source, &mut resources) }.expect("poll") {
            DocumentSourceBindingPoll::Ready(binding) => binding,
            DocumentSourceBindingPoll::Pending => panic!("the second entry covers the tail"),
        };
        assert_eq!(stage.sealed_prefix_len(), 600);
        assert_eq!(resources.remaining_work(), 1);

        let seal = binding.seal();
        assert_eq!(seal.byte_length(), 600);
        assert_eq!(
            seal,
            DocumentSourceBinding::from_resolved(source).expect("one-shot").seal()
        );
    }

    /// Draining the entry changes only WHEN the stage yields: the seal is the segment's, and the credits it costs are
    /// still one per 256-byte quantum. The resumed-entry sibling pins the same identity law across a cooperative
    /// boundary: `a_seal_resumed_across_two_entries_matches_the_one_shot_seal`.
    #[test]
    fn drained_seal_matches_the_one_shot_seal_at_the_same_credit_price() {
        let bytes = [b'q'; 600];
        let source_ref = SourceRef::new(SourceId::new(74), SourceKind::Input);
        let source = ResolvedSource::new(source_ref, "fixture", &bytes, 5);
        let control = ContinueControl;
        let account = RequestAccount::try_new(limits()).expect("account");
        let work = WorkMeter::try_new_v1(64).expect("work");
        let mut resources = ResourceContext::new(account, &control, work).expect("context");
        let mut stage = DocumentSourceBindingStage::new(source).expect("stage");

        // SAFETY: `source` is one immutable stack allocation for the test.
        let binding = match unsafe { stage.poll(source, &mut resources) }.expect("poll") {
            DocumentSourceBindingPoll::Ready(binding) => binding,
            DocumentSourceBindingPoll::Pending => panic!("64 credits cover 600 bytes"),
        };
        assert_eq!(
            binding.seal(),
            DocumentSourceBinding::from_resolved(source).expect("one-shot").seal()
        );
        // 600 bytes is three 256-byte quanta, so 61 of the 64 credits survive.
        assert_eq!(resources.remaining_work(), 61);
    }
}
