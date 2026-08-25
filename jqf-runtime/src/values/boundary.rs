//! Where an adjacent-JSON stream may be cut, and why a cut is SOUND.
//!
//! # The problem this module does not have the luxury of assuming away
//!
//! The NDJSON record route cuts at line feeds because its framer GUARANTEES that every record ends at one. The default
//! stdin has no framer and no guarantee: the default stdin is a stream of RFC 8259 texts separated by arbitrary
//! whitespace, so a `}`, a `]`, or a line feed may sit inside a string literal, a `\"` may hide the quote that would
//! have ended one, and two adjacent numbers may be separated by nothing a scanner can see from a local view (`123456`
//! is ONE value, and cutting it into `123` and `456` produces two perfectly parseable shards and the WRONG answer).
//!
//! # The soundness argument, in two independent halves
//!
//! **Half one — the acceptance rule (this module).** A cut offset `C` is proposed only when the byte at `C - 1` is a
//! `}` or a `]` that returns the scan's nesting depth to zero outside a string literal.
//!
//! **Half two — the drive (the relay's clean-morsel law).** A shard runs the ordinary adjacent-value drive over `[s,
//! e)`, which by construction either consumes its range EXACTLY — it loops until the range is exhausted — or fails, and
//! a failed or diagnostic-emitting shard yields the whole request to serial.
//!
//! Together they give the lemma the lane rests on:
//!
//! > If the adjacent-value drive over `[0, C)` succeeds, and the last
//! > significant byte before `C` is `}` or `]`, then serial's drive over the
//! > WHOLE input parses `[0, C)` identically and has a value boundary at `C`.
//!
//! *Proof.* The drive is a deterministic left-to-right parser, so over the whole input it behaves identically to its
//! restriction until it reads a byte at or past `C`. The only place the two could diverge is the final token of the
//! last value in `[0, C)`: a token whose extent depends on what follows. That token ends in `}` or `]`, and a JSON
//! value that ends in a closing brace or bracket is COMPLETE — no suffix of bytes extends it. Hence serial ends that
//! value at `C` too. Induction over the shard starts extends this to every cut. ∎
//!
//! Two consequences are worth stating because they are what make the rule usable rather than merely true.
//!
//! **The scanner's correctness is not load-bearing.** It is a PROPOSER. If its string tracking were wrong and it
//! proposed a `C` inside a string literal, the shard ending at `C` would hold an unterminated string, fail to parse,
//! and yield the request to serial. The scanner decides how FAST the lane is, never whether it is right. (It is
//! nevertheless written to the exact JSON rules, because yielding is a 2x cost.)
//!
//! **Scalar-terminated values are deliberately not cut after.** A top-level number, string, `true`, `false`, or `null`
//! ends a value too, but only a closing brace or bracket is self-delimiting under a scanner that may be wrong. A stream
//! of bare scalars therefore yields ONE shard and runs serially — a missed win, never a wrong answer.

use std::collections::VecDeque;

use crate::parallel::Morsel;

/// How far the scan will walk before it gives up on ever finding a top-level boundary.
///
/// A single top-level value larger than this cannot be cut at the lane's own minimum shard granularity, and walking a
/// ten-megabyte single document to discover that it holds exactly one value is a measurable tax on every
/// single-document lane the CLI already serves well (`.catalog[500].name` over a 10 MB document is a 30 ms request; a
/// full structural scan of its input is a tenth of that, spent to learn nothing).
///
/// The probe is therefore ONE-SIDED and cheap: if no top-level value has closed within the first
/// `FIRST_VALUE_PROBE_BYTES`, the scan declines and the request runs exactly as it does today. A stream whose first
/// value is huge but whose remainder is shardable is a missed win, and it is recorded as one.
const FIRST_VALUE_PROBE_BYTES: usize = 128 * 1024;

