//! The arity-0 string builtins: three parsers, the codepoint pair, the ASCII case pair, the byte length, and the three
//! trims.
//!
//! One job: own the family and overload records for the eleven builtins whose whole input is one scalar and whose
//! answer is a fresh scalar, plus the one law table the executor dispatches through. Every row is a pure owned-value
//! law, so they share [`crate::exec`]'s owned-law drive and none of them opens a frame.
//!
//! What binds them together is not "they are about strings" — it is that each one's answer is a function of the whole
//! input and of nothing else, so one [`ScalarLaw`] discriminant is enough and eleven dispatch variants would be noise.
//! [`crate::registry::builtins::order`]'s `WholeForm` is the same shape for the same reason.
//!
//! Three of the reference's messages here name a builtin OTHER than the one that raised them, and all three are
//! load-bearing rather than sloppy:
//!
//! ```text
//! '1' | ascii_downcase  → explode input must be a string
//! '1' | ltrim           → trim input must be a string
//! '1' | rtrim           → trim input must be a string
//! ```
//!
//! The reference DEFINES the case pair over `explode`, and the trim trio shares one C implementation that spells
//! `trim`. Borrowing the names keeps error parity; spelling the honest name would fail the corpus. The case pair is
//! nonetheless implemented directly over the text here rather than through a codepoint array, because the array is not
//! observable and the allocation is.
//!
//! Negative space: it owns no number grammar ([`crate::semantics::scan`]), no JSON reader
//! ([`crate::semantics::decode`]) and no number spelling ([`crate::semantics::render`]); it owns no argument-taking
//! builtin (`ltrimstr` and its family are arity-1 and live with the search family); and it owns no message text, which
//! is [`message`]'s.

use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::{Array, Integer, Number, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::error::{EngineRunError, message};
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, SemanticRevision,
};
use crate::semantics::path::raise;
use crate::semantics::{arith, decode, order, scan};

/// The arity-0 string family records.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    TONUMBER_FAMILY,
    TOBOOLEAN_FAMILY,
    FROMJSON_FAMILY,
    EXPLODE_FAMILY,
    IMPLODE_FAMILY,
    ASCII_DOWNCASE_FAMILY,
    ASCII_UPCASE_FAMILY,
    UTF8BYTELENGTH_FAMILY,
    TRIM_FAMILY,
    LTRIM_FAMILY,
    RTRIM_FAMILY,
];

/// The arity-0 string overload records.
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    TONUMBER_OVERLOAD,
    TOBOOLEAN_OVERLOAD,
    FROMJSON_OVERLOAD,
    EXPLODE_OVERLOAD,
    IMPLODE_OVERLOAD,
    ASCII_DOWNCASE_OVERLOAD,
    ASCII_UPCASE_OVERLOAD,
    UTF8BYTELENGTH_OVERLOAD,
    TRIM_OVERLOAD,
    LTRIM_OVERLOAD,
    RTRIM_OVERLOAD,
];

/// The scalar-law execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, ScalarLaw)] = &[
    (id::TONUMBER, ScalarLaw::ToNumber),
    (id::TOBOOLEAN, ScalarLaw::ToBoolean),
    (id::FROMJSON, ScalarLaw::FromJson),
    (id::EXPLODE, ScalarLaw::Explode),
    (id::IMPLODE, ScalarLaw::Implode),
    (id::ASCII_DOWNCASE, ScalarLaw::AsciiDowncase),
    (id::ASCII_UPCASE, ScalarLaw::AsciiUpcase),
    (id::UTF8BYTELENGTH, ScalarLaw::Utf8ByteLength),
    (id::TRIM, ScalarLaw::Trim),
    (id::LTRIM, ScalarLaw::LTrim),
    (id::RTRIM, ScalarLaw::RTrim),
];

/// Which arity-0 scalar law one `Call` runs.
#[derive(Clone, Copy, Debug)]
pub enum ScalarLaw {
    /// `tonumber/0` — a number unchanged, a string through the number reader.
    ToNumber,
    /// `toboolean/0` — a boolean unchanged, `"true"`/`"false"` parsed.
    ToBoolean,
    /// `fromjson/0` — the input string read as one JSON value, the reference's way.
    FromJson,
    /// `explode/0` — the codepoint array.
    Explode,
    /// `implode/0` — the codepoint array's inverse.
    Implode,
    /// `ascii_downcase/0` — `A`–`Z` lowered, everything else untouched.
    AsciiDowncase,
    /// `ascii_upcase/0` — `a`–`z` raised, everything else untouched.
    AsciiUpcase,
    /// `utf8bytelength/0` — the UTF-8 byte count of a string.
    Utf8ByteLength,
    /// `trim/0` — Unicode `White_Space` removed from both ends.
    Trim,
    /// `ltrim/0` — from the front only.
    LTrim,
    /// `rtrim/0` — from the back only.
    RTrim,
}

