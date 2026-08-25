//! The crate-private builtin dispatch table, keyed by [`BuiltinOverloadId`].
//!
//! One job: map a resolved overload id to its execution payload — which native evaluator the executor runs, or which
//! lowering the compiler expands. The public records stay pure data (the registry's negative space); this table is the
//! private half that actually runs. Each family exports an id-carrying `PAYLOADS` slice next to its `OVERLOADS`; this
//! module wraps those payloads into [`BuiltinDispatch`] and concatenates the slices in `OVERLOAD_SLICES` order, so a
//! lookup is one index read. A `const` coverage assertion zips the concatenated table with `OVERLOADS` and proves
//! pairwise id alignment AND execution-kind agreement, so there is no registered-but-payloadless, wrong-kind, or
//! misaligned-slice panic class.
//!
//! Negative space: it stores no records and resolves no names — [`super`] owns the `(name, arity)` resolution; this
//! only classifies an already-resolved id.

use super::builtins::collection::{CollectionPayload, PAYLOADS as COLLECTION_PAYLOADS};
use super::builtins::control::{ControlPayload, PAYLOADS as CONTROL_PAYLOADS};
use super::builtins::core::{CorePayload, PAYLOADS as CORE_PAYLOADS};
use super::builtins::diff::{DiffPayload, PAYLOADS as DIFF_PAYLOADS};
use super::builtins::entries::{EntriesPayload, PAYLOADS as ENTRIES_PAYLOADS};
#[cfg(feature = "ext-hash")]
use super::builtins::extension::{AnalyticsLaw, ExtPayload, ExtensionLaw, PAYLOADS as EXTENSION_PAYLOADS, RandLaw};
use super::builtins::facts::{FactsPayload, PAYLOADS as FACTS_PAYLOADS};
use super::builtins::format::{FormatPayload, PAYLOADS as FORMAT_PAYLOADS};
#[cfg(feature = "ext-fuzzy")]
use super::builtins::fuzzy::{FuzzyLaw, PAYLOADS as FUZZY_PAYLOADS};
use super::builtins::generate::{GeneratePayload, PAYLOADS as GENERATE_PAYLOADS};
use super::builtins::index::{IndexPayload, PAYLOADS as INDEX_PAYLOADS};
#[cfg(feature = "ext-jsonpath")]
use super::builtins::jsonpath::{JsonPathLaw, PAYLOADS as JSONPATH_PAYLOADS};
use super::builtins::kinds::{KindFilter, PAYLOADS as KINDS_PAYLOADS};
use super::builtins::math::{MathEvaluator, PAYLOADS as MATH_PAYLOADS};
#[cfg(feature = "ext-net")]
use super::builtins::net::{NetLaw, PAYLOADS as NET_PAYLOADS};
use super::builtins::order::{OrderPayload, PAYLOADS as ORDER_PAYLOADS, WholeForm};
use super::builtins::parse::{PAYLOADS as PARSE_PAYLOADS, ParseLaw};
use super::builtins::paths::{PAYLOADS as PATHS_PAYLOADS, PathsPayload};
use super::builtins::pointer::{PAYLOADS as POINTER_PAYLOADS, PointerLaw};
use super::builtins::process::{PAYLOADS as PROCESS_PAYLOADS, ProcessEvaluator};
#[cfg(feature = "ext-redact")]
use super::builtins::redact::{PAYLOADS as REDACT_PAYLOADS, RedactLaw};
use super::builtins::regex::{PAYLOADS as REGEX_PAYLOADS, RegexLaw};
use super::builtins::reshape::{PAYLOADS as RESHAPE_PAYLOADS, ReshapePayload};
use super::builtins::rider::{PAYLOADS as RIDER_PAYLOADS, RiderEvaluator};
#[cfg(feature = "ext-schema")]
use super::builtins::schema::{PAYLOADS as SCHEMA_PAYLOADS, SchemaLaw};
use super::builtins::search::{PAYLOADS as SEARCH_PAYLOADS, SearchPayload, TextLaw};
use super::builtins::selector::{PAYLOADS as SELECTOR_PAYLOADS, SelectorPayload};
use super::builtins::streams::{PAYLOADS as STREAMS_PAYLOADS, StreamsLaw};
use super::builtins::strings::{PAYLOADS as STRINGS_PAYLOADS, ScalarLaw};
use super::builtins::text::{PAYLOADS as TEXT_PAYLOADS, TextPayload};
use super::builtins::time::{PAYLOADS as TIME_PAYLOADS, TimeEvaluator};
use super::builtins::top_k::{PAYLOADS as TOP_K_PAYLOADS, TopKDirection};
use super::record::{BuiltinExecution, BuiltinOverloadId};
use crate::semantics::generate::Generator;
use crate::semantics::keyed::KeyMode;