/// The FIRST shard's cut target, smaller than the steady-state shard size.
///
/// The relay publishes a shard's bytes only when that shard COMPLETES (the clean-morsel law must be able to discard the
/// published prefix on a terminal), and a per-morsel flush makes them observable immediately, so the first shard's size
/// is the time-to-first-byte of the parallel adjacent-value lane: on the 200 k NDJSON lane the first 324 KiB shard's
/// drive is ~10 ms and the first byte lands at its completion, while a 64 KiB first shard completes in ~3 ms — measured
/// 16.4 -> 9.3 ms first byte with the steady-state total unchanged. Steady-state shards, the width decision, and the
/// worker envelope are untouched, and the cut rule is unchanged — the first cut is still at the first PROVEN top-level
/// boundary at-or-after the target, so the soundness argument and the clean-morsel law hold byte for byte (shard
/// boundaries are invisible in the output). Below ~64 KiB the first shard's own records stop amortizing the worker's
/// fixed setup.
const FIRST_SHARD_TTFB_BYTES: usize = 64 * 1024;

/// One pull from the adjacent-value boundary scanner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShardStep {
    /// A proven top-level cut. Earlier shards remain valid.
    Shard(Morsel),
    /// No sound cut exists. The coordinator demotes to serial.
    Decline,
    /// Shards were already emitted and the remaining input is malformed. Yield to serial: a worker handed this
    /// remainder would fail the same way.
    Malformed,
    /// The input is exhausted. No further shard.
    Exhausted,
}

/// Incremental structural scan. Workers can start after the first proven cuts; the coordinator does not walk the rest
/// of the input first.
pub(crate) struct ValueShardScan<'input> {
    input: &'input [u8],
    target: usize,
    first_target: usize,
    start: usize,
    depth: u32,
    offset: usize,
    closed_a_value: bool,
    emitted: usize,
    done: bool,
}

impl<'input> ValueShardScan<'input> {
    fn new(input: &'input [u8], target_bytes: u64) -> Self {
        let target = usize::try_from(target_bytes).unwrap_or(usize::MAX).max(1);
        Self {
            input,
            target,
            first_target: FIRST_SHARD_TTFB_BYTES.min(target),
            start: 0,
            depth: 0,
            offset: 0,
            closed_a_value: false,
            emitted: 0,
            done: false,
        }
    }

    /// Bytes visited so far. Tests pin that the first cuts land before EOF.
    #[cfg(test)]
    fn scanned_bytes(&self) -> usize {
        self.offset
    }

    fn fail(&mut self) -> ShardStep {
        self.done = true;
        if self.emitted == 0 {
            ShardStep::Decline
        } else {
            ShardStep::Malformed
        }
    }

    fn emit(&mut self, morsel: Morsel) -> ShardStep {
        self.emitted += 1;
        ShardStep::Shard(morsel)
    }

    fn next(&mut self) -> ShardStep {
        if self.done {
            return ShardStep::Exhausted;
        }
        while self.offset < self.input.len() {
            // A top-level value that has not closed by the probe limit means this input is not the shape the lane
            // serves. Declining costs one wasted window; continuing would cost a walk of the whole input.
            if !self.closed_a_value && self.offset >= FIRST_VALUE_PROBE_BYTES {
                return self.fail();
            }
            match self.input[self.offset] {
                b'"' => {
                    let Some(after) = skip_string(self.input, self.offset + 1) else {
                        // An unterminated string: the input is malformed, or the scanner has lost the thread. Serial
                        // owns the diagnostic.
                        return self.fail();
                    };
                    self.offset = after;
                }
                b'{' | b'[' => {
                    // Checked, symmetric with the close arm below: an input nested past 2^32 containers must decline
                    // the scan (serial owns the diagnostic), never wrap in release or panic in debug.
                    let Some(opened) = self.depth.checked_add(1) else {
                        return self.fail();
                    };
                    self.depth = opened;
                    self.offset += 1;
                }
                b'}' | b']' => {
                    let Some(closed) = self.depth.checked_sub(1) else {
                        // A close with nothing open: malformed input. Serial's diagnostic is the right answer and this
                        // scan has none.
                        return self.fail();
                    };
                    self.depth = closed;
                    self.offset += 1;
                    if self.depth == 0 {
                        self.closed_a_value = true;
                        let shard_target = if self.start == 0 {
                            self.first_target
                        } else {
                            self.target
                        };
                        if self.offset - self.start >= shard_target && self.offset < self.input.len() {
                            let morsel = Morsel::new(self.start, self.offset);
                            self.start = self.offset;
                            return self.emit(morsel);
                        }
                    }
                }
                _ => self.offset = skip_to_structural(self.input, self.offset + 1),
            }
        }
        self.done = true;
        if self.depth != 0 {
            // An unclosed container. Earlier proven cuts stay valid; a worker handed the remainder would fail the same
            // way, so this is terminal rather than a silent dump of those cuts.
            return self.fail();
        }
        if self.start < self.input.len() {
            // A stream that ends with a terminator leaves a whitespace-only tail after the final close. A blank tail
            // after an already-emitted cut is dropped: joining it would require mutating a dispatched shard, and a
            // whitespace-only worker is pure cost. Dropping a cut is always sound.
            let tail_is_blank = self.input[self.start..].iter().all(u8::is_ascii_whitespace);
            if tail_is_blank && self.emitted > 0 {
                return ShardStep::Exhausted;
            }
            let morsel = Morsel::new(self.start, self.input.len());
            self.start = self.input.len();
            return self.emit(morsel);
        }
        ShardStep::Exhausted
    }
}

