//! The host-state and process-control family: `env`, the three origin/search builtins, `stderr`, and the two halt laws.
//!
//! One job: own the family/overload records AND the per-law evaluators the executor dispatches through.
//!
//! Every value law here reads HOST state injected at the request seam (the [`jqf_resource::EnvironmentSnapshot`] the
//! CLI attaches to its `ResourceContext`); the engine itself stays `no_std` and pure. Without a snapshot the honest
//! library answer is the empty form: `env` → `{}`, `get_prog_origin` → `null`, `get_jq_origin` → `"."`,
//! `get_search_list` → the literal default list.
//!
//! `stderr`, `halt`, and `halt_error` join this family because their subject is the process too: `stderr` writes the
//! input compact to stderr AND passes it through to stdout (the stderr callback returns the input); `halt` terminates
//! the run with exit 0 and no output; `halt_error(n)` requires a number argument, prints the input compact to stderr
//! and exits with the (C-truncated) code — `def halt_error: halt_error(5);` is the arity-0 spelling.

use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::{Array, ObjectBuilder, ObjectKey, Value};
use jqf_resource::{EnvironmentSnapshot, ResourceContext};

use super::id;
use crate::error::EngineRunError;
use crate::error::message;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};

/// The literal default module search list (`get_search_list`). The `$ORIGIN` spellings are literal text, never
/// expansions.
const DEFAULT_SEARCH_LIST: [&str; 3] = ["~/.jq", "$ORIGIN/../lib/jq", "$ORIGIN/../lib"];

/// The process-family law discriminants.
#[derive(Clone, Copy, Debug)]
pub enum ProcessLaw {
    /// `env/0` — the environment object, keys in host order.
    Env,
    /// `get_prog_origin/0` — the process working directory.
    GetProgOrigin,
    /// `get_jq_origin/0` — the literal `"."`.
    GetJqOrigin,
    /// `get_search_list/0` — the module search list.
    GetSearchList,
    /// `stderr/0` — write the input compact to stderr, then pass it through.
    Stderr,
    /// `halt/0` — terminate the run, exit 0, no output.
    Halt,
    /// `halt_error/0` — the reference's `def halt_error: halt_error(5);`.
    HaltErrorZero,
    /// `input/0` — the next input value, or the `break` error at the end.
    Input,
    /// `inputs/0` — every remaining input value.
    Inputs,
    /// `input_filename/0` — the current input's filename.
    InputFilename,
    /// `input_line_number/0` — the current input's line number.
    InputLineNumber,
    /// `modulemeta/0` — the metadata of the module named by the input.
    Modulemeta,
}

/// The evaluator payload the registry dispatches for every process overload.
#[derive(Clone, Copy, Debug)]
pub enum ProcessEvaluator {
    Unary(ProcessLaw),
    /// `halt_error/1` — evaluate its number argument, then halt with the input as the message.
    HaltErrorOne,
}

