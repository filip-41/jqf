//! Serial json-seq (RFC 7464) record framing over contiguous retained input.
//!
//! The session owns PHYSICAL framing and NOTHING else. It never parses JSON, never builds a document, never copies a
//! payload byte, and never allocates per record: one poll walks the retained input, hands out
//! [`jqf_codec_core::RecordLease`] borrows of the payload ranges it proves, and appends them to the caller's reused
//! batch. Payload grammar is the strict JSON crate's, reached later by narrowing the same retained source to the range
//! this framer named.
//!
//! ## The framing law (RFC 7464)
//!
//! A record unit is `RS` (0x1E) followed by bytes up to the next raw RS. A raw RS ALWAYS establishes the next boundary,
//! even mid-string or mid-container. Consecutive RS bytes before a later non-RS byte coalesce as the `1*RS` prefix of
//! that next possible-JSON: they consume no ordinal and create no empty semantic document. An input ending in RS after
//! its last complete item — an RS-only input, or a trailing RS run — leaves an unterminated zero-byte
//! possible-JSON: a STRICT framing failure, never zero-item success; the recovering profile discards it silently
//! (reference parity). An input that never contains an RS is UNFRAMED: strict fails it, the recovering profile reports
//! one advisory issue (the unfinished-at-EOF advisory) and ends. Empty input is a valid zero-item text-seq under both
//! profiles.
//!
//! RFC 7464 §2.4: a top-level number, `true`, `false`, or `null` is not self-delimiting and needs at least one
//! JSON-whitespace byte before the next RS or EOF; without it the possible-JSON is
//! [`crate::seq::boundary::UnitClass`]'s truncated scalar, a strict failure or a recovering issue, never a published
//! item. JSON whitespace (space, tab, LF, CR) is ordinary payload here — the record's physical boundary is the RS
//! alone, so LF and CR may pad a possible-JSON exactly as the json-seq parser accepts them.

use jqf_codec_core::byte_scan::prefix_len;
use jqf_codec_core::{
    CodecError, CodecRunContext, RecordBatch, RecordBatchLimit, RecordCompletion, RecordEntry, RecordIssue,
    RecordIssueCode, RecordIssueSeverity, RecordItem, RecordLease, RecordOrdinal, RecordPoll, RecordStreamAbort,
    RecordStreamSession, RecordTerminator,
};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::ResolvedSource;

use crate::byte_scan::Rs;

use super::boundary::{UnitClass, classify};
use super::error::{contract, framing_in};
use super::{JsonSeqDecodeOptions, JsonSeqProfile};

/// Index of the first raw RS (0x1E) in `bytes`, or `None`. A raw RS ALWAYS establishes the next boundary, so the shared
/// stop-set scan answers this directly.
fn find_rs(bytes: &[u8]) -> Option<usize> {
    let admitted = prefix_len::<Rs>(bytes);
    (admitted < bytes.len()).then_some(admitted)
}

/// One framed unit, before its profile decides what to do with it.
struct Unit {
    record_start: usize,
    payload_end: usize,
    physical_end: usize,
    terminator: RecordTerminator,
}

/// What one unit's classification proved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Class {
    /// The unit coalesces into the next text's RS prefix: no entry, no ordinal.
    Coalesced,
    /// The unit is a record whose payload the strict JSON ladder decodes.
    Record,
    /// The unit is the FINAL unterminated unit holding a complete value.
    FinalRecord,
    /// The unit is the FINAL unterminated whitespace-only tail: a valid empty possible-JSON that ends the stream. Only
    /// the LAST unit can be this: a whitespace-only unit with an RS after it is `Coalesced`, and that RS is the
    /// unterminated tail strict rejects (`"\x1e "` clean, `"\x1e \x1e"` not).
    FinalEmpty,
    /// The unit is a framing fault at `offset`.
    Fault { code: RecordIssueCode, offset: usize },
}

/// One unit's outcome from `emit`.
enum EmitOutcome {
    /// An entry was pushed (its payload bytes); keep going.
    Continued {
        /// Payload bytes the entry contributed toward the poll target.
        payload_bytes: u64,
    },
    /// A coalesced empty unit was skipped: no entry, no ordinal, keep going. Distinct from [`Self::Continued`] so a
    /// poll of nothing but coalesced units cannot fill the batch limit with entries that do not exist.
    Skipped,
    /// A terminal failure is pending; stop this poll so the fault surfaces before any further entry.
    Deferred,
    /// The stream reached its terminal state after this unit; the next poll returns `RecordPoll::End`.
    Finished,
}

