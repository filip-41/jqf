//! Equivalence-class receipts: spellings that must publish identical bytes
//! and (allowlist aside) classify and route identically.
//!
//! Obligation (a) bytes is never waived. Route and class exemptions are the
//! narrowest `Exempt` that fits. Driven through [`crate::harness::oracle_run_over`].

use crate::harness::{OracleOutcome, OracleRoute, oracle_run_over};
use crate::harness::{program_for, resources};
use crate::projection::projection_class_label;
use jqf_codec_core::AccessResultKind;
use jqf_data::{DialectId, FormatId};
use jqf_sdk::CodecCatalog;

struct Spelling {
    program: &'static str,
    /// `None` when the spelling carries the FULL obligation — (a) bytes +
    /// completion + `failure_class`, (b) classification, (c) route. `Some(_)`
    /// allowlists it out of (b) and (c) only; NO spelling is ever allowlisted
    /// out of (a).
    allowlist: Option<Allowlisted>,
}

/// One allowlist entry: what a spelling is exempt from, why, and the exact
/// condition under which the exemption must be RETIRED.
struct Allowlisted {
    exempt: Exempt,
    reason: &'static str,
    retire_when: &'static str,
}

/// How much of the class obligation an allowlist entry waives.
///
/// Always the NARROWEST that fits: a spelling whose classification still agrees
/// takes [`Exempt::RouteOnly`], so obligation (b) stays a live check on it. No
/// variant waives (a) — a byte or `failure_class` difference means the spellings are not equivalent
/// at all, and there is nothing to allowlist.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Exempt {
    /// (b) classification AND (c) route.
    ClassAndRoute,
    /// (c) route only.
    RouteOnly,
    /// (b) classification only — the route obligation stays a live check.
    ///
    /// A boundary-less spelling (`.[1:3] | length`) in a class with
    /// boundary-ful ones. `projection_class` is the per-element demand, and a
    /// program with no `.[]` has no element to describe, so it reports the
    /// documented `Subtree` default. That is classification being inapplicable,
    /// not a shape cliff — the route obligation stays in force.
    ClassOnly,
}

impl Exempt {
    const fn label(self) -> &'static str {
        match self {
            Self::ClassAndRoute => "class+route",
            Self::RouteOnly => "route",
            Self::ClassOnly => "class",
        }
    }
}

/// One equivalence class: spellings that must publish identical bytes, and
/// (allowlist aside) classify and route identically.
struct EquivalenceClass {
    name: &'static str,
    spellings: &'static [Spelling],
    /// The probe documents every spelling runs over, as JSON text.
    inputs: &'static [&'static str],
    /// Programs deliberately NOT in the class, each with the probe-established
    /// reason. The harness proves the exclusion is a FACT (some probe input
    /// makes it publish different bytes), not an opinion.
    non_members: &'static [(&'static str, &'static str)],
    /// The rung each probe input takes today, IN ORDER — one entry per input,
    /// `CompleteDocument` being the whole-document floor.
    ///
    /// Pinning the rung per input catches a class that starts leaving the
    /// floor, one that stops, and one that changes lanes. A class whose list
    /// is all `CompleteDocument` is one whose route obligation (c) is floor ≡
    /// floor, and it says so in writing.
    rungs: &'static [AccessResultKind],
}

