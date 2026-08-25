//! Count answers from a document's built skeleton, without building leaves.
//!
//! Walks a container path and either returns a count or declines. A declined demand is the caller's problem: they run
//! the ordinary materializing path. Never guesses.
//!
//! [`CountRow::Collect`] sums one output per item of a static key/index suffix (a miss is null, which still counts as
//! one). [`CountRow::Container`] is the container's own length. A span-backed container asks
//! [`LazySpanMaterializer::count_span`]. A string or number declines.

use jqf_resource::ResourceContext;

use alloc::vec::Vec;

use crate::document::NodeSemantic;
use crate::{DataError, Document, SliceRange, Value, ValueKind, ValueView, resolve_index};

/// Resolves one normalized (non-negative or open) slice range against an observed container length into an
/// inclusive-start/exclusive-end span.
///
/// The range is the BOUNDARY LAW's normalized reading ([`crate::SliceRange`]): v1 admits only bounds a non-negative
/// integer can carry, so against the actual length the reference's resolution is a pure clamp — no len-relative wrap,
/// no rounding. An absent bound is the container edge.
#[must_use]
pub(crate) fn resolve_range(len: usize, range: Option<SliceRange>) -> (usize, usize) {
    // Resolve one bound against the observed length: an absent bound is the edge (`0` for the start, `len` for the
    // end), and a present bound is clamped — negative to `0`, then cast with a fallback that only fires on 32-bit
    // widths, then capped at `len`. The chain, not any single step, is what keeps the result inside `[0, len]`.
    let resolve = |value: Option<i64>, edge: usize| -> usize {
        match value {
            None => edge,
            Some(value) => usize::try_from(value.max(0)).unwrap_or(len).min(len),
        }
    };
    match range {
        None => (0, len),
        Some((start, end)) => {
            let start = resolve(start, 0);
            let end = resolve(end, len);
            (start, end.max(start))
        }
    }
}

/// Descends tag-LAYER nodes to their payload view ([`Document::payload_view`]'s law): a kindless layer owns exactly one
/// payload occurrence, so a probe walk sees through it instead of raising [`DataError::UnrepresentableSemantic`] from
/// [`ValueView::kind`]. A non-layer node is its own answer.
pub(crate) fn descend_tag_layers<'document, 'source>(
    document: &'document Document<'source>,
    view: ValueView<'document, 'source>,
) -> Result<ValueView<'document, 'source>, DataError> {
    match document.tag_payload(view.node())? {
        None => Ok(view),
        Some(_) => document.payload_view(document.node_handle(view.node())?),
    }
}

/// One step of a count demand: an object key or an array index.
///
/// One static key or index step. Used on the container path and on the per-item probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CountStep {
    /// Navigate the object member named `key`.
    ObjectKey(alloc::string::String),
    /// Array child at this signed index. Negative counts from the end.
    ArrayIndex(i64),
}

/// Which count this demand wants.
///
/// [`Self::Collect`] iterates (`.[]`): a null or non-iterable container is a runtime error. [`Self::Container`] is an
/// array's element count or an object's member count (`PATH | length` and `PATH | keys | length` share the row). Null,
/// missing, and every other kind decline; the floor owns `null | length` = 0 and `null | keys | length` = raise. A
/// string's codepoint count is not available from the skeleton.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CountRow {
    /// `[C[] SUFFIX] | length`: Σ per item of the probe's outputs.
    Collect,
    /// `PATH | length` / `PATH | keys | length`: the container's element/member count.
    Container,
}

/// One count demand: container path plus per-item probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CountDemand {
    /// The count row this demand answers.
    pub row: CountRow,
    /// The container's static forward path (empty is the root).
    pub path: Vec<CountStep>,
    /// Count only this slice of the container. `None` is the whole container. Bounds are non-negative or open.
    pub range: Option<SliceRange>,
    /// The per-item residual steps (empty reads nothing of an item).
    pub probe: Vec<CountStep>,
    /// The per-item filter predicate of a collect-filter row (`[C[] | select(.k > LITERAL)] | length`). A filter row
    /// carries NO probe steps — the filter IS the per-item read — and every item contributes 0 or 1: `select` emits
    /// its input once per truthy predicate output and the admitted predicates are provably single-output over their
    /// domain. `None` for the classic rows.
    pub filter: Option<CountFilter>,
}

/// The comparison operator of a collect-filter predicate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CountCompare {
    /// `==`
    Equal,
    /// `!=`
    NotEqual,
    /// `<`
    Less,
    /// `<=`
    LessEqual,
    /// `>`
    Greater,
    /// `>=`
    GreaterEqual,
}

/// The right-hand scalar of a collect-filter comparison: the closed literal vocabulary a count route can compare
/// against without materializing the tested member. Numbers carry their exact decimal parts (sign-stripped digit text
/// plus scale) because the answer must be the engine's EXACT value law, never a binary64 rounding. Array/object
/// literals decline at recognition: they would open same-rank deep compares the closed law does not own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CountLiteral {
    /// The `null` literal.
    Null,
    /// A boolean literal.
    Bool(bool),
    /// An exact finite decimal: sign-stripped digit text, its scale, sign.
    Decimal {
        /// The sign (the digit text never carries one).
        negative: bool,
        /// Sign-stripped digit text (`"0"` for zero).
        digits: alloc::string::String,
        /// The decimal scale: value = digits × 10^-scale.
        scale: i64,
    },
    /// A text literal.
    Text(alloc::string::String),
}

/// The test a collect-filter row applies to one member read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CountTest {
    /// The member's truthiness — only `false` and `null` are falsy.
    Truthy,
    /// The member compared against the compiled literal.
    Compare {
        /// The comparison operator.
        op: CountCompare,
        /// The compiled right-hand scalar.
        rhs: CountLiteral,
    },
}

/// The per-item filter predicate of a collect-filter row: one static key path to the tested member plus the closed
/// test. The engine's recognizer admits only paths it can prove single-output over the item's domain (Key-only steps,
/// no optionals).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CountFilter {
    /// Static Key steps from the item to the tested value.
    pub path: Vec<CountStep>,
    /// The test applied to that value.
    pub test: CountTest,
}

