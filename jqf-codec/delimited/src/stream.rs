//! Serial RFC 4180 record framing over contiguous retained input.
//!
//! The session owns physical framing and NOTHING else. It never parses field grammar, never builds a document, never
//! copies a payload byte, and never allocates per record: one poll walks the retained input, hands out
//! [`jqf_codec_core::RecordLease`] borrows of the payload ranges it proves, and appends them to the caller's reused
//! batch. Field grammar (splitting and quoting, and the header row's NAMES) belongs to the payload decode, reached
//! later by narrowing the same retained source to the range this framer named.
//!
//! Under `csv.rfc4180-header@1` the framer does own one header fact: the first record is CONSUMED as stream-prefix
//! schema rather than framed as data. Like a byte-order mark it takes no ordinal, so ordinal 0 is the first DATA row;
//! unlike a byte-order mark it is legal, and a framing fault inside it is terminal exactly as it would be in a data
//! record. The header's NAMES are still the payload codec's, read independently from the same retained source — no
//! state crosses the record ABI.
//!
//! CSV's dialects are strict: the first framing fault is terminal. There is no recovering dialect yet
//! (`csv.rfc4180-recover@1` is reserved until its gate).

use jqf_codec_core::{
    CodecError, CodecRunContext, RecordBatch, RecordBatchLimit, RecordCompletion, RecordEntry, RecordIssueCode,
    RecordItem, RecordLease, RecordOrdinal, RecordPoll, RecordStreamAbort, RecordStreamSession,
};
use jqf_resource::WorkAdmission;
use jqf_source::ResolvedSource;

use super::boundary::{Frame, frame_at};
use super::error::{contract, framing_in};
use crate::CsvDecodeOptions;

const BYTE_ORDER_MARK: [u8; 3] = [0xef, 0xbb, 0xbf];

/// One framed record unit, before the profile decides what to do with it.
struct Unit {
    record_start: usize,
    payload_end: usize,
    physical_end: usize,
}

/// A framing fault inside one record unit, at its exact absolute offset.
struct Fault {
    code: RecordIssueCode,
    offset: usize,
}

/// Serial RFC 4180 framing state.
///
/// Every field is a scalar: the session retains no payload storage, no parser, and no per-record buffer.
pub(crate) struct CsvRecordSession {
    options: CsvDecodeOptions,
    cursor: usize,
    next_ordinal: u64,
    records: u64,
    started: bool,
    aborted: bool,
    /// A terminal framing failure held back until the batch drains.
    ///
    /// The strict profile's earlier records must stay published and observable — the same law the adjacent-value
    /// drive keeps when a later text fails to parse. Since one poll frames MANY records, a fault discovered while
    /// filling a batch must not discard the records already in it: the batch is returned, and the fault is raised on
    /// the next poll, before any further entry.
    pending_failure: Option<CodecError>,
    /// Absolute exclusive end of the consumed header prefix, set by [`Self::start_stream`] under the headered dialect.
    header_end: Option<u64>,
}

impl CsvRecordSession {
    pub(crate) const fn new(options: CsvDecodeOptions) -> Self {
        Self {
            options,
            cursor: 0,
            next_ordinal: 0,
            records: 0,
            started: false,
            aborted: false,
            pending_failure: None,
            header_end: None,
        }
    }

    const fn completion(&self) -> RecordCompletion {
        // CSV v1 dialects are strict: every fault is terminal, so the stream carries no ordered issues (the issues
        // count stays zero).
        RecordCompletion::new(self.records, 0)
    }

    fn take_ordinal(&mut self) -> Result<RecordOrdinal, CodecError> {
        let ordinal = RecordOrdinal::new(self.next_ordinal);
        self.next_ordinal = ordinal
            .next()
            .ok_or_else(|| contract("CSV record ordinal exhausted"))?
            .get();
        Ok(ordinal)
    }

