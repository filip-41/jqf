//! Diagnostic events: a code, a severity, a few small fields.
//!
//! The message is not here — whoever caught the error already has it. If nobody attached a sink, emit does nothing.
//!
//! Code numbers live in `codes.toml`. A generator writes `codes.rs`; do not edit it by hand because a gate checks
//! freshness.

use core::cell::RefCell;
use std::collections::VecDeque;
use std::string::String;
use std::vec::Vec;

/// The generated diagnostic-code registry (see `codes.toml`).
pub mod codes;

/// Which kind of raise this record is, if any.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordClass {
    /// A typed semantic error (catch-eligible).
    Semantic,
    /// A machine failure: codec, ledger, control, contract (never catch-eligible).
    Machine,
    /// `error/0` or `error/1` raised a value (catch-eligible; the payload is the catchable text).
    ProgramRaised,
    /// No raise channel: route, cost, precision records.
    Informational,
}

/// How serious the record is. Same type as `jqf-source`, so the two cannot drift.
pub use jqf_source::Severity;

/// One diagnostic, borrowed for the `record` call.
///
/// Copy what you need to keep. Nothing here lives past that call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticRecord<'a> {
    /// The registry-pinned code (see the generated `codes` module).
    pub code: u16,
    /// The code's semantic revision: a changed meaning is a new code, so a stale binding can detect the mismatch
    /// instead of misreading.
    pub revision: u16,
    /// The raise channel.
    pub class: RecordClass,
    /// The record's weight.
    pub severity: Severity,
    /// Whether a `try` barrier may catch this raise (the machine class never is; a raised value and a typed semantic
    /// error are).
    pub catchable: bool,
    /// The barrier depth, when a `try` absorbed this raise.
    pub caught: Option<u32>,
    /// `RAISE_HALT` only: the run's halt exit status (the program's own choice, dial-exempt). Absent on every other
    /// record.
    pub halt_status: Option<u32>,
    /// The failing step's index in the program (typed errors carry one).
    pub step_index: Option<u32>,
    /// The input ordinal the event belongs to (format-neutral; absent for program-only events).
    pub input_ordinal: Option<u64>,
    /// The byte offset into that input.
    pub byte_offset: Option<u64>,
    /// Value kind (`"number"`, `"array"`, …). Not the payload.
    pub kind: Option<&'a str>,
    /// A short rendering of the offending value, already capped. Never the full payload.
    pub operand: Option<&'a str>,
    /// `ProgramRaised` only: the catchable text (the raised value, already rendered).
    pub payload: Option<&'a str>,
}

impl DiagnosticRecord<'_> {
    /// A record with just the code. Fill in the other fields yourself.
    ///
    /// This is the WIRE-TOLERANT constructor: an unknown code degrades silently to revision 0 and
    /// `Informational`/`Info` instead of failing. Retired ids retained in the registry keep their recorded metadata.
    /// Producers inside this crate must use [`DiagnosticRecord::new_registered`] so a wrong constant surfaces as a
    /// defect, not a well-formed-looking info record.
    #[must_use]
    pub fn new(code: u16) -> Self {
        let row = codes::describe(code);
        Self {
            code,
            revision: row.map_or(0, |row| row.revision),
            class: row.map_or(RecordClass::Informational, |row| row.class),
            severity: row.map_or(Severity::Info, |row| row.severity),
            catchable: matches!(
                row.map_or(RecordClass::Informational, |row| row.class),
                RecordClass::Semantic | RecordClass::ProgramRaised
            ),
            caught: None,
            halt_status: None,
            step_index: None,
            input_ordinal: None,
            byte_offset: None,
            kind: None,
            operand: None,
            payload: None,
        }
    }

    /// A record whose code must exist in the generated registry. Retained reserved and retired rows are registered too;
    /// producer call sites own whether a registered row is emitted. Debug builds trip on an unknown code, while release
    /// builds keep the tolerant construction above so one bad constant cannot abort a run. Wire decoding must stay on
    /// [`DiagnosticRecord::new`] so unknown ids remain tolerated.
    #[must_use]
    pub fn new_registered(code: u16) -> Self {
        debug_assert!(
            codes::describe(code).is_some(),
            "diagnostic code {code} is not in the generated registry"
        );
        Self::new(code)
    }
}