/// How one resolved overload runs, with its execution payload.
///
/// `Copy` because the const payload tables fill their arrays by overwrite-in-place (const context cannot drop), and
/// because a dispatch value is a fieldless handle either way.
#[derive(Clone, Copy, Debug)]
pub enum BuiltinDispatch {
    /// A native evaluator the executor dispatches by kind.
    Evaluator(Evaluator),
    /// A lowering the compiler expands into other program nodes.
    Lowering(Lowering),
}

/// The native evaluators the executor drives over a `Call` node.
#[derive(Clone, Copy, Debug)]
pub enum Evaluator {
    /// `length/0` — the sign-strip / element-count evaluator.
    Length,
    /// `keys/0` — the sorted key-array evaluator.
    Keys,
    /// `select/1` — the predicate filter evaluator (a consumer frame).
    Select,
    /// `not/0` — the input-falsiness boolean evaluator.
    Not,
    /// `type/0` — the type-name evaluator.
    Type,
    /// `tag/0` — the non-core tag accessor.
    Tag,
    /// `_negate/0` — unary minus's value law (sign flip, spelling preserved).
    Negate,
    /// `error/0` — raises the current input as the error value.
    ErrorZero,
    /// `error/1` — raises the argument filter's first output as the error value.
    ErrorOne,
    /// The kind-filter family (`objects`, `arrays`, …) — pass the input through when its type is admitted, and emit
    /// nothing otherwise.
    Kind(KindFilter),
    /// `path/1` — evaluates its argument in path mode and emits the locations.
    Path,
    /// `paths/0` — the native root-excluding path walk (the `path(..) | select(length > 0)`, evaluated without the
    /// per-path filter machinery).
    Paths,
    /// `getpath/1` — the path read, which is itself a path expression.
    GetPath,
    /// `setpath/2` — the path write.
    SetPath,
    /// `delpaths/1` — the simultaneous multi-path deletion.
    DelPaths,
    /// `xpath/1` — the `xml.xpath@1` profile through the selector seam.
    XPath,
    /// `css/1` — the `html.css@1` profile through the selector seam.
    Css,
    /// The four arity-0 ordering forms (`sort`, `unique`, `min`, `max`) — the element IS the key, and the rejection
    /// class is the arity-0 one.
    Whole(WholeForm),
    /// The five `_by` ordering forms — a keyed consumer frame, one key filter run per element.
    Keyed(KeyMode),
    /// `reverse/0` — the `length`-and-index reversal.
    Reverse,
    /// `bsearch/1` — the sorted-array binary search, one answer per argument output.
    BSearch,
    /// `tostring/0` — a string unchanged, anything else as compact JSON.
    ToString,
    /// `tojson/0` — compact JSON, always.
    ToJson,
    /// `join/1` — the separator-interleaved concatenation, one answer per separator the argument yields.
    Join,
    /// `keys_unsorted/0` — the insertion-order key array.
    KeysUnsorted,
    /// `to_entries/0` — the `{key,value}` array.
    ToEntries,
    /// `from_entries/0` — the `{key,value}` array's inverse.
    FromEntries,
    /// `add/0` — the `+` fold over the input's children (the `reduce .[] as $x (null; . + $x)`, folded natively).
    /// `add/1` keeps its lowering: its fold SOURCE is an arbitrary filter, which only the machine can drive.
    Add,
    /// `flatten/0` and `flatten/1` — the nested-array splice. The arity is read off the call's argument list: an
    /// absent depth is `1e9`, which is "no limit" rather than a different function.
    Flatten,
    /// `transpose/0` — the null-padded row/column pivot.
    Transpose,
    /// `has/1` — the shallow key-presence test, one answer per argument output.
    Has,
    /// `walk/1` — the bottom-up rebuild (a frame drive, not a pure function).
    Walk,
    /// `map_values/1` — the per-member rebuild (`.[] |= f`, one level), a frame drive like `walk`'s object arm but
    /// without the descent.
    MapValues,
    /// `format/1` — one of the ten `@name` transforms, chosen by the argument's text at RUN time, one answer per name
    /// the argument yields.
    Format,
    /// The ten arity-0 scalar laws, one payload per row of [`ScalarLaw`]. They share one variant because they share one
    /// shape — each is a pure function of the whole input — so ten variants would carry no information the payload
    /// does not.
    Scalar(ScalarLaw),
    /// The five argument-taking text laws, one payload per row of [`TextLaw`].
    /// Grouped for `Scalar`'s reason, with the argument drive instead of the owned-law one: each answers once per
    /// argument output.
    Text(TextLaw),
    /// The six value SOURCES, one payload per row of [`crate::semantics::generate::GENERATORS`]. Arity is read off the
    /// call's argument list rather than split into separate payloads, because the arities of one generator differ only
    /// in which argument they supply:
    /// `range/1` defaults `from`, `recurse/0` defaults the child filter to `.[]?`, and `combinations/0` takes the input
    /// as its dimension vector.
    Generate(Generator),
    /// The math evaluator family — one payload per row of [`super::builtins::math::OVERLOADS`], with its law
    /// discriminant carried in the variant.
    Math(MathEvaluator),
    /// The date/time evaluator family — one payload per row of [`super::builtins::time::OVERLOADS`].
    Time(TimeEvaluator),
    /// The regex evaluator family — one payload per row of [`super::builtins::regex::OVERLOADS`].
    Regex(RegexLaw),
    /// The misc-rider evaluator family — one payload per row of [`super::builtins::rider::OVERLOADS`].
    Rider(RiderEvaluator),
    /// The host-state/process evaluator family — one payload per row of [`super::builtins::process::OVERLOADS`].
    Process(ProcessEvaluator),
    /// The streaming-utility evaluator family — one payload per row of [`super::builtins::streams::OVERLOADS`].
    Streams(StreamsLaw),
    /// The jqf extension families — one payload per [`super::builtins::extension::OVERLOADS`] row.
    #[cfg(feature = "ext-hash")]
    Extension(ExtensionLaw),
    /// The jqf PARSERS family: string-to-object laws over the piped value.
    Parser(ParseLaw),
    /// The jqf JSON-Pointer family: navigate an RFC 6901 pointer over the piped value (`json_pointer/1`) or over each
    /// value of a source filter (`json_pointer/2`).
    Pointer(PointerLaw),
    /// The jqf `JSONPath` family: evaluate an RFC 9535 query over the piped value (`jsonpath/1`) or over each value of
    /// a source filter (`jsonpath/2`), emitting one nodelist array per query per source.
    #[cfg(feature = "ext-jsonpath")]
    JsonPath(JsonPathLaw),
    /// The jqf schema family: infer a JSON Schema 2020-12 document (`schema_infer/1`) or validate a value against one
    /// (`schema_validate/2`, `schema_errors/2`).
    #[cfg(feature = "ext-schema")]
    Schema(SchemaLaw),
    /// The jqf DIFF verb: the path-keyed semantic comparison of two argument values, emitting one ordered record array.
    Diff,
    /// The jqf ANALYTICS family: sampling, shuffling, and gap-filling over the piped array (sample/shuffle are impure
    /// effects).
    #[cfg(feature = "ext-hash")]
    Analytics(AnalyticsLaw),
    /// The jqf RAND family: uniform floats, bounded integers, and uniform element choice (the unseeded forms are impure
    /// effects; `rand(seed)` is the deterministic seeded exception).
    #[cfg(feature = "ext-hash")]
    Rand(RandLaw),
    /// The jqf IP/CIDR family — one payload per row of [`super::builtins::net::OVERLOADS`], with its law discriminant
    /// carried in the variant.
    #[cfg(feature = "ext-net")]
    Net(NetLaw),
    /// The jqf TOP-K partial sort — `top_k(n; by?)` with O(n log k) heap.
    TopK(TopKDirection),
    /// The jqf user-declared reusable index (`declare_index/2`): a TRANSPARENT acceleration declaration that builds a
    /// machine-local sorted keyed index over a located container and passes the input through unchanged.
    IndexDeclare,
    /// `json_facts/0` — the attached-facts JSON projection.
    JsonFacts,
}