/// `env/0`: the environment object.
pub fn env(
    environment: Option<&EnvironmentSnapshot>,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let vars: &[(String, String)] = environment.as_ref().map_or(&[], |env| env.vars());
    let mut builder = ObjectBuilder::try_with_capacity(vars.len()).map_err(|_| EngineRunError::allocation_failure())?;
    for (name, value) in vars {
        let key = ObjectKey::try_from_str(name).map_err(|_| EngineRunError::allocation_failure())?;
        let value = Value::try_string(value).map_err(|_| EngineRunError::allocation_failure())?;
        builder
            .try_insert_last(key, value)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    builder
        .try_finish()
        .map(Value::Object)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// `get_prog_origin/0`: the process cwd, or `null` without a snapshot.
pub fn get_prog_origin(
    environment: Option<&EnvironmentSnapshot>,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    match environment.and_then(EnvironmentSnapshot::cwd) {
        Some(cwd) => Value::try_string(cwd).map_err(|_| EngineRunError::allocation_failure()),
        None => Ok(Value::Null),
    }
}

/// `get_jq_origin/0`: the running binary's directory (the reference answers its own executable's directory; jqf answers
/// the jqf executable's), or the literal `"."` without a snapshot.
pub fn get_jq_origin(
    environment: Option<&EnvironmentSnapshot>,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    match environment.and_then(EnvironmentSnapshot::jq_origin) {
        Some(origin) => Value::try_string(origin).map_err(|_| EngineRunError::allocation_failure()),
        None => Value::try_string(".").map_err(|_| EngineRunError::allocation_failure()),
    }
}

/// `get_search_list/0`: the snapshot's list, or the literal default.
pub fn get_search_list(
    environment: Option<&EnvironmentSnapshot>,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let entries: Vec<&str> = match environment {
        Some(snapshot) => snapshot
            .search_list()
            .iter()
            .map(alloc::string::String::as_str)
            .collect(),
        None => DEFAULT_SEARCH_LIST.to_vec(),
    };
    let mut array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    for entry in entries {
        let value = Value::try_string(entry).map_err(|_| EngineRunError::allocation_failure())?;
        array
            .try_push(value)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(array))
}

/// The `halt_error/1` refusal for a non-number argument: the check is on the ARGUMENT, but the message names the PIPED
/// INPUT:
/// `1 | halt_error("boom")` → `number (1) halt_error/1: number required`.
pub fn halt_error_number_required(value: &Value) -> Result<String, EngineRunError> {
    let operand = message::dump_trunc_owned(value)?;
    message::join(&[
        message::kind_name(value.kind()),
        " (",
        &operand,
        ") halt_error/1: number required",
    ])
}

/// The `halt_error/1` exit status for one numeric argument.
///
/// The law is the full exit-byte spelling: truncate toward zero, a negative result exits 0, and the value is masked to
/// the process byte. The saturating double→integer cast pins the two extreme answers — an out-of-range magnitude
/// collapses to the saturated integer's low byte (255) and a non-finite operand truncates to 0. The law lives beside
/// the refusal it shares a builtin with, so both engine drives read one table.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the halt status is the truncated low-byte law by pinned behavior"
)]
#[must_use]
pub fn halt_status(raw: f64) -> u32 {
    let code = raw.trunc() as i64;
    if code < 0 { 0 } else { (code & 0xFF) as u32 }
}

/// Raises one module-load failure as a catchable string error naming the module and the underlying diagnostic — the
/// same shape as the `module not found` refusal, so `try … catch` sees a sentence instead of an uncatchable
/// allocation failure.
fn invalid_module(name: &str, detail: &str, resources: &ResourceContext<'_>) -> EngineRunError {
    match message::join(&["invalid module ", name, ": ", detail]) {
        Ok(text) => crate::semantics::path::raise(&text, resources),
        Err(error) => error,
    }
}

/// One constant-evaluation failure as its user-facing sentence: the parser rejection's message, the named construct, or
/// the resource error's own rendering.
fn constant_error_text(error: &crate::constant::ConstantEvalError) -> &str {
    match error {
        crate::constant::ConstantEvalError::Parse(rejection) => rejection.message(),
        crate::constant::ConstantEvalError::Unsupported { construct, .. } => construct.describe(),
        crate::constant::ConstantEvalError::Resource(_) => "module metadata evaluation exceeded a resource limit",
    }
}