/// One tested member, as the two reader paths classify it: an owned value (the materialized walk and the trait-default
/// leaf) or the span-scan classification of validated JSON text. The scan classes mirror what a decoded value could be;
/// a float-category number is not expressible here because a span leaf that meets one declines instead.
#[derive(Clone, Copy, Debug)]
pub enum CountMember<'a> {
    /// An owned value from a built document or materialized span.
    Value(&'a Value),
    /// The `null` value (also an absent member's read).
    Null,
    /// A boolean.
    Bool(bool),
    /// Decoded text.
    Text(&'a str),
    /// An exact decimal: sign-stripped digit text plus scale.
    Decimal {
        /// The sign.
        negative: bool,
        /// Sign-stripped digit text.
        digits: &'a str,
        /// The decimal scale.
        scale: i64,
    },
    /// An array.
    Array,
    /// An object.
    Object,
}

impl CountLiteral {
    fn band(&self) -> u8 {
        match self {
            Self::Null => 0,
            Self::Bool(_) => 1,
            Self::Decimal { .. } => 2,
            Self::Text(_) => 3,
        }
    }

    /// The exact ordering of two decimal parts: sign, then adjusted exponent (digit count minus scale), then padded
    /// digit text. Zero is normalized across spellings (`-0 == 0`, any scale). No big-integer arithmetic: equal
    /// adjusted exponents make lexicographic digit order the value order, and unequal ones decide by exponent alone.
    ///
    /// Precondition: both digit texts are canonical — no integer-part leading zeros. Both producers reach this
    /// comparison only after validated strict JSON, which guarantees that canonicality.
    fn decimal_cmp(
        negative_a: bool,
        digits_a: &str,
        scale_a: i64,
        negative_b: bool,
        digits_b: &str,
        scale_b: i64,
    ) -> core::cmp::Ordering {
        use core::cmp::Ordering;
        let zero_a = digits_a.bytes().all(|b| b == b'0');
        let zero_b = digits_b.bytes().all(|b| b == b'0');
        match (zero_a, zero_b) {
            (true, true) => return Ordering::Equal,
            (true, false) => {
                return if negative_b { Ordering::Greater } else { Ordering::Less };
            }
            (false, true) => {
                return if negative_a { Ordering::Less } else { Ordering::Greater };
            }
            (false, false) => {}
        }
        match (negative_a, negative_b) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let magnitude = |digits: &str, scale: i64| i128::from(digits.len() as u64) - i128::from(scale);
        let (adj_a, adj_b) = (magnitude(digits_a, scale_a), magnitude(digits_b, scale_b));
        // Both operands share one sign here; flip the magnitude result for the negative band.
        let flip = |ordering: Ordering| {
            if negative_a { ordering.reverse() } else { ordering }
        };
        if adj_a != adj_b {
            return flip(adj_a.cmp(&adj_b));
        }
        // Equal adjusted exponents: compare the digit slices with a virtual right-pad — the same digits at the same
        // decimal position order by digit value, and a missing position reads zero. Byte comparison is safe: the digit
        // text is ASCII. No allocation on this path; it runs once per tested array element.
        let len = digits_a.len().max(digits_b.len());
        let digit_at = |digits: &str, index: usize| digits.as_bytes().get(index).copied().unwrap_or(b'0');
        let mut ordering = Ordering::Equal;
        for index in 0..len {
            ordering = digit_at(digits_a, index).cmp(&digit_at(digits_b, index));
            if ordering != Ordering::Equal {
                break;
            }
        }
        flip(ordering)
    }
}

impl CountTest {
    /// The one evaluation law both reader paths share: rank two members under the engine's total order restricted to
    /// the closed literal vocabulary (`null < false < true < number < string`), compare within-band exactly, and treat
    /// cross-band comparisons as pure rank — the reference's lenient law, whose strict/warn cells live on the floor
    /// because a count route is lenient-only. `None` is a decline: a member the closed law cannot rank (a
    /// float-category number).
    #[must_use]
    pub fn answer(&self, member: CountMember<'_>) -> Option<bool> {
        // Truthiness needs no ranking: only false and null are falsy.
        if let Self::Truthy = self {
            return Some(match member {
                CountMember::Value(value) => !matches!(value.untagged(), Value::Null | Value::Bool(false)),
                CountMember::Null | CountMember::Bool(false) => false,
                CountMember::Bool(true)
                | CountMember::Text(_)
                | CountMember::Decimal { .. }
                | CountMember::Array
                | CountMember::Object => true,
            });
        }
        let (op, rhs) = match self {
            Self::Compare { op, rhs } => (*op, rhs),
            Self::Truthy => unreachable!("the Truthy arm returned above"),
        };
        // Equality is TAG-SENSITIVE in the engine (`semantic_eq`), while ordering unwraps tags first
        // (`order::compare`). This law reads payloads, so an EQUALITY over a tagged member cannot be answered here —
        // decline and let the floor answer it. Ordering keeps the payload rank, which is exactly what the engine
        // computes.
        if matches!(op, CountCompare::Equal | CountCompare::NotEqual)
            && matches!(member, CountMember::Value(value) if matches!(value, Value::Tagged { .. }))
        {
            return None;
        }
        let ordering = rank_against_literal(member, rhs)?;
        Some(match op {
            CountCompare::Equal => ordering == core::cmp::Ordering::Equal,
            CountCompare::NotEqual => ordering != core::cmp::Ordering::Equal,
            CountCompare::Less => ordering == core::cmp::Ordering::Less,
            CountCompare::LessEqual => ordering != core::cmp::Ordering::Greater,
            CountCompare::Greater => ordering == core::cmp::Ordering::Greater,
            CountCompare::GreaterEqual => ordering != core::cmp::Ordering::Less,
        })
    }
}

/// Ranks one classified member against the compiled literal under the engine's band order. `None` only for a member the
/// closed law cannot rank: a float-category number behind [`CountMember::Value`].
fn rank_against_literal(member: CountMember<'_>, rhs: &CountLiteral) -> Option<core::cmp::Ordering> {
    use core::cmp::Ordering;
    // The member's band under the engine's total order restricted to the closed vocabulary; None only for a value the
    // law cannot rank.
    let band = match member {
        CountMember::Value(value) => match value.untagged() {
            Value::Null => Some(0),
            Value::Bool(_) => Some(1),
            Value::Number(_) => Some(2),
            Value::String(_) => Some(3),
            Value::Array(_) => Some(4),
            // The match runs on `value.untagged()`, so a TAGGED value ranks by its PAYLOAD — exactly the engine's
            // ordering law (`order::compare` unwraps tags before ranking). Equality over a tagged member never reaches
            // this table: the answer law declines it first (engine equality is tag-sensitive).
            Value::Object(_) => Some(5),
            _ => None,
        },
        CountMember::Null => Some(0),
        CountMember::Bool(_) => Some(1),
        CountMember::Decimal { .. } => Some(2),
        CountMember::Text(_) => Some(3),
        CountMember::Array => Some(4),
        CountMember::Object => Some(5),
    }?;
    match band.cmp(&rhs.band()) {
        Ordering::Equal => {}
        other => return Some(other),
    }
    // Same band: dispatch on the literal's shape and extract the matching exact parts from the member.
    match rhs {
        CountLiteral::Null => {
            // The bands already agreed, so the member is null too.
            let _ = member;
            Some(Ordering::Equal)
        }
        CountLiteral::Bool(rhs_bool) => {
            let member_bool = match member {
                CountMember::Bool(value) => Some(value),
                CountMember::Value(value) => match value.untagged() {
                    Value::Bool(value) => Some(*value),
                    _ => None,
                },
                _ => None,
            }?;
            Some(member_bool.cmp(rhs_bool))
        }
        CountLiteral::Text(rhs_text) => {
            let member_text = match member {
                CountMember::Text(text) => Some(text),
                CountMember::Value(value) => match value.untagged() {
                    Value::String(text) => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            }?;
            Some(member_text.cmp(rhs_text))
        }
        CountLiteral::Decimal {
            negative,
            digits,
            scale,
        } => {
            // The member's exact parts: borrowed where the storage retains digit text; a machine integer renders into
            // this frame's stack buffer, keeping the per-element compare heap-free.
            let mut machine_text = [0u8; MACHINE_DIGIT_MAX];
            let member_parts = match member {
                CountMember::Decimal {
                    negative,
                    digits,
                    scale,
                } => (negative, digits, scale),
                CountMember::Value(value) => match value.untagged() {
                    Value::Number(number) => match number_parts(number)? {
                        ExactParts::Retained {
                            negative,
                            digits,
                            scale,
                        } => (negative, digits, scale),
                        ExactParts::Machine(machine) => {
                            let negative = machine < 0;
                            (
                                negative,
                                render_machine_digits(machine.unsigned_abs(), &mut machine_text),
                                0,
                            )
                        }
                    },
                    _ => return None,
                },
                _ => return None,
            };
            Some(CountLiteral::decimal_cmp(
                member_parts.0,
                member_parts.1,
                member_parts.2,
                *negative,
                digits,
                *scale,
            ))
        }
    }
}

/// Longest machine-integer magnitude rendered into the buffer: 19 digits. `u64::MAX` itself is 20 digits, but every
/// caller renders an `i64` magnitude via `unsigned_abs`, and `i64::MIN`'s magnitude (2^63) is 19 digits.
const MACHINE_DIGIT_MAX: usize = 19;

/// Renders a machine-integer magnitude into the caller's buffer, most significant digit first. Digits are ASCII, so the
/// slice is valid UTF-8.
fn render_machine_digits(magnitude: u64, buffer: &mut [u8; MACHINE_DIGIT_MAX]) -> &str {
    let mut len = 0;
    let mut value = magnitude;
    loop {
        buffer[len] = b'0' + (value % 10) as u8;
        len += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    buffer[..len].reverse();
    core::str::from_utf8(&buffer[..len]).expect("decimal digits are ASCII")
}

/// The exact decimal parts of an owned number, borrowing the digit text the storage retains. A machine integer carries
/// its value and is rendered into a stack buffer at compare time.
enum ExactParts<'a> {
    Retained {
        negative: bool,
        digits: &'a str,
        scale: i64,
    },
    Machine(i64),
}

fn number_parts(number: &crate::Number) -> Option<ExactParts<'_>> {
    match number.category() {
        crate::NumberCategory::Integer => {
            if let Some(machine) = number.as_machine() {
                return Some(ExactParts::Machine(machine));
            }
            let integer = number.as_integer()?;
            let text = integer.as_str();
            let negative = text.starts_with('-');
            Some(ExactParts::Retained {
                negative,
                digits: text.trim_start_matches('-'),
                scale: 0,
            })
        }
        crate::NumberCategory::Decimal => {
            let decimal = number.as_decimal()?;
            let coefficient = decimal.coefficient().as_str();
            let negative = coefficient.starts_with('-');
            Some(ExactParts::Retained {
                negative,
                digits: coefficient.trim_start_matches('-'),
                scale: decimal.scale(),
            })
        }
        crate::NumberCategory::Float => None,
    }
}

impl CountFilter {
    /// Navigates this filter's static key path over one owned item and answers the test — the per-item contribution
    /// law of the default (materialized) walk. Key steps follow the reference's null precedence: an absent member and a
    /// null member read alike, and a key step over a non-object RAISES in the reference, so `None` (decline) lets the
    /// floor render it. Returns the item's contribution: 0 or 1.
    #[must_use]
    pub fn contributes(&self, item: &Value) -> Option<u64> {
        let mut view = item.untagged();
        for step in &self.path {
            match view {
                Value::Object(object) => {
                    let CountStep::ObjectKey(key) = step else {
                        // The recognizer admits Key-only paths; an index here would be a recognizer defect, so decline.
                        return None;
                    };
                    match object.get(key.as_str()) {
                        // The member value; descend.
                        Some(child) => view = child.untagged(),
                        // The absent member is the reference's null; the remaining steps are total over it.
                        None => return self.test.answer(CountMember::Null).map(u64::from),
                    }
                }
                // A null item reads null through every remaining step; a non-object raises in the reference — decline
                // to the floor.
                Value::Null => return self.test.answer(CountMember::Null).map(u64::from),
                _ => return None,
            }
        }
        self.test.answer(CountMember::Value(view)).map(u64::from)
    }
}

/// Count answer, or a decline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CountVerdict {
    /// Proven count.
    Count(u64),
    /// Could not prove the answer. Caller should run the ordinary path.
    Decline,
}

