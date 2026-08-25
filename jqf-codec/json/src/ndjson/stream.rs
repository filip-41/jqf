//! Serial NDJSON record framing over contiguous retained input.
//!
//! The session owns physical framing and NOTHING else. It never parses JSON, never builds a document, never copies a
//! payload byte, and never allocates per record: one poll walks the retained input, hands out
//! [`jqf_codec_core::RecordLease`] borrows of the payload ranges it proves, and appends them to the caller's reused
//! batch. Payload grammar is the strict JSON crate's, reached later by narrowing the same retained source to the range
//! this framer named.

use jqf_codec_core::{
    CodecError, CodecRunContext, RecordBatch, RecordBatchLimit, RecordCompletion, RecordEntry, RecordIssue,
    RecordIssueCode, RecordIssueSeverity, RecordItem, RecordLease, RecordOrdinal, RecordPoll, RecordStreamAbort,
    RecordStreamSession,
};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::ResolvedSource;

use super::boundary::{Frame, frame_at};
use super::error::{contract, framing_in};
use super::{NdjsonDecodeOptions, NdjsonProfile};

const BYTE_ORDER_MARK: [u8; 3] = [0xef, 0xbb, 0xbf];

/// One framed record unit, before its profile decides what to do with it.
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

/// Whether a framed payload holds no value at all: it is empty, or nothing but padding.
///
/// Strict JSON's insignificant-whitespace set is space, tab, line feed and carriage return, but only SPACE and TAB pad
/// a RECORD payload: the framer owns LF and CR as physical bytes, so a raw one inside a payload is a framing error for
/// the framer to classify, never whitespace swallowed here. A bare CR would otherwise disappear into a valid record.
///
/// A blank payload is a framing outcome with its own ordinal and its own profile law, never a JSON "unexpected end of
/// input" — which is why the question is answered before any decode.
fn payload_is_blank(payload: &[u8]) -> bool {
    payload.iter().all(|byte| matches!(byte, b' ' | b'\t'))
}

/// Serial NDJSON framing state.
///
/// Every field is a scalar: the session retains no payload storage, no parser, and no per-record buffer, which is
/// precisely what the autopsy says a record-route framer must not do (`.docs-intenal/perf-record-route-parallelism.md`
/// §7.1 priced a record-private decode ladder at 1.92x the adjacent-value path).
pub(crate) struct NdjsonRecordSession {
    profile: NdjsonProfile,
    options: NdjsonDecodeOptions,
    cursor: usize,
    next_ordinal: u64,
    records: u64,
    issues: u64,
    started: bool,
    aborted: bool,
    /// A terminal framing failure held back until the batch drains.
    ///
    /// The strict profile's earlier records must stay published and observable — the same law the adjacent-value
    /// drive keeps when a later text fails to parse. Since one poll frames MANY records, a fault discovered while
    /// filling a batch must not discard the records already in it: the batch is returned, and the fault is raised on
    /// the next poll, before any further entry.
    pending_failure: Option<CodecError>,
}

impl NdjsonRecordSession {
    pub(crate) const fn new(profile: NdjsonProfile, options: NdjsonDecodeOptions) -> Self {
        Self {
            profile,
            options,
            cursor: 0,
            next_ordinal: 0,
            records: 0,
            issues: 0,
            started: false,
            aborted: false,
            pending_failure: None,
        }
    }

    const fn completion(&self) -> RecordCompletion {
        RecordCompletion::new(self.records, self.issues)
    }

    fn take_ordinal(&mut self) -> Result<RecordOrdinal, CodecError> {
        let ordinal = RecordOrdinal::new(self.next_ordinal);
        self.next_ordinal = ordinal
            .next()
            .ok_or_else(|| contract("NDJSON record ordinal exhausted"))?
            .get();
        Ok(ordinal)
    }