    /// Consumes the stream's prefix: a byte-order mark, then — under the headered dialect — the header record.
    ///
    /// Both are stream PREFIX: neither consumes an ordinal, so ordinal 0 is the first DATA record. A byte-order mark is
    /// rejected outright; a header record is consumed, but a framing fault inside it is terminal exactly as it would be
    /// in a data record — the header is framed by the same walk.
    fn start_stream(&mut self, source: ResolvedSource<'_>) -> Result<(), CodecError> {
        self.started = true;
        if source.bytes().starts_with(&BYTE_ORDER_MARK) {
            let base = source.base_offset();
            return Err(framing_in(source, base, base, RecordIssueCode::InitialByteOrderMark));
        }
        if !self.options.header() || source.bytes().is_empty() {
            return Ok(());
        }
        let base = source.base_offset();
        let absolute = |index: usize| base.saturating_add(index as u64);
        let (unit, fault) = self.frame_unit(source.bytes());
        // A missing final terminator is NOT a fault here any more than it is in `emit` (RFC 4180 §2.2): a header-only
        // file without a trailing newline is still a clean zero-row stream. Every other fault inside the header stays
        // terminal exactly as it would be in a data record.
        let fault = fault.filter(|fault| fault.code != RecordIssueCode::MissingFinalTerminator);
        if let Some(fault) = fault {
            return Err(framing_in(
                source,
                absolute(unit.record_start),
                absolute(fault.offset),
                fault.code,
            ));
        }
        self.cursor = unit.physical_end;
        self.header_end = Some(absolute(unit.physical_end));
        Ok(())
    }

    /// Frames the record starting at `self.cursor`, or reports its fault.
    ///
    /// The scalar frontier within one record unit is evaluated in ASCENDING absolute offset: a blank payload is a fault
    /// at the record's own start, an oversize record is a fault at its first excess byte, a bare carriage return is a
    /// fault where it stands. A missing final terminator is framed here but is NOT a fault (RFC 4180 §2.2); `emit`
    /// filters it out. The earliest candidate wins.
    fn frame_unit(&self, bytes: &[u8]) -> (Unit, Option<Fault>) {
        let record_start = self.cursor;
        let (payload_end, physical_end, mut fault) =
            match frame_at(bytes, record_start, self.options.delimiter(), self.options.quote()) {
                Frame::Lf { lf } => (lf, lf + 1, None),
                Frame::CrLf { cr } => (cr, cr + 2, None),
                Frame::BareCr { cr } => (
                    cr,
                    cr + 1,
                    Some(Fault {
                        code: RecordIssueCode::BareCarriageReturn,
                        offset: cr,
                    }),
                ),
                Frame::Unterminated => (
                    bytes.len(),
                    bytes.len(),
                    Some(Fault {
                        code: RecordIssueCode::MissingFinalTerminator,
                        offset: bytes.len(),
                    }),
                ),
            };
        let payload = &bytes[record_start..payload_end];
        // A BLANK record (empty payload) is a VALID zero-field CSV record — it publishes as an empty array, not a
        // framing fault. CSV has no "blank line is an error" law; a blank line is a row with no fields.
        if (payload.len() as u64) > self.options.max_record_bytes() {
            let excess =
                record_start.saturating_add(usize::try_from(self.options.max_record_bytes()).unwrap_or(usize::MAX));
            let earlier = fault.as_ref().is_none_or(|candidate| excess < candidate.offset);
            if earlier {
                fault = Some(Fault {
                    code: RecordIssueCode::OversizeRecord,
                    offset: excess,
                });
            }
        }
        (
            Unit {
                record_start,
                payload_end,
                physical_end,
            },
            fault,
        )
    }