/// Serial json-seq framing state.
///
/// Every field is a scalar: the session retains no payload storage, no parser, and no per-record buffer, which is
/// precisely what a record route must not do.
pub(crate) struct JsonSeqRecordSession {
    profile: JsonSeqProfile,
    options: JsonSeqDecodeOptions,
    cursor: usize,
    next_ordinal: u64,
    records: u64,
    issues: u64,
    started: bool,
    aborted: bool,
    /// Whether the last consumed unit was terminated by an RS. A cursor that then reaches end of input is the
    /// unterminated zero-byte tail.
    ///
    /// The whitespace-only asymmetry: `"\x1e "` ends cleanly as `FinalEmpty`, but `"\x1e \x1e"`'s middle unit is
    /// `Coalesced`, so its trailing RS is the unterminated tail strict rejects — a trailing RS is never clean.
    ended_at_rs: bool,
    /// A terminal framing failure held back until the batch drains.
    ///
    /// The strict profile's earlier records must stay published and observable — the same law the adjacent-value
    /// drive keeps when a later text fails to parse. Since one poll frames MANY records, a fault discovered while
    /// filling a batch must not discard the records already in it: the batch is returned, and the fault is raised on
    /// the next poll, before any further entry.
    pending_failure: Option<CodecError>,
}

impl JsonSeqRecordSession {
    pub(crate) const fn new(profile: JsonSeqProfile, options: JsonSeqDecodeOptions) -> Self {
        Self {
            profile,
            options,
            cursor: 0,
            next_ordinal: 0,
            records: 0,
            issues: 0,
            started: false,
            aborted: false,
            ended_at_rs: false,
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
            .ok_or_else(|| contract("json-seq record ordinal exhausted"))?
            .get();
        Ok(ordinal)
    }

    /// Consumes the leading RS run and reports the unframed class at EOF.
    ///
    /// Empty input is a valid zero-item text-seq under BOTH profiles (the RFC grammar permits zero elements); a
    /// non-empty input that never contains an RS is unframed — strict raises, the recovering profile reports one
    /// advisory issue (the unfinished-at-EOF advisory) and ends.
    fn start_stream<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        batch: &mut RecordBatch<'source>,
        _resources: &ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        self.started = true;
        let bytes = source.bytes();
        if bytes.is_empty() {
            return Ok(());
        }
        let base = source.base_offset();
        // The scan starts at byte 0, and EVERYTHING before the first RS is skipped unexamined — under BOTH profiles,
        // strict included. A raw RS always boundaries, so the pre-first-RS bytes can never join a unit, the framer owns
        // no grammar to judge them by, and the reference's seq parser behaves the same: leading junk is silently
        // discarded, never faulted.
        let Some(relative) = find_rs(bytes) else {
            if self.profile == JsonSeqProfile::Strict {
                return Err(framing_in(source, base, base, RecordIssueCode::UnframedInput));
            }
            // The unframed class needs no flag: parking the cursor at end of input IS the carrier — poll's `cursor >=
            // len && !ended_at_rs` arm ends the stream, and the advisory issue is already pushed.
            self.cursor = bytes.len();
            self.issues = self.issues.saturating_add(1);
            batch.try_push(RecordEntry::Issue(RecordIssue::new(
                RecordOrdinal::new(self.next_ordinal),
                RecordIssueSeverity::Advisory,
                RecordIssueCode::UnframedInput,
                base,
            )))?;
            return Ok(());
        };
        self.cursor = relative + 1;
        self.ended_at_rs = true;
        Ok(())
    }

