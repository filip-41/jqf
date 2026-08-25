use std::collections::{BTreeMap, BTreeSet};

use jqf_bench_core::BenchmarkCase as _;

#[derive(Default)]
struct Catalog {
    fixtures: Vec<BTreeMap<String, String>>,
    lanes: Vec<BTreeMap<String, String>>,
    families: Vec<(BTreeMap<String, String>, Vec<String>)>,
    data_lanes: Vec<BTreeMap<String, String>>,
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one catalog audit cross-checks executable inventory, semantics, and evidence roles"
)]
fn catalog_exactly_matches_executable_json_and_data_inventories() {
    let catalog = parse_catalog(include_str!("../lanes.toml"));
    let fixtures: BTreeSet<_> = catalog
        .fixtures
        .iter()
        .map(|fixture| {
            let status = required(fixture, "status");
            match status {
                "Executable" => {
                    required(fixture, "bytes").parse::<usize>().expect("fixture bytes");
                    assert_eq!(required(fixture, "hash").len(), 16);
                }
                "NotYetImplemented" => assert!(!required(fixture, "generator_law").is_empty()),
                _ => panic!("invalid fixture status {status}"),
            }
            required(fixture, "id").to_owned()
        })
        .collect();
    assert_eq!(fixtures.len(), catalog.fixtures.len(), "duplicate fixture ID");

    let mut catalog_ids = BTreeSet::new();
    let mut executable = BTreeMap::new();
    for lane in &catalog.lanes {
        validate_lane(lane, &fixtures, &mut catalog_ids);
        if required(lane, "status") == "Executable" {
            executable.insert(required(lane, "id").to_owned(), lane);
        }
    }
    for (family, members) in &catalog.families {
        assert_eq!(required(family, "status"), "NotYetImplemented");
        assert!(fixtures.contains(required(family, "fixture")));
        assert!(!required(family, "expected_receipt").is_empty());
        validate_campaign_metadata(family);
        if required(family, "fairness") == "strict-validation-scoped-result" {
            assert!(!required(family, "query_law").is_empty());
            assert!(required(family, "density_law").contains("N=ceil"));
        }
        assert!(!members.is_empty());
        for member in members {
            assert!(
                !member.contains(['{', '}']),
                "prospective member is not an exact frozen name: {member}"
            );
            assert!(catalog_ids.insert(member.clone()), "duplicate lane {member}");
        }
    }

    let mut cases = jqf_codec_json_bench::cases();
    let executable_cases: BTreeSet<_> = cases.iter().map(|case| case.metadata().name.to_owned()).collect();
    let executable_catalog: BTreeSet<_> = executable.keys().cloned().collect();
    assert_eq!(executable_catalog, executable_cases);
    let mut semantic_by_fixture = BTreeMap::<String, BTreeSet<String>>::new();
    for case in &mut cases {
        let metadata = case.metadata();
        let lane = executable[metadata.name];
        let receipt = case.preflight().expect("cataloged executable preflight");
        let (fixture, semantic_checksum) = validate_executable_receipt(metadata.name, lane, &receipt);
        semantic_by_fixture
            .entry(fixture)
            .or_default()
            .insert(semantic_checksum);
    }
    for (fixture, checksums) in semantic_by_fixture {
        assert_eq!(
            checksums.len(),
            1,
            "jqf and competitor lanes disagree on {fixture}: {checksums:?}",
        );
    }

    let data_catalog: BTreeSet<_> = catalog
        .data_lanes
        .iter()
        .map(|lane| {
            assert_eq!(required(lane, "status"), "Executable");
            assert!(lane.contains_key("owning_batch"));
            validate_campaign_metadata(lane);
            required(lane, "id").to_owned()
        })
        .collect();
    assert_eq!(data_catalog.len(), catalog.data_lanes.len());
    let data_cases: BTreeSet<_> = jqf_data_bench::cases()
        .iter()
        .map(|case| case.metadata().name.to_owned())
        .collect();
    assert_eq!(data_catalog, data_cases);

    let data_by_id: BTreeMap<_, _> = catalog
        .data_lanes
        .iter()
        .map(|lane| (required(lane, "id"), lane))
        .collect();
    for mut case in jqf_data_bench::cases() {
        let name = case.metadata().name;
        if !(name.starts_with("json/decode/") || name.starts_with("legacy/")) {
            continue;
        }
        let receipt = case.preflight().expect("role-bound data receipt");
        let lane = data_by_id[name];
        let expected_alias = if name.starts_with("json/decode/rich/") {
            assert_eq!(required(lane, "role"), "secondary");
            name.replacen("json/decode/rich/", "json/decode/full/", 1)
        } else {
            assert_eq!(required(lane, "role"), "primary");
            "none".to_owned()
        };
        let expected_role = if expected_alias == "none" { "primary" } else { "alias" };
        let expected_independent = if expected_alias == "none" { "true" } else { "false" };
        let exact_field = |key: &str| {
            receipt
                .detail
                .split_ascii_whitespace()
                .find(|token| token.starts_with(key))
                .and_then(|token| token.strip_prefix(key))
        };
        assert_eq!(exact_field("evidence_role="), Some(expected_role), "{name}");
        assert_eq!(
            exact_field("independent_evidence="),
            Some(expected_independent),
            "{name}"
        );
        assert_eq!(exact_field("alias_of="), Some(expected_alias.as_str()), "{name}");
    }
}

