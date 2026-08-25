//! The concrete implementation home for the jq standard library and jqf families, grouped by family exactly as the
//! crate layout pins.
//!
//! One job: own the per-family const record slices ([`core`], [`control`], [`collection`]) the parent registry
//! concatenates into its `FAMILIES`/ `OVERLOADS` inventories, plus the crate-private evaluator implementations for the
//! pure value builtins that live here. Each module owns exactly the families the layout assigns it: `core` owns
//! primitive value/type builtins (`length`, `keys`), `control` owns `select` and `not`, and `collection` owns `map`.
//!
//! The split starts with five families, one overload each, with the first executable examples and the first evaluator
//! payloads. The stable ids the executable plan stores are pinned in [`id`]; both the records and the crate-private
//! dispatch table key off them, so a record and its payload can never disagree about identity.
//!
//! Negative space: the public records here are pure data. Execution payloads are crate-private — the pure
//! `length`/`keys` evaluators live in [`core`], while `select`'s predicate frame lives in [`crate::exec`] (it drives a
//! subgraph, not a pure function) and `map`'s `[.[] | f]` expansion lives in [`crate::compile`] (it rewrites the arena
//! at lower time).

pub mod collection;
pub mod control;
pub mod core;
pub mod diff;
pub mod entries;
#[cfg(feature = "ext-hash")]
pub mod extension;
pub mod facts;
pub mod format;
#[cfg(feature = "ext-fuzzy")]
pub mod fuzzy;
pub mod generate;
pub mod index;
#[cfg(feature = "ext-jsonpath")]
pub mod jsonpath;
pub mod kinds;
pub mod math;
#[cfg(feature = "ext-net")]
pub mod net;
pub mod order;
pub mod parse;
pub mod paths;
pub mod pointer;
pub mod process;
#[cfg(feature = "ext-redact")]
pub mod redact;
pub mod regex;
pub mod reshape;
pub mod rider;
#[cfg(feature = "ext-schema")]
pub mod schema;
pub mod search;
pub mod selector;
pub mod streams;
pub mod strings;
pub mod text;
pub mod time;
pub mod top_k;

