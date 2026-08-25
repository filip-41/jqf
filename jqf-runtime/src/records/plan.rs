//! The record route's observed width policy and its record-aligned partition.
//!
//! The plan vocabulary itself is `crate::parallel::plan`'s; what lives here is the evidence: constants observed on the
//! NDJSON lane, and the one cut rule the NDJSON framer makes exact.

use crate::parallel::{Morsel, ParallelPlan, PlanDecision, WidthPolicy, WorkerRequest};

/// Below this many input bytes, `auto` stays on the serial path.
///
/// observed, not chosen, and re-observed when the CLI's allocator changed. Sweeping the 200 k lane family at 31 reps
/// per size brackets the crossover between 143 380 bytes (parallel within noise of serial) and 153 780 bytes (parallel
/// 2.4-4.5 % ahead); the pin sits at 1.76x that midpoint. Under it the lane's fixed cost — grant reservation, thread
/// spawn, ordinal ring, one requirement lowering per morsel (the program itself is borrowed, never recompiled) —
/// exceeds what extra cores win back, so `--parallel` on a small input is never slower than the serial path, and this
/// threshold is how that promise is kept. The margin is deliberately one-sided: the crossover moves with record shape
/// and machine load, and the only cost of the margin is a missed win between 149 KB and 256 KiB.
pub(crate) const AUTO_BREAK_EVEN_BYTES: u64 = 256 * 1024;

/// The narrowest width `auto` will ever choose above the break-even point.
///
/// observed. the pin is 4 because two and three workers were SLOWER than serial: macOS `libmalloc`'s multithreaded path
/// taxed the drive's per-record document construction about 2x per thread. The CLI no longer links `libmalloc`, and the
/// tax is gone — two workers now run the 200 k lane in 0.570x serial for the SAME total CPU time as serial, which makes
/// 2 the narrowest width that is still a strict win.
const AUTO_MIN_WORKERS: usize = 2;

/// The smallest morsel the lane will cut, and so `auto`'s unit of width.
///
/// A morsel is a record RANGE sized by BYTES. Below this the per-morsel cost — one thread spawn, one grant, one program
/// compile, one provider — stops amortizing over the records inside it.
///
/// Because this floor binds on every input the break-even admits, it is also what decides how many workers can be
/// USEFULLY employed: an input yields `ceil(input / MIN_MORSEL_BYTES)` morsels at the floor, and a worker beyond that
/// count draws nothing. `auto` therefore counts morsels rather than scaling by a separate per-worker byte budget — a
/// budget that observed wrong at every size between the break-even and 3 MB once two workers became a win.
const MIN_MORSEL_BYTES: u64 = 128 * 1024;

/// The largest morsel the lane will cut.
///
/// The worker envelope is sized from the morsel, so this bounds one worker's memory: a wide request over a huge input
/// divides into more morsels rather than into fatter ones.
const MAX_MORSEL_BYTES: u64 = 4 * 1024 * 1024;

/// Morsels per worker the planner aims for.
///
/// More than one, so a worker that draws a slow morsel does not strand the tail; few enough that dispatch and
/// per-morsel allocation stay amortized. observed over {2, 4, 8, 16} on the 200 k lane: 2 is the fastest by 0.5–5.6 %,
/// and 4 is taken anyway because two extra morsels per worker are what keep an uneven tail from stranding a core.
const MORSELS_PER_WORKER: u64 = 4;

/// The record route's pinned width policy.
pub(crate) const RECORD_WIDTH_POLICY: WidthPolicy = WidthPolicy {
    break_even_bytes: AUTO_BREAK_EVEN_BYTES,
    min_workers: AUTO_MIN_WORKERS,
    min_morsel_bytes: MIN_MORSEL_BYTES,
    max_morsel_bytes: MAX_MORSEL_BYTES,
    morsels_per_worker: MORSELS_PER_WORKER,
};

/// Plans one record-parallel request under the record route's own policy.
#[must_use]
pub fn plan_request(request: WorkerRequest, input_bytes: u64, ineligible: Option<PlanDecision>) -> ParallelPlan {
    crate::parallel::plan_request(request, input_bytes, ineligible, RECORD_WIDTH_POLICY)
}

