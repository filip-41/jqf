//! Ordered record-stream ABI: framed byte ranges over one retained source.
//!
//! The framing codec owns where a record starts, ends, and which ordinal it occupies. Payload decode uses the ordinary
//! access ladder over a borrowed [`RecordLease`]. Nothing is copied per record.
//!
//! [`RecordOrdinal`] is zero-based and counts every physical unit in one stream (records and issues). [`RecordBatch`]
//! is caller-owned and reused. Polling a terminal stream is an internal contract violation. Abort is idempotent.

use core::any::Any;

use alloc::boxed::Box;
use alloc::vec::Vec;
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::{
    CodecError, CodecFailureKind, CodecRunContext, PhysicalRouteId, PhysicalRouteReceipt, ProviderInput,
    RouteDescription, RouteSlot,
};

use crate::execution::SessionTerminal;

/// Seals one record-stream route receipt.
///
/// A record stream is not an access session, so it never passes through the erased access session's receipt seal; it
/// still carries the same three-field receipt so a benchmark or smoke oracle can prove which physical framing route
/// ran.
pub(crate) const fn seal_record_receipt(
    route: PhysicalRouteId,
    provider_id: u64,
    slot: RouteSlot,
) -> PhysicalRouteReceipt {
    PhysicalRouteReceipt::seal(route, provider_id, slot)
}

const fn contract(name: &'static str) -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation { contract: name })
}

/// Zero-based physical position of one record within one stream lifecycle.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct RecordOrdinal(u64);

impl RecordOrdinal {
    /// The first record's ordinal.
    pub const ZERO: Self = Self(0);

    /// Creates an ordinal from a dense zero-based position.
    #[must_use]
    pub const fn new(position: u64) -> Self {
        Self(position)
    }

    /// Returns the dense zero-based position.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next ordinal, or `None` on `u64` exhaustion.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// The physical bytes that end one record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordTerminator {
    /// One line feed.
    Lf,
    /// One carriage return immediately followed by one line feed.
    CrLf,
    /// One ASCII record separator (0x1E): the json-seq (RFC 7464) delimiter.
    Rs,
    /// No terminator: the record ended at end of input.
    None,
}

/// Borrowing observation of one record's payload bytes.
///
/// The lease BORROWS the retained source the framing provider was constructed over; those bytes were charged to the
/// request when the provider bound the source. A lease therefore costs no allocation and no copy, and its lifetime is
/// the source's, not the stream's: dropping or aborting the stream never invalidates a lease already handed out.
///
/// It is not [`Clone`]: the payload slice a lease hands out stays valid for the retained source's whole lifetime, so a
/// consumer that needs the bytes past the entry keeps the slice rather than a second lease.
#[derive(Debug)]
pub struct RecordLease<'source> {
    payload_start: u64,
    payload: &'source [u8],
}

impl<'source> RecordLease<'source> {
    /// Observes one record payload against the retained source.
    ///
    /// The lease's `payload` slice and `payload_start` must name one coherent range, so the absolute end
    /// `payload_start + payload.len()` must be representable — otherwise [`Self::payload_end`] would publish a
    /// saturated end no source range can match. Coherence against the retained source's own byte length is checked
    /// where the source is visible — every framing codec derives `payload` from its retained bytes, and consumers
    /// re-open the range through [`crate::ErasedProvider::open_range_reusing`], which rejects a range outside the
    /// retained source.
    pub fn try_new(payload_start: u64, payload: &'source [u8]) -> Result<Self, CodecError> {
        payload_start
            .checked_add(payload.len() as u64)
            .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        Ok(Self { payload_start, payload })
    }

    /// Absolute start offset of the payload within the source.
    #[must_use]
    pub const fn payload_start(&self) -> u64 {
        self.payload_start
    }

    /// Absolute end offset of the payload within the source.
    #[must_use]
    pub fn payload_end(&self) -> u64 {
        self.payload_start.saturating_add(self.payload.len() as u64)
    }