/// The lowerings the compiler expands at lower time.
///
/// `Copy` for the same table-fill reason as [`BuiltinDispatch`]; both enums are fieldless, so the derives change no
/// contract.
#[derive(Clone, Copy, Debug)]
pub enum Lowering {
    /// `map/1` — expands to `[.[] | f]`.
    Map,
    /// `first/1` — expands to `label $out | g | ., break $out`.
    First,
    /// `limit/2` — expands to the bounded `foreach` the reference defines it as.
    Limit,
    /// `paths/1` — expands to `path(.. | select(f)) | select(length > 0)`.
    PathsFiltered,
    /// `del/1` — expands to `delpaths([path(f)])`.
    Del,
    /// `with_entries/1` — expands to `to_entries | map(f) | from_entries`.
    WithEntries,
    /// `nth/1` — expands to `.[$n]`.
    NthIndex,
    /// `nth/2` — expands to `first(skip($n; g))`.
    Nth,
    /// `skip/2` — expands to the reference's countdown `foreach`.
    Skip,
    /// `add/0` and `add/1` — expand to `reduce <source> as $x (null; . + $x)`.
    Add,
    /// `in/1` — expands to `. as $x | xs | has($x)`.
    In,
    /// `pick/1` — expands to the `path`/`getpath`/`setpath` skeleton rebuild.
    Pick,
    /// `IN/1` and `IN/2` — expand to `any(s == <subject>; .)`.
    InStream,
    /// `INDEX/1` and `INDEX/2` — expand to the re-keying fold, with the key REBOUND to a variable slot.
    Index,
    /// `JOIN/2`, `JOIN/3` and `JOIN/4` — expand to the index left join, with the lookup key REBOUND to a variable
    /// slot.
    JoinIndexed,
    /// `inside/1` — expands to `. as $x | xs | contains($x)`.
    Inside,
}

