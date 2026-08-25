//! Source identity contracts: ids include their kind, resolved sources retain labels and base offsets, and file-range
//! bounds are stored as given. Span construction and diagnostic attachment belong to the sibling cases.

use jqf_source::{ResolvedSource, SourceFileRange, SourceId, SourceKind, SourceRef};

#[test]
fn source_refs_carry_identity_and_kind() {
    let source = SourceRef::new(SourceId::new(17), SourceKind::Input);

    assert_eq!(source.id().get(), 17);
    assert_eq!(source.kind(), SourceKind::Input);
    assert_eq!(format!("{source}"), "input#17");
    assert_eq!(
        format!(
            "{} {}",
            SourceRef::new(SourceId::new(1), SourceKind::Query),
            SourceRef::new(SourceId::new(2), SourceKind::Input)
        ),
        "query#1 input#2"
    );
}

#[test]
fn resolved_source_records_label_and_base_offset() {
    let source = SourceRef::new(SourceId::new(4), SourceKind::Input);
    let resolved = ResolvedSource::new(source, "orders.ndjson:5", b"{\"id\":5}", 120);

    assert_eq!(resolved.source(), source);
    assert_eq!(resolved.label(), "orders.ndjson:5");
    assert_eq!(resolved.bytes(), b"{\"id\":5}");
    assert_eq!(resolved.base_offset(), 120);
}

#[test]
fn resolved_source_base_offset_marks_empty_slice_start() {
    let source = SourceRef::new(SourceId::new(5), SourceKind::Input);
    let resolved = ResolvedSource::new(source, "empty", b"", 42);

    assert_eq!(resolved.bytes(), b"");
    assert_eq!(resolved.base_offset(), 42);
}

/// Adjacent file ranges store abutting bounds. This type does not walk a slice or attribute a spanning value.
#[test]
fn source_file_ranges_store_abutting_bounds() {
    let first = SourceFileRange::new("a", 0, 5);
    let second = SourceFileRange::new("b", 5, 11);

    assert_eq!(first.label(), "a");
    assert_eq!(first.start(), 0);
    assert_eq!(first.end(), 5);
    assert_eq!(second.label(), "b");
    assert_eq!(second.start(), 5);
    assert_eq!(second.end(), 11);
    assert_eq!(first.end(), second.start());
}

/// Pins that an inverted range is stored as given — no `start <= end` check, per CONTRACTS.md.
#[test]
fn source_file_ranges_store_inverted_bounds_as_given() {
    let range = SourceFileRange::new("late.log", 100, 50);

    assert_eq!(range.label(), "late.log");
    assert_eq!(range.start(), 100);
    assert_eq!(range.end(), 50);
}
