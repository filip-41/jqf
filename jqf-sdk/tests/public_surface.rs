//! The public surface is one entry point. This test fails if a second one
//! appears — the SDK's routing must never be an embedder's vocabulary again.

#[test]
fn there_is_exactly_one_execute_entry_point() {
    let surface = std::fs::read_to_string("src/lib.rs").expect("lib.rs");
    let exported: Vec<&str> = surface
        .lines()
        .filter(|line| line.trim_start().starts_with("pub use") || line.trim_start().starts_with("pub fn"))
        .filter(|line| line.contains("execute"))
        .collect();
    assert_eq!(
        exported.len(),
        1,
        "exactly one execute entry point may be public; found:\n{}",
        exported.join("\n")
    );
}

#[test]
fn drive_run_and_ordered_encode_types_are_not_embedder_public() {
    let surface = std::fs::read_to_string("src/lib.rs").expect("lib.rs");
    for name in [
        "EditRun",
        "RoundtripRun",
        "RangeLocateRun",
        "encode_ordered",
        "OrderedResultProducer",
        "OrderedResultPoll",
        "OrderedEncodingReport",
        "OrderedEncodingPolicy",
    ] {
        assert!(
            !surface.contains(name),
            "{name} must not appear on the embedder surface"
        );
    }
}