impl<'document> Document<'document> {
    /// Answers one count demand from this document's skeleton, without materializing leaves.
    ///
    /// Navigates the container path over the built skeleton, then:
    ///
    /// - a BUILT container is counted from the projection arenas (an array's items, an object's members — duplicates
    ///   already resolved by the build) and the probe is walked per item over the arenas;
    /// - a deferred [`crate::document::ContainerSpanKind`] container is   counted by the format-owned
    ///   [`crate::LazySpanMaterializer::count_span`] leaf;
    /// - any shape the skeleton cannot prove (null or missing, a non-container, a path step through a span, a
    ///   string/number   container, a probe the step cannot handle) is [`CountVerdict::Decline`].
    ///
    /// A missing step on the way to the container declines; the floor renders it (`PATH | length` answers 0, `PATH |
    /// keys | length` raises).
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] only for a genuinely invalid document or a refused capability; every "cannot prove it"
    /// shape is a [`CountVerdict::Decline`], never an error.
    pub fn count_children_demand(
        &self,
        demand: &CountDemand,
        resources: &mut ResourceContext<'_>,
    ) -> Result<CountVerdict, DataError> {
        // A document without semantic nodes cannot be navigated; the caller's ordinary route serves it.
        let Ok(root) = self.value_view(self.root_handle()) else {
            return Ok(CountVerdict::Decline);
        };
        let mut view = root;
        for step in &demand.path {
            match step {
                CountStep::ObjectKey(key) => {
                    // A path step through a deferred span cannot navigate without materializing, and a key step over a
                    // non-object is the reference's index-class raise; either way decline and let the floor render it.
                    let Ok(Some(object)) = view.object() else {
                        return Ok(CountVerdict::Decline);
                    };
                    // A missing key is the reference's null. Both rows decline: `PATH | length` answers 0 on the floor,
                    // `PATH | keys | length` raises, and Collect's `null | .[]` raises. The consumer cannot tell them
                    // apart.
                    view = match object.get(key.as_str()) {
                        None => return Ok(CountVerdict::Decline),
                        Some(child) => child,
                    };
                }
                CountStep::ArrayIndex(index) => {
                    let Ok(Some(array)) = view.array() else {
                        return Ok(CountVerdict::Decline);
                    };
                    let len = array.len();
                    match resolve_index(len, *index) {
                        // Out of range is the reference's null (see the key arm): decline and let the floor render it.
                        None => return Ok(CountVerdict::Decline),
                        Some(resolved) => match array.get(resolved) {
                            None => return Ok(CountVerdict::Decline),
                            Some(child) => view = child,
                        },
                    }
                }
            }
        }
        match demand.row {
            CountRow::Container => self.count_container(&view, demand.range, resources),
            CountRow::Collect => {
                self.count_collect(&view, demand.range, &demand.probe, demand.filter.as_ref(), resources)
            }
        }
    }

    /// The [`CountRow::Container`] answer for the resolved container view.
    fn count_container(
        &self,
        view: &ValueView<'_, 'document>,
        range: Option<SliceRange>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<CountVerdict, DataError> {
        // A tag-LAYER node is payload-transparent: see through it before the category probe, exactly as payload_view
        // does.
        let view = &descend_tag_layers(self, *view)?;
        if view.is_container_span()? {
            return self.count_span_leaf(view, range, &[], resources);
        }
        match view.kind()? {
            ValueKind::Array => {
                // A kind that says Array with no array view behind it is storage corruption, not an empty container:
                // refuse exactly as the element walk does.
                let array = view.array()?.ok_or(DataError::InvalidDocument)?;
                // The reference's slice over an array materializes the in-range elements; their count is the resolved
                // span's width.
                let (start, end) = resolve_range(array.len(), range);
                Ok(CountVerdict::Count((end - start) as u64))
            }
            ValueKind::Object => {
                // A slice over an OBJECT is the reference's "Cannot index object with number" raise; the floor renders
                // it. A range-bearing demand over an object declines; the plain member count stands for the no-range
                // spelling.
                if range.is_some() {
                    return Ok(CountVerdict::Decline);
                }
                let object = view.object()?.ok_or(DataError::InvalidDocument)?;
                Ok(CountVerdict::Count(object.len() as u64))
            }
            // Null is `length` = 0 and `keys | length` = raise. Decline so the floor runs the original program.
            // `null[a:b]` stays null either way. A string's codepoint count, a number's magnitude, and the other
            // no-length kinds are payload reads; the floor owns them all the same way.
            _ => Ok(CountVerdict::Decline),
        }
    }

    /// The [`CountRow::Collect`] answer for the resolved container view.
    ///
    /// A filter row (a collect-filter demand's per-item 0-or-1 predicate) is v1-scoped to the span seam: the BUILT arm
    /// declines it, because the built walk's probe law has no filter half yet and the floor reproduces the answer byte
    /// for byte. A missed win, never a wrong one.
    fn count_collect(
        &self,
        view: &ValueView<'_, 'document>,
        range: Option<SliceRange>,
        probe: &[CountStep],
        filter: Option<&CountFilter>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<CountVerdict, DataError> {
        // A tag-LAYER node is payload-transparent: see through it before the category probe, exactly as payload_view
        // does.
        let view = &descend_tag_layers(self, *view)?;
        if view.is_container_span()? {
            if let Some(filter) = filter {
                return self.count_span_leaf_filtered(view, range, filter, resources);
            }
            return self.count_span_leaf(view, range, probe, resources);
        }
        // The built arms own only the static-probe law; a filter declines.
        if filter.is_some() {
            return Ok(CountVerdict::Decline);
        }
        match view.kind()? {
            ValueKind::Array => {
                let array = view.array()?.ok_or(DataError::InvalidDocument)?;
                let (start, end) = resolve_range(array.len(), range);
                // An empty probe emits exactly one output per item, so the collect sum IS the slice width; skip the
                // per-item walk.
                if probe.is_empty() {
                    return Ok(CountVerdict::Count((end - start) as u64));
                }
                let mut total = 0u64;
                for item in array.iter().skip(start).take(end - start) {
                    match Self::probe_contribution(&item, probe)? {
                        CountVerdict::Count(n) => total = total.saturating_add(n),
                        CountVerdict::Decline => return Ok(CountVerdict::Decline),
                    }
                }
                Ok(CountVerdict::Count(total))
            }
            ValueKind::Object => {
                // A slice over an OBJECT is the reference's raise; the floor renders it (the range-bearing demand
                // declines, the plain member collect stands).
                if range.is_some() {
                    return Ok(CountVerdict::Decline);
                }
                let object = view.object()?.ok_or(DataError::InvalidDocument)?;
                let mut total = 0u64;
                for entry in object.iter() {
                    let entry = entry?;
                    match Self::probe_contribution(&entry.value(), probe)? {
                        CountVerdict::Count(n) => total = total.saturating_add(n),
                        CountVerdict::Decline => return Ok(CountVerdict::Decline),
                    }
                }
                Ok(CountVerdict::Count(total))
            }
            // `null | .[]` is the reference's iterate-null raise; every non-container is the index-class raise.
            // Decline, and the floor renders the error byte for byte.
            _ => Ok(CountVerdict::Decline),
        }
    }

    /// The per-item probe walk over one built document item: how many outputs the residual emits for it.
    ///
    /// The reference's laws, exactly:
    ///
    /// - a null item contributes 1 — null's children are null, so every step   is total over it (the null
    ///   precedence);
    /// - a FINAL key/index step contributes 1 for its whole domain — a present member is its value, an ABSENT one is
    ///   the reference's null, and both are exactly one output — so the final step checks the   item's category and
    ///   nothing else;
    /// - an INTERMEDIATE step descends a present member/position and contributes 1 on an absent one (the missing value
    ///   is null, and the   remaining steps are total over it);
    /// - a category outside a step's domain, or a deferred span item the arenas cannot probe, DECLINES (the reference
    ///   raises there and the   floor must render it).
    fn probe_contribution(item: &ValueView<'_, 'document>, probe: &[CountStep]) -> Result<CountVerdict, DataError> {
        let mut view = *item;
        for (index, step) in probe.iter().enumerate() {
            // One node-record read per probed node serves the tag-layer test, the span decline, and the kind below. A
            // tag-LAYER node (the item itself or a navigated child) is payload-transparent; see through it instead of
            // raising, using the shared descent's own reads.
            let record = view.document.node_record(view.node())?;
            let layer = matches!(record.semantic, NodeSemantic::Unrepresentable) && record.intrinsic_tag.is_tagged();
            if layer {
                view = descend_tag_layers(view.document, view)?;
                if view.is_container_span()? {
                    return Ok(CountVerdict::Decline);
                }
            } else if matches!(record.semantic, NodeSemantic::ContainerSpan { .. }) {
                // A deferred item cannot be probed without materializing.
                return Ok(CountVerdict::Decline);
            }
            let kind = if layer {
                view.kind()?
            } else {
                record.semantic.kind().ok_or(DataError::UnrepresentableSemantic)?
            };
            if kind == ValueKind::Null {
                // Null's children are null: every remaining step is total.
                return Ok(CountVerdict::Count(1));
            }
            let final_step = index + 1 == probe.len();
            match step {
                CountStep::ObjectKey(key) => {
                    if kind != ValueKind::Object {
                        return Ok(CountVerdict::Decline);
                    }
                    if final_step {
                        // The member's presence is irrelevant: present or absent, the reference emits exactly one
                        // output.
                        return Ok(CountVerdict::Count(1));
                    }
                    let object = view.object()?.ok_or(DataError::InvalidDocument)?;
                    match object.get(key) {
                        Some(child) => view = child,
                        // The missing member is the reference's null; the remaining steps are total over it.
                        None => return Ok(CountVerdict::Count(1)),
                    }
                }
                CountStep::ArrayIndex(index) => {
                    if kind != ValueKind::Array {
                        return Ok(CountVerdict::Decline);
                    }
                    if final_step {
                        return Ok(CountVerdict::Count(1));
                    }
                    let array = view.array()?.ok_or(DataError::InvalidDocument)?;
                    match resolve_index(array.len(), *index) {
                        Some(resolved) => match array.get(resolved) {
                            Some(child) => view = child,
                            None => return Ok(CountVerdict::Count(1)),
                        },
                        None => return Ok(CountVerdict::Count(1)),
                    }
                }
            }
        }
        Ok(CountVerdict::Count(1))
    }

    /// Delegates a deferred container to the format-owned span-count leaf.
    fn count_span_leaf(
        &self,
        view: &ValueView<'_, 'document>,
        range: Option<SliceRange>,
        probe: &[CountStep],
        resources: &mut ResourceContext<'_>,
    ) -> Result<CountVerdict, DataError> {
        let Some((text, container, materializer)) = self.span_leaf_input(view, range.is_some())? else {
            return Ok(CountVerdict::Decline);
        };
        materializer.count_span(text, container, range, probe, resources)
    }

    /// The filter-row twin of [`Self::count_span_leaf`]: the format leaf evaluates the per-item predicate over the
    /// span's raw bytes. The default (materialize-and-walk) is correct for every codec; the strict-JSON codec overrides
    /// with a byte scan that never builds a leaf.
    fn count_span_leaf_filtered(
        &self,
        view: &ValueView<'_, 'document>,
        range: Option<SliceRange>,
        filter: &CountFilter,
        resources: &mut ResourceContext<'_>,
    ) -> Result<CountVerdict, DataError> {
        let Some((text, container, materializer)) = self.span_leaf_input(view, range.is_some())? else {
            return Ok(CountVerdict::Decline);
        };
        materializer.count_span_filtered(text, container, range, filter, resources)
    }

    /// The shared span-leaf prologue for the count and element consumers: the deferred container's text, kind, and
    /// bound materializer.
    ///
    /// `None` is a decline — a non-span node, a range demand over an object span (the index-class raise the built
    /// arms state), non-UTF-8 bytes, or an unbound leaf. The leaves read TEXT, so the UTF-8 validation is load-bearing:
    /// the raw-byte accessor is the only sound way to reach a span's bytes, and the validating step is what keeps the
    /// fail-closed decline over formats whose spans the leaves cannot read.
    pub(super) fn span_leaf_input<'view>(
        &'view self,
        view: &ValueView<'_, 'document>,
        has_range: bool,
    ) -> Result<
        Option<(
            &'view str,
            crate::document::ContainerSpanKind,
            &'view dyn crate::LazySpanMaterializer,
        )>,
        DataError,
    > {
        let record = self.node_record(view.node())?;
        let (bytes, container) = match &record.semantic {
            crate::document::NodeSemantic::ContainerSpan { text, container } => (self.bytes(*text), *container),
            _ => return Ok(None),
        };
        // A slice over an object is not a length we can prove. Decline.
        if has_range && container == crate::document::ContainerSpanKind::Object {
            return Ok(None);
        }
        let Some(text) = bytes.and_then(|bytes| core::str::from_utf8(bytes).ok()) else {
            return Ok(None);
        };
        let Some(materializer) = self.span_materializer() else {
            return Ok(None);
        };
        Ok(Some((text, container, materializer)))
    }
}

