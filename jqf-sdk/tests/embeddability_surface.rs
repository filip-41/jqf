//! Pins the embeddability re-export block declared in `jqf-sdk/src/lib.rs`.
//!
//! These names have zero external consumers in-tree today, but they are the
//! deliberate embeddability surface an out-of-tree consumer may already name.
//! Deleting or renaming one is a surface change that must be made consciously,
//! not drift: a compile failure here means the re-export block moved, and this
//! pin must be updated in the same commit with the reason stated.

use jqf_sdk::{
    EditRun, EventStreamReport, RangeLocateRun, RoundtripRun, StreamingEventStreamError, StreamingSequenceError,
};

fn assert_name_exists<T>() {}

#[test]
fn embeddability_re_exports_stay_pinned() {
    assert_name_exists::<EditRun>();
    assert_name_exists::<RangeLocateRun>();
    assert_name_exists::<RoundtripRun>();
    assert_name_exists::<EventStreamReport>();
    assert_name_exists::<StreamingEventStreamError<u8, u8>>();
    assert_name_exists::<StreamingSequenceError<u8, u8>>();
}
