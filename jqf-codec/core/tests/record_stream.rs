//! The `jqf.record-stream@1` lifecycle laws, exercised through the sealed erased carrier a real framing codec is
//! reached by.
//!
//! The concrete framer here is deliberately trivial — line-feed splitting plus the three prefix/fault shapes the ABI
//! laws name (a source-start BOM, blank segments, an unterminated tail). What is under test is the ABI: ordinal order,
//! the caller-owned reused batch, the accounted non-`Clone` lease, the terminal law, and idempotent abort.

use jqf_codec_core::{
    AccessFootprintKind, AccessGuarantees, AccessResultKind, CapabilityBundle, CodecDemand, CodecError,
    CodecFailureKind, CodecRunContext, DemandClause, DiagnosticPolicy, ErasedRecordStreamProvider,
    ErasedRecordStreamSession, PhysicalRouteId, ProviderInput, RecordBatch, RecordBatchLimit, RecordEntry, RecordIssue,
    RecordIssueCode, RecordIssueSeverity, RecordLease, RecordOrdinal, RecordPoll, RecordStreamAbort,
    RecordStreamProvider, RecordStreamSession, RouteDescription, RouteSlot,
};
mod common;

use common::resources as crate_resources;
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const ROUTE: PhysicalRouteId = match jqf_codec_core::PhysicalRouteId::derive("jqf-core-test", 4, 1) {
    Some(id) => id,
    None => panic!("nonzero"),
};

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "test", bytes, 0)
}

/// Splits the retained input on line feeds. One entry per poll, so the tests can observe batching and the terminal law
/// separately. It models the three prefix/fault shapes the shared ABI carries: a source-start byte-order mark becomes
/// an ADVISORY that shares the first record's ordinal, a blank segment becomes a `BlankRecord` issue inside the
/// records' dense ordinal sequence, and an unterminated complete tail is WAIVED (a plain record).
struct LineFramer {
    cursor: usize,
    next_ordinal: u64,
    records: u64,
    issues: u64,
    aborted: bool,
}

impl RecordStreamSession for LineFramer {
    fn poll<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        limit: RecordBatchLimit,
        batch: &mut RecordBatch<'source>,
        _run: &mut CodecRunContext<'_, '_>,
    ) -> Result<RecordPoll, CodecError> {
        let bytes = source.bytes();
        let mut produced = 0u32;
        // A source-start byte-order mark is PREFIX metadata: it consumes no ordinal, and its advisory carries the
        // ordinal of the record it precedes (ordinal 0 shares with the first record), counting toward the same hard
        // entry ceiling and the completion's issue count.
        if self.cursor == 0 && bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            batch.try_push(RecordEntry::Issue(RecordIssue::new(
                RecordOrdinal::new(self.next_ordinal),
                RecordIssueSeverity::Advisory,
                RecordIssueCode::InitialByteOrderMark,
                0,
            )))?;
            self.issues += 1;
            self.cursor = 3;
            produced += 1;
        }
        while produced < limit.max_entries() {
            if self.cursor >= bytes.len() {
                if produced == 0 {
                    return Ok(RecordPoll::End(jqf_codec_core::RecordCompletion::new(
                        self.records,
                        self.issues,
                    )));
                }
                break;
            }
            let start = self.cursor;
            // No line feed before end of input is the internal end-of-input candidate; a COMPLETE tail value waives it
            // (accepted as a plain record), and a blank tail falls through to the issue arm below.
            let end = bytes[start..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| start + offset);
            let physical_end = (end + 1).min(bytes.len());
            self.cursor = physical_end;
            let ordinal = RecordOrdinal::new(self.next_ordinal);
            self.next_ordinal += 1;
            if start == end {
                // A blank segment is one ORDINAL that produced no value: an advisory issue in the SAME dense sequence
                // as records, not on a side numbering.
                self.issues += 1;
                batch.try_push(RecordEntry::Issue(RecordIssue::new(
                    ordinal,
                    RecordIssueSeverity::Advisory,
                    RecordIssueCode::BlankRecord,
                    start as u64,
                )))?;
            } else {
                self.records += 1;
                let lease = RecordLease::try_new(start as u64, &bytes[start..end])?;
                batch.try_push(RecordEntry::Record(jqf_codec_core::RecordItem::try_new(
                    ordinal,
                    start as u64,
                    physical_end as u64,
                    lease,
                )?))?;
            }
            produced += 1;
        }
        Ok(RecordPoll::Filled)
    }

    fn abort(&mut self, _run: &mut CodecRunContext<'_, '_>) -> Result<RecordStreamAbort, CodecError> {
        self.aborted = true;
        Ok(RecordStreamAbort::Aborted)
    }
}