/// Stable overload and family ids, shared by the records and the dispatch table.
///
/// An id is fixed forever: a semantic change bumps the overload's [`super::SemanticRevision`] instead, and an
/// incompatible change takes a new id. Family ids and overload ids share one small space here only for legibility (they
/// are distinct newtypes at use; a family id and an overload id may even coincide, as
/// `SCHEMA_DIFF`/`SCHEMA_DIFF_FAMILY_ID` does).
///
/// Every constant here is named by at least one registered overload or family; there are no ahead-of-registration
/// reservations. A new builtin takes the next unused number at registration time — never a number a published plan
/// has already carried.
pub mod id {
    /// `length/0` — the sign-strip / element-count evaluator.
    pub const LENGTH: u16 = 1;
    /// `keys/0` — the sorted key-array evaluator.
    pub const KEYS: u16 = 2;
    /// `select/1` — the predicate filter evaluator.
    pub const SELECT: u16 = 3;
    /// `map/1` — the `[.[] | f]` lowering.
    pub const MAP: u16 = 4;
    /// `not/0` — the input-falsiness boolean evaluator.
    pub const NOT: u16 = 5;
    /// `error` family — the error-raising evaluators.
    pub const ERROR: u16 = 6;
    /// `error/0` — raise the current input as the error value.
    pub const ERROR_ZERO: u16 = 6;
    /// `error/1` — raise the argument filter's first output as the error value.
    pub const ERROR_ONE: u16 = 7;
    /// `first/1` — the `label $out | g | ., break $out` lowering.
    pub const FIRST: u16 = 8;
    /// `limit/2` — the bounded-`foreach` lowering.
    pub const LIMIT: u16 = 9;
    /// `type/0` — the type-name evaluator.
    pub const TYPE: u16 = 10;
    /// `booleans/0` — the kind filter admitting booleans.
    pub const BOOLEANS: u16 = 11;
    /// `numbers/0` — the kind filter admitting numbers.
    pub const NUMBERS: u16 = 12;
    /// `strings/0` — the kind filter admitting strings.
    pub const STRINGS: u16 = 13;
    /// `arrays/0` — the kind filter admitting arrays.
    pub const ARRAYS: u16 = 14;
    /// `objects/0` — the kind filter admitting objects.
    pub const OBJECTS: u16 = 15;
    /// `iterables/0` — the kind filter admitting arrays and objects.
    pub const ITERABLES: u16 = 16;
    /// `scalars/0` — the kind filter admitting everything else.
    pub const SCALARS: u16 = 17;
    /// `path` family — the path-mode evaluator.
    pub const PATH: u16 = 18;
    /// `paths` family — the two whole-document path enumerations.
    pub const PATHS: u16 = 19;
    /// `paths/0` — the `path(..) | select(length > 0)` lowering.
    pub const PATHS_ZERO: u16 = 19;
    /// `paths/1` — the value-filtered enumeration lowering.
    pub const PATHS_ONE: u16 = 20;
    /// `getpath/1` — the path READ evaluator, itself a path expression.
    pub const GETPATH: u16 = 21;
    /// `setpath/2` — the path WRITE evaluator.
    pub const SETPATH: u16 = 22;
    /// `delpaths/1` — the simultaneous multi-path deletion evaluator.
    pub const DELPATHS: u16 = 23;
    /// `del/1` — the `delpaths([path(f)])` lowering.
    pub const DEL: u16 = 24;
    /// `sort/0` — the whole-element ascending sort.
    pub const SORT: u16 = 25;
    /// `sort_by/1` — the keyed ascending sort.
    pub const SORT_BY: u16 = 26;
    /// `group_by/1` — the keyed run partition.
    pub const GROUP_BY: u16 = 27;
    /// `unique/0` — the whole-element duplicate removal.
    pub const UNIQUE: u16 = 28;
    /// `unique_by/1` — the keyed duplicate removal.
    pub const UNIQUE_BY: u16 = 29;
    /// `min/0` — the smallest element.
    pub const MIN: u16 = 30;
    /// `max/0` — the largest element.
    pub const MAX: u16 = 31;
    /// `min_by/1` — the FIRST element of the smallest-key run.
    pub const MIN_BY: u16 = 32;
    /// `max_by/1` — the LAST element of the largest-key run.
    pub const MAX_BY: u16 = 33;
    /// `reverse/0` — the `length`-and-index reversal.
    pub const REVERSE: u16 = 34;
    /// `bsearch/1` — the sorted-array binary search.
    pub const BSEARCH: u16 = 35;
    /// `tostring/0` — the string passthrough / compact-JSON rendering.
    pub const TOSTRING: u16 = 36;
    /// `tojson/0` — the unconditional compact-JSON rendering.
    pub const TOJSON: u16 = 37;
    /// `join/1` — the separator-interleaving reduce lowering.
    pub const JOIN: u16 = 38;
    /// `keys_unsorted/0` — the insertion-order key array.
    pub const KEYS_UNSORTED: u16 = 39;
    /// `to_entries/0` — the `{key,value}` array.
    pub const TO_ENTRIES: u16 = 40;
    /// `from_entries/0` — the `{key,value}` array's inverse.
    pub const FROM_ENTRIES: u16 = 41;
    /// `with_entries/1` — the `to_entries | map(f) | from_entries` lowering.
    pub const WITH_ENTRIES: u16 = 42;
    /// `range` family — the three arities of the numeric generator.
    pub const RANGE: u16 = 43;
    /// `range/1` — `range(0; upto; 1)`.
    pub const RANGE_ONE: u16 = 43;
    /// `range/2` — `range(from; upto; 1)`.
    pub const RANGE_TWO: u16 = 44;
    /// `range/3` — the full `from`/`upto`/`by` generator.
    pub const RANGE_THREE: u16 = 45;
    /// `while/2` — emit-then-step while the condition holds.
    pub const WHILE: u16 = 46;
    /// `until/2` — step until the condition holds, emitting once.
    pub const UNTIL: u16 = 47;
    /// `repeat/1` — re-apply the filter to the ORIGINAL input, forever.
    pub const REPEAT: u16 = 48;
    /// `recurse` family — the three arities of the fixpoint walk.
    pub const RECURSE: u16 = 49;
    /// `recurse/0` — `recurse(.[]?)`.
    pub const RECURSE_ZERO: u16 = 49;
    /// `recurse/1` — `def r: ., (f | r); r`.
    pub const RECURSE_ONE: u16 = 50;
    /// `recurse/2` — the same walk with a child-gating condition.
    pub const RECURSE_TWO: u16 = 51;
    /// `combinations` family — the odometer over a dimension vector.
    pub const COMBINATIONS: u16 = 52;
    /// `combinations/0` — one tuple per Cartesian combination of `.`'s members.
    pub const COMBINATIONS_ZERO: u16 = 52;
    /// `combinations/1` — the same over `n` copies of `.`.
    pub const COMBINATIONS_ONE: u16 = 53;
    /// `nth` family — the index consumer, over a value and over a generator.
    pub const NTH: u16 = 54;
    /// `nth/1` — the `.[$n]` lowering.
    pub const NTH_ONE: u16 = 54;
    /// `nth/2` — the `first(skip($n; g))` lowering.
    pub const NTH_TWO: u16 = 55;
    /// `skip/2` — the countdown-`foreach` lowering.
    pub const SKIP: u16 = 56;
    /// `add` family — the `+` fold, over a container and over a filter.
    pub const ADD: u16 = 57;
    /// `add/0` — `reduce .[] as $x (null; . + $x)`.
    pub const ADD_ZERO: u16 = 57;
    /// `add/1` — `reduce f as $x (null; . + $x)`.
    pub const ADD_ONE: u16 = 58;
    /// `flatten` family — the nested-array splice, unbounded and to a depth.
    pub const FLATTEN: u16 = 59;
    /// `flatten/0` — the unbounded splice evaluator.
    pub const FLATTEN_ZERO: u16 = 59;
    /// `flatten/1` — the depth-bounded splice evaluator, one answer per depth.
    pub const FLATTEN_ONE: u16 = 60;
    /// `transpose/0` — the null-padded row/column pivot.
    pub const TRANSPOSE: u16 = 61;
    /// `has/1` — the shallow key-presence evaluator.
    pub const HAS: u16 = 62;
    /// `in/1` — the `. as $x | xs | has($x)` lowering.
    pub const IN_KEY: u16 = 63;
    /// `walk/1` — the bottom-up rebuild evaluator (a frame drive).
    pub const WALK: u16 = 64;
    /// `map_values/1` — the `.[] |= f` lowering.
    pub const MAP_VALUES: u16 = 65;
    /// `pick/1` — the `path`/`getpath`/`setpath` skeleton-rebuild lowering.
    pub const PICK: u16 = 66;
    /// `IN` family — the short-circuiting membership test.
    pub const IN_STREAM: u16 = 67;
    /// `IN/1` — `any(s == .; .)`.
    pub const IN_STREAM_ONE: u16 = 67;
    /// `IN/2` — `any(s == src; .)`.
    pub const IN_STREAM_TWO: u16 = 68;
    /// `INDEX` family — the stream-to-object re-keying fold.
    pub const INDEX: u16 = 69;
    /// `INDEX/1` — `INDEX(.[]; idx_expr)`.
    pub const INDEX_ONE: u16 = 69;
    /// `INDEX/2` — the full `reduce stream as $row ({}; …)` fold.
    pub const INDEX_TWO: u16 = 70;
    /// `JOIN` family — the index-object left join, in three arities.
    pub const JOIN_INDEXED: u16 = 71;
    /// `JOIN/2` — `[.[] | [., $idx[idx_expr]]]`.
    pub const JOIN_TWO: u16 = 71;
    /// `JOIN/3` — `stream | [., $idx[idx_expr]]`.
    pub const JOIN_THREE: u16 = 72;
    /// `JOIN/4` — the arity-3 pair piped through a final filter.
    pub const JOIN_FOUR: u16 = 73;
    /// `format/1` — the ten `@name` formats, selected by a run-time name.
    pub const FORMAT: u16 = 74;
    /// `tonumber/0` — a number unchanged, a string through the number reader.
    pub const TONUMBER: u16 = 75;
    /// `toboolean/0` — a boolean unchanged, `"true"`/`"false"` parsed.
    pub const TOBOOLEAN: u16 = 76;
    /// `explode/0` — a string's codepoints as an array of numbers.
    pub const EXPLODE: u16 = 77;
    /// `implode/0` — `explode`'s inverse.
    pub const IMPLODE: u16 = 78;
    /// `ascii_downcase/0` — `A`-`Z` lowered.
    pub const ASCII_DOWNCASE: u16 = 79;
    /// `ascii_upcase/0` — `a`-`z` raised.
    pub const ASCII_UPCASE: u16 = 80;
    /// `utf8bytelength/0` — a string's encoded length in bytes.
    pub const UTF8BYTELENGTH: u16 = 81;
    /// `trim/0` — Unicode `White_Space` removed from both ends.
    pub const TRIM: u16 = 82;
    /// `ltrim/0` — from the front only.
    pub const LTRIM: u16 = 83;
    /// `rtrim/0` — from the back only.
    pub const RTRIM: u16 = 84;
    /// `fromjson/0` — the reference's own lenient JSON reader over the input string.
    pub const FROMJSON: u16 = 85;
    /// `startswith/1` — the prefix predicate.
    pub const STARTSWITH: u16 = 86;
    /// `endswith/1` — the suffix predicate.
    pub const ENDSWITH: u16 = 87;
    /// `ltrimstr/1` — one leading occurrence removed.
    pub const LTRIMSTR: u16 = 88;
    /// `rtrimstr/1` — one trailing occurrence removed.
    pub const RTRIMSTR: u16 = 89;
    /// `trimstr/1` — `ltrimstr` then `rtrimstr`.
    pub const TRIMSTR: u16 = 90;
    /// `indices/1` — every position of a needle, or the index fallthrough.
    pub const INDICES: u16 = 91;
    /// `index/1` — the first position. Named `INDEX_OF` because [`INDEX`] is already taken by the UPPERCASE `INDEX`
    /// builtin, which is an unrelated grouping law rather than a search.
    pub const INDEX_OF: u16 = 92;
    /// `rindex/1` — the last position.
    pub const RINDEX_OF: u16 = 93;
    /// `_strindices/1` — the string search route on its own.
    pub const STRINDICES: u16 = 94;
    /// `split/1` — the `/` operator's cut under its own name.
    pub const SPLIT: u16 = 95;
    /// `contains/1` — the containment relation.
    pub const CONTAINS: u16 = 96;
    /// `inside/1` — `contains` with the operands swapped.
    pub const INSIDE: u16 = 97;
    /// `_negate/0` — unary minus's value law, which the reference spells as a builtin of this exact name (`1 |
    /// _negate` is `-1`) and keeps out of `builtins`.
    pub const NEGATE: u16 = 98;