    /// Consumes a source-start byte-order mark under the profile's law.
    ///
    /// A BOM is stream PREFIX metadata: it consumes no ordinal. The recovering profile reports it as an advisory
    /// carrying the ordinal of the record it precedes, so a consumer merging by ordinal still sees it before that
    /// record (records stay dense from zero — the fuzz lifecycle pins it). Later BOM bytes are ordinary payload bytes
    /// strict JSON rejects.
    fn start_stream<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        batch: &mut RecordBatch<'source>,
        _resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        self.started = true;
        if !source.bytes().starts_with(&BYTE_ORDER_MARK) {
            return Ok(());
        }
        let base = source.base_offset();
        if self.profile == NdjsonProfile::Strict {
            return Err(framing_in(source, base, base, RecordIssueCode::InitialByteOrderMark));
        }
        self.cursor = BYTE_ORDER_MARK.len();
        self.issues = self.issues.saturating_add(1);
        // The push's Result is this function's tail expression ON PURPOSE: a refused advisory propagates as an Err,
        // never a silent drop.
        batch.try_push(RecordEntry::Issue(RecordIssue::new(
            RecordOrdinal::new(self.next_ordinal),
            RecordIssueSeverity::Advisory,
            RecordIssueCode::InitialByteOrderMark,
            base,
        )))
    }

    /// Frames the record starting at `self.cursor`, or reports its fault.
    ///
    /// The scalar frontier within one record unit is evaluated in ASCENDING absolute offset: a blank payload is a fault
    /// at the record's own start, an oversize record is a fault at its first excess byte, a bare carriage return is a
    /// fault where it stands, and a COMPLETE final record's missing terminator is not a fault at all under EITHER
    /// profile — it is published as a plain record with no issue (JSON Lines permits the absent final separator;
    /// pinned by `strict_accepts_a_complete_final_value_without_a_terminator`). The earliest candidate wins, which is
    /// why the order of these tests is the order of their offsets and not the order they were convenient to write. At a
    /// TIE the blank classification wins: a blank payload and a bare carriage return at the record's first byte both
    /// sit at offset 0, and the blank check runs after the framing scan, superseding the bare-CR fault. The oversize
    /// bound joins the same earliest-wins law with a STRICT comparison, so a tie (`excess == candidate.offset`) leaves
    /// the carried fault standing and only a strictly earlier excess replaces it.
    fn frame_unit(&self, bytes: &[u8]) -> (Unit, Option<Fault>) {
        let record_start = self.cursor;
        let (payload_end, physical_end, mut fault) = match frame_at(bytes, record_start) {
            Frame::Lf { lf } => (lf, lf + 1, None),
            Frame::CrLf { cr } => (cr, cr + 2, None),
            Frame::BareCr { cr } => {
                // The record ends where recovery can resume: after the next physical line feed, or at end of input.
                let resume = bytes[cr..]
                    .iter()
                    .position(|&byte| byte == b'\n')
                    .map_or(bytes.len(), |index| cr + index + 1);
                (
                    cr,
                    resume,
                    Some(Fault {
                        code: RecordIssueCode::BareCarriageReturn,
                        offset: cr,
                    }),
                )
            }
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
        // A blank payload's fault sits at the record's first byte, so it precedes every other candidate this unit can
        // carry.
        if payload_is_blank(payload) {
            fault = Some(Fault {
                code: RecordIssueCode::BlankRecord,
                offset: record_start,
            });
        } else if (payload.len() as u64) > self.options.max_record_bytes() {
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

        if let Some(fault) = fault {
            // BOTH profiles accept a COMPLETE final value with no terminator: the missing terminator is a legal
            // final-record shape, not a fault (JSON Lines permits the final separator to be absent, every incumbent
            // reader accepts it, and a truncated tail still fails as a malformed PAYLOAD rather than passing silently
            // — so rejecting it here only refused streams that are valid).
            let complete_final_record = fault.code == RecordIssueCode::MissingFinalTerminator;
            if !complete_final_record {
                if self.profile == NdjsonProfile::Strict {
                    let failure = framing_in(source, absolute(unit.record_start), absolute(fault.offset), fault.code);
                    if batch.is_empty() {
                        return Err(failure);
                    }
                    self.pending_failure = Some(failure);
                    return Ok(None);
                }
                // The severity split decides EXIT CLASSES under recovering: one Error-severity issue anywhere forces
                // the request's failure class through `RecordSequenceReport::error_issues`, while advisories leave it
                // to the program's own last-record result. A blank record is the ONE fault jq itself sails past — a
                // blank line is legal whitespace between adjacent values — so recovery's job there is to skip, not to
                // fail; bare CR and oversize payloads are real framing damage and stay Error-severity.
                let severity = if fault.code == RecordIssueCode::BlankRecord {
                    RecordIssueSeverity::Advisory
                } else {
                    RecordIssueSeverity::Error
                };
                self.issues = self.issues.saturating_add(1);
                batch.try_push(RecordEntry::Issue(RecordIssue::new(
                    ordinal,
                    severity,
                    fault.code,
                    absolute(fault.offset),
                )))?;
                return Ok(Some(0));
            }
        }

        let payload = bytes
            .get(unit.record_start..unit.payload_end)
            .ok_or_else(|| contract("NDJSON payload range outside retained input"))?;
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

impl RecordStreamSession for NdjsonRecordSession {
    fn poll<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        limit: RecordBatchLimit,
        batch: &mut RecordBatch<'source>,
        run: &mut CodecRunContext<'_, '_>,
    ) -> Result<RecordPoll, CodecError> {
        if self.aborted {
            return Err(contract("NDJSON record stream polled after abort"));
        }
        if let Some(failure) = self.pending_failure.take() {
            self.aborted = true;
            return Err(failure);
        }
        if !self.started {
            self.start_stream(source, batch, run.resources())?;
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
                    let mut used = 0usize;
                    for _ in 0..granted {
                        if produced >= limit.max_entries()
                            || payload_bytes >= limit.target_bytes()
                            || self.cursor >= source.bytes().len()
                        {
                            break;
                        }
                        let Some(bytes) = self.emit(source, batch)? else {
                            if used < granted {
                                #[expect(
                                    clippy::cast_possible_truncation,
                                    reason = "the grant never exceeds remaining credits, a u32"
                                )]
                                let unused = (granted - used) as u32;
                                run.resources().refund_work(unused);
                            }
                            // A deferred terminal failure: publish what is already framed, then fail on the next poll.
                            return Ok(RecordPoll::Filled);
                        };
                        payload_bytes = payload_bytes.saturating_add(bytes);
                        produced = produced.saturating_add(1);
                        used += 1;
                    }
                    if used < granted {
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "the grant never exceeds remaining credits, a u32"
                        )]
                        let unused = (granted - used) as u32;
                        run.resources().refund_work(unused);
                    }
                }
            }
        }
        // Issues and records share ONE entry budget — both carry ordinals, so a batch that admitted k issues holds
        // N-k records. The ceiling counts entries, never records alone.
        debug_assert!(batch.len() <= usize::try_from(limit.max_entries()).unwrap_or(usize::MAX));
        Ok(RecordPoll::Filled)
    }

    fn abort(&mut self, _run: &mut CodecRunContext<'_, '_>) -> Result<RecordStreamAbort, CodecError> {
        // The framer holds no payload storage and no source offer, so release is one flag: leases already handed out
        // stay valid, exactly as the record ABI's lease law promises.
        self.aborted = true;
        self.cursor = usize::MAX;
        Ok(RecordStreamAbort::Aborted)
    }
}

#[cfg(test)]
mod tests {
    use super::payload_is_blank;

    #[test]
    fn only_space_and_tab_pad_a_payload() {
        assert!(payload_is_blank(b""));
        assert!(payload_is_blank(b"   "));
        assert!(payload_is_blank(b"\t \t"));
        // A framer owns LF and CR: a payload holding one is a value candidate it must classify as a framing error,
        // never a blank record.
        assert!(!payload_is_blank(b"\r"));
        assert!(!payload_is_blank(b"\n"));
        assert!(!payload_is_blank(b"  {\"a\":1} \t"));
    }
}