struct LineProvider {
    routes: [RouteDescription; 1],
}

impl LineProvider {
    fn try_new(resources: &ResourceContext<'_>) -> Result<Self, CodecError> {
        let mut demand = CodecDemand::try_new(resources);
        demand.try_insert(&DemandClause::SemanticRoot)?;
        Ok(Self {
            routes: [RouteDescription::new(
                RouteSlot::new(0),
                CapabilityBundle::new(
                    AccessFootprintKind::Whole,
                    AccessResultKind::RecordStream,
                    demand,
                    AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                ),
            )],
        })
    }
}

impl RecordStreamProvider for LineProvider {
    fn record_route_descriptions(&self) -> &[RouteDescription] {
        &self.routes
    }

    fn open_record_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        provider_id: u64,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedRecordStreamSession<'source>, CodecError> {
        ErasedRecordStreamSession::try_new_with_route::<LineFramer, _>(input.source(), ROUTE, provider_id, slot, || {
            Ok(LineFramer {
                cursor: 0,
                next_ordinal: 0,
                records: 0,
                issues: 0,
                aborted: false,
            })
        })
    }
}

fn open<'source>(
    bytes: &'source [u8],
    resources: &mut ResourceContext<'_>,
) -> (ErasedRecordStreamProvider<'source>, ErasedRecordStreamSession<'source>) {
    let mut provider = ErasedRecordStreamProvider::try_new_provider::<LineProvider, _>(source(bytes), || {
        LineProvider::try_new(resources)
    })
    .expect("provider");
    let session = provider
        .open_record_route(RouteSlot::new(0), resources)
        .expect("session");
    (provider, session)
}

fn limit(entries: u32) -> RecordBatchLimit {
    RecordBatchLimit::new(entries, u64::MAX).expect("limit")
}

#[test]
fn neither_a_zero_entry_nor_a_zero_byte_batch_limit_is_expressible() {
    assert!(RecordBatchLimit::new(0, 16).is_none());
    assert!(RecordBatchLimit::new(16, 0).is_none());
    assert_eq!(limit(4).max_entries(), 4);
}

#[test]
fn entries_arrive_in_strict_ordinal_order_with_borrowed_payloads() {
    let mut resources = crate_resources();
    let input = b"one\ntwo\nthree\n";
    let (_provider, mut session) = open(input, &mut resources);
    let mut batch = RecordBatch::new();
    let mut run = CodecRunContext::new(&mut resources);
    assert_eq!(
        session.poll(limit(16), &mut batch, &mut run).expect("poll"),
        RecordPoll::Filled
    );
    let ordinals: Vec<u64> = batch.entries().iter().map(|entry| entry.ordinal().get()).collect();
    assert_eq!(ordinals, vec![0, 1, 2]);
    let payloads: Vec<&[u8]> = batch
        .entries()
        .iter()
        .map(|entry| match entry {
            RecordEntry::Record(record) => record.lease().payload(),
            RecordEntry::Issue(_) => panic!("no issues expected"),
        })
        .collect();
    assert_eq!(payloads, vec![&b"one"[..], b"two", b"three"]);
}