/// Which ends a trim removes whitespace from.
#[derive(Clone, Copy)]
enum Ends {
    /// `trim`.
    Both,
    /// `ltrim`.
    Front,
    /// `rtrim`.
    Back,
}

/// Evaluates one arity-0 scalar law over an owned input.
///
/// # Errors
///
/// Returns the law's own input refusal (each one is a distinct message class) or an allocation failure.
pub fn apply(law: ScalarLaw, input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match law {
        ScalarLaw::ToNumber => tonumber(input, resources),
        ScalarLaw::ToBoolean => toboolean(input, resources),
        ScalarLaw::FromJson => fromjson(input, resources),
        ScalarLaw::Explode => explode(input, resources),
        ScalarLaw::Implode => implode(input, resources),
        ScalarLaw::AsciiDowncase => ascii_case(input, |character| character.to_ascii_lowercase(), resources),
        ScalarLaw::AsciiUpcase => ascii_case(input, |character| character.to_ascii_uppercase(), resources),
        ScalarLaw::Utf8ByteLength => utf8bytelength(input, resources),
        ScalarLaw::Trim => trim(input, Ends::Both, resources),
        ScalarLaw::LTrim => trim(input, Ends::Front, resources),
        ScalarLaw::RTrim => trim(input, Ends::Back, resources),
    }
}

/// `tonumber`: a number passes through, a string is read with the reference's grammar.
///
/// A NUMBER is returned by handle and not re-parsed, which is what keeps a retained spelling retained. Everything else
/// — including a string the reference's reader refuses — is the one refusal class.
fn tonumber(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match input.untagged() {
        Value::Number(number) => return Ok(Value::Number(number.clone())),
        Value::String(text) => match scan::number(text.as_str()) {
            scan::NumberParse::Value(number) => return Ok(number),
            // The value's canonical storage could not be allocated: the machine class, never the unparsable-number
            // refusal.
            scan::NumberParse::Allocation => {
                return Err(EngineRunError::allocation_failure());
            }
            scan::NumberParse::Refused => {}
        },
        _ => {}
    }
    let operand = message::dump_trunc_owned(input)?;
    let text = message::unparsable_number_message(input.kind(), &operand)?;
    Err(raise(&text, resources))
}

/// `toboolean`: a boolean passes through; only the two exact spellings parse.
///
/// The comparison is over the WHOLE text, so an embedded NUL does not terminate it — `"true\u0000x"` is refused,
/// which is the reference's behaviour and a suite case.
fn toboolean(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match input.untagged() {
        Value::Bool(value) => return Ok(Value::Bool(*value)),
        Value::String(text) => match text.as_str() {
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            _ => {}
        },
        _ => {}
    }
    let operand = message::dump_trunc_owned(input)?;
    let text = message::unparsable_boolean_message(input.kind(), &operand)?;
    Err(raise(&text, resources))
}

/// `fromjson`: the input string through the reference's own reader.
///
/// The kind refusal is this builtin's; everything the READER refuses it reports itself, because the reference's parse
/// message carries a position and the whole input's echo and neither is anything a builtin knows.
fn fromjson(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::String(text) = input.untagged() else {
        let operand = message::dump_trunc_owned(input)?;
        let refusal = message::only_strings_message(input.kind(), &operand)?;
        return Err(raise(&refusal, resources));
    };
    decode::json(text.as_str(), resources)
}