/// Cuts `input` into record-aligned morsels of about `target_bytes` each.
///
/// # Why cutting at a line feed is exact (NDJSON)
///
/// The NDJSON framer ends every record at the FIRST line feed at or after its start, so in any stream it frames without
/// fault every line feed is a record terminator and no record spans one. A cut immediately after a line feed therefore
/// splits the stream exactly where the framer already splits it, and each morsel is itself a well-formed record stream.
///
/// A stream where that is NOT true — a payload holding a raw line feed, a bare carriage return, a blank line, a
/// mid-stream byte-order mark — makes the affected morsel report a diagnostic or fail, which yields the whole request
/// to serial. The alignment is a fast-path assumption whose violation is detected, never assumed away.
#[must_use]
pub(crate) fn partition_morsels(input: &[u8], target_bytes: u64) -> Vec<Morsel> {
    partition_by(input, target_bytes)
}

/// Cuts CSV input into record-aligned morsels of about `target_bytes` each.
///
/// CSV records may contain RAW line feeds inside quoted fields, so a cut at a line feed is only exact when the scan is
/// outside a quoted field. One monotone walk carries the doubled-quote-aware state from the input start — every morsel
/// starts right after an out-of-quote line feed, so the carried state is the walk-from-the-morsel-start state — and an
/// in-quote candidate RESUMES rather than collapses: the cut lands at the NEXT out-of-quote line feed, so a document of
/// multiline-quoted records divides like any other stream instead of running every morsel after the first quoted field
/// to end-of-input (which the relay's oversized-morsel guard converts into a full serial redrive).
///
/// # Soundness
///
/// The walk is DELIBERATELY looser than the CSV framer's quote law: it toggles on every non-doubled quote, with no
/// field-start tracking, where the framer opens a quote only at a field start and treats a lone mid-field quote as
/// non-structural. The two machines can therefore disagree on quote state from the first malformed lone quote onward.
/// That divergence is safe for one reason: any prefix where they disagree implies a malformed field the payload decode
/// rejects, so the affected morsel faults its ordinary `drive_range` and the whole request yields to serial
/// (`Relay::YieldToSerial`) — the scanner is a proposer, never an oracle, and on well-formed RFC 4180 input its cuts
/// coincide with the framer's record boundaries. An unterminated quote finds no further boundary, so that final morsel
/// runs to end-of-input and the relay's oversized guard serves it by the same fallback — the backstop this function
/// keeps honest.
#[must_use]
pub(crate) fn partition_csv_morsels(input: &[u8], target_bytes: u64) -> Vec<Morsel> {
    let target = usize::try_from(target_bytes).unwrap_or(usize::MAX).max(1);
    let first_target = FIRST_MORSEL_TTFB_BYTES.min(target);
    let mut morsels = Vec::new();
    // Cursor and quote state advance monotonically over the whole input and are never restarted at a probe, so one pass
    // is O(n) total; each byte is visited once no matter how many candidates a morsel rejects.
    let mut cursor = 0usize;
    let mut in_quotes = false;
    let mut start = 0usize;
    while start < input.len() {
        let cut = if start == 0 { first_target } else { target };
        let probe = start.saturating_add(cut);
        let end = if probe >= input.len() {
            input.len()
        } else {
            let mut end = input.len();
            while cursor < input.len() {
                match input[cursor] {
                    b'"' => {
                        // A doubled quote is a literal quote; any other toggles state.
                        if input.get(cursor + 1) == Some(&b'"') {
                            cursor += 2;
                        } else {
                            in_quotes = !in_quotes;
                            cursor += 1;
                        }
                    }
                    b'\n' if !in_quotes => {
                        cursor += 1;
                        if cursor >= probe {
                            end = cursor;
                            break;
                        }
                    }
                    _ => cursor += 1,
                }
            }
            end
        };
        morsels.push(Morsel::new(start, end));
        start = end;
    }
    morsels
}

/// Cuts json-seq input into record-aligned morsels of about `target_bytes` each.
///
/// The json-seq framer ends every record at the next raw RS, so a cut IMMEDIATELY BEFORE an RS is exact: the morsel
/// `[start, rs)` contains whole units, and the next morsel `[rs, ...)` starts WITH its own RS, which is what the
/// framer's waiting-for-first-RS state needs to begin parsing at its first byte. Cutting AFTER an RS would hand the
/// next morsel a payload byte with no leading RS and silently lose the first record.
#[must_use]
pub(crate) fn partition_json_seq_morsels(input: &[u8], target_bytes: u64) -> Vec<Morsel> {
    let target = usize::try_from(target_bytes).unwrap_or(usize::MAX).max(1);
    let first_target = FIRST_MORSEL_TTFB_BYTES.min(target);
    let mut morsels = Vec::new();
    let mut start = 0usize;
    while start < input.len() {
        let cut = if start == 0 { first_target } else { target };
        let probe = start.saturating_add(cut);
        let end = if probe >= input.len() {
            input.len()
        } else {
            match memchr::memchr(0x1e, &input[probe..]) {
                Some(offset) => probe + offset,
                None => input.len(),
            }
        };
        morsels.push(Morsel::new(start, end));
        start = end;
    }
    morsels
}

