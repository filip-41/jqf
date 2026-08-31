//! Document oracle answers `execute` matches after a codec locate.
//!
//! One job: answer the committed shortcut from the located document, or decline
//! so the residual graph runs. Exact starts at `located.node()` with an emptied
//! path — see [`jqf_data::Document::count_children_from`] /
//! [`jqf_data::Document::visit_elements_from`]. `node == root` does not tell Exact
//! from Whole. Lenient-only. Decline is byte-identical to the graph.
//!
//! `any`/`all` and `min`/`max` may lose to the graph on measured fixtures;
//! execute still owns those arms. Identity echo and range-locate stay host I/O
//! ([`crate::HostIo`]); those shortcut arms fall through here as `None`.
//!
//! Sibling: [`super`] owns the graph interpreter; [`crate::compile`] owns
//! finish, charge, shortcut commit, and the thin [`CompiledProgram::execute`]
//! job match. Codecs never see a shortcut.

use super::{EngineRun, EngineRunStream, RunInput};
use crate::compile::{Access, CompiledProgram};
use alloc::vec::Vec;
use jqf_builtins::codec_result::{CodecInputOutcome, EngineResult};
use jqf_codec_core::{CodecError, CodecFailureKind, LocatedProduct};
use jqf_data::{CountRow, Integer, Number, ObjectBuilder, ObjectKey, Value};
use jqf_resource::ResourceContext;