/// `modulemeta/0`: the `{<module metadata>, deps, defs}` object for the module file the input names, resolved through
/// the request's module loader and parsed as a library (the `load_module_meta` law).
#[allow(
    clippy::too_many_lines,
    reason = "one evaluator per module-meta shape: the metadata assembly is read as a single table"
)]
pub fn modulemeta(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::String(name) = input.untagged() else {
        let operand = message::dump_trunc_owned(input)?;
        let text = message::join(&[
            message::kind_name(input.kind()),
            " (",
            &operand,
            ") modulemeta input module name must be a string",
        ])?;
        return Err(crate::semantics::path::raise(&text, resources));
    };
    let Some(loader) = crate::host::module_loader(resources) else {
        let text = message::join(&["module not found: ", name.as_str()])?;
        return Err(crate::semantics::path::raise(&text, resources));
    };
    let Some(resolved) = loader.resolve(name.as_str(), None, None, false) else {
        let text = message::join(&["module not found: ", name.as_str()])?;
        return Err(crate::semantics::path::raise(&text, resources));
    };
    let source_ref = jqf_source::SourceRef::new(jqf_source::SourceId::new(0), jqf_source::SourceKind::Query);
    let parsed = jqf_syntax::parse_library(source_ref, &resolved.text)
        .map_err(|error| invalid_module(name.as_str(), &crate::constant::internal_message(&error), resources))?;
    // The first parser diagnostic is the one the compile boundary surfaces too (`ParseRejection::from_diagnostics`), so
    // modulemeta names the same sentence instead of a generic refusal.
    let syntax = parsed.into_valid_syntax().map_err(|diagnostics| {
        let detail = diagnostics
            .first()
            .map_or("parse failed", jqf_source::Diagnostic::message);
        invalid_module(name.as_str(), detail, resources)
    })?;
    let resolved = jqf_source::ResolvedSource::new(source_ref, &resolved.label, resolved.text.as_bytes(), 0);
    let bound = syntax
        .bind(resolved)
        .map_err(|error| invalid_module(name.as_str(), &crate::constant::internal_message(&error), resources))?;
    let unit = bound.root();
    let source = bound.source();
    let mut meta = jqf_data::ObjectBuilder::try_with_capacity(1).map_err(|_| EngineRunError::allocation_failure())?;
    let mut deps = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    let mut def_names: Vec<String> = Vec::new();
    for item in &unit.items {
        match item {
            jqf_syntax::SourceItem::Module(item) => {
                let metadata = crate::constant::evaluate_constant(&item.metadata, source)
                    .map_err(|error| invalid_module(name.as_str(), constant_error_text(&error), resources))?;
                if let Value::Object(object) = metadata.untagged() {
                    for entry in object {
                        meta.try_insert_last(
                            jqf_data::ObjectKey::try_from_str(entry.key())
                                .map_err(|_| EngineRunError::allocation_failure())?,
                            entry.value().clone(),
                        )
                        .map_err(|_| EngineRunError::allocation_failure())?;
                    }
                }
            }
            jqf_syntax::SourceItem::Import(item) => {
                let entry = dep_entry(
                    Some(&item.alias),
                    &item.path,
                    item.metadata.as_ref(),
                    source,
                    name.as_str(),
                    resources,
                )?;
                deps.try_push(entry).map_err(|_| EngineRunError::allocation_failure())?;
            }
            jqf_syntax::SourceItem::Include(item) => {
                let entry = dep_entry(
                    None,
                    &item.path,
                    item.metadata.as_ref(),
                    source,
                    name.as_str(),
                    resources,
                )?;
                deps.try_push(entry).map_err(|_| EngineRunError::allocation_failure())?;
            }
            jqf_syntax::SourceItem::Def(item) => {
                let def_name = source
                    .text()
                    .get(item.name.range())
                    .ok_or_else(EngineRunError::allocation_failure)?;
                def_names.push(alloc::format!("{def_name}/{}", item.params.len()));
            }
        }
    }
    def_names.sort();
    let mut exports = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    for def_name in def_names {
        let value = Value::try_string(&def_name).map_err(|_| EngineRunError::allocation_failure())?;
        exports
            .try_push(value)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    let deps_value = Value::Array(deps);
    let exports_value = Value::Array(exports);
    meta.try_insert_last(
        jqf_data::ObjectKey::try_from_str("deps").map_err(|_| EngineRunError::allocation_failure())?,
        deps_value,
    )
    .map_err(|_| EngineRunError::allocation_failure())?;
    meta.try_insert_last(
        jqf_data::ObjectKey::try_from_str("defs").map_err(|_| EngineRunError::allocation_failure())?,
        exports_value,
    )
    .map_err(|_| EngineRunError::allocation_failure())?;
    meta.try_finish()
        .map(Value::Object)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// One `deps` entry: `{as, is_data, relpath, search?}` with the authored `search` preserved (the `block_take_imports`
/// shape).
fn dep_entry(
    alias: Option<&jqf_source::Span>,
    path: &jqf_syntax::StringTemplate,
    metadata: Option<&jqf_syntax::Expr>,
    source: &jqf_syntax::SyntaxSource<'_>,
    module_name: &str,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut builder =
        jqf_data::ObjectBuilder::try_with_capacity(4).map_err(|_| EngineRunError::allocation_failure())?;
    let (is_data, as_text) = match alias {
        Some(alias) => {
            let alias_text = source
                .text()
                .get(alias.range())
                .ok_or_else(EngineRunError::allocation_failure)?;
            (
                alias_text.starts_with('$'),
                alias_text.strip_prefix('$').unwrap_or(alias_text),
            )
        }
        None => (false, ""),
    };
    let relpath = crate::constant::static_template_text(path, source)
        .map_err(|_| EngineRunError::allocation_failure())?
        .ok_or_else(|| {
            // A NON-static import path (an interpolated template) has no relpath to report, and silence would publish
            // `relpath: ""` — a module that does not exist. The same catchable refusal as every other
            // malformed-module shape above.
            invalid_module(module_name, "import path must be a literal string", resources)
        })?;
    if let Some(metadata) = metadata {
        let value = crate::constant::evaluate_constant(metadata, source)
            .map_err(|error| invalid_module(module_name, constant_error_text(&error), resources))?;
        if let Value::Object(object) = value.untagged()
            && let Some(search) = object.get("search")
        {
            builder
                .try_insert_last(
                    jqf_data::ObjectKey::try_from_str("search").map_err(|_| EngineRunError::allocation_failure())?,
                    search.clone(),
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
    }
    if !as_text.is_empty() {
        builder
            .try_insert_last(
                jqf_data::ObjectKey::try_from_str("as").map_err(|_| EngineRunError::allocation_failure())?,
                Value::try_string(as_text).map_err(|_| EngineRunError::allocation_failure())?,
            )
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    builder
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str("is_data").map_err(|_| EngineRunError::allocation_failure())?,
            Value::Bool(is_data),
        )
        .map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str("relpath").map_err(|_| EngineRunError::allocation_failure())?,
            Value::try_string(&relpath).map_err(|_| EngineRunError::allocation_failure())?,
        )
        .map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_finish()
        .map(Value::Object)
        .map_err(|_| EngineRunError::allocation_failure())
}

// ------------------------------------------------------------------------
// Registry records.

macro_rules! family {
    ($id:expr, $canonical_name:literal, $category:literal, $summary:literal, $detail:literal) => {
        BuiltinFamilyRecord {
            id: BuiltinFamilyId::new($id),
            canonical_name: $canonical_name,
            category: $category,
            summary: $summary,
            detail: $detail,
        }
    };
}

const ENV_FAMILY: BuiltinFamilyRecord = family!(
    id::ENV_FAMILY_ID,
    "env",
    "process",
    "The environment object.",
    "Publishes one object whose keys are the host environment variables, in host order, \
     ignoring the piped input."
);
const GET_PROG_ORIGIN_FAMILY: BuiltinFamilyRecord = family!(
    id::GET_PROG_ORIGIN_FAMILY_ID,
    "get_prog_origin",
    "process",
    "The process working directory.",
    "Publishes the process cwd as a string, ignoring the piped input; `null` when the \
     host supplies no snapshot."
);
const GET_JQ_ORIGIN_FAMILY: BuiltinFamilyRecord = family!(
    id::GET_JQ_ORIGIN_FAMILY_ID,
    "get_jq_origin",
    "process",
    "The jq origin directory.",
    "Publishes the jqf binary's directory (the host's injected origin, used for \
     module search), ignoring the piped input; the literal `\".\"` only when the host \
     supplies no snapshot."
);
const GET_SEARCH_LIST_FAMILY: BuiltinFamilyRecord = family!(
    id::GET_SEARCH_LIST_FAMILY_ID,
    "get_search_list",
    "process",
    "The module search list.",
    "Publishes the module search path as an array of strings, ignoring the piped input."
);
const STDERR_FAMILY: BuiltinFamilyRecord = family!(
    id::STDERR_FAMILY_ID,
    "stderr",
    "process",
    "The stderr writer.",
    "Writes the input value compact to stderr and passes it through to stdout \
     unchanged."
);
const HALT_FAMILY: BuiltinFamilyRecord = family!(
    id::HALT_FAMILY_ID,
    "halt",
    "process",
    "The process terminator.",
    "`halt` exits 0 with no output; `halt_error` prints the input compact to \
     stderr and exits with its (truncated) number argument, defaulting to 5."
);
const INPUT_FAMILY: BuiltinFamilyRecord = family!(
    id::INPUT_FAMILY_ID,
    "input",
    "process",
    "The next input value.",
    "Pulls and emits the next input value from the shared input sequence, \
     raising the `break` error at the end."
);
const INPUTS_FAMILY: BuiltinFamilyRecord = family!(
    id::INPUTS_FAMILY_ID,
    "inputs",
    "process",
    "Every remaining input value.",
    "Emits every remaining input value from the shared input sequence, ending \
     silently at the end."
);
const INPUT_FILENAME_FAMILY: BuiltinFamilyRecord = family!(
    id::INPUT_FILENAME_FAMILY_ID,
    "input_filename",
    "process",
    "The current input's filename.",
    "Publishes the filename of the input the program is currently processing, \
     or `null` without an input sequence."
);
const INPUT_LINE_NUMBER_FAMILY: BuiltinFamilyRecord = family!(
    id::INPUT_LINE_NUMBER_FAMILY_ID,
    "input_line_number",
    "process",
    "The current input's line number.",
    "Publishes the line number of the input the program is currently \
     processing, or `0` without an input sequence."
);
const MODULEMETA_FAMILY: BuiltinFamilyRecord = family!(
    id::MODULEMETA_FAMILY_ID,
    "modulemeta",
    "process",
    "The metadata of the module named by the input.",
    "Publishes `{<module metadata>, deps, defs}` for the module file the input \
     names, resolved through the request's module loader."
);

pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    ENV_FAMILY,
    GET_PROG_ORIGIN_FAMILY,
    GET_JQ_ORIGIN_FAMILY,
    GET_SEARCH_LIST_FAMILY,
    STDERR_FAMILY,
    HALT_FAMILY,
    INPUT_FAMILY,
    INPUTS_FAMILY,
    INPUT_FILENAME_FAMILY,
    INPUT_LINE_NUMBER_FAMILY,
    MODULEMETA_FAMILY,
];

const ENV_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::ENV),
    family: BuiltinFamilyId::new(id::ENV_FAMILY_ID),
    canonical_name: "env",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        program: "env | has(\"PATH\") or (env | length) >= 0",
        input: "null",
        expected: "true\n",
    }],
};