fn validate_executable_receipt(
    name: &str,
    lane: &BTreeMap<String, String>,
    receipt: &jqf_bench_core::PreflightReceipt,
) -> (String, String) {
    let fixture = required(lane, "fixture");
    let fairness = required(lane, "fairness");
    for token in [
        format!("fixture_id={fixture}"),
        "lane_status=Executable".to_owned(),
        format!("fairness={fairness}"),
        "semantic_checksum_schema=json-semantic-fnv1a64-v2".to_owned(),
        "semantic_checksum=0x".to_owned(),
        format!("physical_checksum=0x{:016x}", receipt.checksum),
    ] {
        assert!(
            receipt.detail.contains(&token),
            "{name} lacks receipt token {token:?}: {}",
            receipt.detail,
        );
    }
    for token in required(lane, "expected_receipt").split(';') {
        assert!(
            receipt.detail.contains(token),
            "{name} lacks expected receipt token {token:?}: {}",
            receipt.detail
        );
    }
    let semantic_checksum = receipt
        .detail
        .split_ascii_whitespace()
        .find_map(|token| token.strip_prefix("semantic_checksum="))
        .expect("checked semantic checksum")
        .to_owned();
    (fixture.to_owned(), semantic_checksum)
}

#[test]
fn source_manifest_covers_the_jqf_source_contract_readme() {
    assert!(
        jqf_codec_json_bench::build_source_manifest()
            .iter()
            .any(|(path, _)| *path == "jqf-source/README.md"),
        "jqf-source/README.md changes must invalidate benchmark evidence binaries",
    );
}

