//! Representative successful-path benchmarks for `jqf-data`.

mod build_manifest;
mod checksum;
mod document_cases;
mod fixture;
mod value_cases;

#[cfg(all(test, feature = "allocation-stats"))]
#[global_allocator]
static TEST_ALLOCATOR: jqf_bench_core::allocation::MeasuringAllocator = jqf_bench_core::allocation::MeasuringAllocator;

#[cfg(all(test, feature = "allocation-stats"))]
static ALLOCATION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use jqf_bench_core::BenchmarkCase;

/// Validates one closed schema-4 legacy receipt against its lane and checksum.
///
/// # Errors
///
/// Returns an error for any missing, reordered, mistyped, inconsistent, or
/// role-mismatched field.
pub fn validate_legacy_schema4_receipt(
    detail: &str,
    preflight_checksum: u64,
    expected_lane: &str,
) -> Result<(), String> {
    document_cases::validate_legacy_schema4_receipt(detail, preflight_checksum, expected_lane)
}

/// Validates an exact schema-4 receipt for any required Cachegrind route.
///
/// # Errors
///
/// Returns an error when the lane is outside the closed required inventory or
/// its receipt differs from the freshly derived primary-route receipt.
pub fn validate_cache_schema4_receipt(
    detail: &str,
    preflight_checksum: u64,
    expected_lane: &str,
) -> Result<(), String> {
    document_cases::validate_cache_schema4_receipt(detail, preflight_checksum, expected_lane)
}

/// Returns the frozen schema-4 baseline attribution for a required cache lane.
///
/// # Errors
///
/// Returns an error for lanes outside [`REQUIRED_CACHE_LANES`].
pub fn cache_lane_profile(lane: &str) -> Result<&'static str, String> {
    document_cases::cache_lane_profile(lane)
}

/// Validates the exact data evidence role contract for governed lanes.
///
/// # Errors
///
/// Returns an error when a governed lane's primary/alias role, alias target,
/// or independent-evidence declaration differs from the closed catalog.
pub fn validate_data_evidence_role_receipt(lane: &str, detail: &str) -> Result<bool, String> {
    document_cases::validate_data_evidence_role_receipt(lane, detail)
}

/// Closed schema-4 relationship lanes that must exist before compact4 cutover.
pub const LEGACY_RELATIONSHIP_LANES: &[&str] = &[
    "legacy/topology/target-only-4097",
    "legacy/topology/unkeyed-full-8192",
    "legacy/topology/keyed-full-4096",
    "legacy/array/direct-traversal-4096",
    "legacy/array/owner-reuse-traversal-4096",
    "legacy/array/irregular-indexed-traversal-4096",
    "legacy/object/unique-lookup-iteration-2048",
    "legacy/object/duplicate-50-lookup-iteration-2048",
    "legacy/object/duplicate-90-lookup-iteration-2048",
    "legacy/materialize/root-array-direct-4096",
    "legacy/materialize/subtree-object-duplicate-50",
];

/// Strict-JSON primary routes required in the same schema-4 cache baseline.
pub const STRICT_JSON_PRIMARY_CACHE_LANES: &[&str] = &[
    "json/decode/full/nested",
    "json/decode/full/escape-heavy",
    "json/decode/full/wide-duplicate",
    "json/decode/full/deep",
];

/// Closed cache baseline inventory: 11 legacy relationship routes plus the
/// four strict-JSON primary decode routes.
pub const REQUIRED_CACHE_LANES: &[&str] = &[
    "legacy/topology/target-only-4097",
    "legacy/topology/unkeyed-full-8192",
    "legacy/topology/keyed-full-4096",
    "legacy/array/direct-traversal-4096",
    "legacy/array/owner-reuse-traversal-4096",
    "legacy/array/irregular-indexed-traversal-4096",
    "legacy/object/unique-lookup-iteration-2048",
    "legacy/object/duplicate-50-lookup-iteration-2048",
    "legacy/object/duplicate-90-lookup-iteration-2048",
    "legacy/materialize/root-array-direct-4096",
    "legacy/materialize/subtree-object-duplicate-50",
    "json/decode/full/nested",
    "json/decode/full/escape-heavy",
    "json/decode/full/wide-duplicate",
    "json/decode/full/deep",
];

/// Builds the complete deterministic benchmark inventory.
#[must_use]
pub fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    let mut cases = value_cases::cases();
    cases.extend(document_cases::cases());
    cases
}

/// Verifies that the exact source inputs embedded at build still match disk.
///
/// # Errors
///
/// Returns the first changed source path or hashing failure.
pub fn verify_build_source() -> Result<(), String> {
    build_manifest::verify()
}

/// Returns the aggregate source-manifest identity embedded in this worker.
#[must_use]
pub const fn build_source_identity() -> &'static str {
    build_manifest::identity()
}

/// Returns the ordered repository-relative path and Git blob manifest embedded
/// in this worker.
#[must_use]
pub const fn build_source_manifest() -> &'static [(&'static str, &'static str)] {
    build_manifest::entries()
}

/// Returns the canonical ordered source roots traversed by this worker.
#[must_use]
pub const fn build_source_roots() -> &'static [&'static str] {
    build_manifest::roots()
}

/// Returns portable modes paired by index with `build_source_manifest`.
#[must_use]
pub const fn build_source_modes() -> &'static [u32] {
    build_manifest::modes()
}

/// Returns the immutable source/compiler/code-generation identity of this worker.
#[must_use]
pub fn worker_build_identity() -> serde_json::Value {
    serde_json::json!({
        "package": "jqf-data-bench",
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

/// Adds the build identity to the common benchmark JSON envelope.
///
/// # Errors
///
/// Returns an error when the shared harness emitted invalid JSON.
pub fn bind_worker_json(rendered: &str) -> Result<String, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(rendered)?;
    value["build"] = worker_build_identity();
    serde_json::to_string(&value)
}