    /// The record's payload bytes, without its physical terminator.
    #[must_use]
    pub const fn payload(&self) -> &'source [u8] {
        self.payload
    }
}

/// Stable classification of one record-stream issue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordIssueCode {
    /// The record's payload held no value, only insignificant whitespace.
    BlankRecord,
    /// A source-start byte-order mark preceded the first record.
    InitialByteOrderMark,
    /// A carriage return appeared where the framing law forbids one.
    BareCarriageReturn,
    /// The record's payload is not one complete value of the payload format.
    MalformedPayload,
    /// The record exceeded the per-record byte ceiling.
    OversizeRecord,
    /// The final record ended at end of input with no physical terminator.
    ///
    /// An INTERNAL candidate, never a published outcome: a framer constructs it when input ends without the terminator
    /// and then WAIVES it — an absent final terminator on a complete final value is accepted under both profiles by
    /// law (JSON Lines / RFC 4180 §2.2 permit the final separator to be absent). No codec publishes this code as an
    /// ordered issue and no stream fails because of it; the code exists so the framing frontier can name every
    /// candidate it weighed.
    MissingFinalTerminator,
    /// A json-seq possible-JSON is a non-self-delimiting scalar (number, `true`, `false`, or `null`) with no JSON
    /// whitespace before the RS or EOF: RFC 7464 §2.4's possibly-truncated top-level scalar.
    TruncatedTopLevelScalar,
    /// The input never contained an RS, so no possible-JSON was ever begun.
    UnframedInput,
}

/// Whether one issue merely reports or forces the request's failure class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordIssueSeverity {
    /// Reported in ordinal order; alone it never changes the exit class.
    Advisory,
    /// Reported in ordinal order and FORCES the request's failure class, even when every later record succeeds.
    Error,
}

/// One ordinal that produced no value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordIssue {
    ordinal: RecordOrdinal,
    severity: RecordIssueSeverity,
    code: RecordIssueCode,
    offset: u64,
}

impl RecordIssue {
    /// Creates one ordered issue at its exact scalar-frontier offset.
    #[must_use]
    pub const fn new(
        ordinal: RecordOrdinal,
        severity: RecordIssueSeverity,
        code: RecordIssueCode,
        offset: u64,
    ) -> Self {
        Self {
            ordinal,
            severity,
            code,
            offset,
        }
    }

    /// Physical ordinal this issue occupies.
    #[must_use]
    pub const fn ordinal(self) -> RecordOrdinal {
        self.ordinal
    }

    /// Whether this issue forces the request's failure class.
    #[must_use]
    pub const fn severity(self) -> RecordIssueSeverity {
        self.severity
    }
    /// Stable issue classification.
    #[must_use]
    pub const fn code(self) -> RecordIssueCode {
        self.code
    }
    /// Absolute offset of the scalar-frontier position that produced it.
    #[must_use]
    pub const fn offset(self) -> u64 {
        self.offset
    }
}

/// One delivered record: its ordinal, its physical extent, and its payload lease.
#[derive(Debug)]
pub struct RecordItem<'source> {
    ordinal: RecordOrdinal,
    physical_start: u64,
    physical_end: u64,
    lease: RecordLease<'source>,
}

impl<'source> RecordItem<'source> {
    /// Creates one delivered record over an already-retained payload lease.
    ///
    /// The physical unit is ordered (`physical_start <= physical_end`) and contains the lease's payload range. This
    /// extends the representability check [`RecordLease::try_new`] already made.
    ///
    /// # Errors
    ///
    /// [`CodecFailureKind::InternalContractViolation`] when the extents are incoherent; [`CodecFailureKind::Overflow`]
    /// when the payload end overflows `u64`.
    pub const fn try_new(
        ordinal: RecordOrdinal,
        physical_start: u64,
        physical_end: u64,
        lease: RecordLease<'source>,
    ) -> Result<Self, CodecError> {
        if physical_start > physical_end {
            return Err(contract("record physical unit ends before it starts"));
        }
        if lease.payload_start() < physical_start {
            return Err(contract("record payload begins before its physical unit"));
        }
        match lease.payload_start().checked_add(lease.payload().len() as u64) {
            Some(payload_end) if payload_end <= physical_end => {}
            Some(_) => return Err(contract("record payload extends past its physical unit")),
            None => return Err(CodecError::new(CodecFailureKind::Overflow)),
        }
        Ok(Self {
            ordinal,
            physical_start,
            physical_end,
            lease,
        })
    }

