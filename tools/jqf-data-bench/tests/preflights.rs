use std::collections::{BTreeMap, BTreeSet};

#[test]
fn inventory_matches_the_frozen_case_sizes() {
    let actual: BTreeMap<_, _> = jqf_data_bench::cases()
        .into_iter()
        .map(|case| {
            let metadata = case.metadata();
            (metadata.name, metadata.operations_per_invocation)
        })
        .collect();
    let expected = BTreeMap::from([
        ("integer/parse-mixed-4096", 4_096),
        ("decimal/parse-mixed-4096", 4_096),
        ("object/build-small-8", 8),
        ("object/build-wide-4096-duplicates", 4_096),
        ("object/lookup-small-4096", 4_096),
        ("object/lookup-medium-4096", 4_096),
        ("object/lookup-wide-4096", 4_096),
        ("object/lookup-wide-4096-btree-reference", 4_096),
        ("object/cow-insert-wide-4096", 4_096),
        ("array/build-65536", 65_536),
        ("array/cow-detach-65536", 65_536),
        ("value/deep-clone-balanced-87381", 87_381),
        ("value/shared-clone-object-10", 4_096),
        ("value/shared-clone-balanced-87381", 4_096),
        ("document/build-semantic-65536", 1),
        ("document/build-source-rich-32768", 1),
        ("document/build-accounted-semantic-8192", 1),
        ("document/accounted-checked-clone-drop-8192", 1),
        ("object-view/lookup-wide-4096", 4_096),
        ("reader/topology-source-rich-32768", 65_537),
        ("reader/facts-source-rich-32768", 8_192),
        ("materialize/one-shot-root-65536", 65_537),
        ("materialize/reusable-root-65536", 65_537),
        ("materialize/reusable-subtree-1024", 1_024),
        ("materialize/source-rich-tagged-root-32768", 32_769),
        ("document/build-nested-balanced-json-v1", 1),
        ("document/build-escape-heavy-json-v1", 1),
        ("document/build-wide-object-duplicates-50-json-v1", 1),
        ("document/build-wide-object-duplicates-90-json-v1", 1),
        ("document/build-deep-chain-256-json-v1", 1),
        ("document/compare-minimal-rich-same-semantics-2048", 1),
        ("json/decode/full/nested", 1),
        ("json/decode/full/escape-heavy", 1),
        ("json/decode/full/wide-duplicate", 1),
        ("json/decode/full/deep", 1),
        ("json/decode/rich/nested", 1),
        ("json/decode/rich/escape-heavy", 1),
        ("json/decode/rich/wide-duplicate", 1),
        ("json/decode/rich/deep", 1),
        ("legacy/topology/target-only-4097", 4_097),
        ("legacy/topology/unkeyed-full-8192", 8_192),
        ("legacy/topology/keyed-full-4096", 4_096),
        ("legacy/array/direct-traversal-4096", 4_096),
        ("legacy/array/owner-reuse-traversal-4096", 4_096),
        ("legacy/array/irregular-indexed-traversal-4096", 4_096),
        ("legacy/object/unique-lookup-iteration-2048", 4_096),
        ("legacy/object/duplicate-50-lookup-iteration-2048", 4_096),
        ("legacy/object/duplicate-90-lookup-iteration-2048", 4_096),
        ("legacy/materialize/root-array-direct-4096", 4_096),
        ("legacy/materialize/subtree-object-duplicate-50", 2_049),
    ]);
    assert_eq!(actual, expected);
}

#[test]
fn every_declared_case_has_an_exact_preflight_receipt() {
    let mut cases = jqf_data_bench::cases();
    // 50 after plan 108 removed the five unaccounted/semantic/provenance lanes.
    assert_eq!(cases.len(), 50);

    let mut names = BTreeSet::new();
    for case in &mut cases {
        let metadata = case.metadata();
        assert!(names.insert(metadata.name), "duplicate case name {}", metadata.name);
        assert!(
            metadata.operations_per_invocation > 0,
            "{} must declare positive work",
            metadata.name
        );
        let receipt = case
            .preflight()
            .unwrap_or_else(|error| panic!("{} preflight failed: {error}", metadata.name));
        assert!(
            !receipt.detail.is_empty(),
            "{} must retain exact evidence",
            metadata.name
        );
    }
}