/// Counts one owned container value's children under a probe — the materialize-then-count fallback of
/// [`crate::LazySpanMaterializer::count_span`].
///
/// Shares the probe law with the document walk, stated at [`Document::probe_contribution`]: every key/index step is
/// total, so a present member and an absent one alike contribute 1, a null item short-circuits to 1, and a foreign
/// category declines.
pub(crate) fn count_owned_container(value: &Value, range: Option<SliceRange>, probe: &[CountStep]) -> CountVerdict {
    let value = value.untagged();
    match value {
        Value::Array(array) => {
            let (start, end) = resolve_range(array.len(), range);
            let mut total = 0u64;
            for item in array.iter().skip(start).take(end - start) {
                match owned_probe_contribution(item, probe) {
                    CountVerdict::Count(n) => total = total.saturating_add(n),
                    CountVerdict::Decline => return CountVerdict::Decline,
                }
            }
            CountVerdict::Count(total)
        }
        Value::Object(object) => {
            // The object-slice raise is declined at the span seam ([`Document::count_span_leaf`]), the only route into
            // this fallback, so `range` cannot be set here.
            let mut total = 0u64;
            for entry in object {
                match owned_probe_contribution(entry.value(), probe) {
                    CountVerdict::Count(n) => total = total.saturating_add(n),
                    CountVerdict::Decline => return CountVerdict::Decline,
                }
            }
            CountVerdict::Count(total)
        }
        _ => CountVerdict::Decline,
    }
}