const GET_PROG_ORIGIN_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::GET_PROG_ORIGIN),
    family: BuiltinFamilyId::new(id::GET_PROG_ORIGIN_FAMILY_ID),
    canonical_name: "get_prog_origin",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        // The CLI supplies a cwd (string); a library request answers `null`.
        // The `// ""` fallback makes the example hold either way.
        program: "(get_prog_origin // \"\") | type",
        input: "null",
        expected: "\"string\"\n",
    }],
};

const GET_JQ_ORIGIN_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::GET_JQ_ORIGIN),
    family: BuiltinFamilyId::new(id::GET_JQ_ORIGIN_FAMILY_ID),
    canonical_name: "get_jq_origin",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    // The origin comes from the request-injected environment snapshot, like `env`/`get_prog_origin`/`get_search_list`
    // — its answer depends on the host, not on the input, so the whole family is Impure.
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        // The host's injected origin is the jqf binary's directory, which the examples harness does not supply — so
        // this pin is the snapshot-less fallback `"."`, the same value an ordinary `jq` invocation answers.
        program: "get_jq_origin",
        input: "null",
        expected: "\".\"\n",
    }],
};

const GET_SEARCH_LIST_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::GET_SEARCH_LIST),
    family: BuiltinFamilyId::new(id::GET_SEARCH_LIST_FAMILY_ID),
    canonical_name: "get_search_list",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        program: "get_search_list | type",
        input: "null",
        expected: "\"array\"\n",
    }],
};