/// Wraps one of core's crate-private payloads into the engine-facing dispatch value. One arm per payload VARIANT —
/// the per-overload mapping lives in the family's own `PAYLOADS` slice, not here.
const fn wrap_core(payload: CorePayload) -> BuiltinDispatch {
    match payload {
        CorePayload::Length => BuiltinDispatch::Evaluator(Evaluator::Length),
        CorePayload::Keys => BuiltinDispatch::Evaluator(Evaluator::Keys),
        CorePayload::Type => BuiltinDispatch::Evaluator(Evaluator::Type),
        CorePayload::Tag => BuiltinDispatch::Evaluator(Evaluator::Tag),
        CorePayload::Negate => BuiltinDispatch::Evaluator(Evaluator::Negate),
    }
}

/// Wraps one of math's law payloads; the whole family shares one evaluator variant, so this is the trivial direction.
const fn wrap_math(payload: MathEvaluator) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Math(payload))
}

const fn wrap_kinds(payload: KindFilter) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Kind(payload))
}
const fn wrap_control(payload: ControlPayload) -> BuiltinDispatch {
    match payload {
        ControlPayload::Select => BuiltinDispatch::Evaluator(Evaluator::Select),
        ControlPayload::Not => BuiltinDispatch::Evaluator(Evaluator::Not),
        ControlPayload::ErrorZero => BuiltinDispatch::Evaluator(Evaluator::ErrorZero),
        ControlPayload::ErrorOne => BuiltinDispatch::Evaluator(Evaluator::ErrorOne),
        ControlPayload::First => BuiltinDispatch::Lowering(Lowering::First),
        ControlPayload::Limit => BuiltinDispatch::Lowering(Lowering::Limit),
    }
}
const fn wrap_collection(payload: CollectionPayload) -> BuiltinDispatch {
    match payload {
        CollectionPayload::Map => BuiltinDispatch::Lowering(Lowering::Map),
    }
}
const fn wrap_paths(payload: PathsPayload) -> BuiltinDispatch {
    match payload {
        PathsPayload::Path => BuiltinDispatch::Evaluator(Evaluator::Path),
        PathsPayload::Paths => BuiltinDispatch::Evaluator(Evaluator::Paths),
        PathsPayload::PathsFiltered => BuiltinDispatch::Lowering(Lowering::PathsFiltered),
        PathsPayload::GetPath => BuiltinDispatch::Evaluator(Evaluator::GetPath),
        PathsPayload::SetPath => BuiltinDispatch::Evaluator(Evaluator::SetPath),
        PathsPayload::DelPaths => BuiltinDispatch::Evaluator(Evaluator::DelPaths),
        PathsPayload::Del => BuiltinDispatch::Lowering(Lowering::Del),
    }
}
const fn wrap_pointer(payload: PointerLaw) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Pointer(payload))
}
#[cfg(feature = "ext-jsonpath")]
const fn wrap_jsonpath(payload: JsonPathLaw) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::JsonPath(payload))
}
const fn wrap_order(payload: OrderPayload) -> BuiltinDispatch {
    match payload {
        OrderPayload::Whole(form) => BuiltinDispatch::Evaluator(Evaluator::Whole(form)),
        OrderPayload::Keyed(mode) => BuiltinDispatch::Evaluator(Evaluator::Keyed(mode)),
        OrderPayload::Reverse => BuiltinDispatch::Evaluator(Evaluator::Reverse),
        OrderPayload::BSearch => BuiltinDispatch::Evaluator(Evaluator::BSearch),
    }
}
const fn wrap_top_k(payload: TopKDirection) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::TopK(payload))
}
const fn wrap_text(payload: TextPayload) -> BuiltinDispatch {
    match payload {
        TextPayload::ToString => BuiltinDispatch::Evaluator(Evaluator::ToString),
        TextPayload::ToJson => BuiltinDispatch::Evaluator(Evaluator::ToJson),
        TextPayload::Join => BuiltinDispatch::Evaluator(Evaluator::Join),
    }
}
const fn wrap_format(payload: FormatPayload) -> BuiltinDispatch {
    match payload {
        FormatPayload::Format => BuiltinDispatch::Evaluator(Evaluator::Format),
    }
}
const fn wrap_strings(payload: ScalarLaw) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Scalar(payload))
}
const fn wrap_search(payload: SearchPayload) -> BuiltinDispatch {
    match payload {
        SearchPayload::Text(law) => BuiltinDispatch::Evaluator(Evaluator::Text(law)),
        SearchPayload::Inside => BuiltinDispatch::Lowering(Lowering::Inside),
    }
}
#[cfg(feature = "ext-net")]
const fn wrap_net(payload: NetLaw) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Net(payload))
}
const fn wrap_time(payload: TimeEvaluator) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Time(payload))
}
const fn wrap_regex(payload: RegexLaw) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Regex(payload))
}
#[cfg(feature = "ext-redact")]
const fn wrap_redact(payload: RedactLaw) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Extension(ExtensionLaw::Redact(payload)))
}
#[cfg(feature = "ext-fuzzy")]
const fn wrap_fuzzy(payload: FuzzyLaw) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Extension(ExtensionLaw::Fuzzy(payload)))
}
const fn wrap_index(payload: IndexPayload) -> BuiltinDispatch {
    match payload {
        IndexPayload::Declare => BuiltinDispatch::Evaluator(Evaluator::IndexDeclare),
    }
}
const fn wrap_rider(payload: RiderEvaluator) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Rider(payload))
}
const fn wrap_process(payload: ProcessEvaluator) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Process(payload))
}
const fn wrap_streams(payload: StreamsLaw) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Streams(payload))
}
#[cfg(feature = "ext-hash")]
const fn wrap_extension(payload: ExtPayload) -> BuiltinDispatch {
    match payload {
        ExtPayload::Extension(law) => BuiltinDispatch::Evaluator(Evaluator::Extension(law)),
        ExtPayload::Analytics(law) => BuiltinDispatch::Evaluator(Evaluator::Analytics(law)),
        ExtPayload::Rand(law) => BuiltinDispatch::Evaluator(Evaluator::Rand(law)),
    }
}
const fn wrap_facts(payload: FactsPayload) -> BuiltinDispatch {
    match payload {
        FactsPayload::JsonFacts => BuiltinDispatch::Evaluator(Evaluator::JsonFacts),
    }
}
const fn wrap_diff(payload: DiffPayload) -> BuiltinDispatch {
    match payload {
        DiffPayload::Diff => BuiltinDispatch::Evaluator(Evaluator::Diff),
    }
}
const fn wrap_parse(payload: ParseLaw) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Parser(payload))
}
#[cfg(feature = "ext-schema")]
const fn wrap_schema(payload: SchemaLaw) -> BuiltinDispatch {
    BuiltinDispatch::Evaluator(Evaluator::Schema(payload))
}
const fn wrap_entries(payload: EntriesPayload) -> BuiltinDispatch {
    match payload {
        EntriesPayload::KeysUnsorted => BuiltinDispatch::Evaluator(Evaluator::KeysUnsorted),
        EntriesPayload::ToEntries => BuiltinDispatch::Evaluator(Evaluator::ToEntries),
        EntriesPayload::FromEntries => BuiltinDispatch::Evaluator(Evaluator::FromEntries),
        EntriesPayload::WithEntries => BuiltinDispatch::Lowering(Lowering::WithEntries),
    }
}
const fn wrap_generate(payload: GeneratePayload) -> BuiltinDispatch {
    match payload {
        GeneratePayload::Source(generator) => BuiltinDispatch::Evaluator(Evaluator::Generate(generator)),
        GeneratePayload::NthIndex => BuiltinDispatch::Lowering(Lowering::NthIndex),
        GeneratePayload::Nth => BuiltinDispatch::Lowering(Lowering::Nth),
        GeneratePayload::Skip => BuiltinDispatch::Lowering(Lowering::Skip),
    }
}
const fn wrap_reshape(payload: ReshapePayload) -> BuiltinDispatch {
    match payload {
        ReshapePayload::Add => BuiltinDispatch::Evaluator(Evaluator::Add),
        ReshapePayload::AddLowered => BuiltinDispatch::Lowering(Lowering::Add),
        ReshapePayload::Flatten => BuiltinDispatch::Evaluator(Evaluator::Flatten),
        ReshapePayload::Transpose => BuiltinDispatch::Evaluator(Evaluator::Transpose),
        ReshapePayload::Has => BuiltinDispatch::Evaluator(Evaluator::Has),
        ReshapePayload::In => BuiltinDispatch::Lowering(Lowering::In),
        ReshapePayload::Walk => BuiltinDispatch::Evaluator(Evaluator::Walk),
        ReshapePayload::MapValues => BuiltinDispatch::Evaluator(Evaluator::MapValues),
        ReshapePayload::Pick => BuiltinDispatch::Lowering(Lowering::Pick),
        ReshapePayload::InStream => BuiltinDispatch::Lowering(Lowering::InStream),
        ReshapePayload::Index => BuiltinDispatch::Lowering(Lowering::Index),
        ReshapePayload::JoinIndexed => BuiltinDispatch::Lowering(Lowering::JoinIndexed),
    }
}
const fn wrap_selector(payload: SelectorPayload) -> BuiltinDispatch {
    match payload {
        SelectorPayload::XPath => BuiltinDispatch::Evaluator(Evaluator::XPath),
        SelectorPayload::Css => BuiltinDispatch::Evaluator(Evaluator::Css),
    }
}