/// First morsel target: the value lane's TTFB shrink, applied to records.
const FIRST_MORSEL_TTFB_BYTES: usize = 64 * 1024;

/// Shared morsel cutter for grammars where every line feed is a record terminator (NDJSON; TSV has no quote state).
fn partition_by(input: &[u8], target_bytes: u64) -> Vec<Morsel> {
    let target = usize::try_from(target_bytes).unwrap_or(usize::MAX).max(1);
    let first_target = FIRST_MORSEL_TTFB_BYTES.min(target);
    let mut morsels = Vec::new();
    let mut start = 0usize;
    while start < input.len() {
        let cut = if start == 0 { first_target } else { target };
        let probe = start.saturating_add(cut);
        let end = if probe >= input.len() {
            input.len()
        } else {
            match memchr::memchr(b'\n', &input[probe..]) {
                Some(offset) => probe + offset + 1,
                None => input.len(),
            }
        };
        morsels.push(Morsel::new(start, end));
        start = end;
    }
    morsels
}

#[cfg(test)]
mod tests {
    use super::{
        AUTO_BREAK_EVEN_BYTES, MIN_MORSEL_BYTES, PlanDecision, WorkerRequest, partition_csv_morsels,
        partition_json_seq_morsels, partition_morsels, plan_request,
    };
    use crate::parallel::auto_worker_ceiling;

    #[test]
    fn morsels_cover_the_input_exactly_and_end_on_record_boundaries() {
        let input = b"aaaa\nbbbb\ncccc\ndddd\n";
        let morsels = partition_morsels(input, 4);
        assert_eq!(morsels.len(), 4);
        assert_eq!(morsels[0].start(), 0);
        assert_eq!(morsels[3].end(), input.len());
        for pair in morsels.windows(2) {
            assert_eq!(pair[0].end(), pair[1].start());
        }
        for morsel in &morsels {
            assert_eq!(input[morsel.end() - 1], b'\n');
        }
    }

    #[test]
    fn json_seq_morsels_cut_immediately_before_rs() {
        // Cut immediately before RS so the next morsel starts WITH its own RS. Cutting after RS would hand the next
        // morsel a payload with no leading RS and lose the first record.
        let input = b"\x1eaaa\x1ebbb";
        let morsels = partition_json_seq_morsels(input, 4);
        assert_eq!(morsels.len(), 2);
        assert_eq!((morsels[0].start(), morsels[0].end()), (0, 4));
        assert_eq!((morsels[1].start(), morsels[1].end()), (4, input.len()));
        assert_eq!(input[morsels[1].start()], 0x1e);
    }

    #[test]
    fn a_morsel_grows_past_its_target_to_reach_the_next_record_boundary() {
        // The cut is the first line feed AT OR AFTER the target, so a morsel is never smaller than the target and never
        // splits a record.
        let input = b"aaaa\nbbbb\ncccc\ndddd\n";
        let morsels = partition_morsels(input, 5);
        assert_eq!(morsels.len(), 2);
        assert_eq!(morsels[0].len(), 10);
        assert_eq!(morsels[1].len(), 10);
    }

    #[test]
    fn csv_quote_state_walks_from_the_morsel_start_not_the_probe() {
        // Record 1's quoted field opens at byte 0, inside the first `target` bytes, and holds an embedded LF at byte 6.
        // A walk originating at the probe would miss the opening quote, see the LF as a boundary, and cut MID-QUOTE at
        // 7. The walk originates at the morsel start and RESUMES past the in-quote candidate: the next out-of-quote LF
        // ends record 1 at 19, so the document splits [(0, 19), (19, 27)] instead of collapsing the whole remainder
        // into one morsel.
        let input = b"\"line1\nline2\",rest\n{\"a\":1}\n";
        let morsels = partition_csv_morsels(input, 6);
        assert_eq!(morsels.len(), 2);
        assert_eq!((morsels[0].start(), morsels[0].end()), (0, 19));
        assert_eq!((morsels[1].start(), morsels[1].end()), (19, input.len()));
    }