#[test]
fn published_source_contract_is_ordered_reconstructable_and_complete() {
    let entries = jqf_codec_json_bench::build_source_manifest();
    let modes = jqf_codec_json_bench::build_source_modes();
    let roots = jqf_codec_json_bench::build_source_roots();
    assert_eq!(entries, jqf_data_bench::build_source_manifest());
    assert_eq!(modes, jqf_data_bench::build_source_modes());
    assert_eq!(roots, jqf_data_bench::build_source_roots());
    assert_eq!(
        jqf_codec_json_bench::build_source_identity(),
        jqf_data_bench::build_source_identity()
    );
    assert_eq!(entries.len(), modes.len());
    assert!(!entries.is_empty());
    assert!(!roots.is_empty());
    assert!(entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
    assert!(roots.iter().all(|root| {
        !root.is_empty()
            && !root.starts_with('/')
            && !root
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
    }));
    assert!(entries.iter().all(|(path, blob)| {
        blob.len() == 40
            && blob.bytes().all(|byte| byte.is_ascii_hexdigit())
            && roots
                .iter()
                .any(|root| path == root || path.strip_prefix(root).is_some_and(|suffix| suffix.starts_with('/')))
    }));
    assert!(modes.iter().all(|mode| mode & !0o7777 == 0));
}

#[test]
fn catalog_retains_every_required_prospective_contract() {
    let catalog = parse_catalog(include_str!("../lanes.toml"));
    let mut names: BTreeSet<_> = catalog
        .lanes
        .iter()
        .filter(|lane| required(lane, "status") == "NotYetImplemented")
        .map(|lane| required(lane, "id").to_owned())
        .collect();
    for (_, members) in catalog.families {
        names.extend(members);
    }
    for required_name in [
        "json/capability/scan-validate-nested",
        "json/capability/semantic-nodes-nested",
        "json/capability/semantic-edges-containers-nested",
        "json/capability/physical-source-backing-nested",
        "json/capability/topology-nested",
        "json/capability/tags-facts-attributes-nested",
        "json/capability/provenance-source-ranges-nested",
        "json/capability/successful-diagnostics-nested",
        "json/capability/complete-rich-product-nested",
        "json/scoped-exact-area-1-percent",
        "json/scoped-exact-area-100-percent",
        "json/path-set-1-percent",
        "json/path-set-100-percent",
        "json/repeat-horizontal-stripe-1-percent",
        "json/repeat-horizontal-stripe-100-percent",
        "json/recursive-union-1-percent",
        "json/recursive-union-100-percent",
        "json/generic-full-decode-then-select",
        "json/adaptive-widening",
        "json/index-cold-build",
        "json/index-warm-reuse",
        "json/cache-invalidation",
        "json/source-request-full-retain",
        "json/source-persistent-selected-chunk-copy",
        "json/encode-located-complete",
        "json/encode-located-scoped",
        "e2e/decode-phase-nested",
        "e2e/engine-phase-nested",
        "e2e/encode-phase-nested",
        "compare/selective-api/exact-area-nested",
        "instrument/live-returned-product-retained-capacity",
        "instrument/external-source-pinned-bytes",
        "instrument/resource-retained-bytes",
        "instrument/peak-working-bytes",
        "data/future/semantic-edge-sidecar-layout",
        "data/future/scoped-publication",
        "data/future/diagnostic-attachment",
        "data/future/capacity-retention-after-publication",
        "json/encode-escape-heavy",
        "compare/serde-json/encode-escape-heavy",
        "compare/simd-json/encode-escape-heavy",
        "compare/sonic-rs/encode-escape-heavy",
        "json/encode-wide-4096",
        "compare/serde-json/encode-wide-4096",
        "compare/simd-json/encode-wide-4096",
        "compare/sonic-rs/encode-wide-4096",
        "json/encode-numeric-mixed",
        "compare/serde-json/encode-numeric-mixed",
        "compare/simd-json/encode-numeric-mixed",
        "compare/sonic-rs/encode-numeric-mixed",
        "json/encode-tagged-mixed",
        "compare/serde-json/encode-tagged-mixed",
        "compare/simd-json/encode-tagged-mixed",
        "compare/sonic-rs/encode-tagged-mixed",
        "json/encode-source-backed",
        "compare/serde-json/encode-source-backed",
        "compare/simd-json/encode-source-backed",
        "compare/sonic-rs/encode-source-backed",
    ] {
        assert!(names.contains(required_name), "missing {required_name}");
    }
}

fn validate_lane(lane: &BTreeMap<String, String>, fixtures: &BTreeSet<String>, ids: &mut BTreeSet<String>) {
    let id = required(lane, "id");
    assert!(
        !id.contains(['{', '}']) && !id.contains("same-as") && !id.contains("future"),
        "lane is not an exact frozen name: {id}"
    );
    assert!(ids.insert(id.to_owned()), "duplicate lane {id}");
    assert!(fixtures.contains(required(lane, "fixture")));
    assert!(matches!(required(lane, "status"), "Executable" | "NotYetImplemented"));
    assert!(!required(lane, "fairness").is_empty());
    assert!(!required(lane, "expected_receipt").is_empty());
    validate_campaign_metadata(lane);
    if required(lane, "fairness") == "strict-validation-scoped-result" {
        assert!(!required(lane, "query_law").is_empty());
        assert!(
            required(lane, "density_percent")
                .parse::<u8>()
                .is_ok_and(|value| matches!(value, 1 | 5 | 10 | 25 | 50 | 75 | 90 | 100))
        );
        assert!(required(lane, "selected_item_law").contains("N=ceil"));
    }
}

fn validate_campaign_metadata(table: &BTreeMap<String, String>) {
    assert!(matches!(required(table, "role"), "primary" | "secondary"));
    required(table, "owning_batch")
        .parse::<u8>()
        .expect("numeric owning_batch");
    assert!(!required(table, "affected_batches").is_empty());
    assert!(matches!(
        required(table, "workload_class"),
        "scan-dominant" | "materialization-dominant" | "mixed-scan-materialization" | "data-structure-operation"
    ));
}

fn required<'a>(table: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    table.get(key).unwrap_or_else(|| panic!("missing {key} in {table:?}"))
}