/// Builds one family's id-carrying wrapped table from its raw payload slice:
/// each entry keeps its overload id (the alignment proof reads it) and wraps its payload into the engine-facing
/// dispatch value.
macro_rules! wrapped_family {
    ($name:ident, $payloads:ident, $wrap:ident) => {
        #[doc = "One family's id-carrying wrapped payloads, in `OVERLOADS` order."]
        const $name: [(u16, BuiltinDispatch); $payloads.len()] = {
            let mut out = [(u16::MAX, BuiltinDispatch::Evaluator(Evaluator::Length)); $payloads.len()];
            let mut i = 0;
            while i < $payloads.len() {
                out[i] = ($payloads[i].0, $wrap($payloads[i].1));
                i += 1;
            }
            out
        };
    };
}

wrapped_family!(CORE_WRAPPED, CORE_PAYLOADS, wrap_core);
wrapped_family!(KINDS_WRAPPED, KINDS_PAYLOADS, wrap_kinds);
wrapped_family!(CONTROL_WRAPPED, CONTROL_PAYLOADS, wrap_control);
wrapped_family!(COLLECTION_WRAPPED, COLLECTION_PAYLOADS, wrap_collection);
wrapped_family!(PATHS_WRAPPED, PATHS_PAYLOADS, wrap_paths);
wrapped_family!(POINTER_WRAPPED, POINTER_PAYLOADS, wrap_pointer);
#[cfg(feature = "ext-jsonpath")]
wrapped_family!(JSONPATH_WRAPPED, JSONPATH_PAYLOADS, wrap_jsonpath);
wrapped_family!(ORDER_WRAPPED, ORDER_PAYLOADS, wrap_order);
wrapped_family!(TOP_K_WRAPPED, TOP_K_PAYLOADS, wrap_top_k);
wrapped_family!(TEXT_WRAPPED, TEXT_PAYLOADS, wrap_text);
wrapped_family!(FORMAT_WRAPPED, FORMAT_PAYLOADS, wrap_format);
wrapped_family!(STRINGS_WRAPPED, STRINGS_PAYLOADS, wrap_strings);
wrapped_family!(SEARCH_WRAPPED, SEARCH_PAYLOADS, wrap_search);
wrapped_family!(MATH_WRAPPED, MATH_PAYLOADS, wrap_math);
#[cfg(feature = "ext-net")]
wrapped_family!(NET_WRAPPED, NET_PAYLOADS, wrap_net);
wrapped_family!(TIME_WRAPPED, TIME_PAYLOADS, wrap_time);
wrapped_family!(REGEX_WRAPPED, REGEX_PAYLOADS, wrap_regex);
#[cfg(feature = "ext-redact")]
wrapped_family!(REDACT_WRAPPED, REDACT_PAYLOADS, wrap_redact);
#[cfg(feature = "ext-fuzzy")]
wrapped_family!(FUZZY_WRAPPED, FUZZY_PAYLOADS, wrap_fuzzy);
wrapped_family!(INDEX_WRAPPED, INDEX_PAYLOADS, wrap_index);
wrapped_family!(RIDER_WRAPPED, RIDER_PAYLOADS, wrap_rider);
wrapped_family!(PROCESS_WRAPPED, PROCESS_PAYLOADS, wrap_process);
wrapped_family!(STREAMS_WRAPPED, STREAMS_PAYLOADS, wrap_streams);
#[cfg(feature = "ext-hash")]
wrapped_family!(EXTENSION_WRAPPED, EXTENSION_PAYLOADS, wrap_extension);
wrapped_family!(FACTS_WRAPPED, FACTS_PAYLOADS, wrap_facts);
wrapped_family!(DIFF_WRAPPED, DIFF_PAYLOADS, wrap_diff);
wrapped_family!(PARSE_WRAPPED, PARSE_PAYLOADS, wrap_parse);
#[cfg(feature = "ext-schema")]
wrapped_family!(SCHEMA_WRAPPED, SCHEMA_PAYLOADS, wrap_schema);
wrapped_family!(ENTRIES_WRAPPED, ENTRIES_PAYLOADS, wrap_entries);
wrapped_family!(GENERATE_WRAPPED, GENERATE_PAYLOADS, wrap_generate);
wrapped_family!(RESHAPE_WRAPPED, RESHAPE_PAYLOADS, wrap_reshape);
wrapped_family!(SELECTOR_WRAPPED, SELECTOR_PAYLOADS, wrap_selector);