/// Same walk as [`Document::probe_contribution`], over an owned item.
fn owned_probe_contribution(item: &Value, probe: &[CountStep]) -> CountVerdict {
    let mut view = item.untagged();
    for (index, step) in probe.iter().enumerate() {
        if matches!(view, Value::Null) {
            return CountVerdict::Count(1);
        }
        let final_step = index + 1 == probe.len();
        match (view, step) {
            (Value::Object(object), CountStep::ObjectKey(key)) => {
                if final_step {
                    return CountVerdict::Count(1);
                }
                match object.get(key) {
                    Some(child) => view = child.untagged(),
                    None => return CountVerdict::Count(1),
                }
            }
            (Value::Array(array), CountStep::ArrayIndex(index)) => {
                if final_step {
                    return CountVerdict::Count(1);
                }
                match resolve_index(array.len(), *index).and_then(|i| array.get(i)) {
                    Some(child) => view = child.untagged(),
                    None => return CountVerdict::Count(1),
                }
            }
            _ => return CountVerdict::Decline,
        }
    }
    CountVerdict::Count(1)
}

/// Counts one owned container's children under a collect-filter predicate — the filter twin of
/// [`count_owned_container`]. The container-kind law is the same: an array iterates its items, an object its member
/// values, and every non-container declines. A filter item the closed law cannot rank (or a navigation that raises in
/// the reference) declines the whole answer.
pub(crate) fn count_owned_container_filtered(
    value: &Value,
    range: Option<SliceRange>,
    filter: &CountFilter,
) -> CountVerdict {
    let value = value.untagged();
    let contribute = |item: &Value| -> Option<u64> { filter.contributes(item) };
    match value {
        Value::Array(array) => {
            let (start, end) = resolve_range(array.len(), range);
            let mut total = 0u64;
            for item in array.iter().skip(start).take(end - start) {
                match contribute(item) {
                    Some(n) => total = total.saturating_add(n),
                    None => return CountVerdict::Decline,
                }
            }
            CountVerdict::Count(total)
        }
        Value::Object(object) => {
            // The object-slice raise is declined at the span seam, the only route into this fallback with a range set.
            let mut total = 0u64;
            for entry in object {
                match contribute(entry.value()) {
                    Some(n) => total = total.saturating_add(n),
                    None => return CountVerdict::Decline,
                }
            }
            CountVerdict::Count(total)
        }
        _ => CountVerdict::Decline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn ledger() -> ResourceContext<'static> {
        let account = RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    fn key(text: &str) -> crate::ObjectKey {
        crate::ObjectKey::try_from_str(text).expect("fixture key")
    }

    /// The fixture's span leaf: every arm panics, so a test that reaches one has proved the decline it asserts did NOT
    /// happen.
    struct UnreachableLeaf;

    impl crate::LazySpanMaterializer for UnreachableLeaf {
        fn materialize_span(&self, _text: &str, _resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
            panic!("a span the consumer cannot read must decline before the leaf")
        }

        fn materialize_span_bytes(
            &self,
            _bytes: &[u8],
            _resources: &mut ResourceContext<'_>,
        ) -> Result<Value, DataError> {
            panic!("a span the consumer cannot read must decline before the leaf")
        }
    }

    static UNREACHABLE_LEAF: UnreachableLeaf = UnreachableLeaf;

    /// Publishes a one-node document whose ROOT is a deferred container span over `bytes`, with a leaf that refuses to
    /// be called.
    fn document_over_root_span(bytes: &'static [u8], resources: &mut ResourceContext<'_>) -> Document<'static> {
        let source = jqf_source::SourceRef::new(jqf_source::SourceId::new(7), jqf_source::SourceKind::Input);
        let resolved = jqf_source::ResolvedSource::new(source, source.kind().as_str(), bytes, 0);
        let recipe =
            crate::DocumentSchemaRecipe::try_new("test", None, &["test.array"], &[], &[], &[]).expect("recipe");
        let (mut builder, prepared) = crate::AccountedDocumentBuilder::try_new_prepared_with_coverage(
            &recipe,
            crate::BuilderCoverage::complete(),
        )
        .expect("builder");
        builder.bind_span_materializer(&UNREACHABLE_LEAF);
        builder
            .bind_source(crate::DocumentSourceBinding::from_resolved(resolved).expect("binding"))
            .expect("binding attaches");
        // SAFETY: the fixture's whole source IS the one span, and `bytes` is a static the builder outlives.
        let root = unsafe {
            builder.add_prepared_bound_container_span_node(
                &prepared,
                prepared.node_kind(0).expect("array kind"),
                jqf_source::Span::from_usize(0, bytes.len()),
                crate::ContainerSpanKind::Array,
                resources,
            )
        }
        .expect("span root");
        let mut finalizer = builder.begin_finish(root, resources).expect("finalizer");
        loop {
            match finalizer.poll(resources).expect("finalizer polls") {
                crate::DocumentFinalizationPoll::Pending => {
                    assert!(resources.try_begin_next_cooperative_entry(4096).expect("next entry"));
                }
                crate::DocumentFinalizationPoll::Ready(document) => return document,
            }
        }
    }

    /// A container span whose bytes are not UTF-8 DECLINES on both span-leaf seams instead of reading them as text.
    ///
    /// The leaves take text, and the only sound way to reach a span's bytes is the raw-byte accessor, so the validating
    /// step is load-bearing: without it the bytes reach an unchecked conversion. `0x80` is the row that bites — a
    /// continuation byte no UTF-8 sequence can start with, so a document whose span is one byte long already proves the
    /// check runs. Both seams answer `Decline`, never an error: the count consumer's contract is decline-never-error,
    /// and the element consumer's decline is fail-closed before any visit.
    #[test]
    fn a_span_that_is_not_utf8_declines_on_both_seams() {
        static BINARY: &[u8] = &[0x80, 0x81];
        let mut resources = ledger();
        let document = document_over_root_span(BINARY, &mut resources);

        let count = CountDemand {
            row: CountRow::Container,
            path: Vec::new(),
            range: None,
            probe: Vec::new(),
            filter: None,
        };
        assert_eq!(
            document
                .count_children_demand(&count, &mut resources)
                .expect("declines, never errors"),
            CountVerdict::Decline
        );

        let elements = crate::ElementDemand {
            row: crate::ElementRow::FanOut,
            path: Vec::new(),
            range: None,
            probe: crate::ElementProbe::Path(Vec::new()),
            increment: None,
        };
        assert_eq!(
            document
                .visit_elements(&elements, &mut resources, |_, _| Ok(()))
                .expect("declines, never errors"),
            crate::ElementVerdict::Decline
        );
    }

    fn object_with_name() -> Value {
        let mut builder = crate::ObjectBuilder::try_with_capacity(1).expect("builder");
        builder.try_insert_last(key("name"), Value::Null).expect("insert");
        Value::Object(builder.try_finish().expect("finish"))
    }

    fn empty_object() -> Value {
        Value::Object(
            crate::ObjectBuilder::try_with_capacity(0)
                .expect("builder")
                .try_finish()
                .expect("finish"),
        )
    }

    #[test]
    fn owned_probe_matches_the_reference_law() {
        let probe = [CountStep::ObjectKey(alloc::string::String::from("name"))];
        assert_eq!(
            owned_probe_contribution(&object_with_name(), &probe),
            CountVerdict::Count(1)
        );
        // A missing key is the reference's null: one output, not empty.
        assert_eq!(
            owned_probe_contribution(&empty_object(), &probe),
            CountVerdict::Count(1)
        );
        assert_eq!(owned_probe_contribution(&Value::Null, &probe), CountVerdict::Count(1));
        assert_eq!(
            owned_probe_contribution(
                &Value::Number(crate::Number::integer(crate::Integer::from_i64(5),)),
                &probe
            ),
            CountVerdict::Decline
        );
    }

    // -- Collect-filter evaluation law (both reader paths share it). -------

    fn compare(op: CountCompare, rhs: CountLiteral) -> CountTest {
        CountTest::Compare { op, rhs }
    }

    #[test]
    fn tagged_members_rank_by_payload_but_equality_declines() {
        let tagged_null = Value::Tagged {
            tag: crate::TagId::try_new_unaccounted("!money").expect("tag"),
            payload: crate::Shared::try_new(Value::Null).expect("payload"),
        };
        let zero = CountLiteral::Decimal {
            negative: false,
            digits: alloc::string::String::from("0"),
            scale: 0,
        };
        // ORDERING follows the engine exactly: `order::compare` unwraps tags BEFORE ranking, so a tagged null orders
        // BELOW every number — the answer is the payload's, `> 0` false. (A wrapper ranked at the object band would
        // answer TRUE here, a silent miscount.)
        assert_eq!(
            compare(CountCompare::Greater, zero.clone()).answer(CountMember::Value(&tagged_null)),
            Some(false)
        );
        // EQUALITY is tag-sensitive in the engine (`semantic_eq`), so this payload-reading law cannot answer it —
        // decline to the floor.
        assert_eq!(
            compare(CountCompare::Equal, zero).answer(CountMember::Value(&tagged_null)),
            None
        );
        // Truthiness is payload-transparent in the engine (`owned_is_truthy` recurses through tags): a tagged null is
        // falsy.
        assert_eq!(CountTest::Truthy.answer(CountMember::Value(&tagged_null)), Some(false));
    }

    #[test]
    fn members_the_closed_law_cannot_rank_decline() {
        // A float-category number has no exact decimal reading.
        let float = Value::Number(crate::Number::float(crate::Float::new(f64::NAN)));
        assert_eq!(
            compare(
                CountCompare::Equal,
                CountLiteral::Decimal {
                    negative: false,
                    digits: alloc::string::String::from("0"),
                    scale: 0
                }
            )
            .answer(CountMember::Value(&float)),
            None
        );
    }

    #[test]
    fn decimal_comparison_is_the_exact_value_order() {
        let dec = |negative: bool, digits: &str, scale: i64| CountLiteral::Decimal {
            negative,
            digits: alloc::string::String::from(digits),
            scale,
        };
        let member = |negative: bool, digits: &'static str, scale: i64| CountMember::Decimal {
            negative,
            digits,
            scale,
        };
        let gt = compare(CountCompare::Greater, dec(false, "0", 0));
        // Exponents and scales unify before digits compare.
        assert_eq!(gt.answer(member(false, "1", -3)), Some(true)); // 1e3 > 0
        assert_eq!(gt.answer(member(false, "1000", 3)), Some(true)); // 1.000 > 0
        assert_eq!(gt.answer(member(true, "1", -3)), Some(false)); // -1e3 < 0
        // Zero normalizes across sign and scale (-0 == 0 == 0e10).
        for spelling in [member(false, "0", 0), member(true, "0", 5), member(false, "00", -2)] {
            assert_eq!(
                compare(CountCompare::Equal, dec(false, "0", 0)).answer(spelling),
                Some(true),
                "{spelling:?}"
            );
        }
        // Same adjusted exponent: padded digit text decides.
        assert_eq!(
            compare(CountCompare::Less, dec(false, "12", 0)).answer(member(false, "120", 1)),
            Some(false)
        ); // 1.20e2 == 120: equal, so Less is false
        assert_eq!(
            compare(CountCompare::Equal, dec(false, "120", 1)).answer(member(false, "12", 0)),
            Some(true)
        );
        assert_eq!(
            compare(CountCompare::Less, dec(false, "13", 0)).answer(member(false, "12", 0)),
            Some(true)
        );
        // Cross-band ranks are pure rank: string > number.
        let text_gt_number = compare(CountCompare::Greater, dec(false, "5", 0));
        assert_eq!(text_gt_number.answer(CountMember::Text("x")), Some(true));
        // Bool band order: true ranks above false.
        assert_eq!(
            compare(CountCompare::Greater, CountLiteral::Bool(false)).answer(CountMember::Bool(true)),
            Some(true)
        );
    }

    #[test]
    fn machine_integers_compare_against_decimal_literals_exactly() {
        let answer = |machine: i64, op: CountCompare, rhs: CountLiteral| {
            compare(op, rhs).answer(CountMember::Value(&Value::Number(crate::Number::integer(
                crate::Integer::from_i64(machine),
            ))))
        };
        let dec = |negative: bool, digits: &str, scale: i64| CountLiteral::Decimal {
            negative,
            digits: alloc::string::String::from(digits),
            scale,
        };
        // i64::MIN renders all 19 magnitude digits; equal to its literal.
        assert_eq!(
            answer(i64::MIN, CountCompare::Equal, dec(true, "9223372036854775808", 0)),
            Some(true)
        );
        // Multi-digit magnitudes order digit by digit.
        assert_eq!(answer(100, CountCompare::Greater, dec(false, "99", 0)), Some(true));
        assert_eq!(answer(100, CountCompare::Less, dec(false, "101", 0)), Some(true));
        // Scale reconciles before digits: 10 == 1e1.
        assert_eq!(answer(10, CountCompare::Equal, dec(false, "1", -1)), Some(true));
        // The sign band decides before magnitude.
        assert_eq!(answer(-1, CountCompare::Less, dec(false, "0", 0)), Some(true));
    }

    #[test]
    fn contributes_follows_null_precedence_and_declines_raises() {
        use alloc::vec;
        let filter = CountFilter {
            path: vec![CountStep::ObjectKey(alloc::string::String::from("stock"))],
            test: compare(
                CountCompare::Greater,
                CountLiteral::Decimal {
                    negative: false,
                    digits: alloc::string::String::from("0"),
                    scale: 0,
                },
            ),
        };
        let object_with = |stock: Value| {
            let mut builder = crate::ObjectBuilder::try_with_capacity(1).expect("builder");
            builder.try_insert_last(key("stock"), stock).expect("insert");
            Value::Object(builder.try_finish().expect("finish"))
        };
        // An object item reads the member; an absent member reads null; a null item reads null through every step (the
        // null precedence).
        let present = object_with(Value::Number(crate::Number::integer(crate::Integer::from_i64(5))));
        let absent = object_with(Value::Null);
        assert_eq!(filter.contributes(&present), Some(1));
        assert_eq!(filter.contributes(&absent), Some(0));
        assert_eq!(filter.contributes(&Value::Null), Some(0));
        // A key step over a non-object raises in the reference — decline.
        let mut array = crate::Array::try_with_capacity(1).expect("builder");
        array
            .try_push(Value::Number(crate::Number::integer(crate::Integer::from_i64(1))))
            .expect("push");
        let array_item = Value::Array(array);
        assert_eq!(filter.contributes(&array_item), None);
    }
}