/// Pull-based shard source for the adjacent-value lane.
///
/// [`Self::prove_parallel`] walks only until two cuts exist (or the scan declines). The relay then pulls the rest while
/// workers already run.
pub struct ValueMorsels<'input> {
    scan: ValueShardScan<'input>,
    pending: VecDeque<Morsel>,
}

impl<'input> ValueMorsels<'input> {
    /// A source that yields nothing. Serial plans never pull shards.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            scan: ValueShardScan::new(&[], 1),
            pending: VecDeque::new(),
        }
    }

    pub(crate) fn new(input: &'input [u8], target_bytes: u64) -> Self {
        Self {
            scan: ValueShardScan::new(input, target_bytes),
            pending: VecDeque::new(),
        }
    }

    /// True when at least two proven cuts exist. Stops at the second cut.
    pub(crate) fn prove_parallel(&mut self) -> bool {
        while self.pending.len() < 2 {
            match self.scan.next() {
                ShardStep::Shard(morsel) => self.pending.push_back(morsel),
                ShardStep::Decline | ShardStep::Malformed | ShardStep::Exhausted => {
                    return false;
                }
            }
        }
        true
    }

    pub(crate) fn pull(&mut self) -> ShardStep {
        if let Some(morsel) = self.pending.pop_front() {
            return ShardStep::Shard(morsel);
        }
        self.scan.next()
    }

    #[cfg(test)]
    fn scanned_bytes(&self) -> usize {
        self.scan.scanned_bytes()
    }
}

/// Cuts `input` into shards of about `target_bytes`, each starting at a PROVEN top-level value boundary.
///
/// Returns fewer shards than the target implies whenever the input's structure does not offer a boundary, and returns a
/// single shard covering everything — or nothing at all — when the input cannot be cut soundly. The caller reads `len()
/// < 2` as "run serially". A malformed remainder dumps the whole partition so the unit tests keep the conservative
/// whole-scan answer; the live relay treats that remainder as a terminal instead.
#[cfg(test)]
#[must_use]
fn partition_value_shards(input: &[u8], target_bytes: u64) -> Vec<Morsel> {
    let mut scan = ValueShardScan::new(input, target_bytes);
    let mut shards = Vec::new();
    loop {
        match scan.next() {
            ShardStep::Shard(morsel) => shards.push(morsel),
            ShardStep::Exhausted => {
                // The live pull drops a whitespace-only tail so it does not mutate a shard already handed to a worker.
                // The collector still joins that tail so the unit tests keep exact coverage.
                if let Some(last) = shards.last_mut()
                    && last.end() < input.len()
                    && input[last.end()..].iter().all(u8::is_ascii_whitespace)
                {
                    *last = Morsel::new(last.start(), input.len());
                }
                return shards;
            }
            ShardStep::Decline | ShardStep::Malformed => return Vec::new(),
        }
    }
}

