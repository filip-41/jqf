//! Walk a container one element at a time, without building the whole tree.
//!
//! Sibling of [`super::count`]: same skeleton, same fail-closed decline. Yields each element's probe value to a
//! caller-owned visitor. A span-backed container asks [`crate::LazySpanMaterializer::visit_span_elements`].
//!
//! [`ElementRow::FanOut`] visits every element or none — a pre-pass checks the probe first so a mid-stream decline
//! cannot leave a published prefix. [`ElementRow::ReduceFold`] may decline mid-fold; the fold has published nothing
//! yet.
//!
//! A key/index miss is null. A category the probe cannot handle declines. [`ElementProbe::Length`] counts array items,
//! object members, or null's 0; a string or number declines.

use jqf_resource::ResourceContext;

use alloc::vec::Vec;

use crate::{CountStep, DataError, Document, SliceRange, Value, ValueKind, ValueView, resolve_index};

use super::count::{descend_tag_layers, resolve_range};

/// The per-element read a fan-out or fold demand performs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ElementProbe {
    /// A static `Key`/`Index` navigation over the element (empty is the element itself — the bare `.catalog[]`
    /// fan-out).
    Path(Vec<CountStep>),
    /// The element's `length` (an array's element count, an object's member count, null's 0; a string or number
    /// declines).
    Length,
}

/// Which element row a demand answers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElementRow {
    /// `.catalog[] | PROBE`: one published item per element.
    FanOut,
    /// `reduce (C[] PROBE) as $x (LITERAL; UPDATE)`: one folded state.
    ReduceFold,
}

/// One element-stream demand: container path plus per-element probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElementDemand {
    /// The element row this demand answers.
    pub row: ElementRow,
    /// The container's static forward steps (empty is the root).
    pub path: Vec<CountStep>,
    /// Iterate only this slice. `None` is the whole container. Bounds are non-negative or open.
    pub range: Option<SliceRange>,
    /// The per-element read.
    pub probe: ElementProbe,
    /// The [`ElementRow::ReduceFold`] update's increment: the exact integer `LITERAL` of the recognized `.[$x] +=
    /// LITERAL` update. `None` for a [`ElementRow::FanOut`] demand.
    pub increment: Option<i64>,
    /// The per-element `select(P)` filter of a fan-out row (`.catalog[] | select(.k > LITERAL) | .name`). `None` for
    /// unfiltered fan-out and for reduce-fold. A filtered-out element is skipped, not declined; a predicate the closed
    /// law cannot rank declines the whole demand so the floor renders the raise.
    pub filter: Option<crate::CountFilter>,
}

/// Element-stream answer, or a decline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElementVerdict {
    /// Every element's probe value was handed to the visitor, `n` of them.
    Completed(u64),
    /// Could not prove the answer. Fan-out never ran the visitor. A fold may have run some iterations; the fold has
    /// published nothing.
    Decline,
}