const STDERR_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::STDERR),
    family: BuiltinFamilyId::new(id::STDERR_FAMILY_ID),
    canonical_name: "stderr",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        program: "stderr",
        input: "1",
        expected: "1\n",
    }],
};

const HALT_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::HALT),
    family: BuiltinFamilyId::new(id::HALT_FAMILY_ID),
    canonical_name: "halt",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        program: "halt",
        input: "1",
        expected: "",
    }],
};

const HALT_ERROR_0_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::HALT_ERROR_0),
    family: BuiltinFamilyId::new(id::HALT_FAMILY_ID),
    canonical_name: "halt_error",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        program: "halt_error",
        input: "1",
        expected: "",
    }],
};

const ONE_FILTER: &[ParameterKind] = &[ParameterKind::Filter];

const HALT_ERROR_1_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::HALT_ERROR_1),
    family: BuiltinFamilyId::new(id::HALT_FAMILY_ID),
    canonical_name: "halt_error",
    arity: 1,
    parameters: ONE_FILTER,
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    // The second example pins the CARDINALITY half of the argument law: the status argument iterates like any other,
    // and the FIRST output halts the run, so nothing after it is ever evaluated. Both publish no bytes — the exit
    // STATUS is the whole observable, which is why the examples-as-tests lane records it alongside the published bytes.
    examples: &[
        BuiltinExample {
            program: "halt_error(3)",
            input: "1",
            expected: "",
        },
        BuiltinExample {
            program: "halt_error(1,2)",
            input: "1",
            expected: "",
        },
    ],
};

