//! Exact-path observation over a jqft/jqfjson text walk.
//!
//! The locate pass validates every byte (including unread siblings and trailing content) and records the last-wins
//! winner as a byte span plus, when the winner is a container, the child count proved on that walk. [`crate::scoped`]
//! publishes a cached skeleton for count/element Exact; print without that hint re-parses the span.
//!
//! Tags are payload-transparent: a `@tag("name")` wrapper does not consume a path step and does not push a locate
//! frame, so a mismatch reports the payload kind. Markup nodes are arrays of children.

use alloc::vec::Vec;

use jqf_codec_core::{CodecError, OwnedStep};
use jqf_data::{ContainerSpanKind, ValueKind};

use crate::error;

/// The located answer of the text walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Located {
    /// The value at the target path, as a byte span in the source the walk consumed.
    Node {
        start: usize,
        end: usize,
        child_count: Option<u64>,
        container: Option<ContainerSpanKind>,
    },
    /// The step at which navigation stopped: no member or position exists.
    Missing { step: usize },
    /// The step at which a kind mismatch stopped the path.
    TypeMismatch { step: usize, actual: ValueKind },
}

/// Whether the parser should treat the opened container as a locate frame, the located hit, or neither.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocateOpen {
    Skip,
    Scan,
    Hit,
}

/// Which container the parser just opened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Container {
    Object,
    Array,
}

/// Exact-path observation recorded during the skip-build validate walk. Last-value-wins object members and signed
/// array indices resolve to byte spans; the materialize pass re-parses the winning span.
pub(crate) struct PathLocator {
    steps: Vec<OwnedStep>,
    result: Option<Located>,
    stack: Vec<LocateFrame>,
    /// Type mismatch for a candidate child whose kind does not match the next path step, consumed when that child
    /// closes.
    pending_mismatch: Option<Located>,
}

struct LocateFrame {
    step: usize,
    winner: Option<(usize, usize)>,
    nested: Option<Located>,
    count: usize,
    ring: Option<Ring>,
    key_matches: bool,
}

/// Last-`cap` child observations for a negative index. Retention is the look-back, not the array length.
struct Ring {
    cap: usize,
    entries: Vec<RingEntry>,
    head: usize,
}

struct RingEntry {
    span: (usize, usize),
    nested: Option<Located>,
}

fn step_accepts(step: &OwnedStep, container: Container) -> bool {
    matches!(
        (step, container),
        (OwnedStep::Member(_), Container::Object) | (OwnedStep::Index(_), Container::Array)
    )
}

fn container_kind(container: Container) -> ValueKind {
    match container {
        Container::Object => ValueKind::Object,
        Container::Array => ValueKind::Array,
    }
}

impl PathLocator {
    pub(crate) fn new(steps: Vec<OwnedStep>) -> Self {
        Self {
            steps,
            result: None,
            stack: Vec::new(),
            pending_mismatch: None,
        }
    }

    pub(crate) fn take(&mut self) -> Result<Located, CodecError> {
        self.result.take().ok_or_else(error::data_contract)
    }

    pub(crate) fn on_key(&mut self, key: &str, frame_depth: usize) {
        if self.result.is_some() || frame_depth != self.stack.len() {
            return;
        }
        let Some(frame) = self.stack.last_mut() else {
            return;
        };
        let Some(OwnedStep::Member(name)) = self.steps.get(frame.step) else {
            return;
        };
        frame.key_matches = key == name.as_str();
    }

    /// Returns whether this container is a locate frame, the located hit, or off-path.
    pub(crate) fn on_container_open(&mut self, container: Container, frame_depth: usize) -> LocateOpen {
        if self.result.is_some() {
            return LocateOpen::Skip;
        }
        let kind = container_kind(container);
        if self.stack.is_empty() {
            if self.steps.is_empty() {
                return LocateOpen::Skip;
            }
            if !step_accepts(&self.steps[0], container) {
                self.result = Some(Located::TypeMismatch { step: 0, actual: kind });
                return LocateOpen::Skip;
            }
            self.stack.push(LocateFrame::new(0, &self.steps[0]));
            return LocateOpen::Scan;
        }
        if frame_depth != self.stack.len() {
            return LocateOpen::Skip;
        }
        let Some(frame) = self.stack.last() else {
            return LocateOpen::Skip;
        };
        if !Self::child_is_candidate(frame, &self.steps) {
            return LocateOpen::Skip;
        }
        let next = frame.step + 1;
        if next >= self.steps.len() {
            return LocateOpen::Hit;
        }
        if !step_accepts(&self.steps[next], container) {
            self.pending_mismatch = Some(Located::TypeMismatch {
                step: next,
                actual: kind,
            });
            return LocateOpen::Skip;
        }
        self.stack.push(LocateFrame::new(next, &self.steps[next]));
        LocateOpen::Scan
    }

    /// Records a located container together with the child count the parse walk just proved.
    pub(crate) fn finish_hit(
        &mut self,
        start: usize,
        end: usize,
        child_count: u64,
        container: ContainerSpanKind,
        frame_depth: usize,
    ) {
        let located = Located::Node {
            start,
            end,
            child_count: Some(child_count),
            container: Some(container),
        };
        if self.stack.is_empty() {
            if self.result.is_none() && frame_depth == 0 {
                self.result = Some(located);
            }
            return;
        }
        if frame_depth != self.stack.len() {
            return;
        }
        self.pending_mismatch = None;
        self.record_child(start, end, Some(located));
    }

