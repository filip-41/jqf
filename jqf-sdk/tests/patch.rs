use jqf_sdk::{BytePatch, PatchError, PatchSet};
use jqf_source::{SourceId, SourceKind, SourceRef};

fn patch(start: usize, end: usize, replacement: &[u8]) -> BytePatch {
    BytePatch::try_from_usize(start, end, replacement.to_vec()).unwrap()
}

#[test]
fn patch_validation_precedence_and_application_contract() {
    assert_eq!(
        BytePatch::try_from_usize(2, 1, vec![]),
        Err(PatchError::StartExceedsEnd)
    );
    let source = SourceRef::new(SourceId::new(1), SourceKind::Input);
    assert_eq!(
        PatchSet::try_new(None, usize::MAX, vec![]),
        Err(PatchError::OffsetOverflow)
    );
    assert_eq!(
        PatchSet::try_new(None, 3, vec![patch(2, 4, b"")]),
        Err(PatchError::OutOfBounds)
    );
    assert_eq!(
        PatchSet::try_new(None, 4, vec![patch(2, 3, b"x"), patch(1, 2, b"y")]),
        Err(PatchError::Unsorted)
    );
    assert_eq!(
        PatchSet::try_new(None, 4, vec![patch(1, 1, b"x"), patch(1, 1, b"y")]),
        Err(PatchError::AmbiguousInsertion)
    );
    assert_eq!(
        PatchSet::try_new(None, 4, vec![patch(1, 3, b"x"), patch(1, 2, b"y")]),
        Err(PatchError::Overlap)
    );
    assert_eq!(
        PatchSet::try_new(None, 4, vec![patch(1, 1, b"x"), patch(1, 2, b"y")]),
        Err(PatchError::AmbiguousInsertion)
    );
    assert_eq!(
        PatchSet::try_new(None, 4, vec![patch(1, 3, b"x"), patch(2, 4, b"y")]),
        Err(PatchError::Overlap)
    );

    let set = PatchSet::try_new(
        Some(source),
        6,
        vec![patch(1, 3, b"XY"), patch(3, 3, b"!"), patch(5, 6, b"z")],
    )
    .unwrap();
    assert_eq!(set.apply(Some(source), b"abcdef"), Ok(b"aXY!dez".to_vec()));
    assert_eq!(set.apply(Some(source), b"abc"), Err(PatchError::OriginalLengthMismatch));
    assert_eq!(set.apply(None, b"abcdef"), Err(PatchError::SourceMismatch));
}

#[test]
fn patch_output_size_is_preflighted() {
    let huge = Vec::with_capacity(0);
    let set = PatchSet::try_new(None, 0, vec![BytePatch::try_from_usize(0, 0, huge).unwrap()]).unwrap();
    assert_eq!(set.apply(None, b""), Ok(vec![]));
    assert_eq!(
        PatchSet::try_new(None, 3, vec![patch(0, 1, b"x"), patch(1, 2, b"y")])
            .unwrap()
            .apply(None, b"abc"),
        Ok(b"xyc".to_vec())
    );
}