#[test]
fn legacy_relationship_inventory_is_closed_and_schema4_physical() {
    let mut cases = jqf_data_bench::cases();
    let expected = BTreeSet::from([
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
    ]);
    let mut observed = BTreeSet::new();
    for case in &mut cases {
        let name = case.metadata().name;
        if !name.starts_with("legacy/") {
            continue;
        }
        observed.insert(name);
        let receipt = case
            .preflight()
            .unwrap_or_else(|error| panic!("{name} preflight failed: {error}"));
        for token in [
            "receipt_schema=4",
            "evidence_role=primary",
            "alias_of=none",
            "independent_evidence=true",
            "fixture_revision=1",
            "fixture_recipe_hash=0x",
            "physical_authority=legacy-table-layout-v1",
            "occurrence_authority=legacy-occurrence-record-v1",
            "array_authority=legacy-copied-node-id-projection-v1",
            "object_authority=legacy-copied-target-key-projection-v1",
            "legacy_document_storage_inline_bytes=1384",
            "legacy_relationship_owner_inline_bytes=320",
            "legacy_fixed_nonowner_inline_bytes=1064",
            "legacy_whole_shallow_capacity_bytes=",
            "semantic_checksum_schema=jqf-value-fnv1a64-v1",
            "operation_checksum=0x",
            "physical_checksum=0x",
            "schema5_claims=false",
            "cache_observation_eligible=true",
            "layout_profile_eligible=true",
        ] {
            assert!(
                receipt.detail.contains(token),
                "{name} lacks {token:?}: {}",
                receipt.detail
            );
        }
        for forbidden in [
            "receipt_schema=5",
            "aligned-semantic-edge-compact4",
            "semantic_edge_capacity_bytes=",
            "relationship_storage_route=",
        ] {
            assert!(
                !receipt.detail.contains(forbidden),
                "{name} makes candidate-only claim {forbidden:?}: {}",
                receipt.detail
            );
        }
        assert_eq!(
            receipt.detail.matches(" evidence_role=").count(),
            1,
            "{name} has an ambiguous role receipt"
        );
        if name == "legacy/materialize/subtree-object-duplicate-50" {
            for token in [
                "fixture_shape=object-subtree-duplicate-50",
                "operations_per_invocation=2049",
                "work_items=2049",
                "semantic_relationships=2049",
                "materialized_relationship_count=2048",
                "object_projection_len=2049",
            ] {
                assert!(
                    receipt.detail.contains(token),
                    "subtree lane lacks reconstructed object proof {token}: {}",
                    receipt.detail
                );
            }
        }
    }
    assert_eq!(observed, expected);
}