#[test]
fn the_batch_is_reused_across_polls_and_keeps_its_capacity() {
    let mut resources = crate_resources();
    let input = b"a\nb\nc\nd\n";
    let (_provider, mut session) = open(input, &mut resources);
    let mut batch = RecordBatch::new();
    {
        let mut run = CodecRunContext::new(&mut resources);
        session.poll(limit(2), &mut batch, &mut run).expect("poll");
    }
    assert_eq!(batch.len(), 2);
    batch.clear();
    assert!(batch.is_empty());
    // Clearing keeps the allocation — a record stream must not pay one allocation per batch; the capacity law itself
    // is the crate's unit coverage, so this lane pins the observable reuse semantics.
    {
        let mut run = CodecRunContext::new(&mut resources);
        session.poll(limit(2), &mut batch, &mut run).expect("poll");
    }
    assert_eq!(batch.entries()[0].ordinal(), RecordOrdinal::new(2));
    batch.release();
}

#[test]
fn a_completed_stream_is_terminal_and_refuses_a_further_poll() {
    let mut resources = crate_resources();
    let (_provider, mut session) = open(b"a\n", &mut resources);
    let mut batch = RecordBatch::new();
    {
        let mut run = CodecRunContext::new(&mut resources);
        session.poll(limit(8), &mut batch, &mut run).expect("poll");
        batch.clear();
        assert!(matches!(
            session.poll(limit(8), &mut batch, &mut run).expect("poll"),
            RecordPoll::End(completion) if completion.records() == 1
        ));
    }
    let mut run = CodecRunContext::new(&mut resources);
    let refused = session
        .poll(limit(8), &mut batch, &mut run)
        .expect_err("terminal poll is refused");
    assert!(matches!(
        refused.kind(),
        CodecFailureKind::InternalContractViolation { .. }
    ));
}

#[test]
fn abort_is_idempotent_and_leaves_already_returned_leases_valid() {
    let mut resources = crate_resources();
    let input = b"first\nsecond\n";
    let (_provider, mut session) = open(input, &mut resources);
    let mut batch = RecordBatch::new();
    {
        let mut run = CodecRunContext::new(&mut resources);
        session.poll(limit(1), &mut batch, &mut run).expect("poll");
        assert_eq!(session.abort(&mut run).expect("abort"), RecordStreamAbort::Aborted);
        assert_eq!(
            session.abort(&mut run).expect("abort again"),
            RecordStreamAbort::AlreadyTerminal
        );
    }
    // The lease borrows the retained source, not the stream, so aborting the stream cannot invalidate a record already
    // handed out.
    let RecordEntry::Record(record) = &batch.entries()[0] else {
        panic!("record expected");
    };
    assert_eq!(record.lease().payload(), b"first");
    assert_eq!(record.lease().payload_start(), 0);
    assert_eq!(record.lease().payload_end(), 5);
}

#[test]
fn the_stream_carries_the_sealed_route_receipt_its_provider_advertised() {
    let mut resources = crate_resources();
    let (provider, session) = open(b"x\n", &mut resources);
    let receipt = session.route_receipt();
    assert_eq!(receipt.route(), ROUTE);
    assert_eq!(receipt.provider_id(), provider.provider_id());
    assert_eq!(receipt.slot(), RouteSlot::new(0));
    assert_eq!(provider.record_route_descriptions().len(), 1);
    assert_eq!(
        provider.record_route_descriptions()[0].bundle().result(),
        AccessResultKind::RecordStream
    );
}

#[test]
fn an_unadvertised_slot_is_refused_at_bind_with_provider_route_mismatch() {
    let mut resources = crate_resources();
    let input = b"x\n";
    let mut provider = ErasedRecordStreamProvider::try_new_provider::<LineProvider, _>(source(input), || {
        LineProvider::try_new(&resources)
    })
    .expect("provider");
    let rejected = provider
        .open_record_route(RouteSlot::new(7), &mut resources)
        .expect_err("unadvertised slot");
    assert_eq!(rejected.kind(), CodecFailureKind::ProviderRouteMismatch);
}

