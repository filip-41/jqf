use jqf_codec_core::{
    AccessFootprint, AccessFootprintKind, AccessGuarantees, AccessRequirement, AccessResultKind, CapabilityBundle,
    CodecDemand, DemandClause, DiagnosticPolicy, ExactPath,
};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) fn resources() -> ResourceContext<'static> {
    static CONTROL: ContinueControl = ContinueControl;
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, 1 << 24, u64::MAX, 256);
    ResourceContext::new(
        RequestAccount::try_new(limits).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4096).expect("meter"),
    )
    .expect("context")
}

pub(crate) fn demand(resources: &ResourceContext<'_>) -> CodecDemand {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("root");
    demand
}

pub(crate) fn requirement(resources: &ResourceContext<'_>) -> AccessRequirement {
    let mut path = ExactPath::try_new(resources);
    path.try_push_semantic_member("catalog", resources).expect("key");
    path.try_push_semantic_index(17, resources);
    path.try_push_semantic_member("price", resources).expect("key");
    let footprint = AccessFootprint::try_exact(path, resources);
    AccessRequirement::try_exact(footprint, demand(resources), guarantees(), resources)
        .expect("valid benchmark requirement")
}

pub(crate) fn bundle(resources: &ResourceContext<'_>) -> CapabilityBundle {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("root");
    CapabilityBundle::new(
        AccessFootprintKind::Whole,
        AccessResultKind::CompleteDocument,
        demand,
        guarantees(),
    )
}

const fn guarantees() -> AccessGuarantees {
    AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly)
}

pub(crate) fn provenance() -> String {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let head = command(&repository, "git", &["rev-parse", "HEAD"]);
    let rustc = command(&repository, "rustc", &["-Vv"]);
    let command_line = std::env::args().collect::<Vec<_>>().join(" ");
    format!(
        "case_revision=1 source_tree={:016x} git_head={} rustc={} target={}-{} allocation_stats={} command={}",
        source_hash(&repository),
        head.trim(),
        rustc.lines().next().unwrap_or("unknown"),
        std::env::consts::ARCH,
        std::env::consts::OS,
        cfg!(feature = "allocation-stats"),
        command_line,
    )
}

fn command(repository: &Path, program: &str, arguments: &[&str]) -> String {
    Command::new(program)
        .args(arguments)
        .current_dir(repository)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn source_hash(repository: &Path) -> u64 {
    let mut files = Vec::new();
    for relative in [
        "jqf-codec/core",
        "jqf-codec/json",
        "jqf-data/src",
        "jqf-resource/src",
        "tools/jqf-codec-core-bench",
    ] {
        collect_files(&repository.join(relative), &mut files);
    }
    files.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for file in files {
        file.strip_prefix(repository).unwrap_or(&file).hash(&mut hasher);
        if let Ok(contents) = std::fs::read(&file) {
            contents.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
    if path.is_file() {
        files.push(path.to_owned());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        collect_files(&entry.path(), files);
    }
}