impl<'document> Document<'document> {
    /// Visits every element of the container `demand.path` names, handing the caller the probe value of each element as
    /// an owned value — without materializing the container's tree.
    ///
    /// Navigates the container path over the built skeleton, then:
    ///
    /// - a BUILT container iterates its arena elements, navigating the probe per element (a deferred span CHILD is
    ///   materialized through the   format leaf one at a time);
    /// - a deferred [`crate::document::ContainerSpanKind`] container delegates the whole iteration to the format-owned
    ///   [`crate::LazySpanMaterializer::visit_span_elements`] leaf;
    /// - any shape the skeleton cannot prove — a missing/null/non-container at the path, an intermediate path step
    ///   through a span, a probe   category the probe cannot handle — is [`ElementVerdict::Decline`].
    ///
    /// The visitor runs for EVERY element or (for [`ElementRow::FanOut`]) for NONE — the probe's provability is
    /// pre-passed before the first visit. A [`ElementRow::ReduceFold`] decline may interrupt the iteration; the caller
    /// has published nothing and falls back cleanly.
    ///
    /// # Errors
    ///
    /// Returns [`DataError`] only for a genuinely invalid document, a refused capability, or a visitor failure; every
    /// "cannot prove it" shape is a [`ElementVerdict::Decline`], never an error.
    pub fn visit_elements<F>(
        &self,
        demand: &super::ElementDemand,
        resources: &mut ResourceContext<'_>,
        visit: F,
    ) -> Result<ElementVerdict, DataError>
    where
        F: FnMut(&Value, &mut ResourceContext<'_>) -> Result<(), DataError>,
    {
        // A document without semantic nodes cannot be navigated.
        let Ok(root) = self.value_view(self.root_handle()) else {
            return Ok(ElementVerdict::Decline);
        };
        let mut view = root;
        for step in &demand.path {
            match step {
                CountStep::ObjectKey(key) => {
                    // A key step over a non-object is the reference's index-class raise; decline and let the floor
                    // render it.
                    let Ok(Some(object)) = view.object() else {
                        return Ok(ElementVerdict::Decline);
                    };
                    // A missing container yields null, over which `.[]` RAISES — the floor renders the error.
                    view = match object.get(key.as_str()) {
                        None => return Ok(ElementVerdict::Decline),
                        Some(child) => child,
                    };
                }
                CountStep::ArrayIndex(index) => {
                    let Ok(Some(array)) = view.array() else {
                        return Ok(ElementVerdict::Decline);
                    };
                    match resolve_index(array.len(), *index) {
                        None => return Ok(ElementVerdict::Decline),
                        Some(resolved) => match array.get(resolved) {
                            None => return Ok(ElementVerdict::Decline),
                            Some(child) => view = child,
                        },
                    }
                }
            }
        }
        if view.is_container_span()? {
            return self.visit_span_container(&view, demand, resources, visit);
        }
        self.visit_built_container(&view, demand, resources, visit)
    }

    /// The deferred-span container arm: the whole iteration is the format leaf's
    /// ([`crate::LazySpanMaterializer::visit_span_elements`]); the shared prologue is [`Document::span_leaf_input`].
    fn visit_span_container<F>(
        &self,
        view: &ValueView<'_, 'document>,
        demand: &super::ElementDemand,
        resources: &mut ResourceContext<'_>,
        visit: F,
    ) -> Result<ElementVerdict, DataError>
    where
        F: FnMut(&Value, &mut ResourceContext<'_>) -> Result<(), DataError>,
    {
        let Some((text, container, materializer)) = self.span_leaf_input(view, demand.range.is_some())? else {
            return Ok(ElementVerdict::Decline);
        };
        let mut visit = visit;
        materializer.visit_span_elements(text, container, demand, resources, &mut visit)
    }

    /// The built-container arm: iterate the arena's elements, navigating the probe per element.
    fn visit_built_container<F>(
        &self,
        view: &ValueView<'_, 'document>,
        demand: &super::ElementDemand,
        resources: &mut ResourceContext<'_>,
        mut visit: F,
    ) -> Result<ElementVerdict, DataError>
    where
        F: FnMut(&Value, &mut ResourceContext<'_>) -> Result<(), DataError>,
    {
        // A tag-LAYER node is payload-transparent: see through it before the category probe, exactly as payload_view
        // does.
        let view = &descend_tag_layers(self, *view)?;
        match view.kind()? {
            ValueKind::Array => {
                let array = view.array()?.ok_or(DataError::InvalidDocument)?;
                let (start, end) = resolve_range(array.len(), demand.range);
                self.visit_built_items(
                    || array.iter().skip(start).take(end - start).map(Ok),
                    demand,
                    resources,
                    &mut visit,
                )
            }
            ValueKind::Object => {
                // A slice over an OBJECT is the reference's index-class raise; the range-bearing demand declines and
                // the floor renders it.
                if demand.range.is_some() {
                    return Ok(ElementVerdict::Decline);
                }
                let object = view.object()?.ok_or(DataError::InvalidDocument)?;
                self.visit_built_items(
                    || object.iter().map(|entry| entry.map(crate::ObjectEntryView::value)),
                    demand,
                    resources,
                    &mut visit,
                )
            }
            // `null | .[]` is the reference's iterate-null raise; every non-container is the index-class raise.
            // Decline, and the floor renders the error byte for byte.
            _ => Ok(ElementVerdict::Decline),
        }
    }

    /// Iterates one built container (arrays and objects share the same per-element law): the pre-pass (a
    /// [`ElementRow::FanOut`] demand's provability check) then the visit pass. `elements` is a factory because the
    /// `FanOut` pre-pass and the visit pass iterate the container twice; the container views are `Copy`, so the factory
    /// is a plain closure over the view. A `Key`/`Index` step over a deferred span child materializes it through the
    /// format leaf.
    fn visit_built_items<'source, F, I>(
        &self,
        elements: impl Fn() -> I,
        demand: &super::ElementDemand,
        resources: &mut ResourceContext<'_>,
        visit: &mut F,
    ) -> Result<ElementVerdict, DataError>
    where
        F: FnMut(&Value, &mut ResourceContext<'_>) -> Result<(), DataError>,
        I: Iterator<Item = Result<ValueView<'document, 'source>, DataError>>,
        'source: 'document,
    {
        // The FanOut pre-pass: every element's probe must be provable BEFORE the first visit, so a mid-stream decline
        // can never leave a published prefix a floor rerun would duplicate. A ReduceFold publishes nothing until it
        // completes, so it visits as it goes and a mid-iteration decline stays clean.
        let filter_probe = demand.filter.as_ref().map(|filter| super::ElementDemand {
            row: demand.row,
            path: Vec::new(),
            range: None,
            probe: ElementProbe::Path(filter.path.clone()),
            increment: None,
            filter: None,
        });
        if matches!(demand.row, ElementRow::FanOut) {
            for element in elements() {
                let element = element?;
                match self.element_filter_gate(element, demand, filter_probe.as_ref(), resources)? {
                    FilterGate::Decline => return Ok(ElementVerdict::Decline),
                    FilterGate::Skip => {}
                    FilterGate::Keep => {
                        if !self.element_probe_provable(element, demand, resources)? {
                            return Ok(ElementVerdict::Decline);
                        }
                    }
                }
            }
        }
        let mut visited = 0u64;
        for element in elements() {
            let element = element?;
            match self.element_filter_gate(element, demand, filter_probe.as_ref(), resources)? {
                FilterGate::Decline => return Ok(ElementVerdict::Decline),
                FilterGate::Skip => {}
                FilterGate::Keep => {
                    let Some(value) = self.element_probe_value(element, demand, resources)? else {
                        return Ok(ElementVerdict::Decline);
                    };
                    visit(&value, resources)?;
                    visited = visited.saturating_add(1);
                }
            }
        }
        Ok(ElementVerdict::Completed(visited))
    }

    /// One element's probe value: the owned value the probe names over the element, or `None` when the probe would
    /// raise (the reference's category error) and the floor must render it. The pre-pass checks this for `Some` and the
    /// visit pass re-navigates — the container's elements do not change between the two passes.
    fn element_probe_value(
        &self,
        element: ValueView<'_, 'document>,
        demand: &super::ElementDemand,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<Value>, DataError> {
        if element.is_container_span()? {
            // A deferred span child: the format materializes it one at a time, then the probe navigates the owned
            // value. The RAW-byte accessor and the byte-taking materialization arm are what make this sound for a
            // binary format's spans too.
            let record = self.node_record(element.node())?;
            let crate::document::NodeSemantic::ContainerSpan { text, .. } = &record.semantic else {
                return Ok(None);
            };
            let Some(bytes) = self.bytes(*text) else {
                return Ok(None);
            };
            let Some(materializer) = self.span_materializer() else {
                return Ok(None);
            };
            let owned = materializer.materialize_span_bytes(bytes, resources)?;
            return Ok(owned_probe_value(&owned, &demand.probe));
        }
        probe_value(self, element, &demand.probe, resources)
    }

    /// `FanOut` pre-pass twin of [`Self::element_probe_value`]: true iff the probe would return `Some`. Over BUILT
    /// elements a Path probe does not materialize the landed node and Length is a cheap kind check. Over a deferred
    /// SPAN child this materializes as well — provability is not observable without the owned value — and the visit
    /// pass materializes the same span a second time. That doubling is load-bearing, not waste: this pre-pass is what
    /// keeps a `FanOut` decline prefix-free, so it must stay authoritative over span children too.
    fn element_probe_provable(
        &self,
        element: ValueView<'_, 'document>,
        demand: &super::ElementDemand,
        resources: &mut ResourceContext<'_>,
    ) -> Result<bool, DataError> {
        if element.is_container_span()? {
            let record = self.node_record(element.node())?;
            let crate::document::NodeSemantic::ContainerSpan { text, .. } = &record.semantic else {
                return Ok(false);
            };
            let Some(bytes) = self.bytes(*text) else {
                return Ok(false);
            };
            let Some(materializer) = self.span_materializer() else {
                return Ok(false);
            };
            let owned = materializer.materialize_span_bytes(bytes, resources)?;
            return Ok(owned_probe_provable(&owned, &demand.probe));
        }
        probe_provable(element, &demand.probe)
    }

    fn element_filter_gate(
        &self,
        element: ValueView<'_, 'document>,
        demand: &super::ElementDemand,
        filter_probe: Option<&super::ElementDemand>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<FilterGate, DataError> {
        let Some(filter) = demand.filter.as_ref() else {
            return Ok(FilterGate::Keep);
        };
        let Some(filter_demand) = filter_probe else {
            return Ok(FilterGate::Keep);
        };
        if matches!(filter.test, crate::CountTest::Truthy)
            && let Some(truthy) = truthy_from_view(element, &filter.path)?
        {
            return Ok(if truthy { FilterGate::Keep } else { FilterGate::Skip });
        }
        let Some(owned) = self.element_probe_value(element, filter_demand, resources)? else {
            return Ok(FilterGate::Decline);
        };
        match filter.test.answer(crate::CountMember::Value(&owned)) {
            None => Ok(FilterGate::Decline),
            Some(false) => Ok(FilterGate::Skip),
            Some(true) => Ok(FilterGate::Keep),
        }
    }
}

enum FilterGate {
    Keep,
    Skip,
    Decline,
}

/// Truthiness from the landed view's kind: only `null` and `false` are falsy.
/// Declines when a step is not a key over an object (the floor renders the raise).
fn truthy_from_view(mut view: ValueView<'_, '_>, path: &[CountStep]) -> Result<Option<bool>, DataError> {
    if view.is_container_span()? {
        return Ok(None);
    }
    for step in path {
        match view.kind()? {
            ValueKind::Null => return Ok(Some(false)),
            ValueKind::Object => {
                let CountStep::ObjectKey(key) = step else {
                    return Ok(None);
                };
                let Some(object) = view.object()? else {
                    return Ok(None);
                };
                view = match object.get(key.as_str()) {
                    Some(child) => child,
                    None => return Ok(Some(false)),
                };
                if view.is_container_span()? {
                    return Ok(None);
                }
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(match view.kind()? {
        ValueKind::Null => false,
        ValueKind::Bool => match view.scalar()? {
            Some(crate::ScalarView::Bool(true)) => true,
            Some(crate::ScalarView::Bool(false)) => false,
            _ => return Ok(None),
        },
        _ => true,
    }))
}

/// `length`'s owned value: the container's element/member count as an integer, saturating at `i64::MAX`.
fn length_value(len: usize) -> Value {
    Value::Number(crate::Number::integer(crate::Integer::from_i64(
        i64::try_from(len).unwrap_or(i64::MAX),
    )))
}

/// Navigates one built element view by the probe, returning the owned value the probe names, or `None` when a step
/// addresses a category outside its domain (the reference raises there — the floor renders it).
///
/// The reference's laws, exactly:
///
/// - a null element short-circuits: null's children are null, so every remaining step is total over it and the value is
///   `null`;
/// - a final `Key`/`Index` step contributes its member's value, or the reference's `null` when the member/position is
///   absent;
/// - an intermediate step descends a present member/position, or `null` when absent (the remaining steps are total over
///   it);
/// - a step whose category does not match (`.name` over a number) is the   reference's index-class raise: `None`.
fn probe_value(
    document: &Document<'_>,
    view: ValueView<'_, '_>,
    probe: &ElementProbe,
    resources: &mut ResourceContext<'_>,
) -> Result<Option<Value>, DataError> {
    // A tag-LAYER node is payload-transparent; see through it before probing.
    let view = descend_tag_layers(document, view)?;
    match probe {
        ElementProbe::Length => {
            Ok(match view.kind()? {
                // Null is the empty container's length, `0` — not null; see the module doc's probe law.
                ValueKind::Null => Some(length_value(0)),
                ValueKind::Array => {
                    // A kind that says Array with no array view behind it is storage corruption, not an empty
                    // container: refuse exactly as the visit arm does.
                    let array = view.array()?.ok_or(DataError::InvalidDocument)?;
                    Some(length_value(array.len()))
                }
                ValueKind::Object => {
                    let object = view.object()?.ok_or(DataError::InvalidDocument)?;
                    Some(length_value(object.len()))
                }
                // A string's codepoint count and a number's magnitude are payload reads; the floor owns them.
                _ => None,
            })
        }
        ElementProbe::Path(path) => match probe_path_land(view, path)? {
            PathLand::Decline => Ok(None),
            PathLand::Null => Ok(Some(Value::Null)),
            PathLand::Node(node) => Ok(Some(crate::materialize::materialize_document_node(
                document, node, resources,
            )?)),
        },
    }
}

/// `FanOut` pre-pass: whether [`probe_value`] would return `Some`. Declines on exactly the same inputs; a Path probe
/// does not materialize.
fn probe_provable(view: ValueView<'_, '_>, probe: &ElementProbe) -> Result<bool, DataError> {
    match probe {
        ElementProbe::Length => Ok(probe_length_admitted(view)?),
        ElementProbe::Path(path) => Ok(!matches!(probe_path_land(view, path)?, PathLand::Decline)),
    }
}

fn probe_length_admitted(view: ValueView<'_, '_>) -> Result<bool, DataError> {
    Ok(matches!(
        descend_tag_layers(view.document, view)?.kind()?,
        ValueKind::Null | ValueKind::Array | ValueKind::Object
    ))
}

enum PathLand {
    Decline,
    Null,
    Node(crate::NodeId),
}

fn probe_path_land(mut view: ValueView<'_, '_>, path: &[CountStep]) -> Result<PathLand, DataError> {
    for step in path {
        // A tag-LAYER node (the land itself or a navigated child) is payload-transparent; see through it instead of
        // raising.
        view = descend_tag_layers(view.document, view)?;
        if view.is_container_span()? {
            return Ok(PathLand::Decline);
        }
        if view.kind()? == ValueKind::Null {
            return Ok(PathLand::Null);
        }

        match step {
            CountStep::ObjectKey(key) => {
                let Some(object) = view.object()? else {
                    return Ok(PathLand::Decline);
                };
                match object.get(key.as_str()) {
                    Some(child) => view = child,
                    None => return Ok(PathLand::Null),
                }
            }
            CountStep::ArrayIndex(index) => {
                let Some(array) = view.array()? else {
                    return Ok(PathLand::Decline);
                };
                match resolve_index(array.len(), *index).and_then(|i| array.get(i)) {
                    Some(child) => view = child,
                    None => return Ok(PathLand::Null),
                }
            }
        }
    }
    Ok(PathLand::Node(view.node()))
}

/// The owned-element twin of [`probe_value`]: navigates an owned element value (one the format materialized from a
/// span) by the probe.
///
/// Public so the format leaves ([`crate::LazySpanMaterializer`] implementations) navigate batch-materialized elements
/// through the same law the document core uses — one probe law, never two.
///
/// Navigation is TAG-TRANSPARENT and the answer is not: a tag decides nothing about which member a step names, but the
/// value a step lands on keeps every tag it carries, exactly as the built twin's materialization does. A probe that
/// untagged its answer would make the same element read tagged on one route and bare on the other.
#[must_use]
pub fn owned_probe_value(value: &Value, probe: &ElementProbe) -> Option<Value> {
    match probe {
        ElementProbe::Length => Some(match value.untagged() {
            // Null is the empty container's length, `0` — not null; see the module doc's probe law.
            Value::Null => length_value(0),
            Value::Array(array) => length_value(array.len()),
            Value::Object(object) => length_value(object.len()),
            _ => return None,
        }),
        ElementProbe::Path(path) => {
            let mut value = value;
            for step in path {
                match (value.untagged(), step) {
                    (Value::Null, _) => return Some(Value::Null),
                    (Value::Object(object), CountStep::ObjectKey(key)) => match object.get(key) {
                        Some(child) => value = child,
                        None => return Some(Value::Null),
                    },
                    (Value::Array(array), CountStep::ArrayIndex(index)) => {
                        match resolve_index(array.len(), *index).and_then(|i| array.get(i)) {
                            Some(child) => value = child,
                            None => return Some(Value::Null),
                        }
                    }
                    _ => return None,
                }
            }
            Some(value.clone())
        }
    }
}

/// `FanOut` pre-pass twin of [`owned_probe_value`]: true iff the probe would return `Some`. A Path probe does not clone
/// the landed value.
#[must_use]
fn owned_probe_provable(value: &Value, probe: &ElementProbe) -> bool {
    match probe {
        ElementProbe::Length => matches!(value.untagged(), Value::Null | Value::Array(_) | Value::Object(_)),
        ElementProbe::Path(path) => {
            let mut value = value;
            for step in path {
                match (value.untagged(), step) {
                    (Value::Null, _) => return true,
                    (Value::Object(object), CountStep::ObjectKey(key)) => match object.get(key) {
                        Some(child) => value = child,
                        None => return true,
                    },
                    (Value::Array(array), CountStep::ArrayIndex(index)) => {
                        match resolve_index(array.len(), *index).and_then(|i| array.get(i)) {
                            Some(child) => value = child,
                            None => return true,
                        }
                    }
                    _ => return false,
                }
            }
            true
        }
    }
}

/// The owned-container fallback of [`crate::LazySpanMaterializer::visit_span_elements`]: iterate one owned container's
/// elements, navigating the probe per element. Shares the pre-pass and visit laws with [`Document::visit_built_items`].
pub(crate) fn visit_owned_container<F>(
    value: &Value,
    demand: &super::ElementDemand,
    resources: &mut ResourceContext<'_>,
    mut visit: F,
) -> Result<ElementVerdict, DataError>
where
    F: FnMut(&Value, &mut ResourceContext<'_>) -> Result<(), DataError>,
{
    let len = match value.untagged() {
        Value::Array(array) => array.len(),
        // The object-slice raise is declined at the span seam ([`Document::visit_span_container`]), the only route into
        // this fallback, so `demand.range` cannot be set here.
        Value::Object(object) => object.len(),
        // `null | .[]` and the index-class raises are the floor's.
        _ => return Ok(ElementVerdict::Decline),
    };
    let (start, end) = resolve_range(len, demand.range);
    let owned = |index: usize| -> Option<&Value> {
        match value.untagged() {
            Value::Array(array) => array.get(index),
            Value::Object(object) => object.get_index(index).map(crate::ObjectEntry::value),
            _ => None,
        }
    };
    if matches!(demand.row, ElementRow::FanOut) {
        for index in start..end {
            let Some(element) = owned(index) else {
                return Ok(ElementVerdict::Decline);
            };
            match owned_filter_gate(element, demand.filter.as_ref()) {
                FilterGate::Decline => return Ok(ElementVerdict::Decline),
                FilterGate::Skip => {}
                FilterGate::Keep => {
                    if !owned_probe_provable(element, &demand.probe) {
                        return Ok(ElementVerdict::Decline);
                    }
                }
            }
        }
    }
    let mut visited = 0u64;
    for index in start..end {
        let Some(element) = owned(index) else {
            return Ok(ElementVerdict::Decline);
        };
        match owned_filter_gate(element, demand.filter.as_ref()) {
            FilterGate::Decline => return Ok(ElementVerdict::Decline),
            FilterGate::Skip => {}
            FilterGate::Keep => {
                let Some(probe_value) = owned_probe_value(element, &demand.probe) else {
                    return Ok(ElementVerdict::Decline);
                };
                visit(&probe_value, resources)?;
                visited = visited.saturating_add(1);
            }
        }
    }
    Ok(ElementVerdict::Completed(visited))
}

fn owned_filter_gate(element: &Value, filter: Option<&crate::CountFilter>) -> FilterGate {
    let Some(filter) = filter else {
        return FilterGate::Keep;
    };
    match filter.contributes(element) {
        None => FilterGate::Decline,
        Some(0) => FilterGate::Skip,
        Some(_) => FilterGate::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ElementDemand, ElementProbe, ElementRow};
    use alloc::string::String;
    use alloc::vec;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn unlimited_resources() -> ResourceContext<'static> {
        let account = RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    fn key(text: &str) -> CountStep {
        CountStep::ObjectKey(String::from(text))
    }

    fn name_probe() -> ElementProbe {
        ElementProbe::Path(vec![key("name")])
    }

    #[test]
    fn owned_probe_navigates_and_short_circuits() {
        let mut builder = crate::ObjectBuilder::try_with_capacity(2).expect("builder");
        builder
            .try_insert_last(crate::ObjectKey::try_from_str("name").expect("key"), Value::Null)
            .expect("insert");
        builder
            .try_insert_last(crate::ObjectKey::try_from_str("other").expect("key"), Value::Null)
            .expect("insert");
        let object = Value::Object(builder.try_finish().expect("finish"));

        // A present member yields its value; an absent one the reference's null.
        assert!(matches!(
            owned_probe_value(&Value::Null, &name_probe()),
            Some(Value::Null)
        ));
        assert!(matches!(owned_probe_value(&object, &name_probe()), Some(Value::Null)));
        // A category outside a step's domain is the reference's raise: None.
        assert!(
            owned_probe_value(
                &Value::Number(crate::Number::integer(crate::Integer::from_i64(5))),
                &name_probe()
            )
            .is_none()
        );
        // Length declines the payload categories.
        assert!(
            owned_probe_value(
                &Value::String(crate::Shared::try_from_str("abc").expect("str")),
                &ElementProbe::Length
            )
            .is_none()
        );
    }

    /// The cell VALUES the `length` probe answers, not just their category: an array's element count, an object's
    /// member count, and null's `0`.
    ///
    /// The null cell is the row that bites. It is the one cell whose answer is not read off a container the element
    /// holds, so a probe that mirrors its input answers `null` — a wrong published value on ordinary input, and one
    /// that only shows when every element of a fan-out is a container or null (any string or number element declines
    /// the whole demand to the floor and hides it).
    #[test]
    fn length_probe_answers_zero_for_the_null_cell() {
        let count = |value: &Value| -> i64 {
            let Some(Value::Number(number)) = owned_probe_value(value, &ElementProbe::Length) else {
                panic!("length answers a number");
            };
            number
                .to_integer()
                .and_then(|integer| integer.to_i64())
                .expect("length is a machine integer")
        };

        assert_eq!(count(&Value::Null), 0);

        let mut array = crate::Array::try_with_capacity(2).expect("array");
        array.try_push(Value::Null).expect("push");
        array.try_push(Value::Null).expect("push");
        assert_eq!(count(&Value::Array(array)), 2);

        let mut builder = crate::ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(crate::ObjectKey::try_from_str("name").expect("key"), Value::Null)
            .expect("insert");
        let object = Value::Object(builder.try_finish().expect("finish"));
        assert_eq!(count(&object), 1);

        // The empty container and the null cell answer the same `0`, which is exactly why the null cell cannot be told
        // from a mistake by shape.
        let empty = crate::Array::try_with_capacity(0).expect("array");
        assert_eq!(count(&Value::Array(empty)), 0);
    }

    #[test]
    fn owned_container_visits_every_element_or_none() {
        let mut builder = crate::ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(crate::ObjectKey::try_from_str("name").expect("key"), Value::Null)
            .expect("insert");
        let object = Value::Object(builder.try_finish().expect("finish"));
        let mut array = crate::Array::try_with_capacity(3).expect("array");
        array.try_push(object.clone()).expect("push");
        array.try_push(object).expect("push");
        array.try_push(Value::Null).expect("push");
        let array = Value::Array(array);
        let demand = ElementDemand {
            row: ElementRow::FanOut,
            path: Vec::new(),
            range: None,
            probe: name_probe(),
            increment: None,
            filter: None,
        };
        let mut resources = unlimited_resources();
        let mut visited = 0u64;
        let verdict = visit_owned_container(&array, &demand, &mut resources, |_, _| {
            visited += 1;
            Ok(())
        })
        .expect("visits");
        assert_eq!(verdict, ElementVerdict::Completed(3));
        assert_eq!(visited, 3);

        // A foreign category makes the FanOut pre-pass decline BEFORE any visit.
        let mut bad = crate::Array::try_with_capacity(2).expect("array");
        bad.try_push(Value::Number(crate::Number::integer(crate::Integer::from_i64(5))))
            .expect("push");
        bad.try_push(Value::Null).expect("push");
        let bad = Value::Array(bad);
        let mut resources = unlimited_resources();
        let mut visited = 0u64;
        let verdict = visit_owned_container(&bad, &demand, &mut resources, |_, _| {
            visited += 1;
            Ok(())
        })
        .expect("declines");
        assert_eq!(verdict, ElementVerdict::Decline);
        assert_eq!(visited, 0);
    }

    #[test]
    fn owned_container_visits_only_the_in_range_elements() {
        let mut array = crate::Array::try_with_capacity(5).expect("array");
        for value in [1, 2, 3, 4, 5] {
            array
                .try_push(Value::Number(crate::Number::integer(crate::Integer::from_i64(value))))
                .expect("push");
        }
        let array = Value::Array(array);
        // The bare fan-out over `[1:4]` visits elements 1..4.
        let demand = ElementDemand {
            row: ElementRow::FanOut,
            path: Vec::new(),
            range: Some((Some(1), Some(4))),
            probe: ElementProbe::Path(Vec::new()),
            increment: None,
            filter: None,
        };
        let mut resources = unlimited_resources();
        let mut visited = Vec::new();
        let verdict = visit_owned_container(&array, &demand, &mut resources, |value, _| {
            let Value::Number(number) = value.untagged() else {
                panic!("number element");
            };
            visited.push(number.to_integer().and_then(|i| i.to_i64()).unwrap());
            Ok(())
        })
        .expect("visits");
        assert_eq!(verdict, ElementVerdict::Completed(3));
        assert_eq!(visited, [2, 3, 4]);

        // A range past the container's end clamps: `[3:99]` visits 3..5.
        let demand = ElementDemand {
            row: ElementRow::FanOut,
            path: Vec::new(),
            range: Some((Some(3), Some(99))),
            probe: ElementProbe::Path(Vec::new()),
            increment: None,
            filter: None,
        };
        let mut resources = unlimited_resources();
        let mut visited = 0u64;
        let verdict = visit_owned_container(&array, &demand, &mut resources, |_, _| {
            visited += 1;
            Ok(())
        })
        .expect("visits");
        assert_eq!(verdict, ElementVerdict::Completed(2));
        assert_eq!(visited, 2);

        // A start past the container's end is an empty range.
        let demand = ElementDemand {
            row: ElementRow::FanOut,
            path: Vec::new(),
            range: Some((Some(5), None)),
            probe: ElementProbe::Path(Vec::new()),
            increment: None,
            filter: None,
        };
        let mut resources = unlimited_resources();
        let verdict = visit_owned_container(&array, &demand, &mut resources, |_, _| {
            panic!("empty range must visit nothing")
        })
        .expect("visits");
        assert_eq!(verdict, ElementVerdict::Completed(0));
    }

    /// `probe_provable` declines on exactly the inputs `probe_value` declines. A Path landing that would materialize
    /// still counts as admitted.
    #[test]
    fn probe_provable_declines_exactly_where_probe_value_does() {
        let mut builder = crate::ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(crate::ObjectKey::try_from_str("name").expect("key"), Value::Null)
            .expect("insert");
        let object = Value::Object(builder.try_finish().expect("finish"));
        let mut array = crate::Array::try_with_capacity(1).expect("array");
        array.try_push(Value::Null).expect("push");
        let array = Value::Array(array);
        let number = Value::Number(crate::Number::integer(crate::Integer::from_i64(5)));
        let string = Value::String(crate::Shared::try_from_str("abc").expect("str"));
        let probes = [
            name_probe(),
            ElementProbe::Path(Vec::new()),
            ElementProbe::Length,
            ElementProbe::Path(vec![CountStep::ArrayIndex(0)]),
        ];
        for probe in &probes {
            for value in [&Value::Null, &object, &array, &number, &string] {
                assert_eq!(
                    owned_probe_provable(value, probe),
                    owned_probe_value(value, probe).is_some(),
                    "owned probe {probe:?} over {value:?}"
                );
            }
        }

        let mut resources = unlimited_resources();
        let mut builder = crate::AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let root = builder
            .add_node("test.bool", crate::AccountedSemanticNode::Bool(true), None, &resources)
            .expect("root");
        let document = builder.finish(root, &resources).expect("document");
        let view = document.value_view(document.root_handle()).expect("root view");
        for probe in &probes {
            let admitted = probe_provable(view, probe).expect("provable");
            let value = probe_value(&document, view, probe, &mut resources).expect("value");
            assert_eq!(admitted, value.is_some(), "document probe {probe:?}");
        }
    }
}