#[test]
fn record_ordinals_count_from_zero_and_saturate_visibly() {
    assert_eq!(RecordOrdinal::ZERO.get(), 0);
    assert_eq!(RecordOrdinal::new(4).next().map(RecordOrdinal::get), Some(5));
    assert_eq!(RecordOrdinal::new(u64::MAX).next(), None);
}

/// A framer that lies: it reports progress without appending a single entry.
struct EmptyFilledFramer;

impl RecordStreamSession for EmptyFilledFramer {
    fn poll<'source>(
        &mut self,
        _source: ResolvedSource<'source>,
        _limit: RecordBatchLimit,
        _batch: &mut RecordBatch<'source>,
        _run: &mut CodecRunContext<'_, '_>,
    ) -> Result<RecordPoll, CodecError> {
        Ok(RecordPoll::Filled)
    }

    fn abort(&mut self, _run: &mut CodecRunContext<'_, '_>) -> Result<RecordStreamAbort, CodecError> {
        Ok(RecordStreamAbort::Aborted)
    }
}

/// A well-formed route table over a framer that lies about progress.
struct EmptyFilledProvider {
    routes: [RouteDescription; 1],
}

impl EmptyFilledProvider {
    fn try_new(resources: &ResourceContext<'_>) -> Result<Self, CodecError> {
        let mut demand = CodecDemand::try_new(resources);
        demand.try_insert(&DemandClause::SemanticRoot)?;
        Ok(Self {
            routes: [RouteDescription::new(
                RouteSlot::new(0),
                CapabilityBundle::new(
                    AccessFootprintKind::Whole,
                    AccessResultKind::RecordStream,
                    demand,
                    AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                ),
            )],
        })
    }
}

impl RecordStreamProvider for EmptyFilledProvider {
    fn record_route_descriptions(&self) -> &[RouteDescription] {
        &self.routes
    }

    fn open_record_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        provider_id: u64,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedRecordStreamSession<'source>, CodecError> {
        ErasedRecordStreamSession::try_new_with_route::<EmptyFilledFramer, _>(
            input.source(),
            ROUTE,
            provider_id,
            slot,
            || Ok(EmptyFilledFramer),
        )
    }
}

#[test]
fn a_filled_poll_with_an_empty_batch_is_an_internal_contract_violation() {
    let mut resources = crate_resources();
    let mut provider = ErasedRecordStreamProvider::try_new_provider::<EmptyFilledProvider, _>(source(b"x\n"), || {
        EmptyFilledProvider::try_new(&resources)
    })
    .expect("provider");
    let mut session = provider
        .open_record_route(RouteSlot::new(0), &mut resources)
        .expect("session");
    let mut batch = RecordBatch::new();
    let mut run = CodecRunContext::new(&mut resources);
    // The Filled doc promises at least one ordered entry. Accepting an empty one would livelock any drive that polls
    // until End, so the carrier must raise instead of relaying the lie.
    let rejected = session
        .poll(limit(1), &mut batch, &mut run)
        .expect_err("empty filled poll");
    assert!(matches!(
        rejected.kind(),
        CodecFailureKind::InternalContractViolation { .. }
    ));
    // The lie terminalized the stream: a later poll is refused too.
    let again = session.poll(limit(1), &mut batch, &mut run);
    assert!(matches!(
        again.expect_err("polled after terminal").kind(),
        CodecFailureKind::InternalContractViolation { .. }
    ));
}

/// Registers one route at a NONZERO first slot: sparse against the dense table law.
struct SparseSlotProvider {
    routes: [RouteDescription; 1],
}