/// Receives one diagnostic. Same interior-mutability deal as [`StderrSink`](crate::StderrSink): `record` takes `&self`.
pub trait DiagnosticSink {
    /// Keep (or drop) one diagnostic.
    ///
    /// Implementations own the accounting category of retained allocations. [`DiagnosticBuffer`] charges its storage as
    /// Diagnostic memory.
    fn record(&self, record: DiagnosticRecord<'_>);
}

/// An owned copy of a [`DiagnosticRecord`], for keeping after the run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedDiagnosticRecord {
    /// The registry-pinned code.
    pub code: u16,
    /// The code's semantic revision.
    pub revision: u16,
    /// The raise channel.
    pub class: RecordClass,
    /// The record's weight.
    pub severity: Severity,
    /// Whether a `try` barrier may catch this raise.
    pub catchable: bool,
    /// The barrier depth, when a `try` absorbed this raise.
    pub caught: Option<u32>,
    /// `RAISE_HALT` only: the run's halt exit status (the program's own choice, dial-exempt). Absent on every other
    /// record.
    pub halt_status: Option<u32>,
    /// The failing step's index in the program.
    pub step_index: Option<u32>,
    /// The input ordinal the event belongs to.
    pub input_ordinal: Option<u64>,
    /// The byte offset into that input.
    pub byte_offset: Option<u64>,
    text: PackedText,
}

/// kind / operand / payload in one allocation. A missing field is a `None` range, not an empty slice.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PackedText {
    bytes: String,
    kind: Option<(usize, usize)>,
    operand: Option<(usize, usize)>,
    payload: Option<(usize, usize)>,
}

impl PackedText {
    fn retain(kind: Option<&str>, operand: Option<&str>, payload: Option<&str>) -> Self {
        let mut bytes = String::new();
        let kind = Self::push(&mut bytes, kind);
        let operand = Self::push(&mut bytes, operand);
        let payload = Self::push(&mut bytes, payload);
        Self {
            bytes,
            kind,
            operand,
            payload,
        }
    }

    fn push(bytes: &mut String, field: Option<&str>) -> Option<(usize, usize)> {
        let field = field?;
        let start = bytes.len();
        bytes.push_str(field);
        Some((start, field.len()))
    }

    fn get(&self, span: Option<(usize, usize)>) -> Option<&str> {
        let (start, len) = span?;
        self.bytes.get(start..start + len)
    }
}

impl OwnedDiagnosticRecord {
    /// Retains a borrowed record (copies the text fields).
    #[must_use]
    pub fn retain(record: DiagnosticRecord<'_>) -> Self {
        Self {
            code: record.code,
            revision: record.revision,
            class: record.class,
            severity: record.severity,
            catchable: record.catchable,
            caught: record.caught,
            halt_status: record.halt_status,
            step_index: record.step_index,
            input_ordinal: record.input_ordinal,
            byte_offset: record.byte_offset,
            text: PackedText::retain(record.kind, record.operand, record.payload),
        }
    }

    /// The observed kind's stable name.
    #[must_use]
    pub fn kind(&self) -> Option<&str> {
        self.text.get(self.text.kind)
    }

    /// The bounded operand rendering.
    #[must_use]
    pub fn operand(&self) -> Option<&str> {
        self.text.get(self.text.operand)
    }

    /// `ProgramRaised` only: the catchable text.
    #[must_use]
    pub fn payload(&self) -> Option<&str> {
        self.text.get(self.text.payload)
    }