    // --- math ---
    pub const ABS: u16 = 99;
    pub const FABS: u16 = 100;
    pub const FLOOR: u16 = 101;
    pub const CEIL: u16 = 102;
    pub const ROUND: u16 = 103;
    pub const TRUNC: u16 = 104;
    pub const RINT: u16 = 105;
    pub const NEARBYINT: u16 = 106;
    pub const SQRT: u16 = 107;
    pub const CBRT: u16 = 108;
    pub const EXP: u16 = 109;
    pub const EXPM1: u16 = 110;
    pub const EXP2: u16 = 111;
    pub const EXP10: u16 = 112;
    pub const LOG: u16 = 113;
    pub const LOG1P: u16 = 114;
    pub const LOG2: u16 = 115;
    pub const LOG10: u16 = 116;
    pub const HYPOT: u16 = 117;
    pub const FMOD: u16 = 118;
    pub const COPYSIGN: u16 = 119;
    pub const REMAINDER: u16 = 120;
    pub const POW: u16 = 121;

    // --- trig /0 builtins ---
    pub const SIN: u16 = 122;
    pub const COS: u16 = 123;
    pub const TAN: u16 = 124;
    pub const SINH: u16 = 125;
    pub const COSH: u16 = 126;
    pub const TANH: u16 = 127;
    pub const ASIN: u16 = 128;
    pub const ACOS: u16 = 129;
    pub const ATAN: u16 = 130;
    pub const ASINH: u16 = 131;
    pub const ACOSH: u16 = 132;
    pub const ATANH: u16 = 133;

