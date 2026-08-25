//! Retained strict-JSON parse and static-forward benchmark worker.

mod build_manifest;
mod cases;
mod competitors;
mod fixtures;
mod semantic;

use jqf_bench_core::BenchmarkCase;

/// Returns the retained strict-JSON benchmark inventory.
#[must_use]
pub fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    cases::cases()
}

/// Verifies that every measured source file still matches the manifest
/// embedded when this benchmark binary was built.
///
/// # Errors
///
/// Returns the first drifted path or hashing failure.
pub fn verify_build_source() -> Result<(), String> {
    build_manifest::verify()
}

/// Returns the immutable aggregate identity embedded by the build script.
#[must_use]
pub const fn build_source_identity() -> &'static str {
    build_manifest::identity()
}

/// Returns the exact repository-relative path and Git blob hash manifest
/// embedded in the binary.
#[must_use]
pub const fn build_source_manifest() -> &'static [(&'static str, &'static str)] {
    build_manifest::entries()
}

/// Returns the canonical repository-relative roots traversed by the source
/// manifest. Their order is part of the source checkpoint contract.
#[must_use]
pub const fn build_source_roots() -> &'static [&'static str] {
    build_manifest::roots()
}

/// Returns the portable Unix-style mode paired by index with each source
/// manifest entry.
#[must_use]
pub const fn build_source_modes() -> &'static [u32] {
    build_manifest::modes()
}

/// Returns the compiler, target, profile, and code-generation identity embedded
/// when this worker was built.
#[must_use]
pub fn worker_build_identity() -> serde_json::Value {
    serde_json::json!({
        "package": "jqf-codec-json-bench",
        "allocation_stats": cfg!(feature = "allocation-stats"),
        "source_manifest": build_manifest::identity(),
        "compiler": build_manifest::compiler(),
        "compiler_hash": build_manifest::compiler_hash(),
        "git_head": build_manifest::git_head(),
        "target": build_manifest::target(),
        "profile": build_manifest::profile(),
        "rustflags": build_manifest::rustflags(),
        "encoded_rustflags": build_manifest::encoded_rustflags(),
        "configured_target": build_manifest::configured_target(),
        "release_codegen_units": build_manifest::release_codegen_units(),
    })
}

/// Adds the embedded build identity to a shared-harness JSON result.
///
/// # Errors
///
/// Returns an error when the shared harness emitted invalid JSON.
pub fn bind_worker_json(rendered: &str) -> Result<String, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(rendered)?;
    value["build"] = worker_build_identity();
    serde_json::to_string(&value)
}