#[test]
fn required_cache_inventory_contains_exact_legacy_and_strict_json_primaries() {
    let legacy = jqf_data_bench::LEGACY_RELATIONSHIP_LANES
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let strict_json = jqf_data_bench::STRICT_JSON_PRIMARY_CACHE_LANES
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    assert_eq!(legacy.len(), 11);
    assert_eq!(strict_json.len(), 4);
    assert!(legacy.is_disjoint(&strict_json));
    let expected = legacy.union(&strict_json).copied().collect::<BTreeSet<_>>();
    assert_eq!(
        jqf_data_bench::REQUIRED_CACHE_LANES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(jqf_data_bench::REQUIRED_CACHE_LANES.len(), 15);

    let mut observed = BTreeSet::new();
    for mut case in jqf_data_bench::cases() {
        let name = case.metadata().name;
        if !expected.contains(name) {
            continue;
        }
        assert!(observed.insert(name), "duplicate cache lane {name}");
        let receipt = case.preflight().expect("required cache preflight");
        jqf_data_bench::validate_cache_schema4_receipt(&receipt.detail, receipt.checksum, name)
            .expect("exact cache receipt");
        let expected_profile = if name.starts_with("legacy/") {
            "schema4-legacy-relationship-primary"
        } else {
            "schema4-strict-json-primary"
        };
        assert_eq!(jqf_data_bench::cache_lane_profile(name), Ok(expected_profile));
    }
    assert_eq!(observed, expected);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one closed receipt test pins the complete Batch-2 route token inventory"
)]
fn batch2_baselines_have_fixed_layout_route_and_inventory_receipts() {
    let mut cases = jqf_data_bench::cases();
    let mut observed = 0usize;
    for case in &mut cases {
        let name = case.metadata().name;
        if !(name.starts_with("document/build-nested-")
            || name.starts_with("document/build-escape-heavy-")
            || name.starts_with("document/build-wide-object-duplicates-")
            || name.starts_with("document/build-deep-chain-")
            || name.starts_with("json/decode/"))
        {
            continue;
        }
        observed += 1;
        let receipt = case
            .preflight()
            .unwrap_or_else(|error| panic!("{name} preflight failed: {error}"));
        for token in [
            "receipt_schema=4",
            "fixture_hash=0x",
            "logical_nodes=",
            "semantic_relationships=",
            "authored_occurrences=",
            "unique_keys=",
            "duplicate_occurrences=",
            "decoded_arena_len=",
            "decoded_arena_capacity=",
            "source_string_values=",
            "source_keys=",
            "source_integer_values=",
            "total_source_refs=",
            "stored_string_values=",
            "stored_keys=",
            "stored_integer_refs=",
            "stored_decimal_coefficient_refs=",
            "total_stored_refs=",
            "text_ref_size=",
            "stored_occurrence_key_size=",
            "decoded_text_arena_capacity_bytes=",
            "facts=0",
            "provenance=0",
            "tags=0",
            "source_reference_count=",
            "source_span_sum_bytes=",
            "source_span_union_bytes=",
            "node_capacity=",
            "occurrence_capacity=",
            "node_table_capacity_bytes=",
            "occurrence_table_capacity_bytes=",
            "shallow_table_capacity_bytes=",
            "request_retained_bytes=",
            "request_peak_bytes=",
            "source_identity_present=",
            "physical_source_backing=",
            "trusted_session_source_attachment=",
            "externally_pinned_source_bytes=NotYetImplemented",
        ] {
            assert!(
                receipt.detail.contains(token),
                "{name} lacks {token:?}: {}",
                receipt.detail
            );
        }
        for obsolete in ["source_values=", "stored_values=", "stored_text_range"] {
            assert!(
                !receipt.detail.contains(obsolete),
                "{name} retained obsolete receipt field {obsolete:?}: {}",
                receipt.detail
            );
        }
        if name.starts_with("document/build-") {
            assert!(receipt.detail.contains("route=AccountedDocumentBuilder::finish"));
            assert!(receipt.detail.contains("codec_decode=false"));
            assert!(receipt.detail.contains("physical_source_backing=false"));
            assert!(receipt.detail.contains("trusted_session_source_attachment=false"));
            assert!(receipt.detail.contains("dynamic_existing_schema_fast_append_count="));
            assert!(receipt.detail.contains("dynamic_schema_transaction_append_count="));
        } else {
            for token in [
                "strict_validation=true",
                "builder_frontend=AccountedDocumentBuilder",
                "prepared_schema=true",
                "prepared_schema_recipe_fingerprint=0x",
                "prepared_builder_frontend_accounted=true",
                "prepared_append_count=",
                "dynamic_append_count=0",
                "prepared_working_peak_bytes=NotYetImplemented",
                "identity_table_shallow_bytes=",
                "identity_owned_retained_bytes=NotYetImplemented",
                "physical_route=0x6a736f6e00000101",
                "sealed_slot=0",
                // The strict-JSON decode route requests semantic root + shape, so
                // demand-scoped coverage retains only mandatory semantics.
                "coverage_semantic=true",
                "coverage_topology=false",
                "coverage_facts=false",
                "coverage_source=false",
                "decode_only_timed=true",
            ] {
                assert!(
                    receipt.detail.contains(token),
                    "{name} lacks {token:?}: {}",
                    receipt.detail
                );
            }
            let has_source_refs = !receipt.detail.contains("total_source_refs=0 ");
            // Plan 141 S1/S2 (the JSON edit lane's out-of-band AUTHORED spans,
            // recorded at leaf and container-open time): a strict-JSON decode
            // always commits at least the root container's anchor, so the
            // source presence triplet is uniformly TRUE on these lanes even
            // for a ref-less fixture (escape-heavy). The ref counts above
            // still pin the text half; the presence triplet pins the
            // attachment law.
            for token in [
                "source_identity_present=true",
                "physical_source_backing=true",
                "trusted_session_source_attachment=true",
            ] {
                assert!(
                    receipt.detail.contains(token),
                    "{name} lacks route-dependent {token:?}: {}",
                    receipt.detail
                );
            }
            let _ = has_source_refs;
            if name.starts_with("json/decode/rich/") {
                assert!(receipt.detail.contains("evidence_role=alias"));
                assert!(receipt.detail.contains("independent_evidence=false"));
                assert!(!receipt.detail.contains("alias_of=none"));
            } else {
                assert!(receipt.detail.contains("evidence_role=primary"));
                assert!(receipt.detail.contains("independent_evidence=true"));
                assert!(receipt.detail.contains("alias_of=none"));
            }
        }
    }
    assert_eq!(observed, 13);
}

