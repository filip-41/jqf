//! Versioned, byte-stable serialization of a compiled program's routing facts.
//!
//! This is the serializable half of the `--explain` plan. The
//! [`ExplainPlan`] is a BORROWED view of the routing facts a compiled program
//! derived — read through exactly the accessors the route selector reads, so
//! the plan cannot drift from the route it documents. This module snapshots
//! those facts into an OWNED [`PlanRecord`] and encodes it in a versioned,
//! deterministic binary form that round-trips byte-stable.
//!
//! What is serialized is the plan — the routing facts — not the executable
//! arena graph. That is the honest scope of the cell: a saved plan pins the
//! facts that decide the route, and loading one re-derives those facts from a
//! freshly compiled program and requires them to match (the same
//! cannot-drift law `--explain` states). The saved artifact is therefore a
//! route contract, verified on load rather than trusted.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use crate::analysis::{BoundaryConsumer, ProjectionClass};
use crate::codec_requirement::StaticForwardStep;
use crate::compile::CompiledProgram;
use crate::explain::{ExplainPlan, RungEligibility, ShortcutKind};

/// The versioned, byte-stable serialized form of one compiled program's plan.
///
/// The record is the OWNED mirror of [`ExplainPlan`]: every borrowed fact is
/// snapshotted into an owned value so the plan can leave the program's arena.
/// [`Self::serialize`] and [`Self::deserialize`] round-trip it byte-stable,
/// and equality against a freshly derived record is the load-time drift check.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each boolean is one routing fact of the serialized plan; grouping them would hide the facts"
)]
pub struct PlanRecord {
    /// Whether the program contains any assignment (`=`/`|=` family) node.
    pub modifies: bool,
    /// Whether the root evaluation consumes the entire input document.
    pub consumes_whole_document: bool,
    /// Whether the program is the MORSEL-static-path class.
    pub morsel_static_path: bool,
    /// Whether the program reads the input family (`input`/`inputs`).
    pub uses_input_family: bool,
    /// The backward demand lattice class of one streamed element.
    pub projection_class: ClassRecord,
    /// The pushed-down static prefix as codec path steps; empty is the root
    /// selection.
    pub pushdown: Vec<StepRecord>,
    /// Every route-ladder rung's eligibility.
    pub rungs: RungEligibility,
    /// What consumes the named boundary's elements, when one is named.
    pub boundary_consumer: Option<BoundaryConsumer>,
    /// How many rows of the closed partial-sort table this program matches.
    pub topk_rows: u64,
    /// The one job finish committed. `None` is the residual graph.
    pub shortcut: ShortcutKind,
    /// Whether the compiled program reads the ~inputs resident cursor.
    pub uses_inputs_cursor: bool,
}

/// The owned form of a per-element demand class.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClassRecord {
    /// P0 — boundaries and counts only; no element payload is decoded.
    Structure,
    /// P1 — only the named top-level fields' payloads are needed.
    Fields(Vec<String>),
    /// P2 — the whole element subtree.
    Subtree,
}

/// The owned form of one decoded static forward path step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepRecord {
    /// A decoded UTF-8 object key.
    ObjectKey(String),
    /// A signed array position preserving the negative-index semantics.
    ArrayIndex(i64),
    /// A contiguous element RANGE over an array container, normalized to
    /// signed `i64`-or-open bounds at lower time.
    ArrayRange {
        /// Inclusive start, or `None` for the container's start.
        start: Option<i64>,
        /// Exclusive end, or `None` for the container's end.
        end: Option<i64>,
    },
}

/// Why a saved plan could not be loaded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanError {
    /// The bytes do not start with the plan magic.
    BadMagic,
    /// The format version is not one this binary reads.
    BadVersion(u32),
    /// The input ended before a complete field could be read.
    Truncated,
    /// A boolean field carried a value other than 0 or 1.
    InvalidBool,
    /// An enum tag was not one of the format's known tags.
    InvalidEnum(&'static str),
    /// A length-prefixed byte region was not valid UTF-8.
    InvalidUtf8,
    /// Trailing bytes followed the final field.
    Overrun,
}

