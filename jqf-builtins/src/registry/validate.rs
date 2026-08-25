//! Compile-time integrity of the builtin catalog inventories.
//!
//! One job: reject a malformed inventory at compile time. [`validate`] is a `const fn` the module invokes in a `const
//! _: ()` item, so a duplicate id, a duplicate `(name, arity)`, a missing required doc field, or a demand transfer that
//! disagrees with its execution kind is a build error, never a runtime check. At zero registered records every loop
//! runs zero iterations and validation passes — the machinery is compiled and CI-executed from day one while the
//! inventory is data.
//!
//! Negative space: it stores no records, resolves no names, and performs no runtime work.

use super::record::{BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadRecord, DemandTransfer};

/// Validates the family and overload inventories at compile time.
///
/// # Panics
///
/// Panics (as a compile error, since it is only ever invoked in const context) when a family is missing its required
/// summary, two families share an id, an overload is missing its required examples, an overload's parameter-kind count
/// disagrees with its arity, an overload's execution kind and demand transfer break the `Lowering ⇔ ViaLowering`
/// rule, an overload names a family id with no registered family record, two overloads share an id, or two overloads
/// share a `(canonical name, arity)` pair.
#[allow(
    clippy::cast_lossless,
    reason = "u8 -> usize widening is lossless; `usize::from` is not const-callable here"
)]
pub(super) const fn validate(families: &[BuiltinFamilyRecord], overloads: &[BuiltinOverloadRecord]) {
    let mut i = 0;
    while i < families.len() {
        assert!(
            !families[i].summary.is_empty(),
            "builtin family record is missing its required summary"
        );
        let mut j = i + 1;
        while j < families.len() {
            assert!(
                families[i].id.get() != families[j].id.get(),
                "duplicate builtin family id"
            );
            j += 1;
        }
        i += 1;
    }

    let mut i = 0;
    while i < overloads.len() {
        assert!(
            !overloads[i].examples.is_empty(),
            "builtin overload record is missing its required examples"
        );
        assert!(
            overloads[i].parameters.len() == overloads[i].arity as usize,
            "builtin overload parameter-kind count disagrees with its arity"
        );
        assert!(
            execution_agrees_with_transfer(overloads[i].execution, overloads[i].demand_transfer),
            "builtin overload violates `execution == Lowering <=> transfer == ViaLowering`"
        );
        // An overload may only name a family with a REGISTERED record: a record that named an unregistered family id
        // once sailed through, because nothing cross-checked the two inventories.
        assert!(
            family_registered(families, overloads[i].family.get()),
            "builtin overload names a family id with no registered record"
        );
        let mut j = i + 1;
        while j < overloads.len() {
            assert!(
                overloads[i].id.get() != overloads[j].id.get(),
                "duplicate builtin overload id"
            );
            assert!(
                !(overloads[i].arity == overloads[j].arity
                    && str_eq(overloads[i].canonical_name, overloads[j].canonical_name)),
                "duplicate builtin (name, arity)"
            );
            j += 1;
        }
        i += 1;
    }
}

/// The cross-field rule between an overload's execution kind and its demand transfer: `execution == Lowering ⇔
/// transfer == ViaLowering`.
///
/// Both directions are load-bearing. A `Lowering` overload never reaches the classifier as a `Call` (the compiler
/// rewrote it), so any transfer other than `ViaLowering` would be a dead declaration a reader could mistake for the
/// law; and a non-`Lowering` overload declaring `ViaLowering` would promise a rewrite that never happens, leaving a
/// live `Call` whose transfer is a lie.
const fn execution_agrees_with_transfer(execution: BuiltinExecution, transfer: DemandTransfer) -> bool {
    matches!(
        (execution, transfer),
        (BuiltinExecution::Lowering, DemandTransfer::ViaLowering)
    ) || !matches!(execution, BuiltinExecution::Lowering) && !matches!(transfer, DemandTransfer::ViaLowering)
}

/// The COMPILE-FAIL witness for the cross-field rule, evaluated in const context exactly as [`validate`] evaluates it.
///
/// Field PRESENCE needs no test and cannot honestly get one: `demand_transfer` is a required struct field, so an
/// omitting record does not type-check and no test can express the omission. What a well-typed record CAN still violate
/// is the cross-field rule, so this is the test for THAT rule: the same `const fn`
/// `validate` calls is asserted here to REJECT both violating pairings and to accept both legal ones. Registering a
/// violating record makes `validate`'s `assert!` fail the build for exactly the reason asserted here.
const _: () = {
    assert!(!execution_agrees_with_transfer(
        BuiltinExecution::Lowering,
        DemandTransfer::Subtree
    ));
    assert!(!execution_agrees_with_transfer(
        BuiltinExecution::Evaluator,
        DemandTransfer::ViaLowering
    ));
    assert!(execution_agrees_with_transfer(
        BuiltinExecution::Lowering,
        DemandTransfer::ViaLowering
    ));
    assert!(execution_agrees_with_transfer(
        BuiltinExecution::Evaluator,
        DemandTransfer::Subtree
    ));
};

/// The COMPILE-FAIL witness for the family-registration gate: an overload naming a family id with no registered record
/// must REJECT, a matching pair must ACCEPT. This is the same `const fn` [`validate`] calls on every overload, so the
/// gate cannot drift out of lockstep with its witness — deleting or weakening the helper fails here first.
const fn family_registered(families: &[BuiltinFamilyRecord], family: u16) -> bool {
    let mut k = 0;
    while k < families.len() {
        if families[k].id.get() == family {
            return true;
        }
        k += 1;
    }
    false
}

const _: () = {
    assert!(!family_registered(
        &[BuiltinFamilyRecord {
            id: BuiltinFamilyId::new(1),
            canonical_name: "one",
            category: "jqf-extension",
            summary: "s",
            detail: "d",
        }],
        2,
    ));
    assert!(family_registered(
        &[BuiltinFamilyRecord {
            id: BuiltinFamilyId::new(1),
            canonical_name: "one",
            category: "jqf-extension",
            summary: "s",
            detail: "d",
        }],
        1,
    ));
};

/// Byte-equality of two strings in const context (`str == str` is not const).
const fn str_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut i = 0;
    while i < left.len() {
        if left[i] != right[i] {
            return false;
        }
        i += 1;
    }
    true
}