/// Returns the offset just past the closing quote of the string body starting at `offset`, or `None` when the string
/// never closes.
///
/// The escape rule is JSON's own and is applied at the BYTE level, which is exact for every input: a UTF-8 continuation
/// byte is never `0x22` or `0x5c`, so no multi-byte character can hide or fake a quote or a backslash, and invalid
/// UTF-8 inside a string is the payload decoder's rejection to make.
fn skip_string(input: &[u8], mut offset: usize) -> Option<usize> {
    while offset < input.len() {
        // `position` over a two-byte predicate is what carries the bulk of a string body; the match below only handles
        // the byte it stopped on.
        let rest = input.get(offset..)?;
        let step = memchr::memchr2(b'"', b'\\', rest)?;
        offset += step;
        if input[offset] == b'"' {
            return Some(offset + 1);
        }
        // A backslash consumes itself and the byte it escapes. `\uXXXX` needs no special case: its four hex digits hold
        // neither a quote nor a backslash, so skipping the `u` is enough.
        offset += 2;
    }
    None
}

/// Returns the next offset at or after `offset` holding a byte the scan cares about, or the input length when none
/// remains.
///
/// Everything between two structural bytes — numbers, literals, whitespace, commas, colons — is invisible to the
/// boundary rule, so it is skipped in one vectorizable pass rather than one byte at a time.
fn skip_to_structural(input: &[u8], offset: usize) -> usize {
    let Some(rest) = input.get(offset..) else {
        return input.len();
    };
    rest.iter()
        .position(|byte| matches!(byte, b'"' | b'{' | b'[' | b'}' | b']'))
        .map_or(input.len(), |step| offset + step)
}

#[cfg(test)]
mod tests {
    use super::{FIRST_VALUE_PROBE_BYTES, ShardStep, ValueMorsels, partition_value_shards};

    fn cuts(input: &[u8], target: u64) -> Vec<(usize, usize)> {
        partition_value_shards(input, target)
            .into_iter()
            .map(|shard| (shard.start(), shard.end()))
            .collect()
    }

    #[test]
    fn the_first_shard_cuts_smaller_than_the_steady_state_target() {
        // The TTFB lever: shard 0 publishes when it completes, so it cuts at FIRST_SHARD_TTFB_BYTES while every later
        // shard keeps the target. The steady target sits ABOVE the first target, or the two coincide and the lever is
        // inert (small inputs keep today's partition). A fixed-shape record suffices: the scan needs top-level value
        // boundaries, not varied payloads.
        let input: Vec<u8> = "{\"a\":1}\n".repeat(40_000).into_bytes();
        let shards = cuts(&input, 128 * 1024);
        assert!(shards.len() >= 3, "{} shards", shards.len());
        assert!(
            shards[0].1 - shards[0].0 < 128 * 1024,
            "first shard {} B",
            shards[0].1 - shards[0].0
        );
        for pair in shards.windows(2) {
            assert_eq!(pair[0].1, pair[1].0);
        }
    }

    #[test]
    fn shards_cover_the_input_exactly_and_start_at_value_boundaries() {
        let input = b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n";
        let shards = cuts(input, 7);
        assert_eq!(shards.len(), 3);
        assert_eq!(shards[0], (0, 7));
        assert_eq!(shards[2].1, input.len());
        for pair in shards.windows(2) {
            assert_eq!(pair[0].1, pair[1].0);
        }
    }

    #[test]
    fn a_brace_inside_a_string_is_never_a_boundary() {
        // A naive brace counter cuts after the `}` inside the string and produces two shards that both fail to parse.
        // The scanner must see one value.
        let input = br#"{"a":"}}}}"}{"b":2}"#;
        assert_eq!(cuts(input, 4), vec![(0, 12), (12, input.len())]);
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let input = br#"{"a":"x\"}y"}{"b":2}"#;
        assert_eq!(cuts(input, 4), vec![(0, 13), (13, input.len())]);
    }

    #[test]
    fn an_escaped_backslash_before_a_quote_still_ends_the_string() {
        // `"x\\"` closes: the second backslash is escaped BY the first, so the quote that follows is the terminator.
        let input = br#"{"a":"x\\"}{"b":2}"#;
        assert_eq!(cuts(input, 4), vec![(0, 11), (11, input.len())]);
    }

    #[test]
    fn a_newline_inside_a_string_is_never_a_boundary() {
        let input = b"{\"a\":\"x\\ny\"}\n{\"b\":2}\n";
        assert_eq!(cuts(input, 4), vec![(0, 12), (12, input.len())]);
    }

    #[test]
    fn nested_containers_only_cut_where_depth_returns_to_zero() {
        let input = b"{\"a\":{\"b\":[1,2,{\"c\":3}]}}{\"d\":4}";
        assert_eq!(cuts(input, 4), vec![(0, 25), (25, input.len())]);
    }

