//! The LOCATE transfer table — which programs are provably the value the codec
//! can deliver by LOCATING one exact path, range included.
//!
//! This is the LOCATE gate, the sibling of [`count`](super::count)'s range row.
//! That row answers "is this program's
//! published value the in-range element COUNT?"; this one answers the narrower
//! publishing question: **is this whole program's published value exactly the
//! ARRAY a trailing slice materializes?** If it is, the codec's scoped route can
//! validate the input, navigate the static prefix, cut the byte region holding
//! exactly the in-range elements, and re-parse those bytes alone — so the four
//! fifths of the document the program never reads are never built.
//!
//! Like every other table in this module it is CLOSED with a declining default:
//! one row, and a shape that is not it is not locate-equivalent.
//!
//! # The row
//!
//! | shape | example | why it is exactly the located value |
//! | --- | --- | --- |
//! | `PATH[a:b]` (bare stage, nothing after) | `.catalog[100:110]` | a slice of an ARRAY materializes a fresh array of its in-range elements, which is precisely what the codec's bracket-wrapping range materialization produces |
//!
//! The side conditions are checked here rather than assumed:
//!
//! 1. **Exactly ONE slice, in TRAILING position**. Every earlier step is a
//!    static `Key`/`Index`, so a slice-of-slice
//!    stack, an `.[]` before the slice, a `..`, and a `.[$x]` all decline. The
//!    row is therefore PREFIX-SCOPED by construction: no program with an element
//!    boundary, and no non-`Stage` root, can match it, so the classifications the
//!    (`[.a[] | .tags[1:3]]`, `.a[][0:2]`) cannot move.
//! 2. **Nothing follows the slice** — the stage's last step IS the slice, and the
//!    root IS that stage. There is no residual to run, so the located value is
//!    the published value with no executor step in between.
//! 3. **The bounds normalize** under the boundary law
//!    ([`crate::program::SliceBounds::try_normalize`]), which declines a `Var`
//!    bound and a non-numeric literal bound; a strictly-negative resolved
//!    bound normalizes to its SIGNED value, whose len-relative reading the
//!    consuming range scan owns.
//! 4. **The slice step carries no `?`**. `{} | .[1:2]?` publishes nothing where
//!    `{} | .[1:2]` errors; both are the floor's, exactly as the range-count row
//!    declines them.
//!
//! # The container dispatch is the ROUTE's, not this table's
//!
//! The runtime dispatch (array proceeds, string declines, null and missing
//! publish `null`, object and the other scalars render the slice error
//! from the AUTHORED bound spellings) is not decided here and is not
//! reimplemented anywhere. The codec reports the container KIND only, and the
//! rung that consumes this row publishes NOTHING unless the located record is a
//! resolved node — every other observation falls through to the ordinary route,
//! which is the floor's own answer byte for byte. That is the "obtained by
//! construction" discharge the other range rows got; what this row adds is the
//! decline arm the `Located` route was missing, placed in the DRIVE
//! (before a single byte is published) rather than in `EngineRun`.

use crate::program::{ProgramNode, ProgramNodeId, StageStart, StepAccess};

/// Whether `root` is the locate row `PATH[a:b]`.
///
/// A match means the program root is a bare [`crate::program::ProgramNode::Stage`]
/// whose whole step list is the exact path (static steps plus the one trailing
/// range).
pub fn is_range_locate(nodes: &[ProgramNode], root: ProgramNodeId) -> bool {
    let ProgramNode::Stage {
        start: StageStart::Current,
        steps,
    } = &nodes[root.index()]
    else {
        return false;
    };
    let Some((last, prefix)) = steps.split_last() else {
        return false;
    };
    let StepAccess::Slice(bounds) = last.access() else {
        return false;
    };
    if last.is_optional() {
        return false;
    }
    if bounds.try_normalize().is_none() {
        return false;
    }
    prefix
        .iter()
        .all(|step| matches!(step.access(), StepAccess::Key(_) | StepAccess::Index(_)))
}
