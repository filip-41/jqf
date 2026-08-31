//! Token-aware prelude gate scan, pinned through the test-only export.
//!
//! [`scan_prelude_gate`] decides whether embedded stdlib and extension
//! preludes are parsed before the user query. False hits only waste a parse;
//! false misses are unsound. The scanner lives in
//! `jqf-engine/src/compile/prelude_gate.rs`; the name lists it consults live
//! in `jqf-engine/src/compile/stdlib.rs`. List/sync pins stay in
//! `compile/tests.rs` because they test the prelude text and gate lists
//! together.
//!
//! Integration tests cannot reach `pub(crate)` items; `lib.rs` re-exports the
//! scanner as `#[doc(hidden)]` for this file only.

use jqf_engine::{PreludeGate, scan_prelude_gate};

#[test]
fn the_prelude_gate_skips_hash_line_comments() {
    assert_eq!(scan_prelude_gate(""), PreludeGate::empty());
    assert_eq!(scan_prelude_gate("."), PreludeGate::empty());
    assert_eq!(scan_prelude_gate("# first\n."), PreludeGate::empty());
    assert_eq!(scan_prelude_gate("# import \"x\"\n."), PreludeGate::empty());
}

#[test]
fn the_prelude_gate_hits_prelude_names_and_directives() {
    assert_eq!(
        scan_prelude_gate("any"),
        PreludeGate {
            needs_stdlib: true,
            needs_extension: false,
        }
    );
    assert_eq!(
        scan_prelude_gate("deltas(.[])"),
        PreludeGate {
            needs_stdlib: false,
            needs_extension: true,
        }
    );
    assert_eq!(scan_prelude_gate(".allowed"), PreludeGate::empty());
    for source in ["import \"m\"", "include \"m\"", "module m"] {
        assert_eq!(
            scan_prelude_gate(source),
            PreludeGate {
                needs_stdlib: true,
                needs_extension: true,
            },
            "{source}"
        );
    }
}

#[test]
fn the_prelude_gate_rejects_import_substrings_and_string_literals() {
    for source in ["importance", "reimport"] {
        assert_eq!(scan_prelude_gate(source), PreludeGate::empty(), "{source}");
    }
    assert_eq!(scan_prelude_gate("\"all\""), PreludeGate::empty());
    assert_eq!(scan_prelude_gate("\"deltas\""), PreludeGate::empty());
    assert_eq!(scan_prelude_gate("x + \"any\""), PreludeGate::empty());
    assert_eq!(scan_prelude_gate("\"all"), PreludeGate::empty());
}

#[test]
fn the_prelude_gate_scans_interpolation_holes_as_code() {
    // A `\(…)` hole is an expression, not string text: a prelude name inside
    // it is a real call, and missing it would leave the name undefined at
    // compile. Regression for the token gate that skipped whole strings.
    for source in [
        "\"\\(any)\"",
        "\"x\\(any)y\"",
        "\"\\(  any )\"",
        "\"\\(first | any)\"",
        // A hole can contain a string that contains a hole.
        "\"\\(f(\"\\(any)\"))\"",
    ] {
        assert!(
            scan_prelude_gate(source).needs_stdlib,
            "{source} must reach the stdlib prelude"
        );
    }
    assert!(
        scan_prelude_gate("\"\\(deltas(.[]))\"").needs_extension,
        "extension name inside a hole"
    );
    // A hole with no prelude name still scans clean, and grouped parens do
    // not confuse the hole's closing bracket.
    assert_eq!(scan_prelude_gate("\"\\(nope)\""), PreludeGate::empty());
    assert_eq!(scan_prelude_gate("\"\\((1 + 2))\""), PreludeGate::empty());
    // An escaped backslash is not an interpolation: `"\\("` is a literal
    // backslash then a paren, no hole.
    assert_eq!(scan_prelude_gate("\"\\\\(\""), PreludeGate::empty());
}

#[test]
fn the_prelude_gate_treats_at_prefix_and_dollar_names() {
    // `@` is not an identifier byte, so the directive token after it still matches.
    assert_eq!(
        scan_prelude_gate("@import"),
        PreludeGate {
            needs_stdlib: true,
            needs_extension: true,
        }
    );
    assert_eq!(
        scan_prelude_gate("@all"),
        PreludeGate {
            needs_stdlib: true,
            needs_extension: false,
        }
    );
    // `$` is part of language identifiers; `$all` is not the stdlib name `all`.
    assert_eq!(scan_prelude_gate("$all"), PreludeGate::empty());
}

#[test]
fn the_prelude_gate_exits_once_both_preludes_are_needed() {
    assert_eq!(
        scan_prelude_gate("any deltas"),
        PreludeGate {
            needs_stdlib: true,
            needs_extension: true,
        }
    );
}