    /// Classifies the unit starting at `self.cursor`, advancing nothing.
    ///
    /// The scalar frontier within one unit is evaluated in ASCENDING absolute offset, exactly as NDJSON's: the §2.4
    /// truncation sits at the RS/EOF that ends the unit, the oversize fault at its first excess byte, and the earliest
    /// candidate wins.
    fn classify_unit(&self, bytes: &[u8]) -> (Unit, Class) {
        let record_start = self.cursor;
        let relative = find_rs(&bytes[record_start..]);
        if let Some(offset) = relative {
            let rs = record_start + offset;
            let unit = &bytes[record_start..rs];
            let class = match classify(unit) {
                UnitClass::Empty => Class::Coalesced,
                UnitClass::Complete => Class::Record,
                // Mid-stream §2.4 canary drop: `<RS>true<RS>` faults the unit (the recovering profile publishes the
                // value) — RFC-strictness deviation beyond AGENTS.md's EOF-corner divergence note.
                UnitClass::TruncatedScalar => Class::Fault {
                    code: RecordIssueCode::TruncatedTopLevelScalar,
                    offset: rs,
                },
            };
            (
                Unit {
                    record_start,
                    payload_end: rs,
                    physical_end: rs + 1,
                    terminator: RecordTerminator::Rs,
                },
                class,
            )
        } else {
            // The final unterminated unit runs to end of input.
            let end = bytes.len();
            let unit = &bytes[record_start..];
            let class = match classify(unit) {
                UnitClass::Empty => Class::FinalEmpty,
                UnitClass::Complete => Class::FinalRecord,
                UnitClass::TruncatedScalar => Class::Fault {
                    code: RecordIssueCode::TruncatedTopLevelScalar,
                    offset: end,
                },
            };
            (
                Unit {
                    record_start,
                    payload_end: end,
                    physical_end: end,
                    terminator: RecordTerminator::None,
                },
                class,
            )
        }
    }

    /// The oversize fault for a unit, if its payload exceeds the ceiling.
    ///
    /// Sits at the first excess byte; `None` when the ceiling is not exceeded.
    fn oversize_fault(&self, unit: &Unit) -> Option<(RecordIssueCode, usize)> {
        let payload_len = unit.payload_end.saturating_sub(unit.record_start) as u64;
        if payload_len <= self.options.max_record_bytes() {
            return None;
        }
        let excess = unit
            .record_start
            .saturating_add(usize::try_from(self.options.max_record_bytes()).unwrap_or(usize::MAX));
        Some((RecordIssueCode::OversizeRecord, excess))
    }