/// The seed equivalence classes.
///
/// The law: a shape cliff between two spellings of the same computation is a
/// failing test. A new vertical adds its spellings to these classes.
///
/// # The allowlist
///
/// Some spelling is byte-equal to its class but classifies coarser or routes
/// differently for a RECORDED reason. Those are listed here, visibly, each with
/// its reason and its retirement condition — never dropped from the class, and
/// never exempt from byte identity.
///
/// | class | spelling | exempt from | reason | retire when |
/// | --- | --- | --- | --- | --- |
/// | `collect-count` | `[foreach .[] as $x (null; $x.name)] \| length` | (b) class, (c) route | `foreach`/`reduce` state demand is pinned `Subtree`, so the collect classifies `Subtree` where the `map`/collect spellings classify `Structure`; and `foreach` still declines every projected transfer row, publishing per iteration rather than reaching the boundary through a pipe spine. | the foreach-state demand no longer pinned `Subtree` and a projected transfer row that matches a `foreach` source — then this spelling must classify and route with its class. |
/// | `container-count` | `.a \| length` | (b) class | a boundary-LESS count row has no element for the per-element demand lattice to describe, so it reports the documented `Subtree` default. The ROUTE obligation is unwaived, and it is the one that matters: the container-count row exists precisely so this spelling takes its reference's rung. | the projection lattice gains a vocabulary for programs with no element boundary — the same condition that retires `slice-count`'s `.[1:3] \| length` entry. |
/// | `group-count` | `group_by(.k) \| map({key: (.[0] \| .k), count: length})` | (b) class, (c) route | `group_by` declares `Subtree` and the declaration is honest: its key filter may navigate arbitrarily deep and the partition republishes whole elements. The ordering spelling therefore classifies `Subtree` where the INDEX-shaped fold reaches the member fields it actually reads. Byte identity is unwaived and holds on every probe input. | the keyed ordering builtins gain a per-element transfer row derived from their KEY FILTER rather than from the family, at which point `group_by(.k)` must classify and route with its class. |
const EQUIVALENCE_CLASSES: &[EquivalenceClass] = &[
    EquivalenceClass {
        name: "path-delete",
        // `del(f)` IS `delpaths([path(f)])` — the lowering, not a coincidence.
        // The class exists so the identity stays true through both halves: the
        // path `f` produces and the simultaneous deletion `delpaths` performs.
        spellings: &[
            Spelling {
                program: "delpaths([[\"a\"]])",
                allowlist: None,
            },
            Spelling {
                program: "del(.a)",
                allowlist: None,
            },
            // The assignment vertical's third spelling of the same computation.
            // A `|=` update that emits NOTHING deletes its path through the
            // fold's deferred `delpaths`, so `.a |= empty` reaches the identical
            // deletion by the identical primitive — including the error classes,
            // which it inherits from the same path walk.
            Spelling {
                program: ".a |= empty",
                allowlist: None,
            },
        ],
        inputs: &[r#"{"a":1,"b":2}"#, "{}", r#"{"a":null}"#],
        // A path DELETION is not a path READ, and it is not a deletion of a
        // different path: each non-member publishes different bytes on some
        // probe input above.
        non_members: &[
            (".a", "reads the field instead of removing it"),
            ("delpaths([[\"b\"]])", "removes a different member"),
        ],
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "collect-count",
        spellings: &[
            Spelling {
                program: "[.[] | .name] | length",
                allowlist: None,
            },
            // Safe BY CONSTRUCTION: `map(f)` lowers to exactly `[.[] | f]`, so
            // the two are literally the same plan (the `map_lowering_equivalence`
            // receipt proves the lowering; this proves the class).
            Spelling {
                program: "map(.name) | length",
                allowlist: None,
            },
            Spelling {
                program: "[foreach .[] as $x (null; $x.name)] | length",
                allowlist: Some(Allowlisted {
                    exempt: Exempt::ClassAndRoute,
                    reason: "foreach state demand pinned Subtree, which classifies \
                             coarser, and foreach matches no projected transfer row at all",
                    retire_when: "the foreach-state demand no longer pinned Subtree AND a \
                                  projected transfer row for a foreach source",
                }),
            },
        ],
        inputs: &[
            r#"[{"name":"a"},{"name":"b"}]"#,
            "[]",
            r#"[{"name":null}]"#,
            "[{}]",
            // Error classes: iterating a non-iterable, and a field step on a
            // number element. Both publish nothing and abort, on every spelling.
            "null",
            "[1]",
        ],
        non_members: &[],
        // Every input in this class pins the whole-document floor. `null` is
        // not a countable array, and `[1]` holds an element category the count
        // equivalence does not cover (the route abandons before publishing a
        // byte). Both fall to the floor with the rest of the class.
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "element-count",
        spellings: &[
            Spelling {
                program: "[.[]] | length",
                allowlist: None,
            },
            // The constant-collect-body row, pinned here rather than only in the
            // engine because a class is what forces the two spellings to keep
            // agreeing. `map(1)` reads no part of an element,
            // so unlike `collect-count`'s `map(.name)` it counts every element
            // CATEGORY — which is exactly why it belongs to this class and not
            // to that one, and why it needs no exemption on any probe input.
            Spelling {
                program: "map(1) | length",
                allowlist: None,
            },
            Spelling {
                program: "reduce .[] as $x (0; . + 1)",
                // The gate caught a route cliff here when the collected
                // spelling took the element-stream rung and this fold fell to
                // the floor; the RouteOnly waiver it carried was retired when
                // that rung was deleted with its result kind, because both
                // spellings now fall to the whole-document floor on every
                // probe input and the routes agree outright.
                allowlist: None,
            },
            // The commutative mirror `1 + .`, admitted to the count row on the
            // same soundness as its twin (exact integer addition is
            // commutative). Like the twin it routes identically on every probe
            // input — both fall to the whole-document floor now that the
            // element-stream rung is gone — and the gate fails if either ever
            // diverges.
            Spelling {
                program: "reduce .[] as $x (0; 1 + .)",
                allowlist: None,
            },
        ],
        inputs: &["[1,2,3]", "[]", r#"{"a":1,"b":2}"#, "null", "\"abc\""],
        // the rule: membership is pinned to length-on-CONSTRUCTED spellings.
        // Bare `length` counts the INPUT, so it answers where the constructed
        // spellings raise — `null` (0 vs "Cannot iterate over null") and
        // `"abc"` (3 vs "Cannot iterate over string").
        non_members: &[(
            "length",
            "counts the input rather than a constructed container: 0 on null and 3 on \"abc\" \
             where every class member raises",
        )],
        // The container-count contract is "array elements or object members",
        // so `[.[]] | length` over `{"a":1,"b":2}` is a member count. `null`
        // and the string input land on the whole-document floor like every
        // other input in this class. The `reduce` spellings take the floor
        // everywhere.
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "construct-count",
        // With every member key a static literal, the count of a constructed
        // object is the element count. The class forces the three spellings to
        // agree on bytes, classification, and rung on every probe input.
        spellings: &[
            // The count-row spelling.
            Spelling {
                program: "[.[] | {x: .id}] | length",
                allowlist: None,
            },
            // `map(f)` lowers to exactly `[.[] | f]`, so this is the same plan.
            Spelling {
                program: "map({x: .id}) | length",
                allowlist: None,
            },
            // A second member sharing the SAME member path is still one probe
            // path and one object per element — still the element count.
            Spelling {
                program: "[.[] | {x: .id, y: .id}] | length",
                allowlist: None,
            },
        ],
        inputs: &[
            r#"[{"id":1},{"id":2}]"#,
            "[]",
            r#"[{"id":null}]"#,
            "[{}]",
            // Error class: a number element fails `.id`, and the count row's
            // probe downgrades the drive to the whole-document floor, where
            // the raise reproduces byte-for-byte.
            "[1]",
        ],
        non_members: &[
            // A dynamic key can change the member count per element: `{(.k):
            // .v}` with a null `k` produces nothing for that element, so the
            // length is not the element count.
            (
                "[.[] | {(.k): .v}] | length",
                "a dynamic key can emit zero keys per element, so the count of the \
                 constructed objects is not the element count",
            ),
        ],
        // The `[1]` input's number element violates the `id` probe, so the
        // count drive downgrades to the whole-document floor and raises
        // exactly as the floor does.
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "fan-out",
        // Safe by construction the other way round: grouping is path-normal, so
        // fusion produces the same step list from both spellings.
        spellings: &[
            Spelling {
                program: ".catalog[].name",
                allowlist: None,
            },
            Spelling {
                program: "(.catalog[].name)",
                allowlist: None,
            },
            // Fusion is path-normal, so `.catalog[] | .name` is the same stage
            // list and must take the same rung.
            Spelling {
                program: ".catalog[] | .name",
                allowlist: None,
            },
        ],
        inputs: &[
            r#"{"catalog":[{"name":"a"},{"name":"b"}],"meta":1}"#,
            r#"{"catalog":[]}"#,
            r#"{"catalog":[{"name":null}]}"#,
            "{}",
        ],
        non_members: &[],
        // the class is an ELEMENT row, so it now takes the
        // LAZY WHOLE-DOCUMENT route with the element demand hint (the codec's
        // span skeleton survives for the document-core consumer to iterate
        // it) on every input — the `{}` input included (the missing container
        // declines to the whole-document floor, which raises "Cannot iterate
        // over null" identically).
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "collect-publish",
        // The publishing sibling of `collect-count`, spelled over the same
        // document ROOT so no member of the class carries a static container
        // prefix the others lack (a prefix changes the rung the decline arm
        // falls to, which is a route difference about pushdown rather than
        // about projection).
        spellings: &[
            Spelling {
                program: "[.[] | .name]",
                allowlist: None,
            },
            Spelling {
                program: "[.[].name]",
                allowlist: None,
            },
            // `map(f)` lowers to exactly `[.[] | f]`, so this is the same plan.
            Spelling {
                program: "map(.name)",
                allowlist: None,
            },
        ],
        inputs: &[
            r#"[{"name":"a"},{"name":"b"}]"#,
            "[]",
            r#"[{"name":null}]"#,
            // Error classes: a non-object element (the projector copies it whole,
            // so the residual raises exactly as the floor does) and a container
            // that is not an array at all.
            r#"[{"name":"a"},7]"#,
            "null",
        ],
        // The COUNTING spelling of the same collect body answers a number where
        // this class answers the array itself.
        non_members: &[(
            "[.[] | .name] | length",
            "measures the collected array instead of publishing it: it answers the element count \
             where the class answers the elements",
        )],
        // Collect-publish rows take the whole-document floor on every input:
        // the `null` input is the container-negative outcome, exactly as the
        // floor's `.[]` over `null` raises.
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "slice-count",
        // Two mechanisms that must agree: the boundary-less count row
        // (`PATH[a:b] | length`, whose value is the located range array's length)
        // and the range-boundary row (`PATH[a:b][]`, whose container path carries
        // the range). A class holds them to the same answer.
        //
        // The probe inputs are ARRAYS ONLY, deliberately and not for
        // convenience: over a string, `.[1:3] | length` is a codepoint count of
        // the cut string (2) while `[.[1:3][]] | length` raises "Cannot iterate
        // over string", and over `null` the first answers 0 while the second
        // raises. The two spellings are therefore NOT equivalent off the array
        // input class, and pinning them together there would be a false claim
        // rather than a waived obligation.
        spellings: &[
            // The reference spelling carries the element BOUNDARY, so it is the
            // one whose per-element class is defined at all.
            Spelling {
                program: "[.[1:3][]] | length",
                allowlist: None,
            },
            // The boundary-LESS row. `projection_class` is defined as the
            // per-ELEMENT demand and this program has no element, so it reports
            // the documented `Subtree` default — the classification is
            // inapplicable rather than coarser, which is exactly what
            // `Exempt::ClassOnly` says. Its ROUTE obligation stays live, and it
            // is the obligation that matters: both spellings must take the same
            // rung on every probe input, and they do.
            Spelling {
                program: ".[1:3] | length",
                allowlist: Some(Allowlisted {
                    exempt: Exempt::ClassOnly,
                    reason: "a boundary-LESS count row has no element for the per-element demand \
                             lattice to describe, so it reports the documented Subtree default; \
                             the route obligation is unwaived and holds",
                    retire_when: "the projection lattice gains a vocabulary for programs with no \
                                  element boundary — at which point this spelling must classify \
                                  with its class or the gate fails",
                }),
            },
            // `map(0)` lowers to `[.[] | 0]`, whose body is a literal-start
            // stage — which blocks fusion, so the collect body stays a `FlatMap`
            // over the boundary instead of collapsing into one stage. The
            // count table now has a row for exactly that body, and the un-ranged spelling
            // `.a | map(0) | length` takes it — but this one still declines, and
            // the blocker is the RANGE rather than the body: a literal-start
            // body also blocks the outer container path from fusing in, so
            // `.[1:3]` stays an outer static prefix, and `is_static_container_stage`
            // admits only `Key` and `Index` steps. Measured on this tree:
            // `.[1:3] | map(.name) | length` and `.[1:3] | map(.) | length`
            // decline identically, so the exemption is not about the constant at
            // all. The narrowest exemption that fits is still route-only: its
            // classification has to agree with the class.
            Spelling {
                program: ".[1:3] | map(0) | length",
                allowlist: Some(Allowlisted {
                    exempt: Exempt::RouteOnly,
                    reason: "a literal-start collect body blocks fusion, so the RANGE stays in an \
                             outer static prefix and `is_static_container_stage` admits only key \
                             and index steps; the constant body itself is now a count row, and \
                             `.[1:3] | map(.name) | length` declines the same way, which is what \
                             shows the range is the live blocker",
                    retire_when: "the outer static container prefix admits a trailing RANGE step \
                                  and lowers it into the container path",
                }),
            },
        ],
        inputs: &[
            "[1,2,3,4,5]",
            // Empty container: the range resolves to an empty region without
            // reading an element byte.
            "[]",
            r#"[{"a":1},{"a":2},{"a":3}]"#,
            // Range END past the container: the clamp is the codec's, performed
            // where the observed length lives.
            "[1,2]",
        ],
        // The publishing sibling answers the ELEMENTS where this class answers
        // their count.
        non_members: &[(
            ".[1:3]",
            "publishes the sliced array itself instead of measuring it: `[2,3]` where the class \
             answers `2`",
        )],
        // Every input in this class pins the whole-document floor.
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "container-count",
        // The CONTAINER-count row's class. It exists because the plain spelling
        // of "how many are there" — `.a | length` — fell all the way to the
        // whole-document floor while its collect twin counted without building a
        // node. That was the exact cliff this gate is for, and it cost the
        // `large_count_events` benchmark lane 1.9x.
        //
        // The probe inputs put an ARRAY at `.a`, deliberately and not for
        // convenience, for the same reason `slice-count`'s are arrays only: over
        // a string `.a | length` is a codepoint count while `[.a[]] | length`
        // raises, and over `null` the first answers 0 while the second raises.
        // The two spellings are NOT equivalent off the array class, and pinning
        // them together there would be a false claim. Declining containers are
        // pinned instead by the compat corpus's `PATH | length` rows.
        //
        // The ROOT spellings `length` and `. | length` are absent because the
        // row declines an empty path — the deferred empty-path count, argued in
        // `analysis::count::is_container_count`. They belong in this class the
        // day that item lands.
        spellings: &[
            // The reference spelling carries the element BOUNDARY, so it is the
            // one whose per-element class is defined at all.
            Spelling {
                program: "[.a[]] | length",
                allowlist: None,
            },
            Spelling {
                program: ".a | length",
                allowlist: Some(Allowlisted {
                    exempt: Exempt::ClassOnly,
                    reason: "a boundary-LESS count row has no element for the per-element demand \
                             lattice to describe, so it reports the documented Subtree default; \
                             both spellings now ride the same scoped route (the pushed-down `.a` \
                             prefix, the element-stream rung having been deleted with the \
                             element-stream result kind) — both publish the identical bytes",
                    retire_when: "the projection lattice gains a vocabulary for programs with no \
                                  element boundary — the same condition that retires \
                                  slice-count's `.[1:3] | length` entry",
                }),
            },
        ],
        inputs: &[
            r#"{"a":[1,2,3]}"#,
            // Empty container: counted without reading an element byte.
            r#"{"a":[]}"#,
            r#"{"a":[{"k":1},{"k":2}]}"#,
            // Mixed element categories: a container count restricts none of
            // them, so no run can downgrade it.
            r#"{"a":[1,"x",null,true,[],{"k":1}]}"#,
        ],
        // The nearest neighbour that is NOT this row: one index step deeper, so
        // it measures an element instead of the container it sits in.
        non_members: &[(
            ".a[0] | length",
            "measures the first ELEMENT rather than the container: 1 on {\"a\":[1,2,3]} where \
             the class answers 3",
        )],
        // both spellings are COUNT rows (the collect row's
        // Structure witness holds and the container row needs no witness), so
        // they lower the lazy whole-document requirement with the count hint —
        // the count consumer answers from the span skeleton — on every one of
        // these inputs.
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "correlated-join",
        // The correlated-join vertical's obligation: the INDEXED source
        // iteration and the naive one must publish the same bytes on every probe
        // input. The first and last spellings are recognized correlated scans
        // (the two indexed-source rows); the middle two are semantically identical and
        // each declines for a DIFFERENT documented reason, so the comparison arm
        // really does run the element-by-element scan.
        spellings: &[
            // The indexed route.
            Spelling {
                program: ".rows as $o | .keys[] | . as $u \
                          | [$o[] | select(.k == $u.k)] | length",
                allowlist: None,
            },
            // Naive arm 1: the leftmost top-level conjunct is not an equality, so
            // row F1 declines. `true and P` has P's truthiness exactly, and
            // `select` reads truthiness, so the published bytes cannot differ.
            Spelling {
                program: ".rows as $o | .keys[] | . as $u \
                          | [$o[] | select(true and .k == $u.k)] | length",
                allowlist: None,
            },
            // Naive arm 2: the key side reads a BOUND slot rather than the
            // current element, so it is not a key path and row F1 declines.
            Spelling {
                program: ".rows as $o | .keys[] | . as $u \
                          | [$o[] | . as $c | select($c.k == $u.k)] | length",
                allowlist: None,
            },
            // Row S2: the `map` spelling of the same scan, indexed through the
            // collect barrier. Its presence here is what stops the vertical from
            // teaching one spelling a fast lane its twin cannot take.
            Spelling {
                program: ".rows as $o | .keys[] | . as $u \
                          | ($o | map(select(.k == $u.k)) | length)",
                allowlist: None,
            },
        ],
        inputs: &[
            // The ordinary join: some keys match, some do not.
            r#"{"rows":[{"k":1},{"k":2},{"k":1}],"keys":[{"k":1},{"k":2},{"k":3}]}"#,
            // Hazard: DUPLICATE keys. Every match is emitted, in original order,
            // so the multimap run must not collapse to one hit.
            r#"{"rows":[{"k":7},{"k":7},{"k":7}],"keys":[{"k":7}]}"#,
            // Hazard: an EMPTY source container — the indexed route declines
            // rather than probing once where the naive form probes zero times.
            r#"{"rows":[],"keys":[{"k":1}]}"#,
            // Hazard: an empty OUTER container — the scan never runs at all.
            r#"{"rows":[{"k":1}],"keys":[]}"#,
            // Hazard: MIXED-TYPE keys. `==` is `total_cmp == Equal`, so `1` and
            // `"1"` and `true` sit in different runs; a stringifying index (the
            // `INDEX/2` idiom) would collapse them.
            r#"{"rows":[{"k":1},{"k":"1"},{"k":true},{"k":null},{"k":[1]},{"k":{"a":1}}],"keys":[{"k":1},{"k":"1"},{"k":true},{"k":null},{"k":[1]},{"k":{"a":1}}]}"#,
            // Hazard: NULL and ABSENT keys. `{}|.k` and `{"k":null}|.k` both
            // yield `null`, so both land in one run — matching what `null == $u.k`
            // does in the naive predicate.
            r#"{"rows":[{"k":null},{},{"k":1}],"keys":[{"k":null},{},{"k":1}]}"#,
            // Hazard: NaN-adjacent numeric ordering. The index sorts on
            // `total_cmp`, the same order `==` is defined by, so `-0` and `0` and
            // `1e0` and `1` land where equality says they do.
            r#"{"rows":[{"k":0},{"k":-0},{"k":1},{"k":1.0},{"k":1e0}],"keys":[{"k":0},{"k":1}]}"#,
            // Hazard: the key path RAISES on some child (a number has no `.k`),
            // so the total build declines and the naive scan raises at the same
            // child. Published bytes: none, on every spelling.
            r#"{"rows":[{"k":1},3],"keys":[{"k":1}]}"#,
            // Hazard: the PROBE raises. Same decline, same error position.
            r#"{"rows":[{"k":1}],"keys":[3]}"#,
            // Hazard: a non-iterable source. Every spelling raises identically.
            r#"{"rows":{"k":1},"keys":[{"k":1}]}"#,
        ],
        // The nearest neighbours that are NOT this computation: an inequality
        // reads a different predicate, and an anti-join inverts the answer.
        non_members: &[
            (
                ".rows as $o | .keys[] | . as $u | [$o[] | select(.k != $u.k)] | length",
                "counts the NON-matching rows: 2 where the class answers 1 on the first probe \
                 input",
            ),
            (
                ".rows as $o | .keys[] | . as $u | [$o[] | select(.k == $u.k)] | length | . > 0",
                "publishes a boolean where the class publishes the count",
            ),
        ],
        // A correlated join reads whole elements through a binder and republishes
        // a count per outer element; no projected rung covers that today, so the
        // whole class sits on the document floor. The vertical changes the
        // executor's iteration, never the route — and pinning the floor here is
        // what makes that claim a test rather than a sentence.
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "assign-setpath",
        // `a = b` IS `setpath` at every path `a` names — the assignment vertical
        // lowers it to a fold whose per-path write is the same `set_path` the
        // builtin calls. The class exists so the two can never drift: a write
        // that null-extends in one spelling and raises in the other, or a
        // container rebuild that loses member order in one, fails here.
        spellings: &[
            Spelling {
                program: "setpath([\"a\"];1)",
                allowlist: None,
            },
            Spelling {
                program: ".a = 1",
                allowlist: None,
            },
        ],
        inputs: &[
            r#"{"a":2,"b":3}"#,
            "{}",
            r#"{"a":null}"#,
            // Null-extension: the root itself is built.
            "null",
        ],
        non_members: &[
            (".b = 1", "writes a different member"),
            (".a", "reads the field instead of writing it"),
        ],
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "update-expand",
        // `a |= f` and `a = (a|f)` are the same computation ONLY for an `f` that
        // emits EXACTLY ONE output — which is why the class fixes `f` to `.+1`
        // rather than parameterizing it. For a multi-output `f` the update takes
        // the first and the assignment multiplies the whole document; for an
        // empty `f` the update DELETES and the assignment publishes nothing.
        // Those are the two hazards the vertical exists to get right, and they
        // are pinned in the corpus, not here: an equivalence class states an
        // identity, and this identity holds only on the single-output side.
        spellings: &[
            Spelling {
                program: ".a = (.a|.+1)",
                allowlist: None,
            },
            Spelling {
                program: ".a |= .+1",
                allowlist: None,
            },
        ],
        inputs: &[
            r#"{"a":2,"b":3}"#,
            // The update reads a MISSING member, so both spellings run `f` on
            // `null` — the seed law that makes them agree at all.
            "{}",
            r#"{"a":null}"#,
            "null",
        ],
        non_members: &[
            (".a |= .+2", "applies a different update"),
            (".a", "reads the field instead of updating it"),
        ],
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "sort-identity-key",
        // `sort` and `sort_by(.)` are the same ORDERING — the arity-0 form keys
        // by the element itself and the `_by` form keys by a one-element box,
        // and `[a]` against `[b]` under `total_cmp` is `a` against `b`. The
        // class exists so the arity-0 fast key law can never drift from the
        // general one.
        //
        // The probe inputs are ARRAYS ONLY, and that restriction is the class's
        // one interesting fact rather than a convenience: for a NON-array the
        // two spellings are not equivalent at all. `{"a":1} | sort` raises
        // `object ({"a":1}) cannot be sorted, as it is not an array` while
        // `{"a":1} | sort_by(.)` iterates the object, collects its keys, and
        // only then raises the doubled `object ({"a":1}) and array ([[1]])
        // cannot be sorted, as they are not both arrays`. Those texts are
        // pinned as `stderrparity` corpus rows — so a probe input of a
        // non-array here would be asserting a false identity, not catching a
        // cliff. The class is conditional by construction and says so.
        spellings: &[
            Spelling {
                program: "sort",
                allowlist: None,
            },
            Spelling {
                program: "sort_by(.)",
                allowlist: None,
            },
        ],
        inputs: &[
            "[3,1,2]",
            "[]",
            "[1,1,1]",
            // Across kinds, so the whole total order is exercised, and over
            // containers, where the comparison recurses.
            r#"[{"a":1},[1],"s",1,true,false,null]"#,
            "[[1,2],[1],[2],[]]",
        ],
        non_members: &[
            ("unique", "collapses equal elements instead of keeping them"),
            ("reverse", "orders by position, not by value"),
            ("sort_by(empty)", "keys by a different filter"),
        ],
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "entries-roundtrip",
        // `to_entries | from_entries` is the IDENTITY on an object, and the two
        // halves are written independently — one walks members into `{"key":…,
        // "value":…}` pairs, the other walks a `//` key chain and a `has()`
        // value chain back. The class is what stops the pair from drifting: a
        // rebuild that sorted keys, dropped a null value, or lost first-
        // occurrence order on a duplicate would fail here against plain `.`.
        //
        // Object inputs only. On an ARRAY `to_entries` produces NUMERIC keys and
        // `from_entries` then raises `Cannot use number (0) as object key`, so
        // the round trip is not the identity there — which is itself a corpus
        // row rather than a class member.
        spellings: &[
            Spelling {
                program: ".",
                allowlist: None,
            },
            Spelling {
                program: "to_entries | from_entries",
                allowlist: None,
            },
        ],
        inputs: &[
            r#"{"b":1,"a":2}"#,
            "{}",
            r#"{"a":null}"#,
            r#"{"a":{"b":[1,2]},"c":false}"#,
            r#"{"":0}"#,
        ],
        non_members: &[
            ("to_entries", "stops half way and publishes the pair array"),
            ("keys_unsorted", "publishes the names without the values"),
            ("with_entries(.value = 1)", "rebuilds with a different value"),
        ],
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "index-fold",
        // `INDEX(f)` is the re-keying fold. The surface reaches it through a
        // rebound key (`$k` instead of `.[f]`) because a dynamic index by an
        // arbitrary expression is outside the language. The class is what makes
        // the rebinding a spelling change rather than a semantic one: the hand
        // fold below indexes by `tostring` of the key, and any divergence the
        // rebinding introduced would show up here first.
        spellings: &[
            Spelling {
                program: "reduce .[] as $r ({}; ($r | .id | tostring) as $k | .[$k] = $r)",
                allowlist: None,
            },
            Spelling {
                program: "INDEX(.id)",
                allowlist: None,
            },
            Spelling {
                program: "INDEX(.[]; .id)",
                allowlist: None,
            },
        ],
        inputs: &[
            r#"[{"id":"a","v":1},{"id":"b","v":2}]"#,
            "[]",
            // A duplicate key: last row wins, at the FIRST occurrence's slot.
            r#"[{"id":"a","v":1},{"id":"a","v":2}]"#,
            // A numeric id, which `tostring` renders rather than rejecting.
            r#"[{"id":1},{"id":2}]"#,
            // An OBJECT input: `.[]` iterates its values just as happily.
            r#"{"x":{"id":"a"}}"#,
        ],
        non_members: &[
            ("INDEX(.v)", "keys by a different member"),
            ("map(.id)", "publishes the keys instead of the index"),
            (
                "reduce .[] as $r ({}; ($r | .id | tostring) as $k | .[$k] = $k)",
                "files the key instead of the row",
            ),
        ],
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "group-count",
        // Counting per key is reachable two ways — through `group_by`, which
        // sorts and partitions, and through the INDEX-shaped fold, which hashes
        // into an object. They are the same computation only when the counting
        // is written to match, which is exactly the point: this class pins the
        // stage-1 ordering vocabulary and the stage-3 index vocabulary against
        // each other rather than each against itself.
        spellings: &[
            Spelling {
                program: "[reduce .[] as $r ({}; ($r | .k | tostring) as $c | .[$c] += 1)                           | to_entries[] | {key: .key, count: .value}] | sort_by(.key)",
                allowlist: None,
            },
            Spelling {
                program: "group_by(.k) | map({key: (.[0] | .k), count: length})",
                allowlist: Some(Allowlisted {
                    exempt: Exempt::ClassAndRoute,
                    reason: "`group_by` declares `Subtree` and the declaration is honest — its key \
                             filter may navigate arbitrarily deep and the partition republishes \
                             whole elements — so the ordering spelling classifies `Subtree` where \
                             the fold spelling reaches the member fields it actually reads. Both \
                             publish the identical bytes on every probe input; the difference is \
                             which vocabulary the demand lattice can see through, not what is \
                             computed",
                    retire_when: "the keyed ordering builtins gain a per-element transfer row \
                                  derived from their KEY FILTER rather than from the family, at \
                                  which point `group_by(.k)` must classify with its class",
                }),
            },
        ],
        inputs: &[
            r#"[{"k":"a"},{"k":"b"},{"k":"a"}]"#,
            "[]",
            r#"[{"k":"a"}]"#,
            r#"[{"k":"z"},{"k":"a"},{"k":"z"},{"k":"a"}]"#,
            r#"[{"k":""},{"k":""}]"#,
        ],
        non_members: &[
            (
                "group_by(.k) | map({key: (.[0] | .k), count: 1})",
                "counts one per group instead of per member",
            ),
            ("group_by(.k) | map(length)", "publishes bare counts without their keys"),
            ("unique_by(.k)", "collapses the groups instead of counting them"),
        ],
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "interpolation-concat",
        // `"a\(.x)b"` IS `("a" + (.x|tostring)) + "b"` — the string vertical's
        // lowering, not a resemblance. The class exists so the identity survives
        // both halves: `tostring`'s identity-on-strings law (a string hole must
        // not be requoted) and `+`'s RIGHT-outer fan-out (the first hole varies
        // fastest). A lowering that seeded the chain with `""`, stringified with
        // `tojson`, or associated to the right would break some input below.
        spellings: &[
            Spelling {
                program: r#""a\(.x)b""#,
                allowlist: None,
            },
            Spelling {
                program: r#"("a" + (.x|tostring)) + "b""#,
                allowlist: None,
            },
        ],
        inputs: &[
            r#"{"x":"mid"}"#,
            // A NUMBER hole: `tostring` renders it, retained spelling and all.
            r#"{"x":1.50}"#,
            // The kinds that render as JSON text rather than as themselves.
            r#"{"x":[1,2]}"#,
            r#"{"x":{"k":"v"}}"#,
            r#"{"x":null}"#,
            r#"{"x":true}"#,
            // An ABSENT member is `null`, so the hole renders `null`, not empty.
            "{}",
            // A hole whose text is empty, and one whose text needs escaping when
            // the RESULT is later rendered.
            r#"{"x":""}"#,
            r#"{"x":"q\"q\\"}"#,
            // A hole that RAISES: the error must arrive from the same place.
            "3",
        ],
        non_members: &[
            (
                r#""a\(.x)b" | tojson"#,
                "publishes the JSON-quoted spelling where the class publishes the string",
            ),
            (
                r#""a" + (.x|tojson) + "b""#,
                r#"requotes a string hole: "a\"mid\"b" where the class answers "amidb""#,
            ),
            (r#""b\(.x)a""#, "concatenates the same parts in the other order"),
        ],
        // The class MOVED off the document floor, and the reason is the lowering
        // this comment already describes: a `+` chain is a `Binary` spine, and
        // the spine join hoists the prefix a spine's operands share (the
        // demand-union/span-passthrough plan's M2). Both operands of `+` are
        // evaluated, so the hoist needs no separate witness, and the literal
        // halves read nothing — so what the operands share is `.x`, which is the
        // whole of what the program reads. The chain lowers to a single `.x`
        // read feeding the concatenation, the codec LOCATES that member instead
        // of materializing the document, and both spellings move together
        // because the interpolation IS the chain.
        //
        // Nothing about what is computed changed: obligations (a) and (c) hold
        // with the same published bytes and the same route on both spellings,
        // across all ten probe inputs — the scalar that RAISES included, where
        // the located read is where the raise now comes from. The rung is the
        // only thing that moved, and it moved toward the cheaper route, which is
        // exactly the direction this pin exists to make loud rather than silent.
        rungs: &[
            AccessResultKind::Located,
            AccessResultKind::Located,
            AccessResultKind::Located,
            AccessResultKind::Located,
            AccessResultKind::Located,
            AccessResultKind::Located,
            AccessResultKind::Located,
            AccessResultKind::Located,
            AccessResultKind::Located,
            AccessResultKind::Located,
        ],
    },
    EquivalenceClass {
        name: "format-sigil",
        // `@base64` IS `format("base64")` — the parser aliases the sigil, and
        // one builtin serves both. The class is what keeps that true:
        // a lowering that resolved the name at COMPILE time for the sigil and at
        // run time for the call would still agree on these inputs, but a lowering
        // that gave the sigil its own transform would not.
        spellings: &[
            Spelling {
                program: "@base64",
                allowlist: None,
            },
            Spelling {
                program: r#"format("base64")"#,
                allowlist: None,
            },
        ],
        inputs: &[
            r#""hi""#,
            // Every kind reaches the format: eight of the ten stringify first,
            // so none of these is a refusal.
            r#""""#,
            "null",
            "true",
            "0",
            "[1,2]",
            r#"{"k":"v"}"#,
            // Text whose bytes are not one base64 group, and text whose bytes
            // are multi-byte UTF-8.
            r#""abcd""#,
            r#""é😀""#,
            // A document member rather than the whole document, so the class is
            // exercised behind a path as well.
            r#"{"x":"hi"}"#,
        ],
        non_members: &[
            (
                "@base64d",
                "decodes where the class encodes: \"hi\" answers a refusal, not \"aGk=\"",
            ),
            (
                r#"format("base64") | @base64"#,
                "encodes twice, so \"hi\" answers \"YUdrPQ==\"",
            ),
            ("@text", "publishes the stringified input itself rather than its base64"),
        ],
        // The format reads the WHOLE input (its declared transfer is `Subtree`),
        // so no projected rung applies even for the object inputs.
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        // `split(s)` IS `. / s` — one-argument `split` is the divide operator,
        // and both spellings share one cut law (`semantics::text::split`). The
        // class is what keeps the sharing
        // honest: the two spellings reach that law by different routes (a
        // builtin call with an argument filter, and a binary operator with a
        // materialized right operand), so a second cut written for either side
        // would show up here as a byte difference on one of the edge inputs
        // below rather than years later.
        name: "split-divide",
        spellings: &[
            Spelling {
                program: r#"split(",")"#,
                allowlist: None,
            },
            Spelling {
                program: r#". / ",""#,
                allowlist: None,
            },
        ],
        inputs: &[
            r#""a,b""#,
            // The three edges the cut law does NOT inherit from `str::split`:
            // an empty input is `[]` and not `[""]`, and a separator at either
            // end contributes an empty piece.
            r#""""#,
            r#"",""#,
            r#"",,""#,
            r#""a,,b""#,
            r#""abc""#,
            // Multi-byte and astral pieces, so the cut is exercised over text
            // whose codepoints are not bytes.
            r#""é,😀""#,
            r#""a,é,b""#,
        ],
        non_members: &[
            (
                r#"split("")"#,
                "the empty separator cuts into codepoints, so \"a,b\" answers [\"a\",\",\",\"b\"]",
            ),
            (
                r#"split(",") | length"#,
                "publishes the piece COUNT rather than the pieces",
            ),
            (
                r#"ltrimstr(",")"#,
                "takes one leading occurrence off a string rather than cutting, so \",\" answers \"\" and not [\"\",\"\"]",
            ),
        ],
        // The cut reads every byte of the input (`split/1` declares `Subtree`
        // and `/` materializes both operands at the op barrier), so no
        // projected rung applies.
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
    EquivalenceClass {
        name: "collect-filter-count",
        // The collect-filter row's class: the direct spelling is recognized by
        // the filter-row recognizer (its closed single-output predicate
        // vocabulary carries its own soundness), while `map(f)` lowers to
        // `[.[] | f]` over the piped container, whose CollectArray upstream
        // shape the recognizer does not admit — so the fast filter route
        // fires for one spelling only. The published bytes stay identical
        // because every declining shape falls to the floor, which answers
        // with the same arithmetic; Exempt::RouteOnly keeps obligation (b)
        // live on exactly that pair.
        spellings: &[
            Spelling {
                program: "[.catalog[] | select(.stock > 0)] | length",
                allowlist: None,
            },
            Spelling {
                program: ".catalog | map(select(.stock > 0)) | length",
                allowlist: Some(Allowlisted {
                    exempt: Exempt::RouteOnly,
                    reason: "the lowered map spelling's CollectArray upstream is the piped \
                             `.catalog` stage rather than the recognized container-path form, \
                             so the count-filter row admits only the direct spelling and this \
                             one falls to the whole-document floor",
                    retire_when: "the filter-row recognizer admits the piped-container \
                                  lowering of `map(select(...))`, so both spellings take the \
                                  same route",
                }),
            },
        ],
        inputs: &[
            r#"{"catalog":[{"stock":5},{"stock":-1},{"other":9},{"stock":null},null]}"#,
            r#"{"catalog":[]}"#,
            r#"{"catalog":{"a":{"stock":2},"b":{"stock":-3}}}"#,
            // Cross-band ranks: string and array members outrank the number.
            r#"{"catalog":[{"stock":"many"},{"stock":[1]}]}"#,
            // Error classes: a raising element declines the filter row and the
            // floor renders the raise, identically on both spellings.
            r#"{"catalog":[7]}"#,
            "null",
        ],
        non_members: &[(
            "[.catalog[] | .stock] | length",
            "collects the member values instead of counting the selected items",
        )],
        rungs: &[
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
            AccessResultKind::CompleteDocument,
        ],
    },
];

/// Equivalence-classification gate.
///
/// For every class, every spelling must agree on three things: (a) byte-identical
/// published output, completion, and `failure_class` over the class's probe inputs;
/// (b) an identical projection classification; and
/// (c) an identical route selection (`result` and `range_located`). A shape cliff
/// between equivalent spellings is a failing test.
///
/// The allowlist (documented in full on [`EQUIVALENCE_CLASSES`]) exempts named
/// spellings from (b) and (c) with a reason and a retirement condition. It never
/// exempts anything from (a): a byte difference is never allowlistable, because a
/// byte difference means the spellings are not equivalent at all.
///
/// Non-members are proven, not asserted: each carries a probe input on which it
/// really does publish something different from the class.
///
/// Each class also pins the rung it takes on every probe input
/// ([`EquivalenceClass::rungs`]), so (c) cannot quietly decay into floor ≡ floor.
pub(crate) fn assert_equivalence_classes(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    for class in EQUIVALENCE_CLASSES {
        assert_equivalence_class(catalog, format, dialect, class)?;
    }
    Ok(())
}

/// Proves the three obligations for ONE class, and prints its receipt line.
fn assert_equivalence_class(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    class: &EquivalenceClass,
) -> Result<(), String> {
    let reference = class
        .spellings
        .iter()
        .find(|spelling| spelling.allowlist.is_none())
        .ok_or_else(|| format!("equivalence class {} allowlists every spelling", class.name))?;
    let reference_outcomes = class_outcomes(catalog, format, dialect, reference.program, class)?;
    let reference_class = {
        let resources = resources();
        projection_class_label(&program_for(reference.program, &resources)?)
    };

    let mut allowlisted = 0_u32;
    for spelling in class.spellings {
        let outcomes = class_outcomes(catalog, format, dialect, spelling.program, class)?;
        // (a) bytes + completion + failure_class, for EVERY member including allowlisted ones.
        assert_class_bytes(class, spelling.program, &outcomes, reference, &reference_outcomes)?;
        let exempt = spelling.allowlist.as_ref().map(|entry| {
            allowlisted += 1;
            println!(
                "equivalence-allowlist: class={} spelling={:?} exempt={} reason={:?} retire_when={:?}",
                class.name,
                spelling.program,
                entry.exempt.label(),
                entry.reason,
                entry.retire_when
            );
            entry.exempt
        });
        // (b) identical projection classification.
        if !matches!(exempt, Some(Exempt::ClassAndRoute | Exempt::ClassOnly)) {
            let resources = resources();
            let actual_class = projection_class_label(&program_for(spelling.program, &resources)?);
            if actual_class != reference_class {
                return Err(format!(
                    "equivalence class {} classification cliff: {:?}={actual_class} {:?}={reference_class}",
                    class.name, spelling.program, reference.program
                ));
            }
        }
        // (c) identical route selection, per probe input.
        if !matches!(exempt, Some(Exempt::ClassAndRoute | Exempt::RouteOnly)) {
            assert_class_routes(class, spelling.program, &outcomes, reference, &reference_outcomes)?;
        }
    }

    assert_class_non_members(catalog, format, dialect, class, &reference_outcomes)?;

    // (c) only compares something where the class actually leaves the floor, so
    // each class PINS whether it does — in both directions, so a route that
    // starts or stops firing is a failing test either way.
    let rungs: Vec<AccessResultKind> = reference_outcomes.iter().map(|outcome| outcome.result).collect();
    if rungs != class.rungs {
        return Err(format!(
            "equivalence class {} pins rungs {:?} but took {rungs:?}",
            class.name, class.rungs
        ));
    }
    let non_floor = rungs
        .iter()
        .filter(|rung| **rung != AccessResultKind::CompleteDocument)
        .count();

    println!(
        "equivalence: class={} spellings={} allowlisted={allowlisted} inputs={} non_members={} non_floor_runs={non_floor} rungs={:?}",
        class.name,
        class.spellings.len(),
        class.inputs.len(),
        class.non_members.len(),
        class.rungs
    );
    Ok(())
}

/// Obligation (a): identical published bytes, completion, and `failure_class`, per probe input.
fn assert_class_bytes(
    class: &EquivalenceClass,
    program: &str,
    outcomes: &[OracleOutcome],
    reference: &Spelling,
    reference_outcomes: &[OracleOutcome],
) -> Result<(), String> {
    for (index, (outcome, expected)) in outcomes.iter().zip(reference_outcomes).enumerate() {
        if outcome.bytes != expected.bytes
            || outcome.completed != expected.completed
            || outcome.failure_class != expected.failure_class
        {
            return Err(format!(
                "equivalence class {} byte divergence on input {index} ({:?}): {program:?}=(bytes={:?}, completed={}, class={:?}) {:?}=(bytes={:?}, completed={}, class={:?})",
                class.name,
                class.inputs[index],
                String::from_utf8_lossy(&outcome.bytes),
                outcome.completed,
                outcome.failure_class,
                reference.program,
                String::from_utf8_lossy(&expected.bytes),
                expected.completed,
                expected.failure_class,
            ));
        }
    }
    Ok(())
}

/// Obligation (c): identical route selection, per probe input.
fn assert_class_routes(
    class: &EquivalenceClass,
    program: &str,
    outcomes: &[OracleOutcome],
    reference: &Spelling,
    reference_outcomes: &[OracleOutcome],
) -> Result<(), String> {
    for (index, (outcome, expected)) in outcomes.iter().zip(reference_outcomes).enumerate() {
        if outcome.result != expected.result || outcome.range_located != expected.range_located {
            return Err(format!(
                "equivalence class {} route cliff on input {index} ({:?}): {program:?}={:?}/located={} {:?}={:?}/located={}",
                class.name,
                class.inputs[index],
                outcome.result,
                outcome.range_located,
                reference.program,
                expected.result,
                expected.range_located
            ));
        }
    }
    Ok(())
}

/// Non-membership is a measured law: an excluded spelling must really publish
/// something different on at least one of the class's own probe inputs.
fn assert_class_non_members(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    class: &EquivalenceClass,
    reference_outcomes: &[OracleOutcome],
) -> Result<(), String> {
    for (program, reason) in class.non_members {
        let outcomes = class_outcomes(catalog, format, dialect, program, class)?;
        let differs = outcomes
            .iter()
            .zip(reference_outcomes)
            .any(|(outcome, expected)| outcome.bytes != expected.bytes || outcome.completed != expected.completed);
        if !differs {
            return Err(format!(
                "equivalence class {} excludes {program:?} ({reason}) but it agrees on every probe input",
                class.name
            ));
        }
    }
    Ok(())
}

/// Runs one spelling over every probe input of its class, through the CLI's own
/// route selector (`OracleRoute::Designated`), collecting bytes, completion, and
/// the route receipt.
fn class_outcomes(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    program: &str,
    class: &EquivalenceClass,
) -> Result<Vec<OracleOutcome>, String> {
    let mut outcomes = Vec::new();
    for input in class.inputs {
        outcomes.push(oracle_run_over(
            OracleRoute::Designated,
            catalog,
            format,
            dialect,
            program,
            input.as_bytes(),
        )?);
    }
    Ok(outcomes)
}