#[test]
fn minimal_and_rich_fixture_receipt_proves_equal_semantics() {
    let mut cases = jqf_data_bench::cases();
    let case = cases
        .iter_mut()
        .find(|case| case.metadata().name == "document/compare-minimal-rich-same-semantics-2048")
        .expect("same-semantics lane");
    let receipt = case.preflight().expect("same-semantics preflight");
    assert!(receipt.detail.contains("equal_semantic_projection=true"));
    assert!(receipt.detail.contains("minimal_facts=0"));
    assert!(receipt.detail.contains("rich_facts=512"));
    // RE-PINNED (F3): provenance records were removed.
    assert!(receipt.detail.contains("rich_provenance=0"));
}

#[test]
fn source_and_reader_receipts_retain_route_and_completion_evidence() {
    let mut cases = jqf_data_bench::cases();
    for case in &mut cases {
        let name = case.metadata().name;
        let receipt = case
            .preflight()
            .unwrap_or_else(|error| panic!("{name} preflight failed: {error}"));
        if name.contains("source-rich") || name.starts_with("object-view/") {
            assert!(
                receipt.detail.contains("source_string_values=") && receipt.detail.contains("source_keys="),
                "{name} lacks source-backed text-storage evidence: {}",
                receipt.detail
            );
        }
        if name == "document/build-source-rich-32768" || name == "materialize/source-rich-tagged-root-32768" {
            assert!(
                receipt.detail.contains("occurrences=32768")
                    && receipt.detail.contains("facts=8192")
                    && receipt.detail.contains("provenance=0")
                    && receipt.detail.contains("tags=4097"),
                "{name} lacks exact rich-fixture inventory evidence: {}",
                receipt.detail
            );
        }
        if name == "object-view/lookup-wide-4096" {
            assert!(
                receipt.detail.contains("member_occurrences=32768")
                    && receipt.detail.contains("unique_entries=28672")
                    && receipt.detail.contains("duplicate_occurrences=4096")
                    && receipt.detail.contains("lookups=4096"),
                "{name} lacks exact duplicate and lookup evidence: {}",
                receipt.detail
            );
        }
        if name.starts_with("reader/") {
            assert!(
                receipt.detail.contains("completion=complete")
                    && receipt.detail.contains("completion_fingerprint=")
                    && receipt.detail.contains("renewed=")
                    && receipt.detail.contains("expectation=identity-independent-fixture-law")
                    && receipt.detail.contains("fixture_expected_checksum="),
                "{name} lacks independent correctness or terminal cooperative evidence: {}",
                receipt.detail
            );
        }
        if name == "materialize/source-rich-tagged-root-32768" {
            assert!(
                receipt
                    .detail
                    .contains("source_expectation=identity-independent-fixture-bytes"),
                "{name} lacks an independently validated source checksum: {}",
                receipt.detail
            );
        }
        if name == "value/deep-clone-balanced-87381" || name == "materialize/reusable-subtree-1024" {
            assert!(
                receipt.detail.contains("allocation_free_timed_witness="),
                "{name} lacks its allocation-free timed witness: {}",
                receipt.detail
            );
        }
    }
}