    // --- gamma /0 builtins ----
    pub const GAMMA: u16 = 134;
    pub const TGAMMA: u16 = 135;
    pub const LGAMMA: u16 = 136;

    // --- special-value /0 builtins ----
    pub const SIGNIFICAND: u16 = 137;
    pub const LOGB: u16 = 138;
    pub const FREXP: u16 = 139;
    pub const MODF: u16 = 140;

    // --- special-value /2 builtins ----
    pub const LDEXP: u16 = 141;
    pub const SCALBLN: u16 = 142;
    pub const SCALB: u16 = 143;
    pub const NEXTTOWARD: u16 = 144;
    pub const NEXTAFTER: u16 = 145;

    // --- error function /0 builtins ----
    pub const ERF: u16 = 207;
    pub const ERFC: u16 = 208;

    // --- lgamma_r /0 (the `[log, sign]` pair; lgamma/0 is the scalar log) ----
    pub const LGAMMA_R: u16 = 209;

    // --- special-value constructors and number predicates /0 ----
    pub const NAN: u16 = 210;
    pub const INFINITE: u16 = 211;
    pub const ISNAN: u16 = 212;
    pub const ISINFINITE: u16 = 213;
    pub const ISFINITE: u16 = 214;
    pub const ISNORMAL: u16 = 215;

    // --- extra /2 builtins (reference surface) ----
    pub const DREM: u16 = 146;
    pub const ATAN2: u16 = 147;
    pub const FDIM: u16 = 148;
    pub const FMIN: u16 = 149;
    pub const FMAX: u16 = 150;

    // --- fma /3 ----
    pub const FMA: u16 = 151;

    // --- unique family id space (152+) — one per math family, no overlaps with the overload ids above. The family a
    // name belongs to is the name's own, with the aliases merged where the reference merges them: `tgamma` belongs to
    // the `gamma` family (same law, two spellings). ---
    pub const FABS_FAMILY_ID: u16 = 152;
    pub const ROUND_FAMILY_ID: u16 = 153;
    pub const RINT_FAMILY_ID: u16 = 154;
    pub const NEARBYINT_FAMILY_ID: u16 = 155;
    pub const CBRT_FAMILY_ID: u16 = 156;
    pub const EXP2_FAMILY_ID: u16 = 157;
    pub const EXP10_FAMILY_ID: u16 = 158;
    pub const LOG1P_FAMILY_ID: u16 = 159;
    pub const LOG2_FAMILY_ID: u16 = 160;
    pub const LOG10_FAMILY_ID: u16 = 161;
    pub const HYPOT_FAMILY_ID: u16 = 162;
    pub const FMOD_FAMILY_ID: u16 = 163;
    pub const COPYSIGN_FAMILY_ID: u16 = 164;
    pub const REMAINDER_FAMILY_ID: u16 = 165;
    pub const POW_FAMILY_ID: u16 = 166;
    pub const SIN_FAMILY_ID: u16 = 167;
    pub const COS_FAMILY_ID: u16 = 168;
    pub const TAN_FAMILY_ID: u16 = 169;
    pub const SINH_FAMILY_ID: u16 = 170;
    pub const COSH_FAMILY_ID: u16 = 171;
    pub const TANH_FAMILY_ID: u16 = 172;
    pub const ASIN_FAMILY_ID: u16 = 173;
    pub const ACOS_FAMILY_ID: u16 = 174;
    pub const ATAN_FAMILY_ID: u16 = 175;
    pub const ASINH_FAMILY_ID: u16 = 176;
    pub const ACOSH_FAMILY_ID: u16 = 177;
    pub const ATANH_FAMILY_ID: u16 = 178;
    pub const GAMMA_FAMILY_ID: u16 = 179;
    pub const LGAMMA_FAMILY_ID: u16 = 181;
    pub const SIGNIFICAND_FAMILY_ID: u16 = 182;
    pub const LOGB_FAMILY_ID: u16 = 183;
    pub const FREXP_FAMILY_ID: u16 = 184;
    pub const MODF_FAMILY_ID: u16 = 185;
    pub const LDEXP_FAMILY_ID: u16 = 186;
    pub const SCALBLN_FAMILY_ID: u16 = 187;
    pub const SCALB_FAMILY_ID: u16 = 188;
    pub const NEXTTOWARD_FAMILY_ID: u16 = 189;
    pub const NEXTAFTER_FAMILY_ID: u16 = 190;
    pub const DREM_FAMILY_ID: u16 = 191;
    pub const ATAN2_FAMILY_ID: u16 = 192;
    pub const FDIM_FAMILY_ID: u16 = 193;
    pub const FMIN_FAMILY_ID: u16 = 194;
    pub const FMAX_FAMILY_ID: u16 = 195;
    pub const FMA_FAMILY_ID: u16 = 196;
    pub const ABS_FAMILY_ID: u16 = 200;
    pub const FLOOR_FAMILY_ID: u16 = 201;
    pub const CEIL_FAMILY_ID: u16 = 202;
    pub const TRUNC_FAMILY_ID: u16 = 203;
    pub const SQRT_FAMILY_ID: u16 = 204;
    pub const EXP_FAMILY_ID: u16 = 205;
    pub const LOG_FAMILY_ID: u16 = 206;
    pub const ERF_FAMILY_ID: u16 = 216;
    pub const ERFC_FAMILY_ID: u16 = 217;
    pub const LGAMMA_R_FAMILY_ID: u16 = 218;
    pub const NAN_FAMILY_ID: u16 = 219;
    pub const INFINITE_FAMILY_ID: u16 = 220;
    pub const ISNAN_FAMILY_ID: u16 = 221;
    pub const ISINFINITE_FAMILY_ID: u16 = 222;
    pub const ISFINITE_FAMILY_ID: u16 = 223;
    pub const ISNORMAL_FAMILY_ID: u16 = 224;
    pub const EXPM1_FAMILY_ID: u16 = 225;