    /// Physical ordinal this record occupies.
    #[must_use]
    pub const fn ordinal(&self) -> RecordOrdinal {
        self.ordinal
    }
    /// Absolute start of the physical record unit.
    #[must_use]
    pub const fn physical_start(&self) -> u64 {
        self.physical_start
    }
    /// Absolute end of the physical record unit, terminator included.
    #[must_use]
    pub const fn physical_end(&self) -> u64 {
        self.physical_end
    }
    /// The record's accounted payload observation.
    #[must_use]
    pub const fn lease(&self) -> &RecordLease<'source> {
        &self.lease
    }
}

/// One ordinal's outcome: a delivered record or an issue.
#[derive(Debug)]
pub enum RecordEntry<'source> {
    /// A record whose payload is available through its lease.
    Record(RecordItem<'source>),
    /// An ordinal that produced no value.
    Issue(RecordIssue),
}

impl RecordEntry<'_> {
    /// The physical ordinal this entry occupies.
    #[must_use]
    pub const fn ordinal(&self) -> RecordOrdinal {
        match self {
            Self::Record(record) => record.ordinal,
            Self::Issue(issue) => issue.ordinal,
        }
    }
}

/// Caller-owned, REUSED destination for one poll's ordered entries.
///
/// Reuse is the point: a stream that allocated a batch per poll would pay one allocation and one teardown per record
/// group for no behavioral gain — a measured share of the reference route's deficit. The caller constructs one batch,
/// drains it, and clears it.
#[derive(Debug, Default)]
pub struct RecordBatch<'source> {
    entries: Vec<RecordEntry<'source>>,
}

impl<'source> RecordBatch<'source> {
    /// An empty batch that allocates on its first entry and keeps its capacity.
    #[must_use]
    pub const fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Ordered entries appended by the latest poll.
    #[must_use]
    pub fn entries(&self) -> &[RecordEntry<'source>] {
        self.entries.as_slice()
    }

    /// Number of entries currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.as_slice().len()
    }

    /// Whether the batch holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.as_slice().is_empty()
    }

    /// Drops every entry while retaining the batch's allocated capacity.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Releases the batch's retained capacity.
    pub fn release(&mut self) {
        self.entries = Vec::new();
    }

    /// Appends one ordered entry, growing the retained capacity as needed.
    ///
    /// The growth stays on the fallible path: a batch grows once per record on a stream of unbounded length, so a
    /// refused reservation must surface as a codec failure rather than abort the process.
    pub fn try_push(&mut self, entry: RecordEntry<'source>) -> Result<(), CodecError> {
        self.entries.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
        self.entries.push(entry);
        Ok(())
    }
}

/// Bounds one poll's progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordBatchLimit {
    max_entries: u32,
    target_bytes: u64,
}

impl RecordBatchLimit {
    /// Creates a bound. `max_entries` is a HARD combined ceiling across records and issues; `target_bytes` is
    /// COOPERATIVE, because one legal record may exceed it on its own.
    ///
    /// Both bounds must be non-zero, and for the same reason: a zero ceiling admits no entry at all, and a zero byte
    /// target is met before the first record is read, so either one turns every poll into an empty batch that reports
    /// progress. A caller computing a remaining budget must treat an exhausted budget as its own stopping condition
    /// rather than passing zero down.
    #[must_use]
    pub const fn new(max_entries: u32, target_bytes: u64) -> Option<Self> {
        if max_entries == 0 || target_bytes == 0 {
            None
        } else {
            Some(Self {
                max_entries,
                target_bytes,
            })
        }
    }