    #[test]
    fn a_value_larger_than_the_target_grows_its_shard() {
        let input = b"[1,2,3,4,5,6,7,8,9,0][1][2]";
        assert_eq!(cuts(input, 4), vec![(0, 21), (21, input.len())]);
        // At a target the small values do reach, every one of them cuts.
        assert_eq!(cuts(input, 3), vec![(0, 21), (21, 24), (24, input.len())]);
    }

    #[test]
    fn a_single_value_yields_one_shard_and_runs_serially() {
        let input = b"{\"a\":[1,2,3]}";
        assert_eq!(cuts(input, 4), vec![(0, input.len())]);
    }

    #[test]
    fn empty_input_yields_no_shard() {
        assert!(cuts(b"", 4).is_empty());
    }

    #[test]
    fn whitespace_and_crlf_runs_between_values_join_the_following_shard() {
        let input = b"{\"a\":1}\r\n\r\n  \t{\"b\":2}";
        assert_eq!(cuts(input, 4), vec![(0, 7), (7, input.len())]);
    }

    #[test]
    fn top_level_scalars_are_not_cut_after() {
        // Sound, and deliberately conservative: only a closing brace or bracket is self-delimiting under a scanner that
        // might be wrong, so a stream of bare numbers runs serially rather than risking `123|456`.
        let input = b"1 2 3 4 5 6 7 8";
        assert_eq!(cuts(input, 2), vec![(0, input.len())]);
        // A scalar BETWEEN containers still travels inside a shard.
        let input = b"{\"a\":1} 42 {\"b\":2}";
        assert_eq!(cuts(input, 4), vec![(0, 7), (7, input.len())]);
    }

    #[test]
    fn a_malformed_close_declines_the_whole_partition() {
        assert!(cuts(b"{\"a\":1}}{\"b\":2}", 4).is_empty());
        assert!(cuts(b"{\"a\":1}{\"b\":2", 4).is_empty());
        assert!(cuts(b"{\"a\":\"unterminated}", 4).is_empty());
    }

    #[test]
    fn a_first_value_past_the_probe_limit_declines() {
        let mut input = Vec::from(b"[0");
        while input.len() < FIRST_VALUE_PROBE_BYTES + 16 {
            input.extend_from_slice(b",0");
        }
        input.push(b']');
        input.extend_from_slice(b"[1][2]");
        assert!(cuts(&input, 1024).is_empty());
    }

    #[test]
    fn the_probe_limit_does_not_bind_once_a_boundary_exists() {
        let mut input = Vec::from(b"[0]");
        while input.len() < FIRST_VALUE_PROBE_BYTES * 2 {
            input.extend_from_slice(b"[1]");
        }
        let shards = cuts(&input, 1024);
        assert!(shards.len() > 2);
        assert_eq!(shards[0].0, 0);
        assert_eq!(shards[shards.len() - 1].1, input.len());
    }

    #[test]
    fn prove_parallel_stops_at_the_second_cut() {
        let input: Vec<u8> = "{\"a\":1}\n".repeat(40_000).into_bytes();
        let mut morsels = ValueMorsels::new(&input, 128 * 1024);
        assert!(morsels.prove_parallel());
        assert!(
            morsels.scanned_bytes() < input.len(),
            "second cut at {} of {}",
            morsels.scanned_bytes(),
            input.len()
        );
    }

    #[test]
    fn a_malformed_tail_after_two_cuts_is_terminal() {
        let mut input = Vec::new();
        input.extend_from_slice(b"{\"a\":1}");
        input.extend_from_slice(b"{\"b\":2}");
        input.extend_from_slice(b"{\"c\":");
        let mut morsels = ValueMorsels::new(&input, 4);
        assert!(morsels.prove_parallel());
        let mut saw_malformed = false;
        loop {
            match morsels.pull() {
                ShardStep::Shard(_) => {}
                ShardStep::Malformed => {
                    saw_malformed = true;
                    break;
                }
                ShardStep::Decline => panic!("dump after proven cuts"),
                ShardStep::Exhausted => break,
            }
        }
        assert!(saw_malformed);
    }
}