    #[test]
    fn csv_multiline_quoted_records_divide_into_many_morsels() {
        // Every record ends in a quoted field holding one embedded LF, so the first candidate at-or-after each target
        // is IN-QUOTE. The scan must resume to each record's real terminator: many contiguous morsels, each ending just
        // past an out-of-quote LF with even quote parity from its own start.
        let mut input = Vec::new();
        for tag in 0..24u32 {
            input.extend_from_slice(format!("{tag},\"x\ny\"\n").as_bytes());
        }
        let morsels = partition_csv_morsels(&input, 4);
        assert!(morsels.len() >= 2, "resume must divide the stream");
        let mut start = 0usize;
        for morsel in &morsels {
            assert_eq!(morsel.start(), start);
            assert_eq!(input[morsel.end() - 1], b'\n');
            let mut in_quotes = false;
            for &byte in &input[start..morsel.end()] {
                if byte == b'"' {
                    in_quotes = !in_quotes;
                }
            }
            assert!(!in_quotes, "cut at {} lands mid-quote", morsel.end());
            start = morsel.end();
        }
        assert_eq!(start, input.len());
    }

    #[test]
    fn csv_unterminated_quote_still_runs_the_tail_into_one_morsel() {
        // The quote never closes, so no further boundary exists: the final morsel runs to end-of-input, which the
        // relay's oversized guard — not a silent mis-cut — serves by the serial fallback.
        let input = b"a,\"b\nc\nd";
        let morsels = partition_csv_morsels(input, 4);
        assert_eq!(morsels.len(), 1);
        assert_eq!(morsels[0].end(), input.len());
    }

    #[test]
    fn an_unterminated_tail_stays_inside_the_last_morsel() {
        let input = b"aaaa\nbbbb";
        let morsels = partition_morsels(input, 4);
        assert_eq!(morsels.len(), 2);
        assert_eq!(morsels[1].end(), input.len());
    }

    #[test]
    fn auto_stays_serial_below_the_pinned_break_even() {
        let plan = plan_request(WorkerRequest::Auto, AUTO_BREAK_EVEN_BYTES - 1, None);
        assert_eq!(plan.decision(), PlanDecision::BelowBreakEven);
        assert_eq!(plan.workers(), 0);
    }

    #[test]
    fn auto_asks_for_one_worker_per_drawable_morsel_within_its_bounds() {
        // Two morsels is the narrowest shape the break-even admits, and it is also the floor: the width never drops
        // below two above the pin.
        let plan = plan_request(WorkerRequest::Auto, AUTO_BREAK_EVEN_BYTES, None);
        assert_eq!(plan.decision(), PlanDecision::Parallel);
        assert_eq!(plan.workers(), 2);

        // Between the floor and the ceiling the width tracks the morsel count.
        let ceiling = auto_worker_ceiling();
        if ceiling >= 3 {
            let plan = plan_request(WorkerRequest::Auto, 3 * MIN_MORSEL_BYTES, None);
            assert_eq!(plan.workers(), 3);
        }

        // An input far past the ceiling divides into more morsels, never into more workers.
        let plan = plan_request(WorkerRequest::Auto, 4096 * MIN_MORSEL_BYTES, None);
        assert_eq!(plan.workers(), ceiling);
    }

    #[test]
    fn an_explicit_width_ignores_the_break_even_but_not_the_morsel_floor() {
        let plan = plan_request(WorkerRequest::Explicit(4), MIN_MORSEL_BYTES, None);
        assert_eq!(plan.decision(), PlanDecision::SingleMorsel);

        let plan = plan_request(WorkerRequest::Explicit(4), 16 * MIN_MORSEL_BYTES, None);
        assert_eq!(plan.decision(), PlanDecision::Parallel);
        assert_eq!(plan.workers(), 4);
    }

    #[test]
    fn an_ineligible_request_records_why_it_fell_through() {
        let plan = plan_request(WorkerRequest::Auto, 1 << 30, Some(PlanDecision::ProgramIneligible));
        assert_eq!(plan.decision(), PlanDecision::ProgramIneligible);
        assert!(!plan.is_parallel());
    }
}
