//! Compile-pipeline module fixes: callable rebase across includes, circular
//! import refusal, data-import bindings in filter-parameter defs, and
//! multi-entry `search` metadata.

use std::borrow::ToOwned as _;
use std::boxed::Box;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::vec::Vec;

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, ValidationMode};
use jqf_codec_json::RFC8259_DIALECT_ID;
use jqf_data::{DialectId, FormatId};
use jqf_engine::{
    CodecRequirementPolicy, CompileOptions, EngineCompileError, LoadedModule, ModuleLoader, ModuleLoaderHandle,
    try_compile_program,
};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{CodecCatalog, Diagnostics, EncodedItemReport, FacadeFraming, ItemSink, PipelinePolicy};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;
static CONTROL: ContinueControl = ContinueControl;

struct MapLoader {
    modules: BTreeMap<String, LoadedModule>,
}

impl MapLoader {
    fn new(modules: BTreeMap<String, LoadedModule>) -> Self {
        Self { modules }
    }
}

impl ModuleLoader for MapLoader {
    fn resolve(
        &self,
        relpath: &str,
        search: Option<&[String]>,
        _lib_origin: Option<&str>,
        is_data: bool,
    ) -> Option<LoadedModule> {
        let suffix = if is_data { ".json" } else { ".jq" };
        let key = format!("{relpath}{suffix}");
        if let Some(module) = self.modules.get(&key) {
            return Some(LoadedModule {
                text: module.text.clone(),
                label: module.label.clone(),
                dir: module.dir.clone(),
            });
        }
        let search = search?;
        for dir in search {
            let candidate = if dir.ends_with('/') {
                format!("{dir}{relpath}{suffix}")
            } else {
                format!("{dir}/{relpath}{suffix}")
            };
            if let Some(module) = self.modules.get(&candidate) {
                return Some(LoadedModule {
                    text: module.text.clone(),
                    label: module.label.clone(),
                    dir: module.dir.clone(),
                });
            }
        }
        None
    }
}

struct CollectingSink {
    bytes: Vec<u8>,
}

impl CollectingSink {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }
}

impl ItemSink for CollectingSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn resources_with_loader(loader: MapLoader) -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work"),
    )
    .expect("resources")
    .with_host_extension(Box::new(ModuleLoaderHandle::new(Box::new(loader))))
}

fn compile(program: &str, resources: &ResourceContext<'_>) -> Result<jqf_engine::CompiledProgram, EngineCompileError> {
    try_compile_program(
        program,
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        CompileOptions::new(),
        resources,
    )
}

fn run(program: &str, input: &str, loader: MapLoader) -> Result<String, String> {
    let mut resources = resources_with_loader(loader);
    let registration = jqf_codec_json::registration().expect("json registration");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id");
    let dialect = || DialectId::try_new(RFC8259_DIALECT_ID).expect("dialect id");
    let compiled = compile(program, &resources).map_err(|error| error.to_string())?;
    let requirement = compiled
        .try_requirement(&resources)
        .map_err(|error| error.to_string())?;
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "<test>",
        input.as_bytes(),
        0,
    );
    let mut sink = CollectingSink::new();
    let policy_options = PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &DialectId::try_new(RFC8259_DIALECT_ID).expect("dialect"),
            options: None,
            allow_adjacent_values: false,
            value_separator: &[],
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: jqf_codec_core::PreservationRequest::Report,
        encode_options: None,
        cooperative_credits: COOPERATIVE_CREDITS,
        split: None,
        max_iterations: None,
    };
    let diagnostics = Diagnostics::new(DiagnosticPolicy::ErrorsOnly);
    let request = jqf_sdk::Request::new(&compiled, jqf_sdk::Input::Whole(input.as_bytes()))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(policy_options)
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_diagnostics(diagnostics.as_ref())
        .with_requirement(&requirement);
    match jqf_sdk::execute(request, &mut sink) {
        Ok(_) => Ok(String::from_utf8(sink.bytes).expect("utf-8 output")),
        Err(error) => Err(match error.pipeline_failure() {
            Some(failure) => failure.to_string(),
            None => format!("{:?}", error.pipeline_failure()),
        }),
    }
}

fn module(label: &str, dir: &str, text: &str) -> LoadedModule {
    LoadedModule {
        label: label.to_owned(),
        dir: dir.to_owned(),
        text: text.to_owned(),
    }
}