    pub(crate) fn finish_container(
        &mut self,
        start: usize,
        end: usize,
        scanning: bool,
        frame_depth: usize,
    ) -> Result<(), CodecError> {
        if scanning {
            let located = self.pop_and_resolve()?;
            if self.stack.is_empty() {
                self.result = Some(located);
                return Ok(());
            }
            self.record_child(start, end, Some(located));
            return Ok(());
        }
        if self.stack.is_empty() {
            // Empty path: only the ROOT close is the located node. An inner container closes first and must not steal
            // the observation.
            if self.result.is_none() && frame_depth == 0 {
                self.result = Some(Located::Node {
                    start,
                    end,
                    child_count: None,
                    container: None,
                });
            }
            return Ok(());
        }
        if frame_depth != self.stack.len() {
            return Ok(());
        }
        let nested = self.pending_mismatch.take();
        self.record_child(start, end, nested);
        Ok(())
    }

    pub(crate) fn finish_scalar(&mut self, start: usize, end: usize, kind: ValueKind, frame_depth: usize) {
        if self.result.is_some() {
            return;
        }
        if !self.stack.is_empty() && frame_depth != self.stack.len() {
            return;
        }
        if self.stack.is_empty() {
            if frame_depth == 0 {
                self.result = Some(if self.steps.is_empty() {
                    Located::Node {
                        start,
                        end,
                        child_count: None,
                        container: None,
                    }
                } else {
                    Located::TypeMismatch { step: 0, actual: kind }
                });
            }
            return;
        }
        let Some(frame) = self.stack.last() else {
            return;
        };
        if !Self::child_is_candidate(frame, &self.steps) {
            self.record_child(start, end, None);
            return;
        }
        let next = frame.step + 1;
        let nested = (next < self.steps.len()).then_some(Located::TypeMismatch {
            step: next,
            actual: kind,
        });
        self.record_child(start, end, nested);
    }

    fn child_is_candidate(frame: &LocateFrame, steps: &[OwnedStep]) -> bool {
        match steps.get(frame.step) {
            Some(OwnedStep::Member(_)) => frame.key_matches,
            Some(OwnedStep::Index(index)) if *index >= 0 => {
                usize::try_from(*index).is_ok_and(|target| frame.count == target)
            }
            Some(OwnedStep::Index(_)) => true,
            Some(OwnedStep::Range { .. }) | None => false,
        }
    }

    fn record_child(&mut self, start: usize, end: usize, nested: Option<Located>) {
        let Some(frame) = self.stack.last_mut() else {
            return;
        };
        let candidate = Self::child_is_candidate(frame, &self.steps);
        if candidate {
            let span = (start, end);
            match self.steps.get(frame.step) {
                Some(OwnedStep::Index(index)) if *index < 0 => {
                    if let Some(ring) = frame.ring.as_mut() {
                        ring.push(span, nested);
                    }
                }
                Some(OwnedStep::Member(_) | OwnedStep::Index(_)) => {
                    frame.winner = Some(span);
                    frame.nested = nested;
                }
                Some(OwnedStep::Range { .. }) | None => {}
            }
        }
        frame.count = frame.count.saturating_add(1);
        frame.key_matches = false;
    }

    fn pop_and_resolve(&mut self) -> Result<Located, CodecError> {
        let frame = self.stack.pop().ok_or_else(error::data_contract)?;
        if matches!(self.steps.get(frame.step), Some(OwnedStep::Index(index)) if *index < 0) {
            if let Some(nested) = frame.ring.as_ref().and_then(Ring::winner_nested) {
                return Ok(nested);
            }
            return Ok(match frame.ring.as_ref().and_then(Ring::winner) {
                Some((start, end)) => Located::Node {
                    start,
                    end,
                    child_count: None,
                    container: None,
                },
                None => Located::Missing { step: frame.step },
            });
        }
        if let Some(nested) = frame.nested {
            return Ok(nested);
        }
        Ok(match frame.winner {
            Some((start, end)) => Located::Node {
                start,
                end,
                child_count: None,
                container: None,
            },
            None => Located::Missing { step: frame.step },
        })
    }
}

impl LocateFrame {
    fn new(step: usize, spec: &OwnedStep) -> Self {
        let ring = match spec {
            OwnedStep::Index(index) if *index < 0 => {
                Some(Ring::new(usize::try_from(index.unsigned_abs()).unwrap_or(usize::MAX)))
            }
            _ => None,
        };
        Self {
            step,
            winner: None,
            nested: None,
            count: 0,
            ring,
            key_matches: false,
        }
    }
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            entries: Vec::new(),
            head: 0,
        }
    }

    fn push(&mut self, span: (usize, usize), nested: Option<Located>) {
        if self.cap == 0 {
            return;
        }
        let entry = RingEntry { span, nested };
        if self.entries.len() < self.cap {
            self.entries.push(entry);
        } else {
            self.entries[self.head] = entry;
            self.head = (self.head + 1) % self.cap;
        }
    }

    fn winner_entry(&self) -> Option<&RingEntry> {
        if self.entries.len() == self.cap && self.cap > 0 {
            self.entries.get(self.head)
        } else {
            None
        }
    }

    fn winner(&self) -> Option<(usize, usize)> {
        self.winner_entry().map(|entry| entry.span)
    }

    fn winner_nested(&self) -> Option<Located> {
        self.winner_entry().and_then(|entry| entry.nested)
    }
}