/// The per-family wrapped payload slices, in `OVERLOAD_SLICES` order — the same order and the same cfg gates as
/// [`super::OVERLOAD_SLICES`] itself.
///
/// The alignment proof below is what makes this list safe: if a slice sat in the wrong place (or a cfg gate drifted
/// from `OVERLOAD_SLICES`), some entry's id half would disagree with its `OVERLOADS` neighbor and the build would fail.
const WRAPPED_SLICES: &[&[(u16, BuiltinDispatch)]] = &[
    &CORE_WRAPPED,
    &KINDS_WRAPPED,
    &CONTROL_WRAPPED,
    &COLLECTION_WRAPPED,
    &PATHS_WRAPPED,
    &POINTER_WRAPPED,
    #[cfg(feature = "ext-jsonpath")]
    &JSONPATH_WRAPPED,
    &ORDER_WRAPPED,
    &TOP_K_WRAPPED,
    &TEXT_WRAPPED,
    &FORMAT_WRAPPED,
    &STRINGS_WRAPPED,
    &SEARCH_WRAPPED,
    &MATH_WRAPPED,
    #[cfg(feature = "ext-net")]
    &NET_WRAPPED,
    &TIME_WRAPPED,
    &REGEX_WRAPPED,
    #[cfg(feature = "ext-redact")]
    &REDACT_WRAPPED,
    #[cfg(feature = "ext-fuzzy")]
    &FUZZY_WRAPPED,
    &INDEX_WRAPPED,
    &RIDER_WRAPPED,
    &PROCESS_WRAPPED,
    &STREAMS_WRAPPED,
    #[cfg(feature = "ext-hash")]
    &EXTENSION_WRAPPED,
    &FACTS_WRAPPED,
    &DIFF_WRAPPED,
    &PARSE_WRAPPED,
    #[cfg(feature = "ext-schema")]
    &SCHEMA_WRAPPED,
    &ENTRIES_WRAPPED,
    &GENERATE_WRAPPED,
    &RESHAPE_WRAPPED,
    &SELECTOR_WRAPPED,
];