    // --- date/time ---
    pub const NOW: u16 = 216;
    pub const GMTIME: u16 = 217;
    pub const LOCALTIME: u16 = 218;
    pub const MKTIME: u16 = 219;
    pub const STRFTIME: u16 = 220;
    pub const STRFLOCALTIME: u16 = 221;
    pub const STRPTIME: u16 = 222;
    pub const TODATE: u16 = 223;
    pub const FROMDATE: u16 = 224;
    pub const TODATE_ISO8601: u16 = 225;
    pub const FROMDATE_ISO8601: u16 = 226;

    pub const NOW_FAMILY_ID: u16 = 227;
    pub const GMTIME_FAMILY_ID: u16 = 228;
    pub const LOCALTIME_FAMILY_ID: u16 = 229;
    pub const MKTIME_FAMILY_ID: u16 = 230;
    pub const STRFTIME_FAMILY_ID: u16 = 231;
    pub const STRFLOCALTIME_FAMILY_ID: u16 = 232;
    pub const STRPTIME_FAMILY_ID: u16 = 233;
    pub const TODATE_FAMILY_ID: u16 = 234;
    pub const FROMDATE_FAMILY_ID: u16 = 235;
    pub const TODATE_ISO8601_FAMILY_ID: u16 = 236;
    pub const FROMDATE_ISO8601_FAMILY_ID: u16 = 237;

    // --- regex ---
    pub const TEST_1: u16 = 238;
    pub const TEST_2: u16 = 239;
    pub const MATCH_1: u16 = 240;
    pub const MATCH_2: u16 = 241;
    pub const CAPTURE_1: u16 = 242;
    pub const CAPTURE_2: u16 = 243;
    pub const SCAN_1: u16 = 244;
    pub const SCAN_2: u16 = 245;
    pub const SPLITS_1: u16 = 246;
    pub const SPLITS_2: u16 = 247;
    pub const SPLIT_2: u16 = 248;
    pub const SUB_2: u16 = 249;
    pub const SUB_3: u16 = 250;
    pub const GSUB_2: u16 = 251;
    pub const GSUB_3: u16 = 252;

    pub const TEST_FAMILY_ID: u16 = 253;
    pub const MATCH_FAMILY_ID: u16 = 254;
    pub const CAPTURE_FAMILY_ID: u16 = 255;
    pub const SCAN_FAMILY_ID: u16 = 256;
    pub const SPLITS_FAMILY_ID: u16 = 257;
    pub const SUB_FAMILY_ID: u16 = 258;
    pub const GSUB_FAMILY_ID: u16 = 259;

    // --- misc riders ---
    pub const BUILTINS: u16 = 260;
    pub const HAVE_DECNUM: u16 = 261;
    pub const DEBUG_0: u16 = 262;
    pub const DEBUG_1: u16 = 263;

    pub const BUILTINS_FAMILY_ID: u16 = 264;
    pub const HAVE_DECNUM_FAMILY_ID: u16 = 265;
    pub const DEBUG_FAMILY_ID: u16 = 266;

    // --- parity gaps: number filters, host state, and process control ---
    pub const FINITES: u16 = 267;
    pub const NORMALS: u16 = 268;
    pub const HAVE_LITERAL_NUMBERS: u16 = 269;
    pub const ENV: u16 = 270;
    pub const GET_PROG_ORIGIN: u16 = 271;
    pub const GET_JQ_ORIGIN: u16 = 272;
    pub const GET_SEARCH_LIST: u16 = 273;
    pub const STDERR: u16 = 274;
    pub const HALT: u16 = 275;
    pub const HALT_ERROR_0: u16 = 276;
    pub const HALT_ERROR_1: u16 = 277;

    // --- parity gaps: input family ---
    pub const INPUT: u16 = 278;
    pub const INPUTS: u16 = 279;
    pub const INPUT_FILENAME: u16 = 280;
    pub const INPUT_LINE_NUMBER: u16 = 281;

    // --- parity gaps: streams ---
    pub const TOSTREAM: u16 = 282;
    pub const FROMSTREAM: u16 = 283;
    pub const TRUNCATE_STREAM: u16 = 284;

    // --- parity gaps: Bessel functions (libm) ---
    pub const J0: u16 = 285;
    pub const J1: u16 = 286;
    pub const JN: u16 = 287;
    pub const Y0: u16 = 288;
    pub const Y1: u16 = 289;
    pub const YN: u16 = 290;

