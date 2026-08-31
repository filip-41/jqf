//! Call dispatch and the [`GraphMachine`] helpers every family shares.
//!
//! One job: map a resolved [`Evaluator`] onto the family that owns that
//! builtin, plus the path-register and stage-descend helpers families share.
//! Binary operators, user-call/argument product, keyed aggregates, the
//! remaining builtins, generators, modify/assign, and join indexes live in
//! sibling files. Seed, bind/fold, path-family handoff, and emission routing
//! stay in [`super::eval`], [`super::fold`], [`super::pathmode`], and
//! [`super::route`].

#[allow(clippy::wildcard_imports)]
use super::*;

mod binary;
mod builtins;
mod call;
mod generate;
mod join;
mod keyed;
mod modify;

impl<'program, 'source> GraphMachine<'program, 'source> {
    /// Dispatches one resolved `Call` node over `input`. `length`/`keys` are pure
    /// evaluators producing exactly one routed output (their owned results are
    /// ledger-charged); `select` sets up its predicate consumer frame. Every
    /// stored `Call` is an `Evaluator` — `Lowering` builtins never reach the
    /// arena.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per evaluator family: the call dispatch IS the table, and \
                  splitting it would hide which evaluators are covered"
    )]
    pub(crate) fn eval_call(
        &mut self,
        payload: Evaluator,
        node: ProgramNodeId,
        input: EngineResult<'source>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        // A builtin-domain raise (no-length / no-keys / `error/0`) fails a value
        // routing at this call's `cont`.
        self.raise_cont = cont;
        match payload {
            Evaluator::Length => {
                let result = core_builtins::length(&input, resources)?;
                let result = self.retain_owned(result);
                self.route(result, cont, resources)
            }
            Evaluator::Keys => {
                let value = core_builtins::keys(&input, core_builtins::KeyOrder::Sorted, resources)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            Evaluator::Type => {
                let value = core_builtins::type_name(&input, resources)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            // `tag` reads one intrinsic fact of the input node, so a LOCATED
            // input is never materialized to answer it.
            Evaluator::Tag => {
                let value = core_builtins::tag_name(&input, resources)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            Evaluator::JsonFacts => {
                let value = facts_builtins::json_facts_cached(&input, resources, &mut self.json_facts_cache)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            // `_negate` reads the input's number and nothing else, so a LOCATED
            // operand is re-signed from its borrowed representation rather than
            // materialized: the owned round trip canonicalizes a retained decimal
            // and `-1.500` would answer `-1.5`.
            Evaluator::Negate => {
                let value = core_builtins::negate(&input, resources)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            Evaluator::Select => self.eval_select(node, input, cont, resources),
            // The kind filters (`objects`, `arrays`, …): the admitted input is
            // routed UNCHANGED — no predicate frame, no re-borrow, and no owned
            // copy, so a located value stays a located handle — and a rejected
            // one simply produces nothing.
            Evaluator::Kind(filter) => {
                if kind_builtins::admits(filter, &input, resources)? {
                    self.route(input, cont, resources)
                } else {
                    Ok(None)
                }
            }
            Evaluator::Not => {
                // `not` is the boolean of the input's FALSINESS: `false`/`null`
                // input → `true`, every other value (including `0`, `""`) → `false`.
                let truthy = truth::is_truthy(&input).map_err(|_| internal_contract("not truth check failed"))?;
                let result = self.retain_owned(Value::Bool(!truthy));
                self.route(result, cont, resources)
            }
            Evaluator::ErrorZero => {
                // `error/0` raises the current input as the error value; it never
                // routes a normal output. The error walks to the nearest `try`.
                let value = self.materialize_capture(input, resources)?;
                Err(EngineRunError::Raised(value))
            }
            Evaluator::ErrorOne => self.eval_error_arg(node, input, cont),
            // The path-family evaluators: the tracked families share the
            // walker drive, the reads answer per argument output.
            Evaluator::Path => self.eval_path_family(path_register::PathFamily::Path, node, input, cont, resources),
            Evaluator::Paths => self.eval_path_family(path_register::PathFamily::Paths, node, input, cont, resources),
            Evaluator::GetPath => {
                self.eval_path_family(path_register::PathFamily::GetPath, node, input, cont, resources)
            }
            Evaluator::SetPath => {
                self.eval_path_family(path_register::PathFamily::SetPath, node, input, cont, resources)
            }
            Evaluator::DelPaths => {
                self.eval_path_family(path_register::PathFamily::DelPaths, node, input, cont, resources)
            }
            // The codec-native selector seam: `xpath/1` runs the xml.xpath@1
            // profile and `css/1` runs the html.css@1 profile over the located
            // document authority of the input. Both are pure, deterministic
            // laws; a missing authority, a format mismatch, a compile
            // rejection, or a budget exhaustion is a catchable string raise.
            Evaluator::XPath => self.eval_selector(selector_builtins::SelectorLaw::XPath, node, input, cont, resources),
            Evaluator::Css => self.eval_selector(selector_builtins::SelectorLaw::Css, node, input, cont, resources),
            // The arity-0 ordering forms and the value-shaping evaluators are
            // pure owned-value laws: materialize once, compute, route. Their
            // rejections are raised before anything is published.
            Evaluator::Whole(form) => self.eval_owned_law(input, cont, resources, |subject, resources| {
                order_builtins::whole(form, subject, resources)
            }),
            Evaluator::Reverse => self.eval_owned_law(input, cont, resources, order_builtins::reverse),
            Evaluator::ToString => {
                // A STRING passes through unchanged, and a located one stays
                // located: `tostring` is the identity on strings, so there is
                // nothing to render and nothing to copy.
                //
                // A TAGGED string is excluded from that shortcut, because it is
                // not identity there: `text_builtins::tostring` — the law this
                // arm is an optimization OF — begins with `input.untagged()`,
                // so the builtin's own answer is a CORE string. `type` is
                // payload-transparent, so the kind filter alone cannot see the
                // difference, and taking the shortcut published a value still
                // carrying a tag, which the JSON encoder then refused outright.
                if kind_builtins::admits(kind_builtins::KindFilter::String, &input, resources)?
                    && !carries_non_core_tag(&input)?
                {
                    return self.route(input, cont, resources);
                }
                self.eval_owned_law(input, cont, resources, text_builtins::tostring)
            }
            Evaluator::ToJson => self.eval_owned_law(input, cont, resources, text_builtins::tojson),
            Evaluator::ToEntries => self.eval_owned_law(input, cont, resources, entries_builtins::to_entries),
            Evaluator::FromEntries => self.eval_owned_law(input, cont, resources, entries_builtins::from_entries),
            // `keys_unsorted` reads the input WITHOUT materializing it, which is
            // the whole point of the conservative `Subtree` transfer it declares.
            Evaluator::KeysUnsorted => {
                let value = core_builtins::keys(&input, core_builtins::KeyOrder::Insertion, resources)?;
                let result = self.retain_owned(value);
                self.route(result, cont, resources)
            }
            // The two argument-driven families open a frame instead: their
            // argument is an ordinary filter over the same input, and every
            // output it produces contributes.
            Evaluator::Keyed(mode) => self.eval_keyed(mode, node, input, cont, resources),
            Evaluator::BSearch => self.eval_answer(AnswerForm::BinarySearch, node, input, cont, resources),
            Evaluator::Add => self.eval_owned_law(input, cont, resources, reshape_builtins::add),
            // `flatten` splits on ARITY, not on identity: the depth-bounded form
            // answers once per depth its argument yields, while the unbounded
            // form has no argument to drive and is an ordinary owned law.
            Evaluator::Flatten => match &self.nodes[node.index()] {
                ProgramNode::Call { args, .. } if args.is_empty() => {
                    self.eval_owned_law(input, cont, resources, |subject, resources| {
                        reshape_builtins::flatten(subject, None, resources)
                    })
                }
                ProgramNode::Call { .. } => self.eval_answer(AnswerForm::Flatten, node, input, cont, resources),
                _ => Err(internal_contract("flatten dispatch over a non-call node")),
            },
            // `join` answers once per SEPARATOR its argument yields, because
            // the separator is bound outside the fold — `["a","b"] |
            // [join("-","+")]` is two joins of the same input, not one join of
            // two separators.
            Evaluator::Join => self.eval_answer(AnswerForm::Join, node, input, cont, resources),
            Evaluator::Format => self.eval_answer(AnswerForm::Format, node, input, cont, resources),
            Evaluator::Text(law) => self.eval_answer(AnswerForm::Text(law), node, input, cont, resources),
            Evaluator::Scalar(law) => self.eval_owned_law(input, cont, resources, move |subject, resources| {
                string_builtins::apply(law, subject, resources)
            }),
            // The math family splits on SHAPE, not on name. The /0 forms are
            // pure owned-value laws — one output per input, with `nan`/
            // `infinite` ignoring the input entirely and the `isnan` quartet
            // answering `false` for a non-number without raising. The /2 and
            // /3 forms drive their filter arguments over the SAME input in
            // the right-outer order and ignore the piped value once every
            // parenthesized argument is present.
            Evaluator::Math(evaluator) => self.eval_math(evaluator, node, input, cont, resources),
            // The date/time family splits on SHAPE: the /0 forms are owned
            // value laws (`now` ignores the piped value entirely and publishes
            // the wall clock), and the /1 format laws answer once per output
            // of their format argument over the same input, driven by the
            // argument-answer frame.
            Evaluator::Time(evaluator) => self.eval_time(evaluator, node, input, cont, resources),
            // The regex family evaluates its pattern/flags argument filters
            // eagerly over the same input (the nested-machine drive), and
            // `sub`/`gsub` evaluate their REPLACEMENT filter once per match
            // with dot = the capture object. The outputs stream out of a
            // producer frame.
            Evaluator::Regex(evaluator) => self.eval_regex(evaluator, node, input, cont, resources),
            // The misc riders: `builtins`/`have_decnum` publish owned values,
            // and `debug` passes the piped value through unchanged (`/1` after
            // its message argument has run).
            Evaluator::Rider(evaluator) => self.eval_rider(evaluator, node, input, cont, resources),
            // The host-state/process family: every current law ignores the piped
            // value and publishes one owned value read from the request's
            // environment snapshot, or terminates the run (`halt`/`halt_error`).
            Evaluator::Process(evaluator) => self.eval_process(evaluator, node, input, cont, resources),
            // The streaming utilities: `tostream` walks the owned input and
            // publishes the ordered pair/marker stream; `fromstream`/
            // `truncate_stream` run their argument filter and transform its
            // outputs. All three stream out of a `PathEmit` producer frame.
            Evaluator::Streams(evaluator) => self.eval_streams(evaluator, node, input, cont, resources),
            // The jqf extension families: pure value laws over the input (or
            // over the first output of their filter arguments, the
            // argument-evaluation law).
            #[cfg(feature = "ext-hash")]
            Evaluator::Extension(law) => self.eval_extension(law, node, input, cont, resources),
            // The DIFF verb: evaluate its two filter arguments (the old and
            // the new document) over the input, then apply the path-keyed
            // semantic diff law.
            Evaluator::Diff => self.eval_diff(node, input, cont, resources),
            // The ANALYTICS family: sample/shuffle are impure value laws over
            // the piped array (shuffle unary, sample with its count argument
            // evaluated by the ordinary argument law); fill_forward is a pure
            // copy law.
            #[cfg(feature = "ext-hash")]
            Evaluator::Analytics(law) => self.eval_analytics(law, node, input, cont, resources),
            // The RAND family: uniform floats/integers and uniform element
            // choice. The unseeded forms are impure effects; `rand(seed)` is
            // the deterministic seeded exception.
            #[cfg(feature = "ext-hash")]
            Evaluator::Rand(law) => self.eval_rand(law, node, input, cont, resources),
            // The IP/CIDR family: pure value laws over the piped string, with
            // `ip_in_cidr`'s CIDR filter argument evaluated over the same
            // input (the argument-evaluation law) before the law runs.
            #[cfg(feature = "ext-net")]
            Evaluator::Net(law) => self.eval_net(law, node, input, cont, resources),
            // The TOP-K partial sort: bounded-heap O(n log k) with optional
            // per-element projection filter.
            Evaluator::TopK(law) => self.eval_topk(law, node, input, cont, resources),
            // The PARSERS family: pure string-to-object laws over the piped
            // value. `parse_grok` evaluates its pattern filter argument over
            // the same input (the argument-evaluation law) before the law
            // runs; the unary parsers never touch their (empty) argument list.
            Evaluator::Parser(law) => self.eval_parser(law, node, input, cont, resources),
            // The JSON-Pointer family: navigate an RFC 6901 pointer string
            // over the piped value (read form) or over each value a source
            // filter yields, emitting one `[match]`/`[]` array per source.
            Evaluator::Pointer(law) => self.eval_pointer(law, node, input, cont, resources),
            // The JSONPath family: evaluate an RFC 9535 query string over the
            // piped value (read form) or over each value a source filter
            // yields, emitting one nodelist array per query per source.
            #[cfg(feature = "ext-jsonpath")]
            Evaluator::JsonPath(law) => self.eval_jsonpath(law, node, input, cont, resources),
            // The schema family: infer a JSON Schema 2020-12 document from the
            // evaluated VALUE argument, or validate an evaluated value against
            // an evaluated schema document (boolean or ordered error objects).
            #[cfg(feature = "ext-schema")]
            Evaluator::Schema(law) => self.eval_schema(law, node, input, cont, resources),
            // The user-declared reusable index (`declare_index/2`): a TRANSPARENT
            // acceleration declaration — build a sorted keyed index over a
            // located container and pass the input through unchanged. Every
            // decline is a no-op declaration, never a raise.
            Evaluator::IndexDeclare => self.eval_index_declare(node, input, cont, resources),
            Evaluator::Transpose => self.eval_owned_law(input, cont, resources, reshape_builtins::transpose),
            // `has` reads the input WITHOUT materializing it — the one real
            // claim behind the conservative `Subtree` transfer it declares.
            Evaluator::Has => self.eval_answer(AnswerForm::HasKey, node, input, cont, resources),
            Evaluator::Walk => self.eval_walk(node, input, cont, resources),
            Evaluator::MapValues => self.eval_map_values(node, input, cont, resources),
            // The generator family. Each opens a `Generate` frame and returns:
            // a source's values are produced by RESUMING that frame, never by
            // an eager loop here.
            Evaluator::Generate(generator) => self.eval_generate(generator, node, input, cont, resources),
        }
    }

    pub(crate) fn path_enter_frozen(&mut self) {
        if let Some(register) = self.path.as_mut() {
            register.enter_frozen();
        }
    }

    /// Leaves one frozen position at its consumer (`exit` restores the
    /// snapshot; `abandon` — the raise walk or a cut popped the drive — only
    /// releases the freeze).
    pub(crate) fn path_exit_frozen(&mut self) {
        if let Some(register) = self.path.as_mut() {
            register.exit_frozen();
        }
    }

    pub(crate) fn path_abandon_frozen(&mut self) {
        if let Some(register) = self.path.as_mut() {
            register.abandon_frozen();
        }
    }

    /// Whether the register is currently frozen.
    pub(crate) fn path_frozen(&self) -> bool {
        self.path.as_ref().is_some_and(path_register::PathRegister::is_frozen)
    }

    /// The owned form of one value in flight (located materializes).
    pub(crate) fn path_owned(
        &mut self,
        value: &EngineResult<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        match value {
            EngineResult::Owned(owned) => Ok(owned.clone()),
            EngineResult::Located(_) => {
                self.materialize_capture(value.try_clone().map_err(EngineRunError::Codec)?, resources)
            }
        }
    }

    /// Law-2 pre-check before a stage descent.
    pub(crate) fn path_check_navigation(
        &mut self,
        steps: &[StageStep],
        at: usize,
        value: &EngineResult<'source>,
        _cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let Some(step) = steps.get(at) else {
            return Ok(());
        };
        if self.path_frozen() {
            return Ok(());
        }
        let tracked = self.path.as_ref().is_some_and(|register| register.tracks_result(value));
        if tracked {
            return Ok(());
        }
        let owned = self.path_owned(value, resources)?;
        // The untracked rejection (`.[]` = iterate class, else access class).
        match step.access() {
            StepAccess::Each => Err(jqf_builtins::semantics::path::invalid_iterate(&owned, resources)),
            StepAccess::Descend => Ok(()),
            _ => {
                let env_slots = self.path_env_slots(resources)?;
                let Some(accessor) = path_register::step_component_value(step, &env_slots, resources)? else {
                    return Ok(());
                };
                Err(jqf_builtins::semantics::path::invalid_access(
                    &accessor, &owned, resources,
                ))
            }
        }
    }

    /// The untracked rejection for one step over one value, with the step's
    /// rendered component as the accessor.
    pub(crate) fn path_reject_access(
        &mut self,
        step: &StageStep,
        value: &Value,
        resources: &mut ResourceContext<'_>,
    ) -> Result<EngineRunError, EngineRunError> {
        let env_slots = self.path_env_slots(resources)?;
        let accessor = path_register::step_component_value(step, &env_slots, resources)?
            .ok_or_else(|| EngineRunError::internal_contract("a dynamic step carried a non-component bound"))?;
        Ok(jqf_builtins::semantics::path::invalid_access(
            &accessor, value, resources,
        ))
    }

    /// Law-2 write half: extend the register by one stage's components (the
    /// leaf IS the register's new address by construction — the tracks check
    /// ran in [`Self::path_check_navigation`]).
    pub(crate) fn path_extend_navigation(
        &mut self,
        steps: &[StageStep],
        at: usize,
        leaf: &EngineResult<'source>,
        _cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        if self.path.is_none() || self.path_frozen() {
            return Ok(());
        }
        let at_node = leaf.located().map(LocatedProduct::node);
        let at_value = self.path_owned(leaf, resources)?;
        self.path_extend_components(steps.get(at..).unwrap_or(&[]), &at_value, at_node, resources)
    }

    /// FanOut-prefix write: the steps BEFORE `.[]` extend the register.
    pub(crate) fn path_extend_prefix(
        &mut self,
        steps: &[StageStep],
        at: usize,
        next_at: usize,
        container: &Container<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        if next_at <= at || self.path.is_none() || self.path_frozen() {
            return Ok(());
        }
        // A markup member step fans out through LocatedFiltered: the Key
        // step selected children by element name and is not itself a path
        // component. The document's real paths are the matched indices,
        // recorded per child.
        if matches!(container, Container::LocatedFiltered(..)) {
            return Ok(());
        }
        let (at_value, at_node) = match container {
            Container::Owned(owned) => (owned.clone(), None),
            Container::Located(located) | Container::LocatedFiltered(located, _) => {
                let at_node = Some(located.node());
                let handle = located.try_clone().map_err(EngineRunError::Codec)?;
                (
                    self.materialize_capture(EngineResult::Located(handle), resources)?,
                    at_node,
                )
            }
        };
        self.path_extend_components(&steps[at..next_at], &at_value, at_node, resources)
    }

    /// One tracked extension scoped by a [`GraphFrame::PathStep`] marker.
    pub(crate) fn path_extend_components(
        &mut self,
        prefix: &[StageStep],
        at_value: &Value,
        at_node: Option<jqf_data::NodeHandle>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        let env_slots = self.path_env_slots(resources)?;
        self.path_scope(resources, |register, resources| {
            let components = register.stage_components(prefix, 0, &env_slots, resources)?;
            for component in components {
                register.push_step(component, at_value.clone(), at_node)?;
            }
            Ok(())
        })
    }

    /// Each-child write: the child's component extends the register.
    pub(crate) fn path_each_child(
        &mut self,
        child: &EngineResult<'source>,
        component: Option<jqf_builtins::semantics::path::PathStep>,
        _cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), EngineRunError> {
        if self.path.is_none() || self.path_frozen() {
            return Ok(());
        }
        let Some(component) = component else {
            return Ok(());
        };
        let at_node = child.located().map(LocatedProduct::node);
        let at = self.path_owned(child, resources)?;
        self.path_scope(resources, |register, _| register.push_step(component, at, at_node))
    }

    /// The pre/post of one tracked extension, scoped by a `PathStep` marker.
    pub(crate) fn path_scope(
        &mut self,
        resources: &mut ResourceContext<'_>,
        extend: impl FnOnce(&mut path_register::PathRegister, &mut ResourceContext<'_>) -> Result<(), EngineRunError>,
    ) -> Result<(), EngineRunError> {
        let (steps_len, at, at_node) = {
            let Some(register) = self.path.as_mut() else {
                return Err(internal_contract("path_scope without its register"));
            };
            let pre = register.pre_navigation();
            extend(register, resources)?;
            pre
        };
        self.push_frame(GraphFrame::PathStep { steps_len, at, at_node });
        Ok(())
    }

    /// The owned form of one env slot (a located one materializes).
    pub(crate) fn path_slot_owned(
        &mut self,
        slot: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Value, EngineRunError> {
        match self.env.as_slice().get(slot) {
            Some(EngineResult::Owned(owned)) => Ok(owned.clone()),
            Some(EngineResult::Located(_)) => {
                self.path_owned(&self.env[slot].try_clone().map_err(EngineRunError::Codec)?, resources)
            }
            None => Err(internal_contract(
                "a variable stage read an env slot outside the sized env",
            )),
        }
    }

    /// Returns one owned builtin result as an owned engine result. A computed
    /// value is never the register's: the box makes the allocation identity
    /// reject it (`path(.a | length)` over `{"a":5}` rejects with
    /// `with result 5`).
    pub(crate) fn retain_owned(&self, value: Value) -> EngineResult<'source> {
        if self.path.is_some() {
            EngineResult::owned(path_register::box_computed(value))
        } else {
            EngineResult::owned(value)
        }
    }

    /// Descends a `Stage` node's steps from `at`, routing a leaf through
    /// `[0..cont]` and pushing an `Each` frame on a fan-out.
    pub(crate) fn descend_stage(
        &mut self,
        node: ProgramNodeId,
        value: EngineResult<'source>,
        at: usize,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let nodes = self.nodes;
        let ProgramNode::Stage { steps, .. } = &nodes[node.index()] else {
            return Err(internal_contract("graph machine descended a non-Stage node"));
        };
        // Law 2's check runs BEFORE the descent (`?` never suppresses it).
        // The rejection raises AT the value's routing position — the same
        // `raise_cont` convention the mismatch arm below uses — so an inner
        // `try`/`?` barrier sees it instead of the enclosing path barrier.
        if self.path.is_some()
            && let Err(error) = self.path_check_navigation(steps, at, &value, cont, resources)
        {
            self.raise_cont = cont;
            return Err(error);
        }
        // The stage's own mismatch (index/iterate) fails a value routing at `cont`.
        let descended = {
            let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
            descend(steps, 0, self.env.as_slice(), value, at, &mut scratch)
        };
        self.absorb_descent(node, steps, at, descended, cont, resources)
    }

    /// Descends a `Variable`-start stage over whichever representation the
    /// slot holds, cloning only what it emits. A LOCATED slot takes the
    /// engine's NATIVE walk on a re-borrow (an Arc-cheap `try_clone`, copying
    /// no document bytes), so its leaves stay located; an OWNED slot walks a
    /// BORROW ([`descend_borrowed`]), cloning only the emitted leaf.
    pub(crate) fn descend_variable_stage(
        &mut self,
        node: ProgramNodeId,
        slot: VarSlot,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let nodes = self.nodes;
        let ProgramNode::Stage { steps, .. } = &nodes[node.index()] else {
            return Err(internal_contract("graph machine descended a non-Stage node"));
        };
        // The register's Law-2 pre-check over the BOUND value (a borrow of
        // the slot — a clone would break the allocation identity `$x.a` over
        // a tracked `$x` relies on).
        if self.path.is_some()
            && !self.path_frozen()
            && let Some(step) = steps.first()
            && !matches!(step.access(), StepAccess::Descend)
        {
            // A located slot whose node is the register's current address
            // tracks without rematerializing (the same Law-1 node identity
            // the stage descent uses). An owned slot uses allocation identity.
            let tracked = self
                .env
                .as_slice()
                .get(slot as usize)
                .is_some_and(|held| self.path.as_ref().is_some_and(|register| register.tracks_result(held)));
            if !tracked {
                let owned = self.path_slot_owned(slot as usize, resources)?;
                return match step.access() {
                    StepAccess::Each => Err(jqf_builtins::semantics::path::invalid_iterate(&owned, resources)),
                    _ => Err(self.path_reject_access(step, &owned, resources)?),
                };
            }
        }
        let env = self.env.as_slice();
        let descended = {
            let mut scratch = StepScratch::new(&mut self.workspace, resources).with_deltas(&self.fact_deltas);
            match env.get(slot as usize) {
                Some(EngineResult::Located(located)) => match located.try_clone() {
                    Ok(handle) => descend(steps, 0, env, EngineResult::Located(handle), 0, &mut scratch),
                    Err(error) => Err(EngineRunError::Codec(error)),
                },
                Some(EngineResult::Owned(root)) => descend_borrowed(steps, 0, env, root, 0, &mut scratch),
                None => Err(internal_contract(
                    "a variable stage read an env slot outside the sized env",
                )),
            }
        };
        self.absorb_descent(node, steps, 0, descended, cont, resources)
    }

    /// The env slots as owned values (located ones materialize).
    pub(crate) fn path_env_slots(
        &mut self,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<Option<Value>>, EngineRunError> {
        let mut slots = Vec::new();
        for index in 0..self.env.len() {
            let owned = match self.env[index].try_clone().map_err(EngineRunError::Codec)? {
                EngineResult::Owned(owned) => owned,
                EngineResult::Located(located) => {
                    self.materialize_capture(EngineResult::Located(located), resources)?
                }
            };
            slots.push(Some(owned));
        }
        Ok(slots)
    }

    /// Turns one descent outcome into the machine's next transition: a leaf
    /// routes, a fan-out pushes its `Each` frame, a suppressed step skips, and a
    /// mismatch fails a value routing at `cont`.
    pub(crate) fn absorb_descent(
        &mut self,
        node: ProgramNodeId,
        steps: &'program [StageStep],
        at: usize,
        descended: Result<Descended<'source>, EngineRunError>,
        cont: usize,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineResult<'source>>, EngineRunError> {
        let descended = match descended {
            Ok(descended) => descended,
            Err(error) => {
                self.raise_cont = cont;
                return Err(error);
            }
        };
        match descended {
            Descended::Leaf(item) => {
                // The register's Law-2 write half: extend the register by the
                // stage's components, scoped by a [`GraphFrame::PathStep`].
                self.path_extend_navigation(steps, at, &item, cont, resources)?;
                self.route(item, cont, resources)
            }
            Descended::FanOut { next_at, container } => {
                // A `.[]` fan-out: the PREFIX steps navigate; each child's
                // component extends further via the Each frame.
                self.path_extend_prefix(steps, at, next_at, &container, resources)?;
                self.push_frame(GraphFrame::Each {
                    container,
                    cursor: 0,
                    stage: node,
                    next_at,
                    cont,
                });
                Ok(None)
            }
            // `..` over a container: the frame's children resume AT the descend
            // step (recursing), while the SELF emission continues past it in the
            // pending slot — which the machine drains before advancing any frame,
            // giving the pre-order self-first walk.
            Descended::Descend {
                next_at,
                container,
                value,
                at: continue_at,
            } => {
                // A `..` fan-out: the PREFIX steps (before the descend step)
                // navigate, extending the register to the walked value; each
                // child extends further via the Each frame's emissions, and
                // the self-emission continues with the register already
                // parked on it.
                if self.path.is_some() && !self.path_frozen() {
                    self.path_extend_prefix(steps, at, next_at, &container, resources)?;
                }
                self.push_frame(GraphFrame::Each {
                    container,
                    cursor: 0,
                    stage: node,
                    next_at,
                    cont,
                });
                self.pending = Some(GraphTask::Descend {
                    node,
                    value,
                    at: continue_at,
                    cont,
                });
                Ok(None)
            }
            Descended::Skip => Ok(None),
        }
    }
}