    /// Hard combined entry ceiling for one poll.
    #[must_use]
    pub const fn max_entries(self) -> u32 {
        self.max_entries
    }

    /// Cooperative payload-byte target for one poll.
    #[must_use]
    pub const fn target_bytes(self) -> u64 {
        self.target_bytes
    }
}

/// Clean end-of-stream summary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecordCompletion {
    records: u64,
    issues: u64,
}

impl RecordCompletion {
    /// Creates a completion summary.
    #[must_use]
    pub const fn new(records: u64, issues: u64) -> Self {
        Self { records, issues }
    }
    /// Records delivered with a payload.
    #[must_use]
    pub const fn records(self) -> u64 {
        self.records
    }
    /// Ordinals that produced an issue instead of a payload.
    #[must_use]
    pub const fn issues(self) -> u64 {
        self.issues
    }
}

/// One poll's outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordPoll {
    /// The batch gained at least one ordered entry.
    Filled,
    /// The cooperative budget was exhausted before any entry was produced.
    Pending,
    /// The stream ended cleanly; no further poll is legal.
    End(RecordCompletion),
}

/// One abort's outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordStreamAbort {
    /// Stream state was released.
    Aborted,
    /// The stream was already terminal; abort is idempotent.
    AlreadyTerminal,
}

/// One concrete framing implementation over a retained source.
///
/// The session holds no borrow of the source itself — the erased carrier owns the retained authority and hands it
/// back on every poll, exactly as [`crate::AccessSession`] receives its [`crate::AccessInput`]. That is what keeps the
/// concrete state `'static` and the erased trait object free of a higher-ranked lifetime it could not name.
pub trait RecordStreamSession: Any {
    /// Appends ordered entries up to `limit` into the caller's batch.
    fn poll<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        limit: RecordBatchLimit,
        batch: &mut RecordBatch<'source>,
        run: &mut CodecRunContext<'_, '_>,
    ) -> Result<RecordPoll, CodecError>;

    /// Releases stream-owned state. Idempotent; never invalidates leases already handed out.
    fn abort(&mut self, run: &mut CodecRunContext<'_, '_>) -> Result<RecordStreamAbort, CodecError>;

    /// Exclusive end of the authored stream-prefix that is not a data record (header row + terminator), as an absolute
    /// source offset.
    ///
    /// `None` when the dialect has no such prefix. Consumes the prefix if the stream has not started; a later poll does
    /// not re-consume it.
    ///
    /// Calling this more than once is legal and IDEMPOTENT: an implementation computes the end (and consumes the
    /// prefix) at most once, so every repeat call returns the SAME offset — or the same `None` — without consuming
    /// anything further. A drive that queries before its first poll therefore cannot advance or disturb the record
    /// stream.
    fn header_physical_end(&mut self, source: ResolvedSource<'_>) -> Result<Option<u64>, CodecError> {
        let _ = source;
        Ok(None)
    }
}

/// Sealed, accounted carrier for one record-stream session.
///
/// The carrier owns the TERMINAL LAW: it refuses to poll a stream that has completed, failed, or been aborted, and it
/// stamps every outcome with the physical route receipt the provider sealed.
pub struct ErasedRecordStreamSession<'source> {
    state: Box<dyn RecordStreamSession>,
    receipt: PhysicalRouteReceipt,
    terminal: Option<SessionTerminal>,
    source: ResolvedSource<'source>,
}

impl core::fmt::Debug for ErasedRecordStreamSession<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ErasedRecordStreamSession")
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

impl<'source> ErasedRecordStreamSession<'source> {
    /// Seals one concrete framing session behind the erased lifecycle.
    pub fn try_new_with_route<T, F>(
        source: ResolvedSource<'source>,
        route: PhysicalRouteId,
        provider_id: u64,
        slot: RouteSlot,
        constructor: F,
    ) -> Result<Self, CodecError>
    where
        T: RecordStreamSession,
        F: FnOnce() -> Result<T, CodecError>,
    {
        let state: Box<dyn RecordStreamSession> = crate::erased::fallible_box(constructor()?)?;
        Ok(Self {
            state,
            receipt: seal_record_receipt(route, provider_id, slot),
            terminal: None,
            source,
        })
    }