impl CompiledProgram {
    pub(crate) fn count_answer<'source>(
        &self,
        outcome: &CodecInputOutcome<'source>,
        demand: &jqf_data::CountDemand,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineRun<'_, 'source>>, CodecError> {
        let CodecInputOutcome::Result(EngineResult::Located(located)) = outcome else {
            return Ok(None);
        };
        let document = located.product().document();
        let located_demand;
        let (start, demand) = if self.exact_located() {
            located_demand = jqf_data::CountDemand {
                path: Vec::new(),
                ..demand.clone()
            };
            (located.node(), &located_demand)
        } else {
            (document.root_handle(), demand)
        };
        match document.count_children_from(start, demand, resources) {
            Ok(jqf_data::CountVerdict::Count(n)) => Ok(Some(stream_value(count_u64(n)?))),
            Ok(jqf_data::CountVerdict::Decline) | Err(_) => {
                if self.exact_located() && container_length_row(demand) {
                    match jqf_builtins::registry::builtins::core::length(
                        &EngineResult::Located(
                            LocatedProduct::try_new(located.product(), start)
                                .map_err(|_| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?,
                        ),
                        resources,
                    ) {
                        Ok(value) => Ok(Some(stream_value(value))),
                        Err(error) => Ok(Some(stream_fail(error))),
                    }
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub(crate) fn keys_answer<'source>(
        &self,
        outcome: &CodecInputOutcome<'source>,
        path: &[jqf_data::CountStep],
        resources: &ResourceContext<'_>,
    ) -> Option<EngineRun<'_, 'source>> {
        let result = located_at_path(outcome, self.exact_located(), path)?;
        if self.exact_located()
            && let EngineResult::Located(located) = &result
        {
            let document = located.product().document();
            let kind = document
                .value_view(located.node())
                .ok()
                .and_then(|view| view.kind().ok());
            // Array keys are numbers `[0, n)`; object names stay the string cache.
            if kind == Some(jqf_data::ValueKind::Array) {
                if let Ok(Some(len)) = document.container_span_child_count(located.node()) {
                    return keys_from_array_len(len, resources);
                }
            } else if let Ok(Some(names)) = document.container_span_keys(located.node()) {
                return keys_from_cached_names(names, resources);
            }
        }
        let value = jqf_builtins::registry::builtins::core::keys(
            &result,
            jqf_builtins::registry::builtins::core::KeyOrder::Sorted,
            resources,
        )
        .ok()?;
        Some(stream_value(value))
    }

    pub(crate) fn type_answer<'source>(
        &self,
        outcome: &CodecInputOutcome<'source>,
        path: &[jqf_data::CountStep],
        resources: &ResourceContext<'_>,
    ) -> Option<EngineRun<'_, 'source>> {
        let result = located_at_path(outcome, self.exact_located(), path)?;
        let value = jqf_builtins::registry::builtins::core::type_name(&result, resources).ok()?;
        Some(stream_value(value))
    }

    pub(crate) fn has_answer<'source>(
        &self,
        outcome: &CodecInputOutcome<'source>,
        demand: &crate::HasDemand,
        resources: &ResourceContext<'_>,
    ) -> Option<EngineRun<'_, 'source>> {
        let result = located_at_path(outcome, self.exact_located(), &demand.path)?;
        if self.exact_located()
            && let EngineResult::Located(located) = &result
            && let Ok(Some(present)) = located.product().document().container_span_has_present(located.node())
        {
            return Some(stream_value(Value::Bool(present)));
        }
        let value = jqf_builtins::registry::builtins::reshape::has(&result, &demand.key, resources).ok()?;
        Some(stream_value(value))
    }

    pub(crate) fn any_all_answer<'source>(
        &self,
        outcome: &CodecInputOutcome<'source>,
        demand: &crate::AnyAllDemand,
        resources: &mut ResourceContext<'_>,
    ) -> Option<EngineRun<'_, 'source>> {
        let CodecInputOutcome::Result(EngineResult::Located(located)) = outcome else {
            return None;
        };
        let document = located.product().document();
        let (start, path) = if self.exact_located() || demand.path.is_empty() {
            (located.node(), Vec::new())
        } else {
            (document.root_handle(), demand.path.clone())
        };
        if self.exact_located()
            && let (Ok(Some(hits)), Ok(Some(len))) = (
                document.container_span_filter_count(start),
                document.container_span_child_count(start),
            )
        {
            let answer = match demand.polarity {
                crate::AnyAllPolarity::Any => hits > 0,
                crate::AnyAllPolarity::All => hits == len,
            };
            return Some(stream_value(Value::Bool(answer)));
        }
        let visit_demand = jqf_data::ElementDemand {
            row: jqf_data::ElementRow::ReduceFold,
            path,
            range: None,
            probe: jqf_data::ElementProbe::Path(Vec::new()),
            increment: None,
            filter: None,
        };
        let mut declined = false;
        let mut decided = None;
        // ReduceFold skips the FanOut all-or-none pre-pass. A visitor Err
        // unwinds the walk so `any`/`all` can stop on the first decisive
        // item the graph would stop on; the error is not a document failure.
        let verdict = document.visit_elements_from(start, &visit_demand, resources, |item, _| {
            match demand.filter.contributes(item) {
                None => {
                    declined = true;
                    Err(jqf_data::DataError::InvalidDocument)
                }
                Some(0) if matches!(demand.polarity, crate::AnyAllPolarity::All) => {
                    decided = Some(false);
                    Err(jqf_data::DataError::InvalidDocument)
                }
                Some(n) if n != 0 && matches!(demand.polarity, crate::AnyAllPolarity::Any) => {
                    decided = Some(true);
                    Err(jqf_data::DataError::InvalidDocument)
                }
                Some(_) => Ok(()),
            }
        });
        if let Some(answer) = decided {
            return Some(stream_value(Value::Bool(answer)));
        }
        if declined || !matches!(verdict, Ok(jqf_data::ElementVerdict::Completed(_))) {
            return None;
        }
        let answer = match demand.polarity {
            crate::AnyAllPolarity::Any => false,
            crate::AnyAllPolarity::All => true,
        };
        Some(stream_value(Value::Bool(answer)))
    }

    pub(crate) fn min_max_answer<'source>(
        &self,
        outcome: &CodecInputOutcome<'source>,
        demand: &crate::MinMaxDemand,
        resources: &mut ResourceContext<'_>,
    ) -> Option<EngineRun<'_, 'source>> {
        let CodecInputOutcome::Result(EngineResult::Located(located)) = outcome else {
            return None;
        };
        let document = located.product().document();
        let start = if self.exact_located() || demand.path.is_empty() {
            located.node()
        } else {
            path_node(document, document.root_handle(), &demand.path)?
        };
        let view = document.value_view(start).ok()?;
        if view.kind().ok()? != jqf_data::ValueKind::Array {
            return None;
        }
        if self.exact_located()
            && let Ok(Some(winner)) = document.container_span_minmax(start)
        {
            return Some(stream_value(winner.clone()));
        }
        let visit_demand = jqf_data::ElementDemand {
            row: jqf_data::ElementRow::FanOut,
            path: Vec::new(),
            range: None,
            probe: jqf_data::ElementProbe::Path(Vec::new()),
            increment: None,
            filter: None,
        };
        let mut declined = false;
        let mut winner: Option<(jqf_data::Value, jqf_data::Value)> = None;
        let verdict = document
            .visit_elements_from(start, &visit_demand, resources, |item, _| {
                if declined {
                    return Ok(());
                }
                let Some(key) = min_max_key(item, demand.probe.as_deref()) else {
                    declined = true;
                    return Ok(());
                };
                match winner.as_mut() {
                    None => winner = Some((key.clone(), item.clone())),
                    Some((incumbent_key, incumbent)) => {
                        match jqf_builtins::semantics::order::total_cmp(key, incumbent_key) {
                            Ok(core::cmp::Ordering::Less) if demand.op == crate::MinMaxOp::Min => {
                                *incumbent_key = key.clone();
                                *incumbent = item.clone();
                            }
                            Ok(core::cmp::Ordering::Greater | core::cmp::Ordering::Equal)
                                if demand.op == crate::MinMaxOp::Max =>
                            {
                                *incumbent_key = key.clone();
                                *incumbent = item.clone();
                            }
                            Ok(_) => {}
                            Err(_) => declined = true,
                        }
                    }
                }
                Ok(())
            })
            .ok()?;
        if declined || !matches!(verdict, jqf_data::ElementVerdict::Completed(_)) {
            return None;
        }
        Some(stream_value(match winner {
            None => Value::Null,
            Some((_, element)) => element,
        }))
    }

