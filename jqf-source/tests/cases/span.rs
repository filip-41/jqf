//! Span contracts: checked construction precedence, half-open ranges, merge, panic wording, and `SpanError` as a
//! standard error. Source ownership and stored file ranges are covered by the sibling cases.

use jqf_source::{Span, SpanError};

fn panic_message(call: impl FnOnce() + std::panic::UnwindSafe) -> String {
    let payload = std::panic::catch_unwind(call).expect_err("must panic");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_string()))
        .expect("panic payload is a string")
}

#[test]
fn fallible_span_construction_is_checked() {
    assert_eq!(Span::try_from_usize(4, 3), Err(SpanError::StartExceedsEnd));
    assert_eq!(Span::try_from_usize(3, 4), Ok(Span::new(3, 4)));
    #[cfg(target_pointer_width = "64")]
    assert_eq!(
        Span::try_from_usize(0, usize::try_from(u64::from(u32::MAX) + 1).unwrap()),
        Err(SpanError::OffsetOverflow)
    );
}

/// Pins dual-error precedence: `start > end` on the `usize` pair wins over overflow.
#[test]
#[cfg(target_pointer_width = "64")]
fn try_from_usize_reports_start_exceeds_end_before_offset_overflow() {
    let start = usize::try_from(u64::from(u32::MAX) + 2).unwrap();
    let end = usize::try_from(u64::from(u32::MAX) + 1).unwrap();
    assert_eq!(Span::try_from_usize(start, end), Err(SpanError::StartExceedsEnd));
}

#[test]
fn span_reports_offsets_length_merge_and_empty_try_new() {
    let left = Span::new(3, 8);
    let right = Span::new(6, 12);

    assert_eq!(left.start(), 3);
    assert_eq!(left.end(), 8);
    assert_eq!(left.to_string(), "3..8");
    assert_eq!(left.len(), 5);
    assert!(!left.is_empty());
    assert_eq!(left.merge(right), Span::new(3, 12));
    assert_eq!(right.merge(left), Span::new(3, 12));
    assert!(Span::new(4, 4).is_empty());
    assert_eq!(Span::try_new(4, 4), Some(Span::new(4, 4)));
    assert_eq!(Span::try_new(9, 4), None);
}

#[test]
fn span_merge_covers_disjoint_ranges() {
    let left = Span::new(2, 4);
    let right = Span::new(8, 11);

    assert_eq!(left.merge(right), Span::new(2, 11));
    assert_eq!(right.merge(left), Span::new(2, 11));
}

#[test]
fn span_converts_usize_offsets_for_source_slicing() {
    let source = "alpha\nbeta";
    let span = Span::from_usize(6, 10);

    assert_eq!(span, Span::new(6, 10));
    assert_eq!(&source[span.range()], "beta");
}

#[test]
fn span_new_panics_with_start_exceeds_end_display() {
    assert_eq!(
        panic_message(|| {
            let _ = Span::new(5, 4);
        }),
        SpanError::StartExceedsEnd.to_string()
    );
}

#[test]
fn span_from_usize_panics_with_start_exceeds_end_display() {
    assert_eq!(
        panic_message(|| {
            let _ = Span::from_usize(5, 4);
        }),
        SpanError::StartExceedsEnd.to_string()
    );
}

#[test]
#[cfg(target_pointer_width = "64")]
fn span_from_usize_panics_with_offset_overflow_display_on_start() {
    let start = usize::try_from(u64::from(u32::MAX) + 1).unwrap();
    let end = usize::try_from(u64::from(u32::MAX) + 2).unwrap();
    assert_eq!(
        panic_message(|| {
            let _ = Span::from_usize(start, end);
        }),
        SpanError::OffsetOverflow.to_string()
    );
}

#[test]
#[cfg(target_pointer_width = "64")]
fn span_from_usize_panics_with_offset_overflow_display_on_end() {
    let end = usize::try_from(u64::from(u32::MAX) + 1).unwrap();
    assert_eq!(
        panic_message(|| {
            let _ = Span::from_usize(0, end);
        }),
        SpanError::OffsetOverflow.to_string()
    );
}

#[test]
fn span_error_is_a_standard_error_with_stable_display() {
    fn assert_error<E: core::error::Error>() {}
    assert_error::<SpanError>();
    assert_eq!(SpanError::StartExceedsEnd.to_string(), "span start exceeds span end");
    assert_eq!(SpanError::OffsetOverflow.to_string(), "span offset exceeds u32::MAX");
}