impl core::fmt::Display for PlanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadMagic => write!(f, "the bytes do not start with a jqf plan header"),
            Self::BadVersion(version) => {
                write!(f, "unsupported plan format version {version}")
            }
            Self::Truncated => write!(f, "the plan ended before a complete field was read"),
            Self::InvalidBool => write!(f, "a boolean field carried a value other than 0 or 1"),
            Self::InvalidEnum(what) => write!(f, "an unknown {what} tag in the plan"),
            Self::InvalidUtf8 => write!(f, "a plan string field is not valid UTF-8"),
            Self::Overrun => write!(f, "trailing bytes followed the plan's final field"),
        }
    }
}

impl PlanRecord {
    /// Snapshots a compiled program's borrowed plan into an owned record.
    #[must_use]
    pub fn from_explain(plan: &ExplainPlan<'_>) -> Self {
        let projection_class = match &plan.projection_class {
            ProjectionClass::Structure => ClassRecord::Structure,
            ProjectionClass::Fields(fields) => {
                ClassRecord::Fields(fields.names().iter().map(|name| (*name).to_owned()).collect())
            }
            ProjectionClass::Subtree => ClassRecord::Subtree,
        };
        Self {
            modifies: plan.modifies,
            consumes_whole_document: plan.consumes_whole_document,
            morsel_static_path: plan.morsel_static_path,
            uses_input_family: plan.uses_input_family,
            projection_class,
            pushdown: plan.pushdown.iter().map(StepRecord::from_step).collect(),
            rungs: plan.rungs,
            boundary_consumer: plan.boundary_consumer,
            topk_rows: plan.topk_rows as u64,
            shortcut: plan.shortcut,
            uses_inputs_cursor: plan.uses_inputs_cursor,
        }
    }

    /// Encodes the record in the versioned, deterministic plan format.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(MAGIC);
        w.u32(VERSION);
        w.bool(self.modifies);
        w.bool(self.consumes_whole_document);
        w.bool(self.morsel_static_path);
        w.bool(self.uses_input_family);
        w.class(&self.projection_class);
        w.steps(&self.pushdown);
        w.bool(self.rungs.morsel);
        w.opt_consumer(self.boundary_consumer);
        w.u64(self.topk_rows);
        w.shortcut(self.shortcut);
        w.bool(self.uses_inputs_cursor);
        w.finish()
    }

    /// Decodes a record from the versioned plan format.
    ///
    /// Any malformed, truncated, wrong-version, or unknown-tag input is an
    /// error — never a silent partial record.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, PlanError> {
        let mut r = Reader::new(bytes);
        let magic = r.take(MAGIC.len()).map_err(|_| PlanError::BadMagic)?;
        if magic != MAGIC {
            return Err(PlanError::BadMagic);
        }
        let version = r.u32().map_err(|_| PlanError::Truncated)?;
        if !matches!(version, 7 | 8 | 9 | VERSION) {
            return Err(PlanError::BadVersion(version));
        }
        let identity = if version < 10 { Some(r.bool()?) } else { None };
        let modifies = r.bool()?;
        let consumes_whole_document = r.bool()?;
        let morsel_static_path = r.bool()?;
        let uses_input_family = r.bool()?;
        let projection_class = r.class()?;
        let pushdown = r.steps()?;
        let range_locate = if version < 10 { Some(r.bool()?) } else { None };
        let rungs = RungEligibility { morsel: r.bool()? };
        let boundary_consumer = r.opt_consumer()?;
        let topk_rows = r.u64()?;
        let (shortcut, uses_inputs_cursor) = match version {
            7 => {
                let shortcut = if identity == Some(true) {
                    ShortcutKind::Identity
                } else if range_locate == Some(true) {
                    ShortcutKind::RangeLocate
                } else {
                    ShortcutKind::None
                };
                (shortcut, false)
            }
            8 => {
                let count = r.bool()?;
                let element = r.bool()?;
                let keys = r.bool()?;
                let type_route = r.bool()?;
                let uses_inputs_cursor = r.bool()?;
                let shortcut = if count {
                    ShortcutKind::Count
                } else if element {
                    ShortcutKind::Element
                } else if keys {
                    ShortcutKind::Keys
                } else if type_route {
                    ShortcutKind::Type
                } else if identity == Some(true) {
                    ShortcutKind::Identity
                } else if range_locate == Some(true) {
                    ShortcutKind::RangeLocate
                } else {
                    ShortcutKind::None
                };
                (shortcut, uses_inputs_cursor)
            }
            _ => (r.shortcut()?, r.bool()?),
        };
        let record = Self {
            modifies,
            consumes_whole_document,
            morsel_static_path,
            uses_input_family,
            projection_class,
            pushdown,
            rungs,
            boundary_consumer,
            topk_rows,
            shortcut,
            uses_inputs_cursor,
        };
        if !r.finished() {
            return Err(PlanError::Overrun);
        }
        Ok(record)
    }
}