const INPUT_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::INPUT),
    family: BuiltinFamilyId::new(id::INPUT_FAMILY_ID),
    canonical_name: "input",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        // Without an input sequence, `input` raises the `break` error; the example pins the catch-eligible value.
        program: "try input catch .",
        input: "null",
        expected: "\"break\"\n",
    }],
};

const INPUTS_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::INPUTS),
    family: BuiltinFamilyId::new(id::INPUTS_FAMILY_ID),
    canonical_name: "inputs",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        program: "[inputs]",
        input: "null",
        expected: "[]\n",
    }],
};

const INPUT_FILENAME_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::INPUT_FILENAME),
    family: BuiltinFamilyId::new(id::INPUT_FILENAME_FAMILY_ID),
    canonical_name: "input_filename",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        program: "input_filename",
        input: "null",
        expected: "null\n",
    }],
};

const INPUT_LINE_NUMBER_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::INPUT_LINE_NUMBER),
    family: BuiltinFamilyId::new(id::INPUT_LINE_NUMBER_FAMILY_ID),
    canonical_name: "input_line_number",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        program: "input_line_number",
        input: "null",
        expected: "0\n",
    }],
};

const MODULEMETA_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::MODULEMETA),
    family: BuiltinFamilyId::new(id::MODULEMETA_FAMILY_ID),
    canonical_name: "modulemeta",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Impure,
    examples: &[BuiltinExample {
        // Without a module loader the call answers the module-not-found error naming the module; the example pins that
        // the arity resolves and raises cleanly.
        program: "try modulemeta catch .",
        input: "\"nope\"",
        expected: "\"module not found: nope\"\n",
    }],
};

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    ENV_OVERLOAD,
    GET_PROG_ORIGIN_OVERLOAD,
    GET_JQ_ORIGIN_OVERLOAD,
    GET_SEARCH_LIST_OVERLOAD,
    STDERR_OVERLOAD,
    HALT_OVERLOAD,
    HALT_ERROR_0_OVERLOAD,
    HALT_ERROR_1_OVERLOAD,
    INPUT_OVERLOAD,
    INPUTS_OVERLOAD,
    INPUT_FILENAME_OVERLOAD,
    INPUT_LINE_NUMBER_OVERLOAD,
    MODULEMETA_OVERLOAD,
];

