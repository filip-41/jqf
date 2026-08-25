//! The recursion ceilings for the value laws that walk a value.
//!
//! One job: own the closed table of depth caps and the message each one raises, so that the four walks that need a
//! ceiling — a path's component count, the object-merge recursion, the total order's recursion, and the containment
//! relation's — read ONE limit and ONE spelling instead of growing four private constants.
//!
//! The caps are observable semantics, not an implementation detail: the suite asserts their exact message text as
//! program OUTPUT. A value that reaches the cap is a value the program can still name, so the ceiling is visible
//! semantics, not a resource policy — which is why it lives here rather than in the ledger.
//!
//! Two properties of the table are load-bearing and easy to get wrong:
//!
//! * The path row counts COMPONENTS, before any walk. `getpath` over a
//!   10001-element path array fails without ever touching the document.
//! * The comparison row has TWO spellings, chosen by the CALLER: the operation is named, so `==` says
//!   `Equality check too deep` and a sort says `Comparison too deep`. The guard therefore returns a marker
//!   ([`TooDeep`]) and the caller picks the spelling.
//!
//! The containment row shares the comparison row's ceiling and message family:
//! at nesting the program builds, 9999 answers and 10000 raises for both.
//!
//! Negative space: it is a closed table with no default arm, so it holds no row without a caller, a corpus row and a
//! const assertion. It raises nothing on its own beyond the two prepared errors below; every other caller reads the
//! text and spells its own raise.

use jqf_resource::ResourceContext;

use crate::error::EngineRunError;

use super::path::raise;

/// One walk whose recursion (or length) is bounded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Guarded {
    /// The COMPONENT COUNT of a `getpath`/`setpath`/`delpaths` path array, checked before the walk begins.
    PathLength,
    /// The `*` operator's recursive object merge.
    ObjectMerge,
    /// The one total order's recursion, shared by `==`/`!=`, the ordering operators, and every sorting builtin.
    Comparison,
    /// The containment relation's recursion, shared by `contains` and `inside`.
    Containment,
}

/// How the operation whose cap tripped is named.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Naming {
    /// One spelling, whichever caller trips the row.
    Fixed(&'static str),
    /// Two spellings for one guard, because the OPERATION is named and the total order serves two of them.
    ByCaller {
        /// What `<`, `sort`, `unique`, `group_by` and `bsearch` say.
        ordering: &'static str,
        /// What `==` and `!=` say.
        equality: &'static str,
    },
}

/// One guarded walk's ceiling and message.
#[derive(Clone, Copy, Debug)]
pub struct LimitRow {
    /// The walk this row bounds; equal to the row's index key.
    pub guarded: Guarded,
    /// The greatest depth (or length) that still SUCCEEDS: a 10000-deep pair compares fine and a 10001-deep pair
    /// raises.
    pub limit: usize,
    /// The message, or messages, for this row.
    pub naming: Naming,
}

/// The closed table. Four rows, no default arm, no unused row.
pub const LIMITS: [LimitRow; 4] = [
    LimitRow {
        guarded: Guarded::PathLength,
        limit: 10000,
        naming: Naming::Fixed("Path too deep"),
    },
    LimitRow {
        guarded: Guarded::ObjectMerge,
        limit: 10000,
        naming: Naming::Fixed("Object merge too deep"),
    },
    LimitRow {
        guarded: Guarded::Comparison,
        limit: 10000,
        naming: Naming::ByCaller {
            ordering: "Comparison too deep",
            equality: "Equality check too deep",
        },
    },
    LimitRow {
        guarded: Guarded::Containment,
        limit: 10000,
        naming: Naming::Fixed("Containment check too deep"),
    },
];

/// The table index of one guarded walk — the fieldless enum's implicit discriminant, `as`-cast (const-legal; the
/// `placed` assertions still pin the slots at compile time).
const fn index(guarded: Guarded) -> usize {
    guarded as usize
}

/// One guarded walk's row.
pub const fn row(guarded: Guarded) -> &'static LimitRow {
    &LIMITS[index(guarded)]
}

/// The greatest depth (or component count) `guarded` still admits.
pub const fn limit(guarded: Guarded) -> usize {
    row(guarded).limit
}