    /// Appends exactly one ordinal's outcome, advancing the cursor past it.
    ///
    /// Returns `None` when a terminal framing failure was DEFERRED so the caller can return the entries it already
    /// framed.
    fn emit<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        batch: &mut RecordBatch<'source>,
    ) -> Result<Option<u64>, CodecError> {
        let bytes = source.bytes();
        let base = source.base_offset();
        let absolute = |index: usize| base.saturating_add(index as u64);
        let (unit, fault) = self.frame_unit(bytes);
        let ordinal = self.take_ordinal()?;
        self.cursor = unit.physical_end;

        // A missing final terminator is NOT a fault: RFC 4180 §2.2 says the last record may or may not have an ending
        // line break. It falls through to the ordinary publication path below — a legal final-record shape, published
        // normally.
        let fault = fault.filter(|fault| fault.code != RecordIssueCode::MissingFinalTerminator);

        if let Some(fault) = fault {
            // Framing faults (bare CR, oversize) are terminal for the strict profile: nothing of this record is
            // published.
            let failure = framing_in(source, absolute(unit.record_start), absolute(fault.offset), fault.code);
            if batch.is_empty() {
                return Err(failure);
            }
            self.pending_failure = Some(failure);
            return Ok(None);
        }

        let payload = bytes
            .get(unit.record_start..unit.payload_end)
            .ok_or_else(|| contract("CSV payload range outside retained input"))?;
        let lease = RecordLease::try_new(absolute(unit.record_start), payload)?;
        self.records = self.records.saturating_add(1);
        batch.try_push(RecordEntry::Record(RecordItem::try_new(
            ordinal,
            absolute(unit.record_start),
            absolute(unit.physical_end),
            lease,
        )?))?;
        Ok(Some(payload.len() as u64))
    }
}

impl RecordStreamSession for CsvRecordSession {
    fn poll<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        limit: RecordBatchLimit,
        batch: &mut RecordBatch<'source>,
        run: &mut CodecRunContext<'_, '_>,
    ) -> Result<RecordPoll, CodecError> {
        if self.aborted {
            return Err(contract("CSV record stream polled after abort"));
        }
        if let Some(failure) = self.pending_failure.take() {
            return Err(failure);
        }
        if !self.started {
            self.start_stream(source)?;
        }
        let mut produced = u32::try_from(batch.len()).unwrap_or(u32::MAX);
        let mut payload_bytes = 0u64;
        loop {
            if produced >= limit.max_entries() || payload_bytes >= limit.target_bytes() {
                break;
            }
            if self.cursor >= source.bytes().len() {
                if produced == 0 {
                    return Ok(RecordPoll::End(self.completion()));
                }
                break;
            }
            let remaining = run.resources().remaining_work() as usize;
            match run.resources().admit_work_transitions(remaining.max(1))? {
                WorkAdmission::Pending => {
                    if produced == 0 {
                        return Ok(RecordPoll::Pending);
                    }
                    break;
                }
                WorkAdmission::Granted(granted) => {
                    for _ in 0..granted {
                        if produced >= limit.max_entries()
                            || payload_bytes >= limit.target_bytes()
                            || self.cursor >= source.bytes().len()
                        {
                            break;
                        }
                        let Some(bytes) = self.emit(source, batch)? else {
                            // A deferred terminal failure: publish what is already framed, then fail on the next poll.
                            return Ok(RecordPoll::Filled);
                        };
                        payload_bytes = payload_bytes.saturating_add(bytes);
                        produced = produced.saturating_add(1);
                    }
                }
            }
        }
        Ok(RecordPoll::Filled)
    }

    fn abort(&mut self, _run: &mut CodecRunContext<'_, '_>) -> Result<RecordStreamAbort, CodecError> {
        // The framer holds no payload storage and no source offer, so release is one flag: leases already handed out
        // stay valid, exactly as the record ABI's lease law promises.
        self.aborted = true;
        self.cursor = usize::MAX;
        Ok(RecordStreamAbort::Aborted)
    }

    fn header_physical_end(&mut self, source: ResolvedSource<'_>) -> Result<Option<u64>, CodecError> {
        if !self.options.header() {
            return Ok(None);
        }
        if !self.started {
            self.start_stream(source)?;
        }
        Ok(self.header_end)
    }
}