    pub(crate) fn element_answer<'program, 'source>(
        &self,
        outcome: &CodecInputOutcome<'source>,
        demand: &jqf_data::ElementDemand,
        construct: Option<&[(alloc::string::String, Vec<jqf_data::CountStep>)]>,
        collect: bool,
        resources: &mut ResourceContext<'_>,
    ) -> Option<EngineRun<'program, 'source>> {
        let CodecInputOutcome::Result(EngineResult::Located(located)) = outcome else {
            return None;
        };
        let document = located.product().document();
        let mut located_demand;
        let (start, demand) = if self.exact_located() {
            located_demand = demand.clone();
            located_demand.path.clear();
            (located.node(), &located_demand)
        } else {
            (document.root_handle(), demand)
        };
        if self.exact_located()
            && let Ok(Some(values)) = document.container_span_values(start)
        {
            return cached_fan_out(values, collect);
        }
        match demand.row {
            jqf_data::ElementRow::FanOut => {
                if let Some(fields) = construct {
                    return construct_fan_out(document, start, demand, fields, collect, resources);
                }
                let mut values: Vec<Value> = Vec::new();
                let mut visitor = |value: &Value, _resources: &mut ResourceContext<'_>| {
                    if values.try_reserve(1).is_err() {
                        return Err(jqf_data::DataError::InvalidDocument);
                    }
                    values.push(value.clone());
                    Ok(())
                };
                match document.visit_elements_from(start, demand, resources, &mut visitor) {
                    Ok(jqf_data::ElementVerdict::Completed(_)) if collect => {
                        let Ok(array) = jqf_data::Array::try_from_vec(values) else {
                            return None;
                        };
                        Some(stream_value(Value::Array(array)))
                    }
                    Ok(jqf_data::ElementVerdict::Completed(_)) => Some(stream_values(values)),
                    Ok(jqf_data::ElementVerdict::Decline) | Err(_) => None,
                }
            }
            jqf_data::ElementRow::ReduceFold => fold_histogram(document, start, demand, resources),
        }
    }

    fn exact_located(&self) -> bool {
        matches!(self.plan.kind, Access::Exact)
    }
}

fn container_length_row(demand: &jqf_data::CountDemand) -> bool {
    demand.row == CountRow::Container && demand.range.is_none() && demand.probe.is_empty() && demand.filter.is_none()
}

fn stream_value<'program, 'source>(value: Value) -> EngineRun<'program, 'source> {
    EngineRun::Stream {
        stream: EngineRunStream::seed_value(value),
        input: RunInput::Resolved,
    }
}

fn stream_values<'program, 'source>(values: Vec<Value>) -> EngineRun<'program, 'source> {
    EngineRun::Stream {
        stream: EngineRunStream::seed_owned(values),
        input: RunInput::Resolved,
    }
}

fn stream_fail<'program, 'source>(error: jqf_builtins::error::EngineRunError) -> EngineRun<'program, 'source> {
    EngineRun::Stream {
        stream: EngineRunStream::seed_fail(error),
        input: RunInput::Resolved,
    }
}

fn cached_fan_out<'program, 'source>(values: &[Value], collect: bool) -> Option<EngineRun<'program, 'source>> {
    let mut owned: Vec<Value> = Vec::new();
    owned.try_reserve(values.len()).ok()?;
    owned.extend_from_slice(values);
    if collect {
        let array = jqf_data::Array::try_from_vec(owned).ok()?;
        Some(stream_value(Value::Array(array)))
    } else {
        Some(stream_values(owned))
    }
}