/// `explode`: the input string's codepoints, as an array of numbers.
fn explode(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let text = require_text(input, "explode", "a string", resources)?;
    let mut elements = Vec::new();
    elements
        .try_reserve_exact(text.chars().count())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for character in text.chars() {
        elements.push(integer_value(i64::from(u32::from(character))));
    }
    Array::try_from_vec(elements)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// `implode`: an array of codepoints, as a string.
///
/// The numeric coercion is the reference's and is not obvious: a value is TRUNCATED toward zero (`1.9` implodes as
/// U+0001), and a result below zero, above U+10FFFF, or inside the surrogate range becomes U+FFFD rather than raising.
/// Only a NON-NUMBER element raises, and it names itself.
fn implode(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::Array(array) = input.untagged() else {
        let text = message::wrong_input_kind_message("implode", "an array")?;
        return Err(raise(&text, resources));
    };
    let mut out = String::new();
    for element in array {
        let Value::Number(number) = element.untagged() else {
            let operand = message::dump_trunc_owned(element)?;
            let text = message::not_a_codepoint_message(element.kind(), &operand)?;
            return Err(raise(&text, resources));
        };
        push_char(&mut out, codepoint_of(number))?;
    }
    string_value(&out)
}

/// The largest Unicode scalar value, as the double the reference's range test compares.
const MAX_SCALAR: f64 = 0x0010_FFFF_u32 as f64;

/// The character one imploded codepoint becomes.
///
/// The reference reads the element as a double, truncates it with a C cast to `int`, then substitutes U+FFFD for
/// anything below zero, above U+10FFFF, or inside the UTF-16 surrogate range. The range test here is performed on the
/// DOUBLE rather than after the cast, which additionally makes NaN and a magnitude no `int` could hold answer U+FFFD
/// instead of reaching C's undefined conversion.
fn codepoint_of(number: &Number) -> char {
    let truncated = arith::trunc_toward_zero(order::to_f64(number));
    // NaN fails this test in both directions, which is the answer we want.
    if !(0.0..=MAX_SCALAR).contains(&truncated) {
        return char::REPLACEMENT_CHARACTER;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the range test above bounds the value to the Unicode scalar range"
    )]
    let codepoint = truncated as u32;
    // `from_u32` is what refuses the surrogates the range test let through.
    char::from_u32(codepoint).unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// `ascii_downcase` / `ascii_upcase`: only the 52 ASCII letters change.
///
/// The mapping runs per CODEPOINT and not per byte. Per byte would be tempting — every byte of a multi-byte sequence
/// is `>= 0x80` and the ASCII case maps leave those alone — but rebuilding the text from those bytes means deciding
/// what each one MEANS, and a `u8` promoted to a `char` is a codepoint rather than a byte:
/// it would re-encode `É`'s two bytes as two two-byte characters.
fn ascii_case(input: &Value, map: fn(char) -> char, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let text = require_text(input, "explode", "a string", resources)?;
    let mut out = String::new();
    out.try_reserve_exact(text.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for character in text.chars() {
        // The reservation above is exact and the mapping is length-preserving — an ASCII letter's case counterpart is
        // one byte too — so this push cannot grow the buffer and cannot reach an infallible allocation.
        out.push(map(character));
    }
    string_value(&out)
}

/// `utf8bytelength`: the encoded length of a string, in bytes.
fn utf8bytelength(input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::String(text) = input.untagged() else {
        let operand = message::dump_trunc_owned(input)?;
        let message = message::not_measurable_message(input.kind(), &operand)?;
        return Err(raise(&message, resources));
    };
    // A string longer than `i64::MAX` bytes cannot exist — every allocation is charged against limits far below that
    // — so the conversion failure is an unreachable invariant, not a resource answer.
    let length = i64::try_from(text.as_str().len())
        .map_err(|_| EngineRunError::internal_contract("string byte length exceeds i64"))?;
    Ok(integer_value(length))
}

/// `trim` / `ltrim` / `rtrim`: Unicode `White_Space` removed from the named ends.
///
/// The trimmed set is the `White_Space` property and NOT `isspace`: U+00A0, U+2028 and U+3000 are trimmed while U+200B
/// and U+FEFF are not. Pinned exhaustively over every codepoint below U+10000, where the reference matches Rust's
/// [`char::is_whitespace`] exactly.
fn trim(input: &Value, ends: Ends, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let text = require_text(input, "trim", "a string", resources)?;
    let trimmed = match ends {
        Ends::Both => text.trim(),
        Ends::Front => text.trim_start(),
        Ends::Back => text.trim_end(),
    };
    if trimmed.len() == text.len() {
        // Nothing moved, so there is nothing to copy: the input's own payload is the answer, retained rather than
        // reallocated.
        if let Value::String(payload) = input.untagged() {
            return Ok(Value::String(payload.clone_shared()));
        }
    }
    string_value(trimmed)
}

/// The input's text, or the operand-less refusal the reference spells for `name`.
fn require_text<'value>(
    input: &'value Value,
    name: &str,
    expected: &str,
    resources: &ResourceContext<'_>,
) -> Result<&'value str, EngineRunError> {
    if let Value::String(text) = input.untagged() {
        return Ok(text.as_str());
    }
    let message = message::wrong_input_kind_message(name, expected)?;
    Err(raise(&message, resources))
}

/// One exact integer as a value.
fn integer_value(value: i64) -> Value {
    Value::Number(Number::integer(Integer::from_i64(value)))
}

/// One owned string value.
fn string_value(text: &str) -> Result<Value, EngineRunError> {
    Value::try_string(text).map_err(|_| EngineRunError::allocation_failure())
}