/// The message `guarded` raises for every caller but the equality test.
pub const fn message(guarded: Guarded) -> &'static str {
    match row(guarded).naming {
        Naming::Fixed(text) | Naming::ByCaller { ordering: text, .. } => text,
    }
}

/// The comparison row's OTHER spelling, which only `==` and `!=` use.
pub const fn equality_message() -> &'static str {
    match row(Guarded::Comparison).naming {
        Naming::ByCaller { equality, .. } => equality,
        // The comparison row is the one row with two spellings; a `Fixed` one here would mean the table was edited
        // without its callers.
        Naming::Fixed(text) => text,
    }
}

/// The comparison row's error, spelled for an ORDERING caller (`<`, `sort`, `unique`, `group_by`, `bsearch`).
pub fn comparison_error(resources: &ResourceContext<'_>) -> EngineRunError {
    raise(message(Guarded::Comparison), resources)
}

/// The comparison row's error, spelled for the EQUALITY caller (`==`, `!=`).
pub fn equality_error(resources: &ResourceContext<'_>) -> EngineRunError {
    raise(equality_message(), resources)
}

/// A walk that ran past its row's limit.
///
/// A MARKER, not a message. The guard fires inside a shared recursion that cannot know whether its caller is a sort or
/// an `==`, and the operation is named rather than the guard — so the caller reads its own spelling out of the table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TooDeep;

/// Whether a row sits at the index its own lookup returns.
///
/// Spelled as a `matches!` over the pairing because a derived `PartialEq` is not callable in a `const` block, and this
/// assertion has to run at COMPILE time — a row added in the wrong slot would otherwise silently hand `==` the path
/// row's message.
const fn placed(row: &LimitRow, guarded: Guarded) -> bool {
    matches!(
        (row.guarded, guarded),
        (Guarded::PathLength, Guarded::PathLength)
            | (Guarded::ObjectMerge, Guarded::ObjectMerge)
            | (Guarded::Comparison, Guarded::Comparison)
            | (Guarded::Containment, Guarded::Containment)
    )
}

/// Every row is at the index its lookup uses, and every row carries a real ceiling and a non-empty message.
const _: () = {
    assert!(placed(row(Guarded::PathLength), Guarded::PathLength));
    assert!(placed(row(Guarded::ObjectMerge), Guarded::ObjectMerge));
    assert!(placed(row(Guarded::Comparison), Guarded::Comparison));
    assert!(placed(row(Guarded::Containment), Guarded::Containment));
    assert!(LIMITS[index(Guarded::PathLength)].limit == 10000);
    assert!(LIMITS[index(Guarded::ObjectMerge)].limit == 10000);
    assert!(LIMITS[index(Guarded::Comparison)].limit == 10000);
    assert!(LIMITS[index(Guarded::Containment)].limit == 10000);
    assert!(!message(Guarded::PathLength).is_empty());
    assert!(!message(Guarded::ObjectMerge).is_empty());
    assert!(!message(Guarded::Comparison).is_empty());
    assert!(!message(Guarded::Containment).is_empty());
    assert!(!equality_message().is_empty());
    let mut i = 0;
    while i < LIMITS.len() {
        assert!(LIMITS[i].limit > 0);
        i += 1;
    }
};

#[cfg(test)]
mod tests {
    use super::{Guarded, LIMITS, equality_message, limit, message, row};

    #[test]
    fn every_row_answers_for_its_own_walk() {
        for guarded in [
            Guarded::PathLength,
            Guarded::ObjectMerge,
            Guarded::Comparison,
            Guarded::Containment,
        ] {
            assert_eq!(row(guarded).guarded, guarded);
        }
        assert_eq!(LIMITS.len(), 4);
    }

    #[test]
    fn the_comparison_row_names_two_operations() {
        assert_eq!(message(Guarded::Comparison), "Comparison too deep");
        assert_eq!(equality_message(), "Equality check too deep");
        assert_eq!(limit(Guarded::Comparison), 10000);
    }

    /// The containment row is its own operation and shares the comparison row's ceiling — the table's arrangement
    /// rather than a coincidence: at nesting the program builds, 9999 answers and 10000 raises for `contains` and for
    /// `==` alike.
    #[test]
    fn the_containment_row_names_its_own_operation() {
        assert_eq!(message(Guarded::Containment), "Containment check too deep");
        assert_eq!(limit(Guarded::Containment), limit(Guarded::Comparison));
    }
}