/// The execution payloads of every registered overload, concatenated from the per-family wrapped slices in
/// `OVERLOAD_SLICES` order — the same way `FAMILIES` / `OVERLOADS` concatenate. Position `i` belongs to
/// `OVERLOADS[i]`; the const coverage assertion below proves it pairwise.
#[allow(
    clippy::large_const_arrays,
    reason = "the payload table IS a compile-time table; a runtime build would cost the \
              same bytes with no const guarantees"
)]
const PAYLOAD_TABLE: [(u16, BuiltinDispatch); super::OVERLOADS.len()] = super::concat(
    WRAPPED_SLICES,
    (u16::MAX, BuiltinDispatch::Evaluator(Evaluator::Length)),
);

/// Classifies one resolved overload id into its execution payload.
///
/// Returns `None` for an id with no registered payload; the compiler never reaches this with such an id, because
/// resolution only yields registered overloads and the coverage assertion below proves each has a payload.
///
/// The lookup is by POSITION through the id→position index into the concatenated payload table — position-derived
/// lookup cannot misalign, because the table's alignment with `OVERLOADS` is proven pairwise at compile time.
pub const fn dispatch(id: BuiltinOverloadId) -> Option<BuiltinDispatch> {
    let Some(position) = super::id_position(id.get()) else {
        return None;
    };
    Some(PAYLOAD_TABLE[position].1)
}