    pub const FINITES_FAMILY_ID: u16 = 291;
    pub const NORMALS_FAMILY_ID: u16 = 292;
    pub const HAVE_LITERAL_NUMBERS_FAMILY_ID: u16 = 293;
    pub const ENV_FAMILY_ID: u16 = 294;
    pub const GET_PROG_ORIGIN_FAMILY_ID: u16 = 295;
    pub const GET_JQ_ORIGIN_FAMILY_ID: u16 = 296;
    pub const GET_SEARCH_LIST_FAMILY_ID: u16 = 297;
    pub const STDERR_FAMILY_ID: u16 = 298;
    pub const HALT_FAMILY_ID: u16 = 299;
    pub const INPUT_FAMILY_ID: u16 = 300;
    pub const INPUTS_FAMILY_ID: u16 = 301;
    pub const INPUT_FILENAME_FAMILY_ID: u16 = 302;
    pub const INPUT_LINE_NUMBER_FAMILY_ID: u16 = 303;
    pub const TOSTREAM_FAMILY_ID: u16 = 304;
    pub const FROMSTREAM_FAMILY_ID: u16 = 305;
    pub const TRUNCATE_STREAM_FAMILY_ID: u16 = 306;
    pub const J0_FAMILY_ID: u16 = 307;
    pub const J1_FAMILY_ID: u16 = 308;
    pub const JN_FAMILY_ID: u16 = 309;
    pub const Y0_FAMILY_ID: u16 = 310;
    pub const Y1_FAMILY_ID: u16 = 311;
    pub const YN_FAMILY_ID: u16 = 312;
    pub const MODULEMETA: u16 = 313;
    pub const MODULEMETA_FAMILY_ID: u16 = 314;

    // --- jqf extension families. Overload ids 315+; family ids 500+. Every extension is a `jqf-extension` category
    // family whose name must never collide with a reference builtin — the collision guard test pins the boundary. ---
    pub const UNION: u16 = 315;
    pub const INTERSECT: u16 = 316;
    pub const EXCEPT: u16 = 317;
    pub const UUID: u16 = 318;
    pub const UUID_V4: u16 = 319;
    pub const UUID_V7: u16 = 320;
    pub const MD5: u16 = 321;
    pub const SHA1: u16 = 322;
    pub const SHA256: u16 = 323;
    pub const SHA512: u16 = 324;
    pub const XXHASH: u16 = 325;
    pub const HEX_ENCODE: u16 = 326;
    pub const HEX_DECODE: u16 = 327;
    pub const BASE64_ENCODE: u16 = 328;
    pub const BASE64_DECODE: u16 = 329;
    pub const E: u16 = 330;
    pub const PI: u16 = 331;
    pub const TAU: u16 = 332;
    pub const DEGREES: u16 = 333;
    pub const RADIANS: u16 = 334;
    pub const POW10: u16 = 335;
    pub const RECIP: u16 = 336;
    pub const ROUND_EVEN: u16 = 337;
    pub const SIGNUM: u16 = 338;
    pub const FRACT: u16 = 339;
    pub const V: u16 = 340;
    pub const LOG_1: u16 = 341;
    pub const LOG_2: u16 = 342;
    pub const ROUND_1: u16 = 343;
    pub const ROUND_2: u16 = 344;
    pub const SUM_1: u16 = 345;
    pub const AVG_1: u16 = 346;
    pub const MEDIAN_1: u16 = 347;
    pub const QUANTILE_2: u16 = 348;
    pub const STDDEV_1: u16 = 349;
    pub const VARIANCE_1: u16 = 350;
    pub const FREQUENCY_1: u16 = 352;
    pub const PIVOT: u16 = 360;
    pub const MELT: u16 = 361;
    pub const COUNT_1: u16 = 375;
    pub const PARSE_GROK: u16 = 382;
    pub const PARSE_LOGFMT: u16 = 383;
    pub const PARSE_QUERY_STRING: u16 = 384;
    pub const PARSE_SYSLOG: u16 = 385;
    pub const PARSE_URL: u16 = 386;
    pub const PARSE_USER_AGENT: u16 = 387;
    pub const LOWER: u16 = 391;
    pub const UPPER: u16 = 392;
    pub const JSON_POINTER_1: u16 = 393;
    pub const JSON_POINTER_2: u16 = 394;
    pub const JSONPATH_1: u16 = 395;
    pub const JSONPATH_2: u16 = 396;
    pub const DIFF_2: u16 = 414;
    pub const SAMPLE_1: u16 = 415;
    pub const SHUFFLE_0: u16 = 416;
    pub const FILL_FORWARD_0: u16 = 417;
    pub const HMAC: u16 = 418;
    pub const RAND_0: u16 = 419;
    pub const RAND_1: u16 = 420;
    pub const RANDINT_1: u16 = 421;
    pub const RANDINT_2: u16 = 422;
    pub const CHOICE_1: u16 = 423;
    pub const XPATH_1: u16 = 485;
    pub const CSS_1: u16 = 486;
    pub const SCHEMA_INFER: u16 = 408;
    pub const SCHEMA_VALIDATE: u16 = 409;
    pub const SCHEMA_ERRORS: u16 = 410;
    pub const SCHEMA_DIFF: u16 = 664;
    pub const SCHEMA_INFER_2: u16 = 458;
    pub const BASE64URL_ENCODE: u16 = 424;
    pub const BASE64URL_DECODE: u16 = 425;
    pub const PERCENT_ENCODE: u16 = 426;
    pub const PERCENT_DECODE: u16 = 427;
    pub const BASE32_ENCODE: u16 = 428;
    pub const BASE32_DECODE: u16 = 429;
    pub const QUOTED_PRINTABLE_ENCODE: u16 = 430;
    pub const QUOTED_PRINTABLE_DECODE: u16 = 431;
    pub const HMAC_SHA1: u16 = 432;
    pub const HMAC_SHA512: u16 = 433;
    pub const HMAC_SHA1_BASE64URL: u16 = 434;
    pub const HMAC_SHA256_BASE64URL: u16 = 435;
    pub const HMAC_SHA512_BASE64URL: u16 = 436;
    pub const BLAKE3: u16 = 437;
    pub const CRC32: u16 = 438;