/// Appends one character, growing fallibly.
pub fn push_char(out: &mut String, character: char) -> Result<(), EngineRunError> {
    out.try_reserve(character.len_utf8())
        .map_err(|_| EngineRunError::allocation_failure())?;
    out.push(character);
    Ok(())
}

const TONUMBER_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TONUMBER),
    canonical_name: "tonumber",
    category: "text",
    summary: "The input as a number: a number unchanged, a string parsed.",
    detail: "The accepted spellings are the reference's and are LOOSER than JSON's: a \
             leading `+`, redundant leading zeroes, and a decimal point with \
             nothing on one side of it all parse. Surrounding whitespace, hex, \
             and digit separators do not. Every other kind raises.",
};

const TONUMBER_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TONUMBER),
    family: BuiltinFamilyId::new(id::TONUMBER),
    canonical_name: "tonumber",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[.[] | tonumber]",
            input: "[\"1\",\"00123\",\".5\",\"+5.43\",2]",
            expected: "[1,123,0.5,5.43,2]\n",
        },
        BuiltinExample {
            program: "[.[] | try tonumber catch \"no\"]",
            input: "[\" 4\",\"0x10\",\"\"]",
            expected: "[\"no\",\"no\",\"no\"]\n",
        },
    ],
};

const TOBOOLEAN_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TOBOOLEAN),
    canonical_name: "toboolean",
    category: "text",
    summary: "The input as a boolean: a boolean unchanged, `\"true\"`/`\"false\"` \
              parsed.",
    detail: "Only those two spellings, exactly: `\"TRUE\"`, `\"1\"` and `\"true \"` \
             all raise, and so does every non-string, non-boolean kind.",
};

const TOBOOLEAN_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TOBOOLEAN),
    family: BuiltinFamilyId::new(id::TOBOOLEAN),
    canonical_name: "toboolean",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[.[] | toboolean]",
            input: "[\"false\",\"true\",false,true]",
            expected: "[false,true,false,true]\n",
        },
        BuiltinExample {
            program: "try toboolean catch \"no\"",
            input: "\"TRUE\"",
            expected: "\"no\"\n",
        },
    ],
};

const FROMJSON_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::FROMJSON),
    canonical_name: "fromjson",
    category: "text",
    summary: "The input string read as one JSON value, with the reference's own reader.",
    detail: "The acceptance is the reference's parser and not strict JSON: a leading BOM, a \
             leading `+`, redundant zeroes and a bare `.5` all parse, duplicate \
             keys keep their first position and last value, and a refusal names \
             the reason, the line and byte column, and the whole input. Nesting \
             is capped at 10 000 stack slots, where an object with a key waiting \
             holds two.",
};

const FROMJSON_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::FROMJSON),
    family: BuiltinFamilyId::new(id::FROMJSON),
    canonical_name: "fromjson",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "[.[] | fromjson]",
            input: "[\"[1,2]\",\"{\\\"a\\\":1}\",\" +01 \"]",
            expected: "[[1,2],{\"a\":1},1]\n",
        },
        BuiltinExample {
            program: "try fromjson catch .",
            input: "\"[1,]\"",
            expected: "\"Expected another array element at line 1, column 4 (while parsing '[1,]')\"\n",
        },
    ],
};

const EXPLODE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::EXPLODE),
    canonical_name: "explode",
    category: "text",
    summary: "The input string's codepoints, as an array of numbers.",
    detail: "One element per Unicode scalar value, not per byte or per UTF-16 \
             unit: `\"😀\"` explodes to `[128512]`. A non-string input raises.",
};

const EXPLODE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::EXPLODE),
    family: BuiltinFamilyId::new(id::EXPLODE),
    canonical_name: "explode",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "explode",
            input: "\"héllo\"",
            expected: "[104,233,108,108,111]\n",
        },
        BuiltinExample {
            program: "explode",
            input: "\"\"",
            expected: "[]\n",
        },
    ],
};

const IMPLODE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::IMPLODE),
    canonical_name: "implode",
    category: "text",
    summary: "An array of codepoints, as a string.",
    detail: "A codepoint is truncated toward zero, and one that is negative, \
             above U+10FFFF, or a surrogate becomes U+FFFD rather than raising. \
             A non-number ELEMENT raises and names itself; a non-array input \
             raises with its own message.",
};

const IMPLODE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::IMPLODE),
    family: BuiltinFamilyId::new(id::IMPLODE),
    canonical_name: "implode",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "implode",
            input: "[104,233]",
            expected: "\"hé\"\n",
        },
        BuiltinExample {
            program: "explode | implode",
            input: "\"round trip\"",
            expected: "\"round trip\"\n",
        },
    ],
};