impl CompiledProgram {
    /// The owned, serializable routing-facts record of this program.
    #[must_use]
    pub fn plan_record(&self) -> PlanRecord {
        PlanRecord::from_explain(&self.explain())
    }

    /// Serializes this program's routing-facts plan into the versioned format.
    #[must_use]
    pub fn serialize_plan(&self) -> Vec<u8> {
        self.plan_record().serialize()
    }
}

impl StepRecord {
    fn from_step(step: &StaticForwardStep<'_>) -> Self {
        match step {
            StaticForwardStep::ObjectKey(key) => StepRecord::ObjectKey((*key).to_owned()),
            StaticForwardStep::ArrayIndex(index) => StepRecord::ArrayIndex(*index),
            StaticForwardStep::ArrayRange { start, end } => StepRecord::ArrayRange {
                start: *start,
                end: *end,
            },
        }
    }
}

/// The format magic: the four bytes `JQFP`.
const MAGIC: &[u8; 4] = b"JQFP";
/// The current format version. Bump when the field layout changes. v10
/// omits the identity and `range_locate` bools (the [`ShortcutKind`] tag
/// already carries both). v9 wrote those bools plus the closed shortcut
/// tag; v8 stored four route bools; v7 added `topk_rows`. The decoder
/// accepts 7, 8, and 9.
const VERSION: u32 = 10;