/// The coverage tag of one dispatch payload, for the compile-time assertion:
/// `0` is an evaluator, `1` is a lowering.
const fn payload_tag(payload: BuiltinDispatch) -> u8 {
    match payload {
        BuiltinDispatch::Evaluator(_) => 0,
        BuiltinDispatch::Lowering(_) => 1,
    }
}

/// Compile-time coverage over the concatenated payload table, zipped with `OVERLOADS`: every entry's id half must equal
/// its `OVERLOADS` neighbor's id (the ALIGNMENT PROOF — length equality alone would not catch a swapped pair inside
/// one family), and every entry must agree with that record on execution kind, so there is no
/// registered-but-payloadless or wrong-kind panic class.
///
/// `Definition`/`Operator` overloads (none registered) carry no dispatch payload by design, so they are skipped.
const _: () = {
    let overloads = super::OVERLOADS;
    assert!(
        PAYLOAD_TABLE.len() == overloads.len(),
        "the payload table does not cover exactly the overload inventory"
    );
    let mut i = 0;
    while i < overloads.len() {
        assert!(
            PAYLOAD_TABLE[i].0 == overloads[i].id.get(),
            "a payload entry names the wrong overload id"
        );
        let tag = payload_tag(PAYLOAD_TABLE[i].1);
        match overloads[i].execution {
            BuiltinExecution::Evaluator => assert!(
                tag == 0,
                "a registered Evaluator overload has no evaluator dispatch payload"
            ),
            BuiltinExecution::Lowering => assert!(
                tag == 1,
                "a registered Lowering overload has no expansion dispatch payload"
            ),
            BuiltinExecution::Definition | BuiltinExecution::Operator => {}
        }
        i += 1;
    }
};