const ASCII_DOWNCASE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::ASCII_DOWNCASE),
    canonical_name: "ascii_downcase",
    category: "text",
    summary: "The input string with `A`-`Z` lowered.",
    detail: "ASCII only: `É` is left alone. A non-string input raises, and the \
             message says `explode` because the reference defines this builtin over it.",
};

const ASCII_DOWNCASE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::ASCII_DOWNCASE),
    family: BuiltinFamilyId::new(id::ASCII_DOWNCASE),
    canonical_name: "ascii_downcase",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "ascii_downcase",
            input: "\"AÉZ~\"",
            expected: "\"aÉz~\"\n",
        },
        BuiltinExample {
            program: "ascii_downcase",
            input: "\"\"",
            expected: "\"\"\n",
        },
    ],
};

const ASCII_UPCASE_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::ASCII_UPCASE),
    canonical_name: "ascii_upcase",
    category: "text",
    summary: "The input string with `a`-`z` raised.",
    detail: "ASCII only: `é` is left alone. A non-string input raises with \
             `explode`'s message, as `ascii_downcase` does.",
};

const ASCII_UPCASE_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::ASCII_UPCASE),
    family: BuiltinFamilyId::new(id::ASCII_UPCASE),
    canonical_name: "ascii_upcase",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "ascii_upcase",
            input: "\"aéz\"",
            expected: "\"AéZ\"\n",
        },
        BuiltinExample {
            program: "ascii_upcase",
            input: "\"[\\\\]^_`{|}\"",
            expected: "\"[\\\\]^_`{|}\"\n",
        },
    ],
};

const UTF8BYTELENGTH_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::UTF8BYTELENGTH),
    canonical_name: "utf8bytelength",
    category: "text",
    summary: "The input string's length in UTF-8 bytes.",
    detail: "Distinct from `length`, which counts CODEPOINTS: `\"😀\"` has \
             length 1 and byte length 4. Only strings have one; every other \
             kind raises.",
};

const UTF8BYTELENGTH_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::UTF8BYTELENGTH),
    family: BuiltinFamilyId::new(id::UTF8BYTELENGTH),
    canonical_name: "utf8bytelength",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "utf8bytelength",
            input: "\"asdfμ\"",
            expected: "6\n",
        },
        BuiltinExample {
            program: "[length, utf8bytelength]",
            input: "\"😀\"",
            expected: "[1,4]\n",
        },
    ],
};

const TRIM_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::TRIM),
    canonical_name: "trim",
    category: "text",
    summary: "The input string with whitespace removed from both ends.",
    detail: "The trimmed set is Unicode's `White_Space` property, so U+00A0 and \
             U+3000 are trimmed while U+200B and U+FEFF are not. A non-string \
             input raises.",
};

const TRIM_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::TRIM),
    family: BuiltinFamilyId::new(id::TRIM),
    canonical_name: "trim",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "trim",
            input: "\"  a b  \"",
            expected: "\"a b\"\n",
        },
        BuiltinExample {
            program: "trim",
            input: "\"   \"",
            expected: "\"\"\n",
        },
    ],
};

const LTRIM_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::LTRIM),
    canonical_name: "ltrim",
    category: "text",
    summary: "The input string with whitespace removed from the front.",
    detail: "`trim`'s law on one end. Its refusal says `trim`, which is the reference's own \
             wording for all three.",
};

const LTRIM_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::LTRIM),
    family: BuiltinFamilyId::new(id::LTRIM),
    canonical_name: "ltrim",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "ltrim",
            input: "\"  a b  \"",
            expected: "\"a b  \"\n",
        },
        BuiltinExample {
            program: "ltrim",
            input: "\"a\"",
            expected: "\"a\"\n",
        },
    ],
};

const RTRIM_FAMILY: BuiltinFamilyRecord = BuiltinFamilyRecord {
    id: BuiltinFamilyId::new(id::RTRIM),
    canonical_name: "rtrim",
    category: "text",
    summary: "The input string with whitespace removed from the back.",
    detail: "`trim`'s law on the other end. Its refusal says `trim` too.",
};

const RTRIM_OVERLOAD: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::RTRIM),
    family: BuiltinFamilyId::new(id::RTRIM),
    canonical_name: "rtrim",
    arity: 0,
    parameters: &[],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        BuiltinExample {
            program: "rtrim",
            input: "\"  a b  \"",
            expected: "\"  a b\"\n",
        },
        BuiltinExample {
            program: "rtrim",
            input: "\"a\"",
            expected: "\"a\"\n",
        },
    ],
};
