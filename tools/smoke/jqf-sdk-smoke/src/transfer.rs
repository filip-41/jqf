//! Demand-transfer registry receipt: one row per registered builtin overload.
//!
//! The table and the engine registry must be the same set — a newly
//! registered overload fails the battery until it is declared here.
//! Probes classify through [`crate::projection::projection_class_label`].

use crate::harness::{program_for, resources};
use crate::projection::projection_class_label;
use jqf_engine::{BuiltinExecution, DemandTransfer, builtin_overloads};

/// One overload's declared demand transfer, plus probes that must classify as
/// that transfer predicts.
struct TransferDeclaration {
    name: &'static str,
    arity: u8,
    transfer: DemandTransfer,
    /// `(program, expected projection class)` probes. Every declaration carries
    /// at least one; a tag with two arms (`length`) carries one per arm.
    probes: &'static [(&'static str, &'static str)],
}

/// The seeded declarations for EVERY currently registered overload. The receipt
/// requires this table and the registry inventory to be the same set, so a newly
/// registered overload fails the battery until its transfer is declared here too.
const TRANSFER_DECLARATIONS: &[TransferDeclaration] = &[
    TransferDeclaration {
        name: "length",
        arity: 0,
        transfer: DemandTransfer::CountOfConstructedInput,
        probes: &[
            // Constructed input: a count over boundaries the constructor knows.
            ("[.catalog[] | .name] | length", "Structure"),
            // Document value: a codepoint count / numeric magnitude needs payload.
            ("[.catalog[] | length]", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "keys",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        // The PER-ELEMENT class stays conservative and always will: an element's
        // keys are that element's own payload, so a projected route would have to
        // decode it. The whole-program demand on the value at a path is now
        // served by the lazy whole-document binding, never a stand-in.
        probes: &[
            ("[.catalog[] | keys] | length", "Subtree"),
            ("[.catalog[] | .id | keys] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "select",
        arity: 1,
        transfer: DemandTransfer::ConditionUnionPassThrough,
        probes: &[
            ("[.catalog[] | select(.id > 1)] | length", "Fields[id]"),
            ("[.catalog[] | select(.id > 1) | .name]", "Fields[id,name]"),
        ],
    },
    TransferDeclaration {
        name: "map",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        // No `Call` survives lowering, so the probe proves the LOWERED graph's
        // node arms carry the demand — which is exactly what `ViaLowering` says.
        probes: &[
            ("map(.name) | length", "Structure"),
            (".catalog | map(.name) | length", "Structure"),
        ],
    },
    TransferDeclaration {
        name: "not",
        arity: 0,
        transfer: DemandTransfer::InputPassThrough,
        probes: &[
            ("[.catalog[] | .id | not] | length", "Structure"),
            ("[.catalog[] | .id | not]", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "error",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | error] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "error",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | error(.name)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "type",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        // The same ruling as `keys`: conservative per element, and the lazy
        // whole-document binding answers the root form without materializing
        // payloads.
        probes: &[
            ("[.catalog[] | type] | length", "Subtree"),
            ("[.catalog[] | .id | type] | length", "Fields[id]"),
        ],
    },
    // `tag` shares `type`'s registry arm and therefore `type`'s conservative
    // ruling: per-element demand is `Subtree` even though the answer is the
    // node's own tag layer.
    TransferDeclaration {
        name: "tag",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | tag] | length", "Subtree"),
            ("[.catalog[] | .id | tag] | length", "Fields[id]"),
        ],
    },
    // `_negate` is unary minus's value law. Unlike `type`/`keys` it is NOT a
    // function of the shallow structure: the answer is the input's own number,
    // re-signed, and the refusal renders a bounded prefix of whatever the input
    // was — so the demand is the subtree, the same ruling the kind filters get
    // one comment below for the same reason.
    TransferDeclaration {
        name: "_negate",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | -.] | length", "Subtree"),
            ("[.catalog[] | .id | -.] | length", "Fields[id]"),
        ],
    },
    // The kind-filter family. All seven read the input's KIND but pass an
    // admitted input through WHOLE, so unlike `type`/`keys` their demand is
    // the whole subtree. The probes are
    // the same pair for each — unprojected reaches `Subtree`, and reached
    // through a projected path the demand still stops at that path.
    TransferDeclaration {
        name: "booleans",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | booleans] | length", "Subtree"),
            ("[.catalog[] | .id | booleans] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "numbers",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | numbers] | length", "Subtree"),
            ("[.catalog[] | .id | numbers] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "strings",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | strings] | length", "Subtree"),
            ("[.catalog[] | .id | strings] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "arrays",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | arrays] | length", "Subtree"),
            ("[.catalog[] | .id | arrays] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "objects",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | objects] | length", "Subtree"),
            ("[.catalog[] | .id | objects] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "iterables",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | iterables] | length", "Subtree"),
            ("[.catalog[] | .id | iterables] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "scalars",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | scalars] | length", "Subtree"),
            ("[.catalog[] | .id | scalars] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "first",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[first(.catalog[] | .name)]", "Fields[name]"),
            ("[first(.catalog[])] | length", "Structure"),
        ],
    },
    TransferDeclaration {
        name: "limit",
        arity: 2,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[limit(2; .catalog[] | .name)]", "Fields[name]"),
            ("[limit(2; .catalog[])] | length", "Structure"),
        ],
    },
    // The path family. Every one of these is `Subtree` or `ViaLowering`, and
    // the reason is the same for all of them: a path expression's demand is not
    // a function of the program text. `path(f)` re-decides at RUNTIME whether
    // each value is still the one its tracked position addresses, and
    // `getpath`/`setpath`/`delpaths` take their components from DATA. A demand
    // lattice that reads the program cannot describe a component the program
    // does not contain, so the family declares the conservative transfer and
    // says why here rather than pretending to a precision it cannot have.
    TransferDeclaration {
        name: "path",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | path(.name)]", "Subtree"),
            ("[path(.catalog[])] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "paths",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | paths] | length", "Subtree"),
            ("[.catalog[] | .id | paths] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "paths",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[.catalog[] | paths(type == \"string\")] | length", "Subtree"),
            ("[paths(true)] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "getpath",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | getpath([\"name\"])]", "Subtree"),
            ("[getpath([\"catalog\"])] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "setpath",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | setpath([\"name\"]; 1)]", "Subtree"),
            ("[setpath([\"x\"]; 1)] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "delpaths",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | delpaths([[\"name\"]])]", "Subtree"),
            ("[delpaths([[\"catalog\"]])] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "del",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[.catalog[] | del(.name)]", "Subtree"),
            ("[del(.catalog)] | length", "Subtree"),
        ],
    },
    // The ordering family. Every one of these reads ELEMENTS, not just the
    // structure around them — a comparison is defined over whole values — so the
    // whole family is `Subtree` and none of it can ever be otherwise. The second
    // probe of each row is the same ruling read from the other side: through a
    // projected path the subtree demand stops AT that path.
    TransferDeclaration {
        name: "sort",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | sort] | length", "Subtree"),
            ("[.catalog[] | .id | sort] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "sort_by",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | sort_by(.name)] | length", "Subtree"),
            ("[.catalog[] | .id | sort_by(.)] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "group_by",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | group_by(.name)] | length", "Subtree"),
            ("[.catalog[] | .id | group_by(.)] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "unique",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | unique] | length", "Subtree"),
            ("[.catalog[] | .id | unique] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "unique_by",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | unique_by(.name)] | length", "Subtree"),
            ("[.catalog[] | .id | unique_by(.)] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "min",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | min] | length", "Subtree"),
            ("[.catalog[] | .id | min] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "max",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | max] | length", "Subtree"),
            ("[.catalog[] | .id | max] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "min_by",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | min_by(.name)] | length", "Subtree"),
            ("[.catalog[] | .id | min_by(.)] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "max_by",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | max_by(.name)] | length", "Subtree"),
            ("[.catalog[] | .id | max_by(.)] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "reverse",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | reverse] | length", "Subtree"),
            ("[.catalog[] | .id | reverse] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "bsearch",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | bsearch(1)] | length", "Subtree"),
            ("[.catalog[] | .id | bsearch(1)] | length", "Fields[id]"),
        ],
    },
    // The stringifiers render the WHOLE value, so there is no shallower demand
    // they could take. `join` is the same row: it publishes text built from the
    // whole input.
    TransferDeclaration {
        name: "tostring",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | tostring] | length", "Subtree"),
            ("[.catalog[] | .id | tostring] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "tojson",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | tojson] | length", "Subtree"),
            ("[.catalog[] | .id | tojson] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "join",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        // `join` publishes text built from the WHOLE input, so its own demand is
        // the subtree. It is `tojson`'s row, not the ordering family's: an
        // upstream `.id` NARROWS the class to that field, because the subtree
        // the builtin needs is the one under the path it is composed after.
        probes: &[
            ("[.catalog[] | join(\",\")] | length", "Subtree"),
            ("[.catalog[] | .id | join(\",\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "format",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        // Also `tojson`'s row, and for the same reason: eight of the ten formats
        // stringify the whole input and the other two read every cell of it, so
        // the demand is the subtree — narrowed by an upstream path, never by the
        // format name.
        probes: &[
            ("[.catalog[] | format(\"json\")] | length", "Subtree"),
            ("[.catalog[] | .id | format(\"json\")] | length", "Fields[id]"),
        ],
    },
    // The eleven arity-0 scalar laws are `tojson`'s row eleven times over, and for one
    // reason: each answers from the WHOLE input, and an upstream path narrows the
    // class to that path's field rather than to anything the law itself knows.
    // They are listed one by one all the same — the receipt asserts set equality
    // against the registry, so a law that gained an overload without gaining a
    // declaration has to be a failure and not an omission.
    TransferDeclaration {
        name: "tonumber",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | tonumber] | length", "Subtree"),
            ("[.catalog[] | .id | tonumber] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "toboolean",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | toboolean] | length", "Subtree"),
            ("[.catalog[] | .id | toboolean] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "fromjson",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | fromjson] | length", "Subtree"),
            ("[.catalog[] | .id | fromjson] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "explode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | explode] | length", "Subtree"),
            ("[.catalog[] | .id | explode] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "implode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | implode] | length", "Subtree"),
            ("[.catalog[] | .id | implode] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "ascii_downcase",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | ascii_downcase] | length", "Subtree"),
            ("[.catalog[] | .id | ascii_downcase] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "ascii_upcase",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | ascii_upcase] | length", "Subtree"),
            ("[.catalog[] | .id | ascii_upcase] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "utf8bytelength",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | utf8bytelength] | length", "Subtree"),
            ("[.catalog[] | .id | utf8bytelength] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "trim",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | trim] | length", "Subtree"),
            ("[.catalog[] | .id | trim] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "ltrim",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | ltrim] | length", "Subtree"),
            ("[.catalog[] | .id | ltrim] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "rtrim",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | rtrim] | length", "Subtree"),
            ("[.catalog[] | .id | rtrim] | length", "Fields[id]"),
        ],
    },
    // The five argument-taking text laws declare `Subtree` for the reason the
    // arity-0 ten do, with one addition: the ARGUMENT is an ordinary filter over
    // the same input, so no claim narrower than the subtree would cover it.
    TransferDeclaration {
        name: "startswith",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | startswith(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | startswith(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "endswith",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | endswith(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | endswith(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "ltrimstr",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | ltrimstr(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | ltrimstr(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "rtrimstr",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | rtrimstr(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | rtrimstr(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "trimstr",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | trimstr(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | trimstr(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "indices",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | indices(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | indices(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "index",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        // `index/1` is now a direct evaluator (a first-match scan) rather than
        // the piped `indices | .[0]` lowering, so the probe proves the CALL
        // carries the demand — which is what `Subtree` says. A first-match
        // scan still reads the whole haystack in the worst case, so Subtree
        // is the honest class and the probes are unchanged from the lowering
        // era.
        probes: &[
            ("[.catalog[] | index(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | index(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "rindex",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | rindex(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | rindex(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "_strindices",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | _strindices(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | _strindices(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "split",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | split(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | split(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "contains",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | contains(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | contains(\"a\")] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "inside",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        // No `Call` to `inside` survives lowering — it becomes a bind around a
        // `contains` call — so the probe proves the LOWERED graph carries the
        // demand, which is what `ViaLowering` says.
        probes: &[
            ("[.catalog[] | inside(\"a\")] | length", "Subtree"),
            ("[.catalog[] | .id | inside(\"a\")] | length", "Fields[id]"),
        ],
    },
    // `keys_unsorted` joins `keys`' and `type`'s conservative class: the
    // ANSWER is the key list, which the lazy whole-document binding answers
    // without materializing member payloads.
    TransferDeclaration {
        name: "keys_unsorted",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | keys_unsorted] | length", "Subtree"),
            ("[.catalog[] | .id | keys_unsorted] | length", "Fields[id]"),
        ],
    },
    // The entry forms REBUILD the value, so they read all of it. `with_entries`
    // is a lowering over the other two.
    TransferDeclaration {
        name: "to_entries",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | to_entries] | length", "Subtree"),
            ("[.catalog[] | .id | to_entries] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "from_entries",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | from_entries] | length", "Subtree"),
            ("[.catalog[] | .id | from_entries] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "with_entries",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        // The second probe records the expansion leaking through: `.value` is
        // read from an ENTRY, not from the document, but the classifier is a
        // sound over-approximation and unions the name in anyway. Harmless — a
        // wider field set only fetches more than it needs.
        probes: &[
            ("[.catalog[] | with_entries(.value)] | length", "Subtree"),
            ("[.catalog[] | .id | with_entries(.value)] | length", "Fields[id,value]"),
        ],
    },
    // The generator family. `range` reads NOTHING of the input — its outputs are
    // numbers it invents — but its bounds are ordinary filters over that same
    // input, and `recurse`/`combinations` walk the input whole, so the family
    // declares the conservative transfer.
    //
    // `range`'s two probes record the CONSEQUENCE rather than a narrowing: a
    // bound that reads one field and a bound that reads the whole document
    // classify the same, because the declared transfer dominates whatever the
    // bound expression alone would have permitted. That is the honest reading of
    // the row, and the reason a narrower transfer for `range` is an open item
    // rather than a hidden one.
    TransferDeclaration {
        name: "range",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[range(.catalog | length)] | length", "Subtree"),
            ("[.catalog[] | range(.id)] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "range",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[range(0; .catalog | length)] | length", "Subtree"),
            ("[.catalog[] | range(0; .id)] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "range",
        arity: 3,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[range(0; .catalog | length; 1)] | length", "Subtree"),
            ("[.catalog[] | range(0; .id; 1)] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "while",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | .id | while(. > 0; . - 1)] | length", "Fields[id]"),
            ("[limit(2; while(true; .))] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "until",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | .id | until(. <= 0; . - 1)] | length", "Fields[id]"),
            ("[limit(2; until(false; .))] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "repeat",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[limit(2; .catalog[] | .id | repeat(.))] | length", "Fields[id]"),
            ("[limit(2; repeat(.))] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "recurse",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | recurse] | length", "Subtree"),
            ("[.catalog[] | .id | recurse] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "recurse",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | recurse(.[]?)] | length", "Subtree"),
            ("[.catalog[] | .id | recurse(empty)] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "recurse",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | recurse(.[]?; true)] | length", "Subtree"),
            ("[.catalog[] | .id | recurse(empty; true)] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "combinations",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | combinations?] | length", "Subtree"),
            ("[.catalog[] | .id | combinations?] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "combinations",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | combinations(2)?] | length", "Subtree"),
            ("[.catalog[] | .id | combinations(2)?] | length", "Fields[id]"),
        ],
    },
    // The counted consumers, all lowerings over the same countdown `foreach`.
    TransferDeclaration {
        name: "nth",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[.catalog | nth(0)] | length", "Subtree"),
            ("[.catalog[] | .name | nth(0)?] | length", "Fields[name]"),
        ],
    },
    TransferDeclaration {
        name: "nth",
        arity: 2,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[nth(0; .catalog[] | .name)]", "Fields[name]"),
            ("[nth(0; .catalog[])] | length", "Structure"),
        ],
    },
    TransferDeclaration {
        name: "skip",
        arity: 2,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[skip(1; .catalog[] | .name)]", "Fields[name]"),
            ("[skip(1; .catalog[])] | length", "Structure"),
        ],
    },
    TransferDeclaration {
        name: "add",
        arity: 0,
        // Native fold: the call reads the whole input payload, so Subtree.
        // Over a construction of `.id` values it needs only the ids (the
        // constructed array's elements ARE the payloads it folds); over whole
        // elements it needs the subtree.
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | .id] | add", "Fields[id]"),
            ("[.catalog[] | add]", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "add",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        probes: &[("add(.catalog[] | .id)", "Fields[id]"), ("add(.catalog[])", "Subtree")],
    },
    TransferDeclaration {
        name: "flatten",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | flatten]", "Subtree"),
            ("[.catalog[] | [.id] | flatten]", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "flatten",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | flatten(1)]", "Subtree"),
            ("[.catalog[] | [.id] | flatten(1)]", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "transpose",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | transpose]", "Subtree"),
            ("[.catalog[] | [[.id]] | transpose]", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "has",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        // Same split as `keys`: the PER-ELEMENT class stays conservative (an
        // element's own membership is that element's payload), and the root
        // form is answered by the lazy whole-document binding.
        probes: &[
            ("[.catalog[] | has(\"id\")] | length", "Subtree"),
            ("[.catalog[] | .id | has(0)] | length", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "in",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[.catalog[] | .id | in([1,2])] | length", "Fields[id]"),
            ("[.catalog[] | in({})] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "walk",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | walk(.)]", "Subtree"),
            ("[.catalog[] | .id | walk(.)]", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "map_values",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[
            ("[.catalog[] | map_values(.)]", "Subtree"),
            // The call's demand on its INPUT is Subtree (it consumes the whole
            // constructed object), but the element-level demand is what the
            // CONSTRUCTOR needs: only `.id`. The native evaluator classifies
            // through the constructor; the old Modify-lowering forced a coarse
            // Subtree here.
            ("[.catalog[] | {a: .id} | map_values(.)]", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "pick",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[.catalog[] | pick(.id)]", "Subtree"),
            ("[.catalog[] | {a: .id} | pick(.a)]", "Fields[id]"),
        ],
    },
    TransferDeclaration {
        name: "IN",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[.catalog[] | .id | IN(1, 2)] | length", "Fields[id]"),
            ("[.catalog[] | IN(1)] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "IN",
        arity: 2,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("IN(.catalog[] | .id; 1)", "Fields[id]"),
            ("IN(.catalog[]; 1)", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "INDEX",
        arity: 1,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            (".catalog | INDEX(.id) | length", "Subtree"),
            ("[.catalog[] | INDEX(.id)] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "INDEX",
        arity: 2,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("INDEX(.catalog[]; .id) | length", "Subtree"),
            ("INDEX(.catalog[] | .name; .) | length", "Fields[name]"),
        ],
    },
    TransferDeclaration {
        name: "JOIN",
        arity: 2,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            (".catalog | JOIN({}; .id) | length", "Subtree"),
            (".catalog | JOIN({}; .) | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "JOIN",
        arity: 3,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[JOIN({}; .catalog[]; .id)] | length", "Fields[id]"),
            ("[JOIN({}; .catalog[] | .name; .)] | length", "Fields[name]"),
        ],
    },
    TransferDeclaration {
        name: "JOIN",
        arity: 4,
        transfer: DemandTransfer::ViaLowering,
        probes: &[
            ("[JOIN({}; .catalog[]; .id; .[0])] | length", "Fields[id]"),
            ("[JOIN({}; .catalog[] | .name; .; .[0])] | length", "Fields[name]"),
        ],
    },
    // Math overloads: every math overload is a pure function of
    // its operand values, so `Subtree` is the honest declaration throughout
    // (the same ruling `error/0` gets one comment above). Each probe shows the
    // CALL classifying the element it is applied to as `Subtree` — none of the
    // laws can promise a shallower read. ---
    TransferDeclaration {
        name: "abs",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | abs] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fabs",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fabs] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "floor",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | floor] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "ceil",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | ceil] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "round",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | round] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "trunc",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | trunc] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "rint",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | rint] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "nearbyint",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | nearbyint] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "sqrt",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | sqrt] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "cbrt",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | cbrt] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "exp",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | exp] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "expm1",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | expm1] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "exp2",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | exp2] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "exp10",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | exp10] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "log",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | log] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "log1p",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | log1p] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "log2",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | log2] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "log10",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | log10] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "erf",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | erf] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "erfc",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | erfc] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "sin",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | sin] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "cos",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | cos] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "tan",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | tan] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "sinh",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | sinh] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "cosh",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | cosh] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "tanh",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | tanh] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "asin",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | asin] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "acos",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | acos] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "atan",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | atan] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "asinh",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | asinh] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "acosh",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | acosh] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "atanh",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | atanh] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "gamma",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | gamma] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "tgamma",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | tgamma] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "lgamma",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | lgamma] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "lgamma_r",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | lgamma_r] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "significand",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | significand] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "logb",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | logb] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "frexp",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | frexp] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "modf",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | modf] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "nan",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | nan] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "infinite",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | infinite] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "isnan",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | isnan] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "isinfinite",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | isinfinite] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "isfinite",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | isfinite] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "isnormal",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | isnormal] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "hypot",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | hypot(3;4)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "pow",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | pow(2;10)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "atan2",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | atan2(1;1)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fmod",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fmod(5.5;3)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "copysign",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | copysign(-1;2)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "remainder",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | remainder(7;3)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "drem",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | drem(3;4)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fdim",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fdim(1;2)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fmin",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fmin(1;2)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fmax",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fmax(1;2)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "ldexp",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | ldexp(1;3)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "scalbln",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | scalbln(1;3)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "scalb",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | scalb(1;3)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "nexttoward",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | nexttoward(1;2)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "nextafter",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | nextafter(1;2)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fma",
        arity: 3,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fma(3;4;5)] | length", "Subtree")],
    },
    // Date overloads: every date law reads the whole piped value
    // (a timestamp number, a parsed-datetime array, or a date string) or every
    // byte of its format argument, so `Subtree` is the honest declaration
    // throughout. `now` publishes the wall clock and reads nothing, but the
    // conservative whole-document transfer stays sound. ---
    TransferDeclaration {
        name: "now",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | now] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "gmtime",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | gmtime] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "localtime",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | localtime] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "mktime",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | mktime] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "todate",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | todate] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fromdate",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fromdate] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "todateiso8601",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | todateiso8601] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fromdateiso8601",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fromdateiso8601] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "strftime",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | strftime(\"%Y\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "strflocaltime",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | strflocaltime(\"%Y\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "strptime",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | strptime(\"%Y-%m-%dT%H:%M:%SZ\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fromrfc3339",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fromrfc3339] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "torfc3339",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | torfc3339] | length", "Subtree")],
    },
    // Regex overloads: every law reads the whole input string and
    // the whole pattern/flags arguments (and `sub`/`gsub` read every capture
    // of every match), so `Subtree` is the honest declaration throughout. ---
    TransferDeclaration {
        name: "test",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | test(\"a\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "test",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | test(\"a\"; \"i\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "match",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | match(\"a\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "match",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | match(\"a\"; \"g\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "capture",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | capture(\"(?<x>a)\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "capture",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | capture(\"(?<x>a)\"; \"\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "scan",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | scan(\"a\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "scan",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | scan(\"a\"; \"\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "splits",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | splits(\",\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "splits",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | splits(\",\"; \"\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "split",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | split(\",\"; \"g\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "sub",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | sub(\"a\"; \"X\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "sub",
        arity: 3,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | sub(\"a\"; \"X\"; \"g\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "gsub",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | gsub(\"a\"; \"X\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "gsub",
        arity: 3,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | gsub(\"a\"; \"X\"; \"\")] | length", "Subtree")],
    },
    // Misc riders: `builtins` enumerates the whole registry,
    // `have_decnum` answers the number model's own fact, and `debug` passes
    // the whole piped value through — `Subtree` is sound for all of them. ---
    TransferDeclaration {
        name: "builtins",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | builtins] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "have_decnum",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | have_decnum] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "debug",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | debug] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "debug",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | debug(\"x\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "finites",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | finites] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "normals",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | normals] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "have_literal_numbers",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | have_literal_numbers] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "env",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | env] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "get_prog_origin",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | get_prog_origin] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "get_jq_origin",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | get_jq_origin] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "get_search_list",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | get_search_list] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "stderr",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | stderr] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "halt",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | halt] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "halt_error",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | halt_error] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "halt_error",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | halt_error(3)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "j0",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | j0] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "j1",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | j1] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "jn",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | jn(1;2)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "y0",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | y0] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "y1",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | y1] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "yn",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | yn(1;2)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "tostream",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | tostream] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fromstream",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fromstream(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "truncate_stream",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | truncate_stream(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "input",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | input] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "inputs",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | inputs] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "input_filename",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | input_filename] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "input_line_number",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | input_line_number] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "modulemeta",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | modulemeta] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "union",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | union(.;.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "intersect",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | intersect(.;.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "except",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | except(.;.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "uuid",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | uuid] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "uuid_v4",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | uuid_v4] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "uuid_v7",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | uuid_v7] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "md5",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | md5] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "sha1",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | sha1] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "sha256",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | sha256] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "sha512",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | sha512] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "xxhash",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | xxhash] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "hex_encode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | hex_encode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "hex_decode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | hex_decode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "base64_encode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | base64_encode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "base64_decode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | base64_decode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "base64url_encode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | base64url_encode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "base64url_decode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | base64url_decode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "percent_encode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | percent_encode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "percent_decode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | percent_decode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "base32_encode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | base32_encode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "base32_decode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | base32_decode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "quoted_printable_encode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | quoted_printable_encode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "quoted_printable_decode",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | quoted_printable_decode] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "hmac_sha1",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | hmac_sha1(\"key\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "hmac_sha256",
        arity: 1,
        // The plain hmac_sha256 overload; its demand is the family's own —
        // the whole string input, Subtree.
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | hmac_sha256(\"key\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "hmac_sha512",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | hmac_sha512(\"key\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "hmac_sha1_base64url",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | hmac_sha1_base64url(\"key\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "hmac_sha256_base64url",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | hmac_sha256_base64url(\"key\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "hmac_sha512_base64url",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | hmac_sha512_base64url(\"key\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "blake3",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | blake3] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "crc32",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | crc32] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "ip_valid",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | ip_valid] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "ip_version",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | ip_version] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "ip_class",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | ip_class] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "ip_canonical",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | ip_canonical] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "ip_in_cidr",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | ip_in_cidr(\"10.0.0.0/8\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "gzip_compress",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | gzip_compress] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "gzip_decompress",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | gzip_decompress] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "deflate_compress",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | deflate_compress] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "deflate_decompress",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | deflate_decompress] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "zlib_compress",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | zlib_compress] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "xpath",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        // The selector seam reads the WHOLE document: the xml.xpath@1 profile
        // matches elements anywhere in the recovered tree, so no shallower
        // claim is honest.
        probes: &[
            ("[xpath(\"//item\")] | length", "Subtree"),
            ("[.catalog[] | xpath(\"//item\")] | length", "Subtree"),
        ],
    },
    TransferDeclaration {
        name: "css",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        // The html.css@1 profile is the same law over the HTML document.
        probes: &[("[css(\"div.item\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "zlib_decompress",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | zlib_decompress] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "numfmt",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | numfmt(\"%.2f\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "top_k",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | top_k(5)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "top_k",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | top_k(5; .v)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "redact",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | redact] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "redact",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | redact(\"x\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "redact",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | redact(\"x\"; \"*\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "redact_keyed",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | redact_keyed(\"k\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "edit_distance",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | edit_distance(\"x\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "similarity",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | similarity(\"x\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fuzzy_match",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fuzzy_match(\"x\"; 0.5)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "e",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | e] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "pi",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | pi] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "tau",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | tau] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "degrees",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | degrees] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "radians",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | radians] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "pow10",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | pow10] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "recip",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | recip] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "round_even",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | round_even] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "signum",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | signum] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fract",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fract] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "log",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | log(10)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "log",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | log(.;10)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "round",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | round(2)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "round",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | round(.;2)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "sum",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | sum(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "avg",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | avg(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "median",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | median(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "quantile",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | quantile(.;0.5)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "parse_url",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | parse_url] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "parse_query_string",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | parse_query_string] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "parse_logfmt",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | parse_logfmt] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "parse_syslog",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | parse_syslog] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "parse_user_agent",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | parse_user_agent] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "parse_grok",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | parse_grok(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "stddev",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | stddev(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "variance",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | variance(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "count",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | count(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "frequency",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | frequency(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "melt",
        arity: 4,
        transfer: DemandTransfer::Subtree,
        probes: &[(
            "[.catalog[] | melt([\"id\"]; [\"a\"]; \"k\"; \"v\")] | length",
            "Subtree",
        )],
    },
    TransferDeclaration {
        name: "pivot",
        arity: 4,
        transfer: DemandTransfer::Subtree,
        probes: &[(
            "[.catalog[] | pivot([\"id\"]; \"k\"; \"v\"; [\"a\"])] | length",
            "Subtree",
        )],
    },
    TransferDeclaration {
        name: "diff",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | diff(.; .)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "sample",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | sample(1)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "shuffle",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | shuffle] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "fill_forward",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | fill_forward] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "json_pointer",
        arity: 1,
        // A pointer can address any location in its source, so no shallower
        // demand is honest — the same answer getpath/setpath/delpaths give.
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | json_pointer(\"/id\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "json_pointer",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | json_pointer(.; \"/id\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "jsonpath",
        arity: 1,
        // A JSONPath can address any location in its source, so no shallower
        // demand is honest — the same answer json_pointer gives.
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | jsonpath(\"$..id\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "declare_index",
        arity: 2,
        // The user-declared reusable index builds its keyed
        // multimap over a container reached by a static path from the input's
        // ROOT — the classifier cannot see which path from the declaration's
        // filter arguments, so the whole input's demand is the honest law.
        // The declaration's OUTPUT is the input (a transparent acceleration),
        // which is exactly what the probe pins: the classification is the
        // call's demand on its input, not its output shape.
        transfer: DemandTransfer::Subtree,
        probes: &[("declare_index(.catalog; .id)", "Subtree")],
    },
    TransferDeclaration {
        name: "json_facts",
        arity: 0,
        // The facts projection rebuilds the whole input value and reads
        // every attached fact — Subtree is the only honest declaration,
        // matching the registry's seeded transfer.
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | json_facts] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "jsonpath",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | jsonpath(.; \"$..id\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "hmac",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | hmac(\"key\")] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "rand",
        arity: 0,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | rand] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "rand",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | rand(1)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "randint",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | randint(10)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "randint",
        arity: 2,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | randint(1; 10)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "choice",
        arity: 1,
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | choice(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "schema_infer",
        arity: 1,
        // The inferred schema is a function of the WHOLE piped value — every
        // kind, every container depth — so no shallower demand is honest.
        transfer: DemandTransfer::Subtree,
        probes: &[("[.catalog[] | schema_infer(.)] | length", "Subtree")],
    },
    TransferDeclaration {
        name: "schema_infer",
        arity: 2,
        // The two-argument overload: the OPTIONS argument only selects which
        // CORE keywords are emitted, so the VALUE is still read whole.
        transfer: DemandTransfer::Subtree,
        probes: &[(
            "[.catalog[] | schema_infer(.; {\"arrays\":\"length\"})] | length",
            "Subtree",
        )],
    },
    TransferDeclaration {
        name: "schema_validate",
        arity: 2,
        // Validation reads the whole value AND the whole schema document —
        // every member the schema demands, every keyword it carries.
        transfer: DemandTransfer::Subtree,
        probes: &[(
            "[.catalog[] | schema_validate(.; {\"type\":\"object\"})] | length",
            "Subtree",
        )],
    },
    TransferDeclaration {
        name: "schema_errors",
        arity: 2,
        // The ordered errors name the failing instance locations and schema
        // keyword locations, so the whole value and the whole schema are read.
        transfer: DemandTransfer::Subtree,
        probes: &[(
            "[.catalog[] | schema_errors(.; {\"type\":\"object\"})] | length",
            "Subtree",
        )],
    },
    TransferDeclaration {
        name: "schema_diff",
        arity: 2,
        // schema_diff infers a schema from the whole VALUE
        // and reads the whole SCHEMA argument — same Subtree class as the
        // rest of the schema family.
        transfer: DemandTransfer::Subtree,
        probes: &[(
            "[.catalog[] | schema_diff(.; {\"type\":\"object\"})] | length",
            "Subtree",
        )],
    },
];

/// Demand-transfer registry receipt: the demand transfers are REGISTRY LAW, and this is the
/// receipt that the registry and the classifier agree overload by overload.
///
/// Three facts, in order. (1) The declaration table and the registry inventory
/// are the SAME SET — a newly registered overload cannot slip through
/// undeclared, and a declaration cannot outlive its overload. (2) Each row's
/// expected tag equals the record's `demand_transfer` field. (3) Each row's
/// probe programs classify exactly as that tag predicts, which is what makes the
/// declaration the classifier's actual law rather than decoration: the
/// classifier reads the record, so a changed declaration moves these probes.
///
/// Field PRESENCE is not tested here and cannot be: `demand_transfer` is a
/// required record field, so an omitting record does not compile. The
/// cross-field rule (`execution == Lowering ⇔ transfer == ViaLowering`) is
/// asserted in const context by the registry's own validation.
#[allow(
    clippy::too_many_lines,
    reason = "the registry receipt is one table walked once: length, probes, transfer, lowering, coverage"
)]
pub(crate) fn assert_demand_transfer_registry() -> Result<(), String> {
    let overloads = builtin_overloads();
    if overloads.len() != TRANSFER_DECLARATIONS.len() {
        return Err(format!(
            "demand-transfer table covers {} overloads but the registry holds {}",
            TRANSFER_DECLARATIONS.len(),
            overloads.len()
        ));
    }

    let mut probes = 0_u32;
    for declaration in TRANSFER_DECLARATIONS {
        let Some(record) = overloads
            .iter()
            .find(|record| record.canonical_name == declaration.name && record.arity == declaration.arity)
        else {
            return Err(format!(
                "demand-transfer table declares {}/{} which the registry does not hold",
                declaration.name, declaration.arity
            ));
        };
        // Every declaration carries at least one behavioral probe. A
        // declaration whose transfer is never probed would rot silently (a
        // call_demand bug on an unprobed overload passes the length check
        // alone).
        if declaration.probes.is_empty() {
            return Err(format!(
                "{}/{} declares no probe at all — every declaration must carry at least one",
                declaration.name, declaration.arity
            ));
        }
        if record.demand_transfer != declaration.transfer {
            return Err(format!(
                "{}/{} declares {:?} but the receipt expects {:?}",
                declaration.name, declaration.arity, record.demand_transfer, declaration.transfer
            ));
        }
        if (record.execution == BuiltinExecution::Lowering) != (record.demand_transfer == DemandTransfer::ViaLowering) {
            return Err(format!(
                "{}/{} breaks `execution == Lowering <=> transfer == ViaLowering`: {:?} vs {:?}",
                declaration.name, declaration.arity, record.execution, record.demand_transfer
            ));
        }
        for (source, expected) in declaration.probes {
            let resources = resources();
            let program = program_for(source, &resources)?;
            let actual = projection_class_label(&program);
            if actual != *expected {
                return Err(format!(
                    "{}/{} transfer {:?}: {source:?} classifies {actual}, not {expected}",
                    declaration.name, declaration.arity, declaration.transfer
                ));
            }
            probes += 1;
        }
    }

    // Reverse coverage: the length check above is not set-equality (the
    // forward `find` passes for a duplicate (name,arity) row as long as the
    // counts balance), so every registry overload must ALSO be covered by at
    // least one declaration row.
    for overload in overloads {
        let covered = TRANSFER_DECLARATIONS
            .iter()
            .any(|declaration| declaration.name == overload.canonical_name && declaration.arity == overload.arity);
        if !covered {
            return Err(format!(
                "registry overload {}/{} is not covered by any demand-transfer declaration",
                overload.canonical_name, overload.arity
            ));
        }
    }

    println!(
        "demand-transfer: overloads={} declared={} probes={probes}",
        overloads.len(),
        TRANSFER_DECLARATIONS.len()
    );
    Ok(())
}