    /// Appends exactly one ordinal's outcome, advancing the cursor past it.
    #[expect(
        clippy::too_many_lines,
        reason = "one unit's outcome is a single sequence: tail states, classification, the oversize frontier, and the record/issue/final branches; splitting it would thread the batch and cursor through helpers for no reader"
    )]
    fn emit<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        batch: &mut RecordBatch<'source>,
    ) -> Result<EmitOutcome, CodecError> {
        let bytes = source.bytes();
        let base = source.base_offset();
        let absolute = |index: usize| base.saturating_add(index as u64);
        // The tail states: a cursor at end of input is either the clean end or the unterminated zero-byte item a
        // trailing RS run began.
        if self.cursor >= bytes.len() {
            if self.ended_at_rs && self.profile == JsonSeqProfile::Strict {
                let failure = super::error::trailing_rs_in(source, base.saturating_add(bytes.len() as u64));
                if batch.is_empty() {
                    return Err(failure);
                }
                self.pending_failure = Some(failure);
                return Ok(EmitOutcome::Deferred);
            }
            // The recovering profile discards the unterminated zero-byte tail SILENTLY (reference parity: a trailing RS
            // warns nothing) and ends.
            self.ended_at_rs = false;
            return Ok(EmitOutcome::Finished);
        }
        let (unit, class) = self.classify_unit(bytes);
        // A coalesced empty unit is the `1*RS` prefix of the next text: it consumes NO ordinal and creates no entry.
        // Handle it before the ordinal is taken, so a coalesced run never leaves a gap.
        if class == Class::Coalesced {
            self.cursor = unit.physical_end;
            self.ended_at_rs = true;
            return Ok(EmitOutcome::Skipped);
        }
        // The oversize fault joins the classification's own fault, earliest absolute offset winning (it cannot apply to
        // a coalesced empty unit, which is gone above).
        let class = match class {
            Class::Fault { code, offset } => match self.oversize_fault(&unit) {
                Some((oversize_code, oversize_offset)) if oversize_offset < offset => Class::Fault {
                    code: oversize_code,
                    offset: oversize_offset,
                },
                _ => Class::Fault { code, offset },
            },
            Class::Record | Class::FinalRecord | Class::FinalEmpty => {
                if let Some((code, offset)) = self.oversize_fault(&unit) {
                    Class::Fault { code, offset }
                } else {
                    class
                }
            }
            Class::Coalesced => unreachable!("handled above"),
        };
        let ordinal = self.take_ordinal()?;
        match class {
            Class::Coalesced => unreachable!("handled before the ordinal was taken"),
            Class::Record => {
                self.cursor = unit.physical_end;
                self.ended_at_rs = unit.terminator == RecordTerminator::Rs;
                let payload = bytes
                    .get(unit.record_start..unit.payload_end)
                    .ok_or_else(|| contract("json-seq payload range outside retained input"))?;
                let lease = RecordLease::try_new(absolute(unit.record_start), payload)?;
                self.records = self.records.saturating_add(1);
                batch.try_push(RecordEntry::Record(RecordItem::try_new(
                    ordinal,
                    absolute(unit.record_start),
                    absolute(unit.physical_end),
                    lease,
                )?))?;
                Ok(EmitOutcome::Continued {
                    payload_bytes: payload.len() as u64,
                })
            }
            Class::Fault { code, offset } => {
                self.cursor = unit.physical_end;
                self.ended_at_rs = unit.terminator == RecordTerminator::Rs;
                if self.profile == JsonSeqProfile::Strict {
                    let failure = framing_in(source, absolute(unit.record_start), absolute(offset), code);
                    if batch.is_empty() {
                        return Err(failure);
                    }
                    self.pending_failure = Some(failure);
                    return Ok(EmitOutcome::Deferred);
                }
                // The recovering profile reports every framing fault as an ADVISORY issue and continues after the
                // unit's physical end: The reference's `--seq` parse errors never affect the exit class.
                self.issues = self.issues.saturating_add(1);
                batch.try_push(RecordEntry::Issue(RecordIssue::new(
                    ordinal,
                    RecordIssueSeverity::Advisory,
                    code,
                    absolute(offset),
                )))?;
                Ok(EmitOutcome::Continued { payload_bytes: 0 })
            }
            Class::FinalEmpty => {
                // A final unterminated whitespace-only unit is a valid empty possible-JSON that ends the stream (the
                // recovering profile accepts it silently).
                self.cursor = unit.physical_end;
                self.ended_at_rs = false;
                Ok(EmitOutcome::Finished)
            }
            Class::FinalRecord => {
                // The final unterminated unit holding a complete value: the last item needs no terminating RS (the RFC
                // grammar, and the reference accepts it silently).
                self.cursor = unit.physical_end;
                self.ended_at_rs = false;
                let payload = bytes
                    .get(unit.record_start..unit.payload_end)
                    .ok_or_else(|| contract("json-seq payload range outside retained input"))?;
                let lease = RecordLease::try_new(absolute(unit.record_start), payload)?;
                self.records = self.records.saturating_add(1);
                batch.try_push(RecordEntry::Record(RecordItem::try_new(
                    ordinal,
                    absolute(unit.record_start),
                    absolute(unit.physical_end),
                    lease,
                )?))?;
                Ok(EmitOutcome::Finished)
            }
        }
    }
}