    /// Retained source authority the framed ranges address.
    #[must_use]
    pub const fn source(&self) -> ResolvedSource<'source> {
        self.source
    }

    /// Sealed physical route receipt for this stream.
    #[must_use]
    pub const fn route_receipt(&self) -> PhysicalRouteReceipt {
        self.receipt
    }

    /// Appends ordered entries into the caller's batch.
    ///
    /// A poll on a terminal stream is an internal contract violation: an exhausted stream must never look like an empty
    /// one. A `Filled` poll that appended no entry is the same class of lie in the other direction — the batch law
    /// promises at least one entry per `Filled`, so accepting an empty one would let a misbehaving framing provider
    /// livelock a drive that polls until [`RecordPoll::End`].
    pub fn poll(
        &mut self,
        limit: RecordBatchLimit,
        batch: &mut RecordBatch<'source>,
        run: &mut CodecRunContext<'_, '_>,
    ) -> Result<RecordPoll, CodecError> {
        if self.terminal.is_some() {
            return Err(contract("record stream polled after terminal"));
        }
        match self.state.poll(self.source, limit, batch, run) {
            Ok(RecordPoll::End(completion)) => {
                self.terminal = Some(SessionTerminal::Complete);
                Ok(RecordPoll::End(completion))
            }
            Ok(RecordPoll::Filled) if batch.is_empty() => {
                let error = contract("record stream reported progress with an empty batch");
                self.terminal = Some(SessionTerminal::Failed(error.kind()));
                Err(error)
            }
            Ok(other) => Ok(other),
            Err(error) => {
                self.terminal = Some(SessionTerminal::Failed(error.kind()));
                Err(error)
            }
        }
    }

    /// Exclusive end of the authored stream-prefix that is not a data record (header row + terminator). `None` when the
    /// dialect has no such prefix.
    ///
    /// A headered edit drive publishes `[source_start, end)` before splicing data records, so identity `--edit` keeps
    /// the authored header bytes.
    pub fn header_physical_end(&mut self) -> Result<Option<u64>, CodecError> {
        if self.terminal.is_some() {
            return Err(contract("record stream queried after terminal"));
        }
        match self.state.header_physical_end(self.source) {
            Ok(end) => Ok(end),
            Err(error) => {
                self.terminal = Some(SessionTerminal::Failed(error.kind()));
                Err(error)
            }
        }
    }

    /// Releases stream-owned state. Idempotent.
    ///
    /// A stream that already reached a terminal state has no abort left to run: its concrete state releases through
    /// this carrier's `Drop`, so a failed or completed stream is released too, never leaked — the trait's release
    /// promise is kept on every path, just not always through this method.
    pub fn abort(&mut self, run: &mut CodecRunContext<'_, '_>) -> Result<RecordStreamAbort, CodecError> {
        if self.terminal.is_some() {
            return Ok(RecordStreamAbort::AlreadyTerminal);
        }
        match self.state.abort(run) {
            Ok(RecordStreamAbort::Aborted) => {
                self.terminal = Some(SessionTerminal::Aborted);
                Ok(RecordStreamAbort::Aborted)
            }
            Ok(RecordStreamAbort::AlreadyTerminal) => {
                self.terminal = Some(SessionTerminal::Aborted);
                Ok(RecordStreamAbort::AlreadyTerminal)
            }
            Err(error) => {
                self.terminal = Some(SessionTerminal::Failed(error.kind()));
                Err(error)
            }
        }
    }
}