/// The host-state/process execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, ProcessEvaluator)] = &[
    (id::ENV, ProcessEvaluator::Unary(ProcessLaw::Env)),
    (id::GET_PROG_ORIGIN, ProcessEvaluator::Unary(ProcessLaw::GetProgOrigin)),
    (id::GET_JQ_ORIGIN, ProcessEvaluator::Unary(ProcessLaw::GetJqOrigin)),
    (id::GET_SEARCH_LIST, ProcessEvaluator::Unary(ProcessLaw::GetSearchList)),
    (id::STDERR, ProcessEvaluator::Unary(ProcessLaw::Stderr)),
    (id::HALT, ProcessEvaluator::Unary(ProcessLaw::Halt)),
    (id::HALT_ERROR_0, ProcessEvaluator::Unary(ProcessLaw::HaltErrorZero)),
    (id::HALT_ERROR_1, ProcessEvaluator::HaltErrorOne),
    (id::INPUT, ProcessEvaluator::Unary(ProcessLaw::Input)),
    (id::INPUTS, ProcessEvaluator::Unary(ProcessLaw::Inputs)),
    (id::INPUT_FILENAME, ProcessEvaluator::Unary(ProcessLaw::InputFilename)),
    (
        id::INPUT_LINE_NUMBER,
        ProcessEvaluator::Unary(ProcessLaw::InputLineNumber),
    ),
    (id::MODULEMETA, ProcessEvaluator::Unary(ProcessLaw::Modulemeta)),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{LoadedModule, ModuleLoader, ModuleLoaderHandle};
    use alloc::borrow::ToOwned as _;
    use alloc::boxed::Box;
    use alloc::format;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    /// Serves the oracle module text; anything else is not found.
    struct OracleLoader;

    impl ModuleLoader for OracleLoader {
        fn resolve(
            &self,
            relpath: &str,
            _search: Option<&[String]>,
            _lib_origin: Option<&str>,
            _is_data: bool,
        ) -> Option<LoadedModule> {
            match relpath {
                "oracle" => Some(LoadedModule {
                    text: concat!(
                        "module {\"origin\": \"test\"};\n",
                        "import \"util/base\" as $b {search: [\"./vendor\"]};\n",
                        "include \"extra\";\n",
                        "def f: 1;\n",
                        "def g(x): x + f;\n",
                    )
                    .to_owned(),
                    label: "oracle.jq".to_owned(),
                    dir: ".".to_owned(),
                }),
                _ => None,
            }
        }
    }

    fn resources_with_loader() -> ResourceContext<'static> {
        let loader = ModuleLoaderHandle::new(Box::new(OracleLoader));
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
        .with_host_extension(Box::new(loader))
    }

    /// The modulemeta output-shape ORACLE (no other lane pins this shape):
    /// the authored module metadata leads, `deps` and `defs` follow in that order; a dep entry carries `search`/`as`
    /// only when authored, then `is_data`, then `relpath`; an include has neither; def names are sorted with their
    /// arities.
    #[test]
    fn modulemeta_output_shape_is_pinned() {
        let resources = resources_with_loader();
        let name = Value::try_string("oracle").expect("name");
        let value = modulemeta(&name, &resources).expect("modulemeta");
        let json = crate::semantics::render::to_json(&value).expect("render");
        assert_eq!(
            json,
            concat!(
                "{\"origin\":\"test\",",
                "\"deps\":[",
                "{\"search\":[\"./vendor\"],\"as\":\"b\",\"is_data\":true,\"relpath\":\"util/base\"},",
                "{\"is_data\":false,\"relpath\":\"extra\"}",
                "],",
                "\"defs\":[\"f/0\",\"g/1\"]}"
            )
        );
    }

    /// A module whose import path is not a static string refuses as a catchable invalid-module error — never a silent
    /// `relpath: ""`.
    #[test]
    fn modulemeta_refuses_a_non_static_import_path() {
        struct DynamicPathLoader;

        impl ModuleLoader for DynamicPathLoader {
            fn resolve(
                &self,
                relpath: &str,
                _search: Option<&[String]>,
                _lib_origin: Option<&str>,
                _is_data: bool,
            ) -> Option<LoadedModule> {
                match relpath {
                    "dynamic" => Some(LoadedModule {
                        // The import path is a STRING token, but an interpolated one: the parser accepts it, and the
                        // static-text extraction cannot — exactly the shape that used to publish `relpath: ""`.
                        text: "import \"a\\(1)b\" as m;\n".to_owned(),
                        label: "dynamic.jq".to_owned(),
                        dir: ".".to_owned(),
                    }),
                    _ => None,
                }
            }
        }
        let loader = ModuleLoaderHandle::new(Box::new(DynamicPathLoader));
        let resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
        .with_host_extension(Box::new(loader));
        let name = Value::try_string("dynamic").expect("name");
        let result = modulemeta(&name, &resources);
        let rendered = format!("{:?}", result.expect_err("expected a refusal"));
        assert!(
            rendered.contains("invalid module"),
            "the refusal must be the catchable invalid-module sentence, got {rendered}"
        );
    }
}