/// The length-prefix byte width of the plan format.
const LEN_BYTES: usize = 4;

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn bool(&mut self, value: bool) {
        self.buf.push(u8::from(value));
    }

    /// A length prefix. An oversize length clamps to `u32::MAX` rather than
    /// panicking; the writer's inputs are self-produced plans whose field
    /// collections are bounded far below 2^32 elements, so the clamp is a
    /// totality measure, not a reachable path.
    fn len(&mut self, len: usize) {
        self.u32(u32::try_from(len).unwrap_or(u32::MAX));
    }

    fn opt_i64(&mut self, value: Option<i64>) {
        self.bool(value.is_some());
        if let Some(value) = value {
            self.i64(value);
        }
    }

    fn string(&mut self, value: &str) {
        self.len(value.len());
        self.bytes(value.as_bytes());
    }

    fn class(&mut self, class: &ClassRecord) {
        match class {
            ClassRecord::Structure => self.u8(0),
            ClassRecord::Fields(fields) => {
                self.u8(1);
                self.len(fields.len());
                for name in fields {
                    self.string(name);
                }
            }
            ClassRecord::Subtree => self.u8(2),
        }
    }

    fn steps(&mut self, steps: &[StepRecord]) {
        self.len(steps.len());
        for step in steps {
            self.step(step);
        }
    }

    fn step(&mut self, step: &StepRecord) {
        match step {
            StepRecord::ObjectKey(key) => {
                self.u8(0);
                self.string(key);
            }
            StepRecord::ArrayIndex(index) => {
                self.u8(1);
                self.i64(*index);
            }
            StepRecord::ArrayRange { start, end } => {
                self.u8(2);
                self.opt_i64(*start);
                self.opt_i64(*end);
            }
        }
    }

    fn opt_consumer(&mut self, consumer: Option<BoundaryConsumer>) {
        self.bool(consumer.is_some());
        if let Some(consumer) = consumer {
            self.u8(match consumer {
                BoundaryConsumer::Residual => 0,
                BoundaryConsumer::Fold => 1,
                BoundaryConsumer::Binding => 2,
                BoundaryConsumer::Collect => 3,
            });
        }
    }

    fn shortcut(&mut self, shortcut: ShortcutKind) {
        self.u8(match shortcut {
            ShortcutKind::None => 0,
            ShortcutKind::Count => 1,
            ShortcutKind::Keys => 2,
            ShortcutKind::Type => 3,
            ShortcutKind::Element => 4,
            ShortcutKind::RangeLocate => 5,
            ShortcutKind::Identity => 6,
            ShortcutKind::Has => 7,
            ShortcutKind::AnyAll => 8,
            ShortcutKind::MinMax => 9,
        });
    }

    fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], PlanError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(PlanError::Truncated)?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn bytes(&mut self) -> Result<&'a [u8], PlanError> {
        let len = self.len()?;
        self.take(len)
    }

    fn len(&mut self) -> Result<usize, PlanError> {
        let raw = self.take(LEN_BYTES)?;
        let value = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        // Every supported target's `usize` holds a `u32`, so the conversion
        // cannot fail; the expectation names the format invariant.
        Ok(usize::try_from(value).expect("a length prefix fits usize"))
    }

    /// The declared element count of a length-prefixed collection, refused
    /// before the first allocation when no encoding of that many elements
    /// could fit in the remaining bytes (every element consumes at least one).
    fn declared_count(&mut self) -> Result<usize, PlanError> {
        let count = self.len()?;
        if count > self.bytes.len() - self.pos {
            return Err(PlanError::Truncated);
        }
        Ok(count)
    }

    fn u8(&mut self) -> Result<u8, PlanError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, PlanError> {
        let raw = self.take(4)?;
        Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn u64(&mut self) -> Result<u64, PlanError> {
        let raw = self.take(8)?;
        let mut out = [0u8; 8];
        out.copy_from_slice(raw);
        Ok(u64::from_le_bytes(out))
    }

    fn i64(&mut self) -> Result<i64, PlanError> {
        let raw = self.take(8)?;
        let mut out = [0u8; 8];
        out.copy_from_slice(raw);
        Ok(i64::from_le_bytes(out))
    }

    fn bool(&mut self) -> Result<bool, PlanError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PlanError::InvalidBool),
        }
    }

    fn opt_i64(&mut self) -> Result<Option<i64>, PlanError> {
        if self.bool()? { self.i64().map(Some) } else { Ok(None) }
    }

    fn string(&mut self) -> Result<String, PlanError> {
        let bytes = self.bytes()?;
        core::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| PlanError::InvalidUtf8)
    }

    fn class(&mut self) -> Result<ClassRecord, PlanError> {
        match self.u8()? {
            0 => Ok(ClassRecord::Structure),
            1 => {
                let count = self.declared_count()?;
                let mut fields = Vec::new();
                for _ in 0..count {
                    fields.push(self.string()?);
                }
                Ok(ClassRecord::Fields(fields))
            }
            2 => Ok(ClassRecord::Subtree),
            _ => Err(PlanError::InvalidEnum("projection class")),
        }
    }

    fn steps(&mut self) -> Result<Vec<StepRecord>, PlanError> {
        let count = self.declared_count()?;
        let mut steps = Vec::new();
        for _ in 0..count {
            steps.push(self.step()?);
        }
        Ok(steps)
    }

    fn step(&mut self) -> Result<StepRecord, PlanError> {
        match self.u8()? {
            0 => Ok(StepRecord::ObjectKey(self.string()?)),
            1 => Ok(StepRecord::ArrayIndex(self.i64()?)),
            2 => Ok(StepRecord::ArrayRange {
                start: self.opt_i64()?,
                end: self.opt_i64()?,
            }),
            _ => Err(PlanError::InvalidEnum("forward step")),
        }
    }

    fn opt_consumer(&mut self) -> Result<Option<BoundaryConsumer>, PlanError> {
        if !self.bool()? {
            return Ok(None);
        }
        match self.u8()? {
            0 => Ok(Some(BoundaryConsumer::Residual)),
            1 => Ok(Some(BoundaryConsumer::Fold)),
            2 => Ok(Some(BoundaryConsumer::Binding)),
            3 => Ok(Some(BoundaryConsumer::Collect)),
            _ => Err(PlanError::InvalidEnum("boundary consumer")),
        }
    }

    fn shortcut(&mut self) -> Result<ShortcutKind, PlanError> {
        match self.u8()? {
            0 => Ok(ShortcutKind::None),
            1 => Ok(ShortcutKind::Count),
            2 => Ok(ShortcutKind::Keys),
            3 => Ok(ShortcutKind::Type),
            4 => Ok(ShortcutKind::Element),
            5 => Ok(ShortcutKind::RangeLocate),
            6 => Ok(ShortcutKind::Identity),
            7 => Ok(ShortcutKind::Has),
            8 => Ok(ShortcutKind::AnyAll),
            9 => Ok(ShortcutKind::MinMax),
            _ => Err(PlanError::InvalidEnum("shortcut")),
        }
    }

    fn finished(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::borrow::ToOwned;
    use alloc::vec;

    /// A fully-populated record exercising every field and enum arm.
    fn populated() -> PlanRecord {
        PlanRecord {
            modifies: true,
            consumes_whole_document: false,
            morsel_static_path: false,
            uses_input_family: true,
            projection_class: ClassRecord::Fields(vec!["id".to_owned(), "name".to_owned()]),
            pushdown: vec![
                StepRecord::ObjectKey("catalog".to_owned()),
                StepRecord::ArrayIndex(-2),
                StepRecord::ArrayRange {
                    start: Some(1),
                    end: None,
                },
            ],
            rungs: RungEligibility { morsel: false },
            boundary_consumer: Some(BoundaryConsumer::Residual),
            topk_rows: 3,
            shortcut: ShortcutKind::Count,
            uses_inputs_cursor: false,
        }
    }

    /// v7/v8/v9 body: identity and `range_locate` bools still sit on that wire.
    fn write_legacy_body(w: &mut Writer, record: &PlanRecord, identity: bool, range_locate: bool) {
        w.bool(identity);
        w.bool(record.modifies);
        w.bool(record.consumes_whole_document);
        w.bool(record.morsel_static_path);
        w.bool(record.uses_input_family);
        w.class(&record.projection_class);
        w.steps(&record.pushdown);
        w.bool(range_locate);
        w.bool(record.rungs.morsel);
        w.opt_consumer(record.boundary_consumer);
        w.u64(record.topk_rows);
    }

    fn serialize_v7(record: &PlanRecord, identity: bool, range_locate: bool) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(MAGIC);
        w.u32(7);
        write_legacy_body(&mut w, record, identity, range_locate);
        w.finish()
    }

    fn serialize_v8(record: &PlanRecord, identity: bool, range_locate: bool, routes: [bool; 4]) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(MAGIC);
        w.u32(8);
        write_legacy_body(&mut w, record, identity, range_locate);
        w.bool(routes[0]);
        w.bool(routes[1]);
        w.bool(routes[2]);
        w.bool(routes[3]);
        w.bool(record.uses_inputs_cursor);
        w.finish()
    }

    fn serialize_v9(record: &PlanRecord, identity: bool, range_locate: bool) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(MAGIC);
        w.u32(9);
        write_legacy_body(&mut w, record, identity, range_locate);
        w.shortcut(record.shortcut);
        w.bool(record.uses_inputs_cursor);
        w.finish()
    }

    #[test]
    fn round_trip_preserves_every_fact() {
        let record = populated();
        let bytes = record.serialize();
        let decoded = PlanRecord::deserialize(&bytes).expect("decode should succeed");
        assert_eq!(record, decoded);
    }

    #[test]
    fn empty_record_round_trips() {
        let record = PlanRecord {
            modifies: false,
            consumes_whole_document: true,
            morsel_static_path: true,
            uses_input_family: false,
            projection_class: ClassRecord::Structure,
            pushdown: Vec::new(),
            rungs: RungEligibility { morsel: true },
            boundary_consumer: None,
            topk_rows: 0,
            shortcut: ShortcutKind::Identity,
            uses_inputs_cursor: false,
        };
        let bytes = record.serialize();
        let decoded = PlanRecord::deserialize(&bytes).expect("decode should succeed");
        assert_eq!(record, decoded);
    }

    #[test]
    fn all_enum_arms_round_trip() {
        let base = populated();
        let classes = [
            ClassRecord::Structure,
            ClassRecord::Fields(vec!["a".to_owned()]),
            ClassRecord::Subtree,
        ];
        for class in classes {
            let mut record = base.clone();
            record.projection_class = class;
            let decoded = PlanRecord::deserialize(&record.serialize()).expect("decode should succeed");
            assert_eq!(record, decoded);
        }
        let steps = [
            StepRecord::ObjectKey("k".to_owned()),
            StepRecord::ArrayIndex(0),
            StepRecord::ArrayRange {
                start: None,
                end: Some(5),
            },
        ];
        for step in steps {
            let mut record = base.clone();
            record.pushdown = vec![step];
            let decoded = PlanRecord::deserialize(&record.serialize()).expect("decode should succeed");
            assert_eq!(record, decoded);
        }
        let shortcuts = [
            ShortcutKind::None,
            ShortcutKind::Count,
            ShortcutKind::Keys,
            ShortcutKind::Type,
            ShortcutKind::Element,
            ShortcutKind::RangeLocate,
            ShortcutKind::Identity,
            ShortcutKind::Has,
            ShortcutKind::AnyAll,
            ShortcutKind::MinMax,
        ];
        for shortcut in shortcuts {
            let mut record = base.clone();
            record.shortcut = shortcut;
            let decoded = PlanRecord::deserialize(&record.serialize()).expect("decode should succeed");
            assert_eq!(record, decoded);
        }
    }

    #[test]
    fn serialization_is_byte_stable() {
        let record = populated();
        let decoded = PlanRecord::deserialize(&record.serialize()).expect("decode");
        assert_eq!(decoded.serialize(), record.serialize());
    }

    #[test]
    fn rejects_bad_magic() {
        let error = PlanRecord::deserialize(b"NOPE").expect_err("bad magic must fail");
        assert_eq!(error, PlanError::BadMagic);
    }

    #[test]
    fn v7_payload_decodes_shortcut_none() {
        let record = populated();
        let bytes = serialize_v7(&record, false, false);
        let decoded = PlanRecord::deserialize(&bytes).expect("v7 decodes");
        assert_eq!(decoded.shortcut, ShortcutKind::None);
        assert!(!decoded.uses_inputs_cursor);
        assert_eq!(decoded.topk_rows, record.topk_rows);
        assert_eq!(decoded.modifies, record.modifies);
        let identity = serialize_v7(&record, true, false);
        assert_eq!(
            PlanRecord::deserialize(&identity).expect("v7 identity").shortcut,
            ShortcutKind::Identity
        );
        let range = serialize_v7(&record, false, true);
        assert_eq!(
            PlanRecord::deserialize(&range).expect("v7 range").shortcut,
            ShortcutKind::RangeLocate
        );
    }

    #[test]
    fn v8_payload_decodes_count_flag_as_count_shortcut() {
        let record = populated();
        let bytes = serialize_v8(&record, false, false, [true, false, false, false]);
        let decoded = PlanRecord::deserialize(&bytes).expect("v8 decodes");
        assert_eq!(decoded.shortcut, ShortcutKind::Count);
        assert!(!decoded.uses_inputs_cursor);
        assert_eq!(decoded.topk_rows, record.topk_rows);
    }

    #[test]
    fn v9_payload_skips_identity_and_range_locate() {
        let record = populated();
        let bytes = serialize_v9(&record, true, true);
        let decoded = PlanRecord::deserialize(&bytes).expect("v9 decodes");
        assert_eq!(decoded.shortcut, record.shortcut);
        assert_eq!(decoded.rungs.morsel, record.rungs.morsel);
        assert_eq!(decoded.modifies, record.modifies);
    }

    #[test]
    fn rejects_bad_version() {
        let mut bytes = populated().serialize();
        bytes[4] = 99;
        let error = PlanRecord::deserialize(&bytes).expect_err("bad version must fail");
        assert_eq!(error, PlanError::BadVersion(99));
    }

    #[test]
    fn rejects_truncated_input() {
        let bytes = populated().serialize();
        assert!(PlanRecord::deserialize(&bytes[..bytes.len() - 1]).is_err());
        assert!(PlanRecord::deserialize(&[]).is_err());
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = populated().serialize();
        bytes.push(0);
        let error = PlanRecord::deserialize(&bytes).expect_err("trailing bytes must fail");
        assert_eq!(error, PlanError::Overrun);
    }
}