fn count_u64(count: u64) -> Result<Value, CodecError> {
    let integer = i64::try_from(count)
        .map(Integer::from_i64)
        .map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
    Ok(Value::Number(Number::integer(integer)))
}

fn navigate_count_path<'d, 's>(
    mut view: jqf_data::ValueView<'d, 's>,
    path: &[jqf_data::CountStep],
) -> Option<jqf_data::ValueView<'d, 's>> {
    for step in path {
        match step {
            jqf_data::CountStep::ObjectKey(key) => {
                let object = view.object().ok().flatten()?;
                view = object.get(key.as_str())?;
            }
            jqf_data::CountStep::ArrayIndex(index) => {
                let array = view.array().ok().flatten()?;
                let resolved = jqf_data::resolve_index(array.len(), *index)?;
                view = array.get(resolved)?;
            }
        }
    }
    Some(view)
}

fn path_node(
    document: &jqf_data::Document<'_>,
    start: jqf_data::NodeHandle,
    path: &[jqf_data::CountStep],
) -> Option<jqf_data::NodeHandle> {
    let view = navigate_count_path(document.value_view(start).ok()?, path)?;
    document.node_handle(view.node()).ok()
}

fn located_at_path<'source>(
    outcome: &CodecInputOutcome<'source>,
    exact: bool,
    path: &[jqf_data::CountStep],
) -> Option<EngineResult<'source>> {
    let CodecInputOutcome::Result(EngineResult::Located(located)) = outcome else {
        return None;
    };
    if exact || path.is_empty() {
        return Some(EngineResult::Located(
            LocatedProduct::try_new(located.product(), located.node()).ok()?,
        ));
    }
    let document = located.product().document();
    let node = path_node(document, document.root_handle(), path)?;
    Some(EngineResult::Located(
        LocatedProduct::try_new(located.product(), node).ok()?,
    ))
}

fn min_max_key<'a>(element: &'a Value, probe: Option<&str>) -> Option<&'a Value> {
    let key = match probe {
        None => element,
        Some(name) => match element.untagged() {
            Value::Object(object) => object.get(name)?,
            _ => return None,
        },
    };
    let Value::Number(number) = key.untagged() else {
        return None;
    };
    match number.as_float() {
        Some(float) if !float.get().is_finite() => None,
        _ => Some(key),
    }
}

fn construct_fan_out<'program, 'source>(
    document: &jqf_data::Document<'_>,
    start: jqf_data::NodeHandle,
    demand: &jqf_data::ElementDemand,
    fields: &[(alloc::string::String, Vec<jqf_data::CountStep>)],
    collect: bool,
    resources: &mut ResourceContext<'_>,
) -> Option<EngineRun<'program, 'source>> {
    if fields.is_empty() {
        return None;
    }
    if collect
        && fields
            .iter()
            .all(|(_, path)| matches!(path.as_slice(), [jqf_data::CountStep::ObjectKey(_)]))
    {
        return collect_construct_columns(document, start, demand, fields, resources);
    }
    let mut values: Vec<Value> = Vec::new();
    let mut visitor = |value: &Value, _resources: &mut ResourceContext<'_>| {
        let Some(constructed) = construct_static_object(value, fields) else {
            return Err(jqf_data::DataError::InvalidDocument);
        };
        if values.try_reserve(1).is_err() {
            return Err(jqf_data::DataError::InvalidDocument);
        }
        values.push(constructed);
        Ok(())
    };
    match document.visit_elements_from(start, demand, resources, &mut visitor) {
        Ok(jqf_data::ElementVerdict::Completed(_)) if collect => {
            let array = jqf_data::Array::try_from_vec(values).ok()?;
            Some(stream_value(Value::Array(array)))
        }
        Ok(jqf_data::ElementVerdict::Completed(_)) => Some(stream_values(values)),
        Ok(jqf_data::ElementVerdict::Decline) | Err(_) => None,
    }
}