impl SparseSlotProvider {
    fn try_new(resources: &ResourceContext<'_>) -> Result<Self, CodecError> {
        let mut demand = CodecDemand::try_new(resources);
        demand.try_insert(&DemandClause::SemanticRoot)?;
        Ok(Self {
            routes: [RouteDescription::new(
                RouteSlot::new(2),
                CapabilityBundle::new(
                    AccessFootprintKind::Whole,
                    AccessResultKind::RecordStream,
                    demand,
                    AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                ),
            )],
        })
    }
}

impl RecordStreamProvider for SparseSlotProvider {
    fn record_route_descriptions(&self) -> &[RouteDescription] {
        &self.routes
    }

    fn open_record_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        provider_id: u64,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedRecordStreamSession<'source>, CodecError> {
        ErasedRecordStreamSession::try_new_with_route::<LineFramer, _>(input.source(), ROUTE, provider_id, slot, || {
            Ok(LineFramer {
                cursor: 0,
                next_ordinal: 0,
                records: 0,
                issues: 0,
                aborted: false,
            })
        })
    }
}

/// Registers the SAME slot twice: duplicate against the dense table law.
struct DuplicateSlotProvider {
    routes: [RouteDescription; 2],
}

impl DuplicateSlotProvider {
    fn try_new(resources: &ResourceContext<'_>) -> Result<Self, CodecError> {
        let route = |resources: &ResourceContext<'_>| -> Result<RouteDescription, CodecError> {
            let mut demand = CodecDemand::try_new(resources);
            demand.try_insert(&DemandClause::SemanticRoot)?;
            Ok(RouteDescription::new(
                RouteSlot::new(0),
                CapabilityBundle::new(
                    AccessFootprintKind::Whole,
                    AccessResultKind::RecordStream,
                    demand,
                    AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
                ),
            ))
        };
        Ok(Self {
            routes: [route(resources)?, route(resources)?],
        })
    }
}

impl RecordStreamProvider for DuplicateSlotProvider {
    fn record_route_descriptions(&self) -> &[RouteDescription] {
        &self.routes
    }

    fn open_record_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        provider_id: u64,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedRecordStreamSession<'source>, CodecError> {
        ErasedRecordStreamSession::try_new_with_route::<LineFramer, _>(input.source(), ROUTE, provider_id, slot, || {
            Ok(LineFramer {
                cursor: 0,
                next_ordinal: 0,
                records: 0,
                issues: 0,
                aborted: false,
            })
        })
    }
}

#[test]
fn sparse_or_duplicate_record_route_slots_fail_at_provider_construction() {
    let resources = crate_resources();
    let sparse = ErasedRecordStreamProvider::try_new_provider::<SparseSlotProvider, _>(source(b"x\n"), || {
        SparseSlotProvider::try_new(&resources)
    });
    assert!(
        matches!(
            sparse.expect_err("sparse slot table").kind(),
            CodecFailureKind::InternalContractViolation { .. }
        ),
        "a record provider skipping route-table validation would diverge only at dispatch"
    );

    let duplicate = ErasedRecordStreamProvider::try_new_provider::<DuplicateSlotProvider, _>(source(b"x\n"), || {
        DuplicateSlotProvider::try_new(&resources)
    });
    assert!(matches!(
        duplicate.expect_err("duplicate slots").kind(),
        CodecFailureKind::InternalContractViolation { .. }
    ));
}