    /// The record's code name (see the generated `codes` module).
    #[must_use]
    pub fn code_name(&self) -> &'static str {
        codes::describe(self.code).map_or("<unknown>", |row| row.name)
    }
}

/// A ring of recent diagnostics. Newest wins; the marked failure always stays; overflow is counted, never silently
/// dropped.
///
/// Retained storage is charged as Diagnostic memory. You must pick a cap with [`Self::with_cap`].
#[derive(Debug)]
pub struct DiagnosticBuffer {
    state: RefCell<DiagnosticBufferState>,
}

#[derive(Debug)]
struct DiagnosticBufferState {
    records: VecDeque<OwnedDiagnosticRecord>,
    cap: usize,
    dropped: usize,
    terminal: Option<usize>,
}

impl DiagnosticBuffer {
    /// Keep at most `cap` records (`cap` of 0 becomes 1).
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        Self {
            state: RefCell::new(DiagnosticBufferState {
                records: VecDeque::new(),
                cap: cap.max(1),
                dropped: 0,
                terminal: None,
            }),
        }
    }

    /// The retained records, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<OwnedDiagnosticRecord> {
        self.state.borrow().records.iter().cloned().collect()
    }

    /// The number of retained records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.state.borrow().records.len()
    }

    /// Whether no records are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state.borrow().records.is_empty()
    }

    /// Calls `inspect` with one retained record, without cloning it.
    ///
    /// `inspect` must not mutate this buffer while the record is borrowed.
    pub fn with_record<R>(&self, index: usize, inspect: impl FnOnce(&OwnedDiagnosticRecord) -> R) -> Option<R> {
        self.state.borrow().records.get(index).map(inspect)
    }

    /// The terminal failure record, when one was retained.
    #[must_use]
    pub fn failure(&self) -> Option<OwnedDiagnosticRecord> {
        let state = self.state.borrow();
        state.terminal.and_then(|index| state.records.get(index)).cloned()
    }

    /// How many records overflowed the cap.
    #[must_use]
    pub fn dropped(&self) -> usize {
        self.state.borrow().dropped
    }

    /// Keep the last record even if newer ones overflow the cap.
    pub fn mark_terminal(&self) {
        let mut state = self.state.borrow_mut();
        state.terminal = state.records.len().checked_sub(1);
    }

    /// Empty the buffer. Call this when you reuse it for another run.
    pub fn clear(&self) {
        let _category = crate::ambient::category_scope(crate::MemoryCategory::Diagnostic);
        let mut state = self.state.borrow_mut();
        state.records.clear();
        state.dropped = 0;
        state.terminal = None;
    }
}

impl DiagnosticSink for DiagnosticBuffer {
    fn record(&self, record: DiagnosticRecord<'_>) {
        let _category = crate::ambient::category_scope(crate::MemoryCategory::Diagnostic);
        let mut state = self.state.borrow_mut();
        if state.records.len() < state.cap {
            state.records.push_back(OwnedDiagnosticRecord::retain(record));
            return;
        }
        // Full: newest wins. Drop the oldest NON-terminal record; when the only resident record is the terminal one,
        // count the overflow without copying text that cannot be retained. Cost is not a flat constant: the scan stops
        // at the first non-terminal slot (the front, or slot 1 with a terminal pinned there), but removing from that
        // middle slot shifts every record after it, so a flood over a pinned terminal pays an O(cap) shift per
        // overflowed record. Caps are small; that is accepted rather than traded for a second index structure.
        if let Some(drop_at) = (0..state.records.len()).find(|&index| Some(index) != state.terminal) {
            let victim = state.records.remove(drop_at);
            state.dropped = state.dropped.saturating_add(1);
            // The terminal index shifts down when the drop was before it.
            if let Some(index) = state.terminal
                && index > drop_at
            {
                state.terminal = Some(index - 1);
            }
            drop(victim);
            state.records.push_back(OwnedDiagnosticRecord::retain(record));
        } else {
            state.dropped = state.dropped.saturating_add(1);
        }
    }
}