#[test]
fn multi_include_callable_rebase_resolves_the_second_module_def() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "m1.jq".to_owned(),
        module("m1.jq", ".", "def dummy: [range(100)] | .[];"),
    );
    modules.insert("m2.jq".to_owned(), module("m2.jq", ".", "def f: 42; def h: f;"));
    let output = run("include \"m1\"; include \"m2\"; h", "null", MapLoader::new(modules))
        .expect("multi-include callable rebase must execute");
    assert_eq!(output.trim(), "42");
}

fn resources_without_loader() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work"),
    )
    .expect("resources")
}

#[test]
fn include_without_module_loader_fails_at_prepare() {
    let resources = resources_without_loader();
    let error = compile("include \"a\"; .", &resources).expect_err("missing loader must refuse");
    let message = error.to_string();
    assert!(
        message.contains("no module loader attached"),
        "expected prepare-time loader refusal, got {message}"
    );
}

#[test]
fn circular_import_is_a_compile_error_not_a_stack_overflow() {
    let mut modules = BTreeMap::new();
    modules.insert("a.jq".to_owned(), module("a.jq", ".", "include \"b\";"));
    modules.insert("b.jq".to_owned(), module("b.jq", ".", "include \"a\";"));
    let resources = resources_with_loader(MapLoader::new(modules));
    let error = compile("include \"a\"; .", &resources).expect_err("circular import must refuse");
    let message = error.to_string();
    assert!(
        message.contains("circular import"),
        "expected circular-import refusal, got {message}"
    );
    assert!(
        message.contains("a.jq"),
        "expected the cycle label in the refusal, got {message}"
    );
}

#[test]
fn filter_param_exported_def_sees_data_import_binding() {
    let mut modules = BTreeMap::new();
    modules.insert(
        "m.jq".to_owned(),
        module("m.jq", ".", "import \"data\" as $d; def f(g): $d | g;"),
    );
    modules.insert("data.json".to_owned(), module("data.json", ".", "{\"x\":1}"));
    let output = run("import \"m\" as m; m::f(.)", "null", MapLoader::new(modules))
        .expect("data-import filter-parameter def must execute");
    assert_eq!(output.trim(), "[{\"x\":1}]");
}

#[test]
fn multi_entry_search_metadata_compiles() {
    let mut modules = BTreeMap::new();
    modules.insert("./a/m.jq".to_owned(), module("m.jq", "./a", "def x: 1;"));
    let output = run(
        "import \"m\" as m {search: [\"./a\", \"./b\"]}; m::x",
        "null",
        MapLoader::new(modules),
    )
    .expect("multi-entry search metadata must compile");
    assert_eq!(output.trim(), "1");
}

#[test]
fn included_callable_collect_add_rewrites_inside_the_body() {
    let mut modules = BTreeMap::new();
    modules.insert("m.jq".to_owned(), module("m.jq", ".", "def g: [.[]] | add;"));
    let output =
        run("import \"m\" as m; m::g", "[1,2]", MapLoader::new(modules)).expect("module callable collect|add must run");
    assert_eq!(output.trim(), "3");
}

#[test]
fn imported_callable_rewrite_indexes_the_fused_not_the_pre_fuse_arena() {
    // Regression: the callable-body collect|add rewrite read PRE-fuse body ids
    // against the FUSED (compacted, renumbered) arena. Unused defs shrink the
    // fused arena below the stale pre-fuse id, and the compile panicked with
    // an out-of-bounds index. The rewrite must resolve body ids through the
    // fuse map — this shape compiled to a panic on the broken tree and must
    // compile and answer here.
    let mut text = String::new();
    for index in 0..80 {
        let _ = writeln!(text, "def d_{index}: .a | .b;");
    }
    text.push_str("def zz: .a | .b | [.[]] | add;\n");
    let mut modules = BTreeMap::new();
    modules.insert("m.jq".to_owned(), module("m.jq", ".", &text));
    let output = run(
        "import \"m\" as m; m::zz",
        "{\"a\":{\"b\":[1,2,3]}}",
        MapLoader::new(modules),
    )
    .expect("compaction-shaped module compile must not panic");
    assert_eq!(output.trim(), "6");
}