    pub const UNION_FAMILY_ID: u16 = 500;
    pub const INTERSECT_FAMILY_ID: u16 = 501;
    pub const EXCEPT_FAMILY_ID: u16 = 502;
    pub const UUID_FAMILY_ID: u16 = 503;
    pub const UUID_V4_FAMILY_ID: u16 = 504;
    pub const UUID_V7_FAMILY_ID: u16 = 505;
    pub const MD5_FAMILY_ID: u16 = 506;
    pub const SHA1_FAMILY_ID: u16 = 507;
    pub const SHA256_FAMILY_ID: u16 = 508;
    pub const SHA512_FAMILY_ID: u16 = 509;
    pub const XXHASH_FAMILY_ID: u16 = 510;
    pub const HEX_ENCODE_FAMILY_ID: u16 = 511;
    pub const HEX_DECODE_FAMILY_ID: u16 = 512;
    pub const BASE64_ENCODE_FAMILY_ID: u16 = 513;
    pub const BASE64_DECODE_FAMILY_ID: u16 = 514;
    pub const E_FAMILY_ID: u16 = 515;
    pub const PI_FAMILY_ID: u16 = 516;
    pub const TAU_FAMILY_ID: u16 = 517;
    pub const DEGREES_FAMILY_ID: u16 = 518;
    pub const RADIANS_FAMILY_ID: u16 = 519;
    pub const POW10_FAMILY_ID: u16 = 520;
    pub const RECIP_FAMILY_ID: u16 = 521;
    pub const ROUND_EVEN_FAMILY_ID: u16 = 522;
    pub const SIGNUM_FAMILY_ID: u16 = 523;
    pub const FRACT_FAMILY_ID: u16 = 524;
    pub const SUM_FAMILY_ID: u16 = 526;
    pub const AVG_FAMILY_ID: u16 = 527;
    pub const MEDIAN_FAMILY_ID: u16 = 528;
    pub const QUANTILE_FAMILY_ID: u16 = 529;
    pub const STDDEV_FAMILY_ID: u16 = 530;
    pub const VARIANCE_FAMILY_ID: u16 = 531;
    pub const FREQUENCY_FAMILY_ID: u16 = 533;
    pub const SAMPLE_FAMILY_ID: u16 = 537;
    pub const SHUFFLE_FAMILY_ID: u16 = 538;
    pub const PIVOT_FAMILY_ID: u16 = 539;
    pub const MELT_FAMILY_ID: u16 = 540;
    pub const COUNT_FAMILY_ID: u16 = 550;
    pub const FILL_FORWARD_FAMILY_ID: u16 = 553;
    pub const PARSE_GROK_FAMILY_ID: u16 = 555;
    pub const PARSE_LOGFMT_FAMILY_ID: u16 = 556;
    pub const PARSE_QUERY_STRING_FAMILY_ID: u16 = 557;
    pub const PARSE_SYSLOG_FAMILY_ID: u16 = 558;
    pub const PARSE_URL_FAMILY_ID: u16 = 559;
    pub const PARSE_USER_AGENT_FAMILY_ID: u16 = 560;
    // 565 is an unused number: `PARSE_FAMILY_ID` stopped being registered when the family-registration gate replaced
    // the phantom `"parse"` family with the six real parser families (555-560), and its never-re-registered constant
    // was pruned (2026-08-23 ponytail audit). The number stays unclaimed until a family actually registers with it.
    pub const JSON_POINTER_FAMILY_ID: u16 = 566;
    pub const JSONPATH_FAMILY_ID: u16 = 567;
    pub const SCHEMA_INFER_FAMILY_ID: u16 = 577;
    pub const SCHEMA_VALIDATE_FAMILY_ID: u16 = 578;
    pub const SCHEMA_ERRORS_FAMILY_ID: u16 = 579;
    pub const SCHEMA_DIFF_FAMILY_ID: u16 = 664;
    pub const DIFF_FAMILY_ID: u16 = 581;
    pub const HMAC_FAMILY_ID: u16 = 582;
    pub const RAND_FAMILY_ID: u16 = 583;
    pub const RANDINT_FAMILY_ID: u16 = 584;
    pub const CHOICE_FAMILY_ID: u16 = 585;
    pub const XPATH_FAMILY_ID: u16 = 625;
    pub const CSS_FAMILY_ID: u16 = 626;
    pub const BASE64URL_ENCODE_FAMILY_ID: u16 = 600;
    pub const BASE64URL_DECODE_FAMILY_ID: u16 = 601;
    pub const PERCENT_ENCODE_FAMILY_ID: u16 = 602;
    pub const PERCENT_DECODE_FAMILY_ID: u16 = 603;
    pub const BASE32_ENCODE_FAMILY_ID: u16 = 604;
    pub const BASE32_DECODE_FAMILY_ID: u16 = 605;
    pub const QUOTED_PRINTABLE_ENCODE_FAMILY_ID: u16 = 606;
    pub const QUOTED_PRINTABLE_DECODE_FAMILY_ID: u16 = 607;
    pub const HMAC_SHA1_FAMILY_ID: u16 = 608;
    pub const HMAC_SHA512_FAMILY_ID: u16 = 609;
    pub const HMAC_SHA1_BASE64URL_FAMILY_ID: u16 = 610;
    pub const HMAC_SHA256_BASE64URL_FAMILY_ID: u16 = 611;
    pub const HMAC_SHA512_BASE64URL_FAMILY_ID: u16 = 612;
    pub const BLAKE3_FAMILY_ID: u16 = 613;
    pub const CRC32_FAMILY_ID: u16 = 614;
    /// `tag/0` — the non-core tag accessor.
    pub const TAG: u16 = 586;
    // --- temporal completion: RFC 3339 parse/format builtins ---
    pub const FROMRFC3339: u16 = 439;
    pub const TORFC3339: u16 = 440;
    pub const FROMRFC3339_FAMILY_ID: u16 = 615;
    pub const TORFC3339_FAMILY_ID: u16 = 616;
    // --- IP/CIDR family ---
    pub const IP_VALID: u16 = 480;
    pub const IP_VERSION: u16 = 481;
    pub const IP_CLASS: u16 = 482;
    pub const IP_CANONICAL: u16 = 483;
    pub const IP_IN_CIDR: u16 = 484;
    pub const IP_VALID_FAMILY_ID: u16 = 650;
    pub const IP_VERSION_FAMILY_ID: u16 = 651;
    pub const IP_CLASS_FAMILY_ID: u16 = 652;
    pub const IP_CANONICAL_FAMILY_ID: u16 = 653;
    pub const IP_IN_CIDR_FAMILY_ID: u16 = 654;
    // --- compression: gzip/deflate/zlib over a string's UTF-8 bytes, carrying the compressed payload as base64.
    // Overload ids 441+; family ids 617+. ---
    pub const GZIP_COMPRESS: u16 = 441;
    pub const GZIP_DECOMPRESS: u16 = 442;
    pub const DEFLATE_COMPRESS: u16 = 443;
    pub const DEFLATE_DECOMPRESS: u16 = 444;
    pub const ZLIB_COMPRESS: u16 = 445;
    pub const ZLIB_DECOMPRESS: u16 = 446;
    pub const GZIP_COMPRESS_FAMILY_ID: u16 = 617;
    pub const GZIP_DECOMPRESS_FAMILY_ID: u16 = 618;
    pub const DEFLATE_COMPRESS_FAMILY_ID: u16 = 619;
    pub const DEFLATE_DECOMPRESS_FAMILY_ID: u16 = 620;
    pub const ZLIB_COMPRESS_FAMILY_ID: u16 = 621;
    pub const ZLIB_DECOMPRESS_FAMILY_ID: u16 = 622;
    // --- top_k: true partial-sort O(n log k) --- Overload ids 447-448; family id 623.
    pub const TOP_K_1: u16 = 447;
    pub const TOP_K_2: u16 = 448;
    pub const TOP_K_FAMILY_ID: u16 = 623;
    // --- numfmt: printf-style number formatting. Overload id 449; family id 624. ---
    pub const NUMFMT: u16 = 449;
    pub const NUMFMT_FAMILY_ID: u16 = 624;
    // --- redact/fuzzy families: value redaction (`redact/0,1,2` and `redact_keyed/1`) and the fuzzy string family
    // (`edit_distance/1`, `similarity/1`, `fuzzy_match/2`). Overload ids 450-456; family ids 655-659. ---
    pub const REDACT_0: u16 = 450;
    pub const REDACT_1: u16 = 451;
    pub const REDACT_2: u16 = 452;
    pub const REDACT_KEYED: u16 = 453;
    pub const EDIT_DISTANCE: u16 = 454;
    pub const SIMILARITY: u16 = 455;
    pub const FUZZY_MATCH: u16 = 456;
    pub const REDACT_FAMILY_ID: u16 = 655;
    pub const REDACT_KEYED_FAMILY_ID: u16 = 656;
    pub const EDIT_DISTANCE_FAMILY_ID: u16 = 657;
    pub const SIMILARITY_FAMILY_ID: u16 = 658;
    pub const FUZZY_MATCH_FAMILY_ID: u16 = 659;
    // --- user-declared reusable index: the `declare_index/2` TRANSPARENT acceleration declaration. Overload id 457;
    // family id 660. ---
    pub const DECLARE_INDEX: u16 = 457;
    pub const DECLARE_INDEX_FAMILY_ID: u16 = 660;
    // --- the explicit hex HMAC-SHA256 spelling: the missing member of an otherwise symmetric HMAC family — `hmac/1`
    // defaults to sha256 and every sibling hex/base64url spelling existed but this one. Overload id 662; family id 663.
    // ---
    pub const HMAC_SHA256: u16 = 662;
    pub const HMAC_SHA256_FAMILY_ID: u16 = 663;
    // --- json_facts (the --json-facts projection): one overload, one family.
    // Overload id 459; family id 588. ---
    pub const JSON_FACTS: u16 = 459;
    pub const JSON_FACTS_FAMILY_ID: u16 = 588;
}