fn collect_construct_columns<'program, 'source>(
    document: &jqf_data::Document<'_>,
    start: jqf_data::NodeHandle,
    demand: &jqf_data::ElementDemand,
    fields: &[(alloc::string::String, Vec<jqf_data::CountStep>)],
    resources: &mut ResourceContext<'_>,
) -> Option<EngineRun<'program, 'source>> {
    let mut columns: Vec<Vec<Value>> = Vec::new();
    columns.try_reserve(fields.len()).ok()?;
    for (_, path) in fields {
        let mut field_demand = demand.clone();
        field_demand.probe = jqf_data::ElementProbe::Path(path.clone());
        let mut column: Vec<Value> = Vec::new();
        let mut visitor = |value: &Value, _resources: &mut ResourceContext<'_>| {
            if column.try_reserve(1).is_err() {
                return Err(jqf_data::DataError::InvalidDocument);
            }
            column.push(value.clone());
            Ok(())
        };
        match document.visit_elements_from(start, &field_demand, resources, &mut visitor) {
            Ok(jqf_data::ElementVerdict::Completed(_)) => columns.push(column),
            Ok(jqf_data::ElementVerdict::Decline) | Err(_) => return None,
        }
    }
    let width = columns.first().map(Vec::len)?;
    if columns.iter().any(|column| column.len() != width) {
        return None;
    }
    let mut values: Vec<Value> = Vec::new();
    values.try_reserve(width).ok()?;
    for row in 0..width {
        let mut builder = ObjectBuilder::try_with_capacity(fields.len()).ok()?;
        for (column, (key, _)) in columns.iter().zip(fields) {
            let key = ObjectKey::try_from_str(key).ok()?;
            builder.try_insert_or_replace(key, column[row].clone()).ok()?;
        }
        values.push(Value::Object(builder.try_finish().ok()?));
    }
    let array = jqf_data::Array::try_from_vec(values).ok()?;
    Some(stream_value(Value::Array(array)))
}

fn construct_static_object(
    element: &Value,
    fields: &[(alloc::string::String, Vec<jqf_data::CountStep>)],
) -> Option<Value> {
    let mut builder = ObjectBuilder::try_with_capacity(fields.len()).ok()?;
    for (key, path) in fields {
        let probe = jqf_data::ElementProbe::Path(path.clone());
        let value = jqf_data::owned_probe_value(element, &probe)?;
        let key = ObjectKey::try_from_str(key).ok()?;
        builder.try_insert_or_replace(key, value).ok()?;
    }
    builder.try_finish().ok().map(Value::Object)
}

fn fold_histogram<'program, 'source>(
    document: &jqf_data::Document<'_>,
    start: jqf_data::NodeHandle,
    demand: &jqf_data::ElementDemand,
    resources: &mut ResourceContext<'_>,
) -> Option<EngineRun<'program, 'source>> {
    let delta = demand.increment?;
    let mut counts: Vec<(alloc::string::String, i64)> = Vec::new();
    let mut visitor = |value: &Value, _resources: &mut ResourceContext<'_>| {
        let Value::String(key_text) = value.untagged() else {
            return Err(jqf_data::DataError::InvalidDocument);
        };
        let key = key_text.as_str();
        if let Some((_, count)) = counts.iter_mut().find(|(name, _)| name == key) {
            let Some(sum) = count.checked_add(delta) else {
                return Err(jqf_data::DataError::InvalidDocument);
            };
            *count = sum;
        } else {
            if counts.try_reserve(1).is_err() {
                return Err(jqf_data::DataError::InvalidDocument);
            }
            counts.push((key.into(), delta));
        }
        Ok(())
    };
    let Ok(jqf_data::ElementVerdict::Completed(_)) =
        document.visit_elements_from(start, demand, resources, &mut visitor)
    else {
        return None;
    };
    let mut builder = ObjectBuilder::try_with_capacity(counts.len()).ok()?;
    for (key, count) in counts {
        let key = ObjectKey::try_from_str(&key).ok()?;
        builder
            .try_insert_or_replace(key, Value::Number(Number::integer(Integer::from_i64(count))))
            .ok()?;
    }
    Some(stream_value(Value::Object(builder.try_finish().ok()?)))
}

fn keys_from_cached_names<'program, 'source>(
    names: &[alloc::string::String],
    _resources: &ResourceContext<'_>,
) -> Option<EngineRun<'program, 'source>> {
    let mut names = names.to_vec();
    names.sort();
    let mut values = Vec::new();
    values.try_reserve(names.len()).ok()?;
    for name in names {
        values.push(Value::try_string(&name).ok()?);
    }
    let array = jqf_data::Array::try_from_vec(values).ok()?;
    Some(stream_value(Value::Array(array)))
}

fn keys_from_array_len<'program, 'source>(
    len: u64,
    _resources: &ResourceContext<'_>,
) -> Option<EngineRun<'program, 'source>> {
    let len = usize::try_from(len).ok()?;
    let mut values = Vec::new();
    values.try_reserve(len).ok()?;
    for index in 0..len {
        values.push(count_u64(u64::try_from(index).ok()?).ok()?);
    }
    let array = jqf_data::Array::try_from_vec(values).ok()?;
    Some(stream_value(Value::Array(array)))
}