impl Drop for DiagnosticBuffer {
    fn drop(&mut self) {
        let records = std::mem::take(&mut self.state.get_mut().records);
        let _category = crate::ambient::category_scope(crate::MemoryCategory::Diagnostic);
        drop(records);
    }
}

#[cfg(test)]
mod tests {
    use super::{DiagnosticBuffer, DiagnosticRecord, DiagnosticSink, OwnedDiagnosticRecord, Severity, codes};

    fn record(code: u16) -> DiagnosticRecord<'static> {
        DiagnosticRecord::new_registered(code)
    }

    #[test]
    fn the_buffer_retains_in_order_and_reports_overflow() {
        let buffer = DiagnosticBuffer::with_cap(3);
        buffer.record(record(codes::RAISE_ITERATE));
        buffer.record(record(codes::RAISE_INDEX));
        buffer.record(record(codes::RAISE_DIVIDE_BY_ZERO));
        assert_eq!(
            buffer.records().iter().map(|record| record.code).collect::<Vec<_>>(),
            [codes::RAISE_ITERATE, codes::RAISE_INDEX, codes::RAISE_DIVIDE_BY_ZERO]
        );
        buffer.record(record(codes::RAISE_HALT));
        assert_eq!(
            buffer.records().iter().map(|record| record.code).collect::<Vec<_>>(),
            [codes::RAISE_INDEX, codes::RAISE_DIVIDE_BY_ZERO, codes::RAISE_HALT],
            "newest wins"
        );
        assert_eq!(buffer.dropped(), 1, "an eviction of a non-terminal is overflow");
    }

    #[test]
    fn the_terminal_failure_survives_overflow() {
        let buffer = DiagnosticBuffer::with_cap(2);
        buffer.record(record(codes::RAISE_ITERATE));
        buffer.record(record(codes::RAISE_INDEX));
        buffer.mark_terminal();
        buffer.record(record(codes::RAISE_DIVIDE_BY_ZERO));
        buffer.record(record(codes::RAISE_HALT));
        let failure = buffer.failure().expect("terminal retained");
        assert_eq!(failure.code, codes::RAISE_INDEX);
        assert_eq!(
            buffer.dropped(),
            2,
            "two non-terminal evictions are overflow; the terminal stayed"
        );
        assert_eq!(
            buffer.records().iter().map(|record| record.code).collect::<Vec<_>>(),
            [codes::RAISE_INDEX, codes::RAISE_HALT]
        );
    }

    #[test]
    fn a_cap_one_buffer_drops_everything_after_the_terminal() {
        let buffer = DiagnosticBuffer::with_cap(1);
        let mut terminal = record(codes::RAISE_ITERATE);
        terminal.payload = Some("keep");
        buffer.record(terminal);
        buffer.mark_terminal();
        let mut incoming = record(codes::RAISE_INDEX);
        incoming.payload = Some("drop");
        buffer.record(incoming);
        buffer.record(incoming);
        assert_eq!(buffer.dropped(), 2);
        assert_eq!(buffer.records()[0].code, codes::RAISE_ITERATE);
        assert_eq!(buffer.failure().expect("terminal").payload(), Some("keep"));
    }

    #[test]
    fn a_cap_one_buffer_without_a_terminal_retains_incoming_text() {
        let buffer = DiagnosticBuffer::with_cap(1);
        buffer.record(record(codes::RAISE_ITERATE));
        let mut incoming = record(codes::RAISE_INDEX);
        incoming.kind = Some("array");
        incoming.operand = Some("[1]");
        incoming.payload = Some("kept");
        buffer.record(incoming);
        let kept = &buffer.records()[0];
        assert_eq!(kept.code, codes::RAISE_INDEX);
        assert_eq!(kept.kind(), Some("array"));
        assert_eq!(kept.operand(), Some("[1]"));
        assert_eq!(kept.payload(), Some("kept"));
    }

    #[test]
    fn a_larger_buffer_with_a_terminal_retains_incoming_text() {
        let buffer = DiagnosticBuffer::with_cap(2);
        buffer.record(record(codes::RAISE_ITERATE));
        buffer.record(record(codes::RAISE_INDEX));
        buffer.mark_terminal();
        let mut incoming = record(codes::RAISE_HALT);
        incoming.kind = Some("object");
        incoming.operand = Some("{}");
        buffer.record(incoming);
        assert_eq!(buffer.failure().expect("terminal").code, codes::RAISE_INDEX);
        let records = buffer.records();
        let kept = records.last().expect("newest");
        assert_eq!(kept.code, codes::RAISE_HALT);
        assert_eq!(kept.kind(), Some("object"));
        assert_eq!(kept.operand(), Some("{}"));
    }

    #[test]
    fn a_zero_cap_uses_the_minimum_one_record_ring() {
        let buffer = DiagnosticBuffer::with_cap(0);
        buffer.record(record(codes::RAISE_ITERATE));
        buffer.record(record(codes::RAISE_INDEX));
        assert_eq!(buffer.records()[0].code, codes::RAISE_INDEX);
        assert_eq!(buffer.dropped(), 1);
    }

    #[test]
    fn inspecting_a_record_borrows_its_retained_text() {
        let buffer = DiagnosticBuffer::with_cap(1);
        assert!(buffer.is_empty());
        let mut incoming = record(codes::RAISE_ITERATE);
        incoming.payload = Some("retained");
        buffer.record(incoming);
        let stored = buffer.state.borrow().records[0].payload().expect("payload").as_ptr();

        assert_eq!(buffer.len(), 1);
        assert!(!buffer.is_empty());
        let inspected = buffer
            .with_record(0, |record| (record.code, record.payload().expect("payload").as_ptr()))
            .expect("record");
        assert_eq!(inspected, (codes::RAISE_ITERATE, stored));
        assert!(buffer.with_record(1, |_| ()).is_none());
    }

    #[test]
    fn retain_keeps_absent_and_empty_text_distinct() {
        let mut empty = record(codes::RAISE_ITERATE);
        empty.kind = Some("");
        empty.operand = Some("");
        let absent = OwnedDiagnosticRecord::retain(record(codes::RAISE_ITERATE));
        let empty = OwnedDiagnosticRecord::retain(empty);
        assert_eq!(absent.kind(), None);
        assert_eq!(empty.kind(), Some(""));
        assert_eq!(empty.operand(), Some(""));
        assert_eq!(empty.payload(), None);
    }

    #[test]
    fn severity_is_registry_supplied() {
        let record = record(codes::RAISE_ITERATE);
        assert_eq!(record.severity, Severity::Error);
        assert!(record.catchable);
        let info = DiagnosticRecord::new_registered(codes::ROUTE_SELECTED);
        assert_eq!(info.severity, Severity::Info);
        assert!(!info.catchable);
    }

    /// The two construction paths split exactly at the registry: a producer-side code must be registered
    /// (`new_registered` answers for known ids and trips debug on the rest), while `new` keeps tolerating unregistered
    /// ids so decoding an older run's output never fails.
    #[test]
    fn the_tolerant_path_degrades_unknown_ids_and_the_registered_one_does_not() {
        // 200 is not in codes.toml (the table ends well below it).
        let unknown = DiagnosticRecord::new(200);
        assert_eq!(unknown.revision, 0);
        assert_eq!(unknown.severity, Severity::Info);
        assert_eq!(unknown.class, super::RecordClass::Informational);
        assert!(!unknown.catchable);

        let known = DiagnosticRecord::new_registered(codes::RAISE_ITERATE);
        assert_eq!(known.code, codes::RAISE_ITERATE);
        assert!(known.revision > 0, "registered rows carry their revision");
    }
}