/// The codec-neutral open envelope a registered record-provider factory receives.
///
/// Profiles and option structs live with the grammar owner. This envelope carries only the primitives those crates
/// reconstruct into their own types, so codec-core never names JSON indent or a CSV delimiter law.
#[derive(Clone, Copy, Debug)]
pub enum RecordProviderOpen {
    /// NDJSON framing: recovering vs strict, plus the normalized ceiling.
    Ndjson {
        /// `true` is the recovering profile; `false` is strict.
        recovering: bool,
        /// The normalized per-record ceiling.
        max_record_bytes: u64,
    },
    /// JSON Text Sequence framing: recovering vs strict, plus the normalized ceiling.
    JsonSeq {
        /// `true` is the `--seq` recovering profile; `false` is strict.
        recovering: bool,
        /// The normalized per-record ceiling.
        max_record_bytes: u64,
    },
    /// CSV/TSV framing: delimiter, header, quote, and the normalized ceiling.
    Delimited {
        /// Field delimiter byte.
        delimiter: u8,
        /// Whether the first record is a header.
        header: bool,
        /// `Some(b'"')` is RFC 4180 quoting; `None` is the TSV no-quote grammar.
        quote: Option<u8>,
        /// The normalized per-record ceiling.
        max_record_bytes: u64,
    },
}

/// One decoder that advertises record-stream routes over a retained source.
pub trait RecordStreamProvider: Any {
    /// Complete record-route bundles in deterministic provider order.
    #[must_use]
    fn record_route_descriptions(&self) -> &[RouteDescription];

    /// Opens exactly one advertised record route.
    fn open_record_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        provider_id: u64,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedRecordStreamSession<'source>, CodecError>;
}

/// Sealed, source-bound erased record-stream provider.
///
/// The carrier validates the concrete provider's route table at construction — the same dense-slot law the
/// access-side carrier enforces — so a codec that registers sparse or duplicate slots fails at construction instead
/// of diverging at dispatch.
pub struct ErasedRecordStreamProvider<'source> {
    source: ResolvedSource<'source>,
    owner: Box<dyn RecordStreamProvider>,
    provider_id: u64,
    // Thread posture is declared, not accidental: the bare `Any` bound happens to be non-Send today, and this marker
    // holds even if a `Send` supertrait ever joins the trait (matches the erased.rs carriers).
    _not_send: core::marker::PhantomData<alloc::rc::Rc<()>>,
}

impl core::fmt::Debug for ErasedRecordStreamProvider<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ErasedRecordStreamProvider")
            .field("provider_id", &self.provider_id)
            .finish_non_exhaustive()
    }
}

impl<'source> ErasedRecordStreamProvider<'source> {
    /// Seals one concrete record provider and charges its retained state.
    pub fn try_new_provider<T, F>(source: ResolvedSource<'source>, constructor: F) -> Result<Self, CodecError>
    where
        T: RecordStreamProvider,
        F: FnOnce() -> Result<T, CodecError>,
    {
        let owner: Box<dyn RecordStreamProvider> = crate::erased::fallible_box(constructor()?)?;
        crate::binder::validate_routes(owner.record_route_descriptions())?;
        Ok(Self {
            source,
            owner,
            provider_id: crate::provider::fresh_provider_id()?,
            _not_send: core::marker::PhantomData,
        })
    }

    /// Retained source authority the provider frames.
    #[must_use]
    pub const fn source(&self) -> ResolvedSource<'source> {
        self.source
    }

    /// Core-sealed provider identity carried by every opened stream's receipt.
    #[must_use]
    pub const fn provider_id(&self) -> u64 {
        self.provider_id
    }

    /// Complete record-route bundles in deterministic provider order.
    #[must_use]
    pub fn record_route_descriptions(&self) -> &[RouteDescription] {
        self.owner.record_route_descriptions()
    }

    /// Opens exactly one advertised record route; dispatch failure never falls back.
    pub fn open_record_route(
        &mut self,
        slot: RouteSlot,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedRecordStreamSession<'source>, CodecError> {
        let advertised = self
            .owner
            .record_route_descriptions()
            .iter()
            .any(|description| description.slot() == slot);
        if !advertised {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        let input = ProviderInput::new(self.source);
        self.owner.open_record_route(input, slot, self.provider_id, resources)
    }
}
