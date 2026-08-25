use core::mem::size_of;

use jqf_data::{Float, FractionalSecond, KnownUtcOffset, LocalDate, LocalTime, Shared, TagId, Value};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

static CONTROL: ContinueControl = ContinueControl;

/// Test context. Constructors take it and ignore it.
fn ledger() -> ResourceContext<'static> {
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
    let account = RequestAccount::try_new(limits).expect("test account");
    let work = WorkMeter::try_new_v1(1).expect("test work meter");
    ResourceContext::new(account, &CONTROL, work).expect("test ledger")
}

/// A deep tag stack clones by bumping the outermost handle. It does not walk the wrappers: the clone shares the deepest
/// payload allocation, and a separately built twin stack shares nothing even though every layer wraps equal text.
#[test]
fn deeply_tagged_values_clone_without_recursion() {
    let _resources = ledger();
    let tag = TagId::try_new_unaccounted("!layer").expect("valid tag");
    // The innermost payload is heap-backed so allocation identity is observable through `shares_allocation_with`.
    let mut value = Value::try_string("core").expect("innermost text allocates");
    for _ in 0..4_096 {
        value = Value::try_tagged(tag.clone(), value).expect("tag wrapper allocates");
    }
    let cloned = value.clone();
    assert_eq!(cloned.kind(), value.kind());
    assert!(
        cloned.shares_allocation_with(&value),
        "a deep-tag clone shares its payload allocation instead of copying it"
    );

    let mut twin = Value::try_string("core").expect("twin text allocates");
    for _ in 0..4_096 {
        twin = Value::try_tagged(tag.clone(), twin).expect("tag wrapper allocates");
    }
    assert!(!cloned.shares_allocation_with(&twin));
}

#[test]
#[cfg(target_pointer_width = "64")]
fn value_size_is_guarded_on_supported_64_bit_targets() {
    assert_eq!(size_of::<Value>(), 32);
}

/// The growable text payload answers what rebuilding the string answers, at every step and through both of its arms.
///
/// The pieces are chosen to make the reserved tail observable if it ever leaks: one of them ENDS in a NUL, which is the
/// byte the reservation is padded with, so an implementation that recovered the used length by trimming trailing NULs
/// instead of storing it would fail here rather than in production.
#[test]
fn growing_text_agrees_with_rebuilding_it() {
    let pieces = ["a", "b\u{0}", "é", "日本", "", "\u{0}\u{0}"];
    let mut grown = Shared::try_from_str("").expect("empty text allocates");
    let mut rebuilt = String::new();
    for (step, piece) in pieces.iter().cycle().take(96).enumerate() {
        grown.try_extend(piece).expect("append");
        rebuilt.push_str(piece);
        assert_eq!(grown.as_str(), rebuilt, "as_str at step {step}");
        assert_eq!(&*grown, rebuilt.as_str(), "deref at step {step}");
        assert_eq!(format!("{grown:?}"), format!("{rebuilt:?}"), "debug at {step}");
    }
}

/// A payload a second handle can still see is never written through, even when it has reserved tail bytes the append
/// would otherwise land in.
///
/// This is the aliasing law the whole mechanism rests on: a `foreach` fold publishes the intermediate states of its
/// accumulator while the fold keeps accumulating, so an in-place append that ignored the second handle would rewrite
/// values that have already been emitted.
#[test]
fn text_another_handle_can_see_is_never_written_through() {
    let mut grown = Shared::try_from_str("").expect("empty text allocates");
    // Three single-byte appends leave the payload reserving ahead, so the fourth is the one that WOULD land in place.
    for _ in 0..3 {
        grown.try_extend("a").expect("append");
    }
    let witness = grown.clone_shared();
    grown.try_extend("b").expect("append past a second handle");
    assert_eq!(witness.as_str(), "aaa");
    assert_eq!(grown.as_str(), "aaab");
}

/// A payload may be extended by text it already holds: sharing is what makes it non-unique, and the reallocating arm
/// reads both operands before it replaces either.
#[test]
fn text_can_be_extended_by_the_payload_it_already_holds() {
    let mut grown = Shared::try_from_str("ab").expect("text allocates");
    let twin = grown.clone_shared();
    grown.try_extend(twin.as_str()).expect("self append");
    assert_eq!(grown.as_str(), "abab");
    assert_eq!(twin.as_str(), "ab");
}

/// An empty suffix is not an append: it neither reallocates nor copies.
#[test]
fn extending_text_by_nothing_leaves_it_alone() {
    let mut grown = Shared::try_from_str("held").expect("text allocates");
    let before = grown.as_str().as_ptr();
    grown.try_extend("").expect("empty append");
    assert_eq!(grown.as_str(), "held");
    assert_eq!(grown.as_str().as_ptr(), before, "no reallocation");
}

#[test]
fn float_storage_preserves_nan_payload_and_signed_zero_bits() {
    let nan_bits = 0x7ff8_0000_0000_0042;
    assert_eq!(Float::new(f64::from_bits(nan_bits)).bits(), nan_bits);
    assert_ne!(Float::new(-0.0).bits(), Float::new(0.0).bits());
}

#[test]
fn temporal_boundaries_are_explicit_and_canonical() {
    assert!(LocalDate::new(2000, 2, 29).is_some());
    assert!(LocalDate::new(1900, 2, 29).is_none());
    assert!(KnownUtcOffset::new(86_399).is_some());
    assert!(KnownUtcOffset::new(86_400).is_none());
    let fraction = FractionalSecond::parse("1200").expect("valid fraction");
    assert_eq!(fraction.digits(), "12");
    assert!(LocalTime::new(23, 59, 60, fraction).is_some());
}