impl RecordStreamSession for JsonSeqRecordSession {
    fn poll<'source>(
        &mut self,
        source: ResolvedSource<'source>,
        limit: RecordBatchLimit,
        batch: &mut RecordBatch<'source>,
        run: &mut CodecRunContext<'_, '_>,
    ) -> Result<RecordPoll, CodecError> {
        if self.aborted {
            return Err(contract("json-seq record stream polled after abort"));
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
            if self.cursor >= source.bytes().len() && !self.ended_at_rs {
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
                        if produced >= limit.max_entries() || payload_bytes >= limit.target_bytes() {
                            break;
                        }
                        used += 1;
                        let outcome = self.emit(source, batch)?;
                        match outcome {
                            EmitOutcome::Continued { payload_bytes: added } => {
                                payload_bytes = payload_bytes.saturating_add(added);
                                produced = produced.saturating_add(1);
                            }
                            EmitOutcome::Skipped => {}
                            EmitOutcome::Deferred | EmitOutcome::Finished => {
                                if used < granted {
                                    #[expect(
                                        clippy::cast_possible_truncation,
                                        reason = "the grant never exceeds remaining credits, a u32"
                                    )]
                                    let unused = (granted - used) as u32;
                                    run.resources().refund_work(unused);
                                }
                                if matches!(outcome, EmitOutcome::Finished) && batch.is_empty() {
                                    return Ok(RecordPoll::End(self.completion()));
                                }
                                return Ok(RecordPoll::Filled);
                            }
                        }
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
    use alloc::{vec, vec::Vec};

    use jqf_codec_core::{
        CodecRunContext, DiagnosticPolicy, RecordBatch, RecordBatchLimit, RecordEntry, RecordPoll, RouteSlot,
    };
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    use super::find_rs;
    use crate::seq::provider::create_record_provider;
    use crate::seq::{JsonSeqDecodeOptions, JsonSeqProfile};
    use crate::test_support;

    /// Drives one profile to completion, returning the payloads, the issue count, and whether the stream ended cleanly.
    fn drive(bytes: &[u8], profile: JsonSeqProfile) -> (Vec<Vec<u8>>, u32, bool) {
        let mut resources = test_support::resources();
        let source = ResolvedSource::new(
            SourceRef::new(SourceId::new(1), SourceKind::Input),
            "test.json-seq",
            bytes,
            0,
        );
        let options = JsonSeqDecodeOptions::try_new(None, 1 << 20).expect("ceiling");
        let mut provider = create_record_provider(
            source,
            profile,
            options,
            DiagnosticPolicy::ErrorsOnly,
            profile.validation(),
            &mut resources,
        )
        .expect("provider");
        let mut stream = provider
            .open_record_route(RouteSlot::new(0), &mut resources)
            .expect("route");
        let limit = RecordBatchLimit::new(64, 1 << 20).expect("limit");
        let mut batch = RecordBatch::new();
        let mut payloads = Vec::new();
        let mut issues = 0u32;
        let mut completed = false;
        loop {
            batch.clear();
            let mut run = CodecRunContext::new(&mut resources);
            match stream.poll(limit, &mut batch, &mut run) {
                Ok(RecordPoll::End(_)) => {
                    completed = true;
                    break;
                }
                Ok(RecordPoll::Pending) => {
                    resources.try_begin_next_cooperative_entry(4_096).expect("resume");
                    continue;
                }
                Ok(RecordPoll::Filled) => {}
                Err(_) => break,
            }
            for entry in batch.entries() {
                match entry {
                    RecordEntry::Record(record) => payloads.push(record.lease().payload().to_vec()),
                    RecordEntry::Issue(_) => issues += 1,
                }
            }
        }
        (payloads, issues, completed)
    }

    /// The RS byte: ASCII record separator, RFC 7464 §2.1.
    const RS: u8 = 0x1e;

    #[test]
    fn the_word_scan_finds_an_rs_at_every_byte_alignment() {
        // The NDJSON boundary scan's own alignment-exhaustive check, ported for the RS byte: the word-at-a-time loop
        // must never skip an RS a naive scan would find, at any offset within a word.
        let mut input = [b'x'; 96];
        for position in 0..input.len() {
            input = [b'x'; 96];
            input[position] = RS;
            assert_eq!(find_rs(&input), Some(position), "RS at byte {position}");
        }
        assert_eq!(find_rs(b"no rs here"), None);
        assert_eq!(find_rs(b""), None);
    }

    #[test]
    fn a_stream_with_rs_boundaries_at_every_word_alignment_is_byte_identical() {
        // Quoted payloads of widths 1..=16 put the RS boundaries at every byte offset within a usize word; the word
        // scan must frame each unit byte-identically whatever the alignment.
        let mut input = Vec::new();
        let mut expected = Vec::new();
        for width in 1..=16 {
            let mut payload = vec![b'"'];
            payload.extend(core::iter::repeat_n(b'a', width));
            payload.push(b'"');
            input.push(RS);
            input.extend_from_slice(&payload);
            expected.push(payload);
        }
        let (payloads, issues, completed) = drive(&input, JsonSeqProfile::Strict);
        assert!(completed);
        assert_eq!(issues, 0);
        assert_eq!(payloads, expected);
    }
}