fn parse_catalog(source: &str) -> Catalog {
    enum Section {
        Root,
        Fixture(usize),
        Lane(usize),
        Family(usize),
        DataLane(usize),
    }

    let mut catalog = Catalog::default();
    let mut section = Section::Root;
    let mut collecting_members = false;
    for raw_line in source.lines() {
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        match line {
            "[[fixture]]" => {
                catalog.fixtures.push(BTreeMap::new());
                section = Section::Fixture(catalog.fixtures.len() - 1);
                collecting_members = false;
                continue;
            }
            "[[lane]]" => {
                catalog.lanes.push(BTreeMap::new());
                section = Section::Lane(catalog.lanes.len() - 1);
                collecting_members = false;
                continue;
            }
            "[[prospective_family]]" => {
                catalog.families.push((BTreeMap::new(), Vec::new()));
                section = Section::Family(catalog.families.len() - 1);
                collecting_members = false;
                continue;
            }
            "[[data_lane]]" => {
                catalog.data_lanes.push(BTreeMap::new());
                section = Section::DataLane(catalog.data_lanes.len() - 1);
                collecting_members = false;
                continue;
            }
            _ => {}
        }
        if collecting_members {
            if line == "]" {
                collecting_members = false;
            } else if let Section::Family(index) = section {
                catalog.families[index].1.push(parse_string(line.trim_end_matches(',')));
            } else {
                panic!("members outside family");
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            panic!("unsupported catalog line: {line}");
        };
        let key = key.trim();
        let value = value.trim();
        if key == "members" {
            assert_eq!(value, "[");
            assert!(matches!(section, Section::Family(_)));
            collecting_members = true;
            continue;
        }
        let value = if value.starts_with('"') {
            parse_string(value)
        } else {
            value.to_owned()
        };
        match section {
            Section::Root => {}
            Section::Fixture(index) => {
                catalog.fixtures[index].insert(key.to_owned(), value);
            }
            Section::Lane(index) => {
                catalog.lanes[index].insert(key.to_owned(), value);
            }
            Section::Family(index) => {
                catalog.families[index].0.insert(key.to_owned(), value);
            }
            Section::DataLane(index) => {
                catalog.data_lanes[index].insert(key.to_owned(), value);
            }
        }
    }
    assert!(!collecting_members, "unterminated members array");
    catalog
}

fn parse_string(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or_else(|| panic!("expected simple TOML string, got {value:?}"))
        .to_owned()
}