#[test]
fn ordinals_are_dense_from_zero_across_records_and_issues() {
    let mut resources = crate_resources();
    let (_provider, mut session) = open(b"\nb\nc\n\n", &mut resources);
    let mut batch = RecordBatch::new();
    let mut run = CodecRunContext::new(&mut resources);
    session.poll(limit(16), &mut batch, &mut run).expect("poll");
    // An issue occupies the SAME dense-from-zero physical ordinal sequence as a record — never a side numbering —
    // and both kinds count toward one hard entry ceiling.
    let shapes: Vec<(&str, u64)> = batch
        .entries()
        .iter()
        .map(|entry| match entry {
            RecordEntry::Record(record) => ("record", record.ordinal().get()),
            RecordEntry::Issue(issue) => ("issue", issue.ordinal().get()),
        })
        .collect();
    assert_eq!(shapes, vec![("issue", 0), ("record", 1), ("record", 2), ("issue", 3)]);
    assert!(matches!(
        session.poll(limit(16), &mut batch, &mut run).expect("poll"),
        RecordPoll::End(completion) if completion.records() == 2 && completion.issues() == 2
    ));
}

#[test]
fn a_source_start_bom_advisory_shares_ordinal_zero_with_the_first_record() {
    let mut resources = crate_resources();
    let (_provider, mut session) = open(b"\xEF\xBB\xBFone\ntwo\n", &mut resources);
    let mut batch = RecordBatch::new();
    let mut run = CodecRunContext::new(&mut resources);
    session.poll(limit(16), &mut batch, &mut run).expect("poll");
    // Prefix metadata consumes NO ordinal: the advisory PRECEDES the first record carrying that record's own ordinal,
    // so a consumer merging by ordinal still sees it first.
    let RecordEntry::Issue(bom) = &batch.entries()[0] else {
        panic!("BOM advisory expected");
    };
    assert_eq!(bom.code(), RecordIssueCode::InitialByteOrderMark);
    assert_eq!(bom.severity(), RecordIssueSeverity::Advisory);
    assert_eq!(bom.offset(), 0);
    assert_eq!(bom.ordinal().get(), 0);
    let ordinals: Vec<u64> = batch.entries().iter().map(|entry| entry.ordinal().get()).collect();
    assert_eq!(ordinals, vec![0, 0, 1]);
    let RecordEntry::Record(first) = &batch.entries()[1] else {
        panic!("first record expected");
    };
    assert_eq!(first.lease().payload(), &b"one"[..]);
    assert!(matches!(
        session.poll(limit(16), &mut batch, &mut run).expect("poll"),
        RecordPoll::End(completion) if completion.records() == 2 && completion.issues() == 1
    ));
}

#[test]
fn a_complete_final_record_without_a_terminator_is_accepted() {
    let mut resources = crate_resources();
    let input = b"one\ntail";
    let (_provider, mut session) = open(input, &mut resources);
    let mut batch = RecordBatch::new();
    let mut run = CodecRunContext::new(&mut resources);
    session.poll(limit(16), &mut batch, &mut run).expect("poll");
    // The scan reaching end of input without a terminator is the internal missing-final-terminator candidate; for a
    // COMPLETE tail value it is waived outright — a plain record, no issue, under either profile law (JSON Lines
    // permits the absent final separator).
    assert_eq!(batch.len(), 2);
    assert!(
        batch
            .entries()
            .iter()
            .all(|entry| matches!(entry, RecordEntry::Record(_)))
    );
    let RecordEntry::Record(last) = &batch.entries()[1] else {
        panic!("final record expected");
    };
    assert_eq!(last.ordinal().get(), 1);
    assert_eq!(last.lease().payload(), b"tail");
    assert_eq!(last.physical_end(), input.len() as u64);
    assert!(matches!(
        session.poll(limit(16), &mut batch, &mut run).expect("poll"),
        RecordPoll::End(completion) if completion.records() == 2 && completion.issues() == 0
    ));
}

#[test]
fn an_incoherent_record_item_is_a_contract_error() {
    let lease = RecordLease::try_new(10, b"hi").expect("lease");
    let error = jqf_codec_core::RecordItem::try_new(RecordOrdinal::ZERO, 0, 5, lease).expect_err("payload past unit");
    assert!(matches!(
        error.kind(),
        CodecFailureKind::InternalContractViolation { contract }
            if contract == "record payload extends past its physical unit"
    ));
}
