//! The jqf EXTENSION families.
//!
//! Every family is `category: "jqf-extension"` and every name is OUTSIDE the reference's builtin vocabulary. The
//! collision-guard test in `registry/mod.rs` pins that boundary: a name the reference owns may never be re-registered
//! as an extension, so future reference growth cannot silently collide with the extension surface — when the
//! reference someday adds a same-named builtin, the extension is renamed or namespaced, never shadowed.
//!
//! The inventory: set algebra (`union`/`intersect`/`except`), UUID (`uuid`/`uuid_v4`/`uuid_v7`), hashing/encodings
//! (`md5`/`sha1`/`sha256`/ `sha512`/`xxhash`/`hex_encode`/`hex_decode`/`base64_encode`/ `base64_decode`), keyed hashing
//! (`hmac` — HMAC-SHA256), math extensions (`e`/`pi`/`tau`/`degrees`/`radians`/
//! `pow10`/`recip`/`round_even`/`signum`/`fract`/`log/1,2`/`round/1,2`), profiling (`frequency`), and wide↔long
//! reshape (`melt`/`pivot`).

use alloc::collections::BTreeMap;
use alloc::collections::btree_map::Entry;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;

use jqf_data::{Array, Number, Object, ObjectBuilder, ObjectKey, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::error::EngineRunError;
use crate::error::message;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::order::{self, semantic_eq, total_cmp};
use crate::semantics::path::raise;
use crate::semantics::rand::{Prng, rand_float, with_prng};

/// The extension law discriminants, one per evaluator shape.
#[derive(Clone, Copy, Debug)]
pub enum ExtensionLaw {
    Set(SetLaw),
    Uuid(UuidLaw),
    Hash(HashLaw),
    Hmac(HmacLaw),
    Compress(CompressLaw),
    NumFmt,
    Math(MathExtLaw),
    Stats(StatsLaw),
    /// Wide→long: one output object per present value key of each input row.
    Melt,
    /// Long→wide: one output object per first-seen id-key tuple.
    Pivot,
    /// Value-count rows over one source array, descending count then total order.
    Frequency,
    /// The redact family rides this family's argument-product drive because it shares the exact argument-evaluation law
    /// (filter arguments evaluated over the call's input, one answer per combination); the law itself lives in
    /// [`super::redact`].
    #[cfg(feature = "ext-redact")]
    Redact(super::redact::RedactLaw),
    /// The fuzzy family, for the same reason as the `Redact` variant; the law lives in [`super::fuzzy`]. (The `Redact`
    /// link is plain text, not an intra-doc link: `ext-fuzzy` without `ext-redact` is a legal corner where the `Redact`
    /// variant is cfg'd out.)
    #[cfg(feature = "ext-fuzzy")]
    Fuzzy(super::fuzzy::FuzzyLaw),
}

#[derive(Clone, Copy, Debug)]
pub enum SetLaw {
    Union,
    Intersect,
    Except,
}

/// The analytics law discriminants: sampling, shuffling, and gap-filling.
#[derive(Clone, Copy, Debug)]
pub enum AnalyticsLaw {
    /// `sample(n)`: `n` elements drawn WITHOUT replacement, uniformly.
    Sample,
    /// `shuffle`: one uniform permutation of the input array.
    Shuffle,
    /// `fill_forward`: every `null` replaced by the nearest preceding non-null (leading nulls stay null).
    FillForward,
}

/// The rand-family law discriminants: uniform floats, bounded integers, and uniform element choice.
#[derive(Clone, Copy, Debug)]
pub enum RandLaw {
    /// `rand/0` — a float uniform in `[0, 1)`. IMPURE: each draw seeds a fresh [`Prng`] from uuid v4 entropy, exactly
    /// like `sample`/`shuffle`.
    Uniform,
    /// `rand/1` — a float uniform in `[0, 1)` drawn from a SEEDED xoshiro256** state, deterministic given the seed.
    /// PURE: the same seed always answers the same float, which is what makes the seeded form reproducible.
    UniformSeeded,
    /// `randint/1` — an integer uniform in `[0, n)`. IMPURE.
    RandintOne,
    /// `randint/2` — an integer uniform in `[a, b)`. IMPURE.
    RandintTwo,
    /// `choice/1` — one uniform element of the argument array. IMPURE.
    Choice,
}

#[derive(Clone, Copy, Debug)]
pub enum UuidLaw {
    Parse,
    V4,
    V7,
}

#[derive(Clone, Copy, Debug)]
pub enum HashLaw {
    Md5,
    Sha1,
    Sha256,
    Sha512,
    Xxhash,
    HexEncode,
    HexDecode,
    Base64Encode,
    Base64Decode,
    Base64urlEncode,
    Base64urlDecode,
    PercentEncode,
    PercentDecode,
    Base32Encode,
    Base32Decode,
    QuotedPrintableEncode,
    QuotedPrintableDecode,
    Blake3,
    Crc32,
}

/// One keyed-hash (HMAC) law. Like the plain hash family, the input value is the message; unlike it, a keyed hash takes
/// a KEY argument, so the shape lives beside (not inside) [`HashLaw`].
#[derive(Clone, Copy, Debug)]
pub enum HmacLaw {
    Sha256,
    Sha1,
    Sha512,
    Sha1Base64url,
    Sha256Base64url,
    Sha512Base64url,
}

/// One compression/decompression law.
///
/// The three compressors are DETERMINISTIC by construction: they are pure functions of the input, with no timestamp and
/// no random state. Gzip is built as framing AROUND the raw DEFLATE stream (fixed 10-byte header with mtime=0, then the
/// deflate body, then CRC32 + ISIZE little-endian trailer), while raw DEFLATE and the zlib wrapper are delegated to
/// `miniz_oxide`.
#[derive(Clone, Copy, Debug)]
pub enum CompressLaw {
    GzipCompress,
    GzipDecompress,
    DeflateCompress,
    DeflateDecompress,
    ZlibCompress,
    ZlibDecompress,
}

#[derive(Clone, Copy, Debug)]
pub enum MathExtLaw {
    E,
    Pi,
    Tau,
    Degrees,
    Radians,
    Pow10,
    Recip,
    RoundEven,
    Signum,
    Fract,
    LogOne,
    LogTwo,
    RoundOne,
    RoundTwo,
}

#[derive(Clone, Copy, Debug)]
pub enum StatsLaw {
    Sum,
    Avg,
    Median,
    Quantile,
    Stddev,
    Variance,
    Count,
}

/// One set-algebra result: both arguments are arrays; the output is the union/intersection/difference, sorted and
/// de-duplicated under the total order.
#[allow(
    clippy::needless_pass_by_value,
    reason = "SetLaw is a two-word Copy enum; the match consumes it by value"
)]
pub fn set_law(
    law: SetLaw,
    left: Value,
    right: Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let name = match law {
        SetLaw::Union => "union",
        SetLaw::Intersect => "intersect",
        SetLaw::Except => "except",
    };
    let Value::Array(left_array) = left.untagged() else {
        return Err(raise(&format!("{name} left expects an array"), resources));
    };
    let Value::Array(right_array) = right.untagged() else {
        return Err(raise(&format!("{name} right expects an array"), resources));
    };
    let mut left: Vec<Value> = Vec::new();
    for value in left_array {
        left.push(value.clone());
    }
    let mut right: Vec<Value> = Vec::new();
    for value in right_array {
        right.push(value.clone());
    }
    let mut out = match law {
        SetLaw::Union => {
            left.extend(right);
            left
        }
        // Intersect and except were linear `any`/`all` scans over the right array (quadratic over 5k-element lanes).
        // Both arrays are sorted once under the internal total order, then walked with a two-pointer merge, including
        // the TooDeep no-match arm (an errored comparison keeps an `except` element and drops an `intersect` one,
        // exactly as the old `== Ok(true)` tests did).
        //
        // The ADVANCE reads the internal order (it is what the two arrays were sorted under, and a merge needs a
        // consistent one) while the MATCH reads `semantic_eq`, which is the membership question these laws ask.
        // The two differ only on a NaN: a NaN is therefore a member of NOTHING, so it survives `except` and vanishes
        // from `intersect` — the same answer `[nan] - [nan]` gives, which is the reference's own on the operator
        // these mirror.
        SetLaw::Intersect | SetLaw::Except => {
            left.sort_by(|a, b| total_cmp(a, b).unwrap_or(Ordering::Equal));
            right.sort_by(|a, b| total_cmp(a, b).unwrap_or(Ordering::Equal));
            let keep_on_equal = matches!(law, SetLaw::Intersect);
            let mut right_index = 0usize;
            left.into_iter()
                .filter(|value| {
                    while right_index < right.len() {
                        let Ok(cmp) = total_cmp(value, &right[right_index]) else {
                            return !keep_on_equal;
                        };
                        if cmp.is_gt() {
                            right_index += 1;
                        } else {
                            break;
                        }
                    }
                    let Some(other) = right.get(right_index) else {
                        return !keep_on_equal;
                    };
                    match semantic_eq(value, other) {
                        Ok(equal) => equal == keep_on_equal,
                        Err(_) => !keep_on_equal,
                    }
                })
                .collect()
        }
    };
    out.sort_by(|a, b| total_cmp(a, b).unwrap_or(Ordering::Equal));
    let mut deduped: Vec<Value> = Vec::new();
    for value in out {
        if deduped.last().is_none_or(|last| semantic_eq(last, &value) != Ok(true)) {
            deduped.push(value);
        }
    }
    let mut array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    for value in deduped {
        array
            .try_push(value)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(array))
}

/// One UUID law.
///
/// The timestamp casts truncate the wall clock's fractional seconds deliberately: the floor is the whole-second Unix
/// timestamp and the fraction is its sub-second part, each converted to the uuid crate's integer fields.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the floor and the fraction are the whole/sub-second parts of the wall clock by               construction, each non-negative at the moment of reading"
)]
pub fn uuid_law(law: UuidLaw, input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match law {
        UuidLaw::V4 => Value::try_string(&alloc::format!("{}", uuid::Uuid::new_v4()))
            .map_err(|_| EngineRunError::allocation_failure()),
        UuidLaw::V7 => {
            // One clock read per UUID: a second read between the floor and the fraction could straddle a tick and tear
            // the timestamp.
            let seconds = crate::registry::builtins::time::wall_clock_seconds();
            Value::try_string(&alloc::format!(
                "{}",
                uuid::Uuid::new_v7(uuid::timestamp::Timestamp::from_unix(
                    uuid::NoContext,
                    seconds.floor() as u64,
                    (seconds.fract() * 1_000_000_000.0) as u32,
                ))
            ))
            .map_err(|_| EngineRunError::allocation_failure())
        }
        UuidLaw::Parse => {
            let Value::String(text) = input.untagged() else {
                return Err(raise("uuid requires string input", resources));
            };
            match uuid::Uuid::parse_str(text.as_str()) {
                Ok(parsed) => {
                    Value::try_string(&alloc::format!("{parsed}")).map_err(|_| EngineRunError::allocation_failure())
                }
                Err(_) => Err(raise("invalid UUID input", resources)),
            }
        }
    }
}

/// One hashing/encoding law over a string input.
pub fn hash_law(law: HashLaw, input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    use sha2::Digest as _;
    let Value::String(text) = input.untagged() else {
        return Err(raise("hash requires string input", resources));
    };
    let output = match law {
        HashLaw::Md5 => hex_encode_bytes(md5::Md5::digest(text.as_bytes()).as_slice()),
        HashLaw::Sha1 => hex_encode_bytes(sha1::Sha1::digest(text.as_bytes()).as_slice()),
        HashLaw::Sha256 => hex_encode_bytes(sha2::Sha256::digest(text.as_bytes()).as_slice()),
        HashLaw::Sha512 => hex_encode_bytes(sha2::Sha512::digest(text.as_bytes()).as_slice()),
        HashLaw::Xxhash => format!("{:016x}", xxhash_rust::xxh3::xxh3_64(text.as_bytes())),
        HashLaw::HexEncode => hex_encode_bytes(text.as_bytes()),
        HashLaw::HexDecode => {
            String::from_utf8(hex_decode_bytes(text.as_bytes()).map_err(|()| raise("invalid hex input", resources))?)
                .map_err(|_| raise("invalid UTF-8 after hex decode", resources))?
        }
        HashLaw::Base64Encode => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
        }
        HashLaw::Base64Decode => {
            use base64::Engine as _;
            String::from_utf8(
                base64::engine::general_purpose::STANDARD
                    .decode(text.as_bytes())
                    .map_err(|_| raise("invalid base64 input", resources))?,
            )
            .map_err(|_| raise("invalid UTF-8 after base64 decode", resources))?
        }
        HashLaw::Base64urlEncode => base64url_encode(text.as_bytes()),
        HashLaw::Base64urlDecode => String::from_utf8(
            base64url_decode(text.as_bytes()).map_err(|()| raise("invalid base64url input", resources))?,
        )
        .map_err(|_| raise("invalid UTF-8 after base64url decode", resources))?,
        HashLaw::PercentEncode => percent_encode(text.as_str()),
        HashLaw::PercentDecode => {
            percent_decode(text.as_str()).map_err(|()| raise("invalid percent-encoding input", resources))?
        }
        HashLaw::Base32Encode => base32_encode(text.as_bytes()),
        HashLaw::Base32Decode => {
            String::from_utf8(base32_decode(text.as_bytes()).map_err(|()| raise("invalid base32 input", resources))?)
                .map_err(|_| raise("invalid UTF-8 after base32 decode", resources))?
        }
        HashLaw::QuotedPrintableEncode => quoted_printable_encode(text.as_str()),
        HashLaw::QuotedPrintableDecode => {
            quoted_printable_decode(text.as_str()).map_err(|()| raise("invalid quoted-printable input", resources))?
        }
        HashLaw::Blake3 => hex_encode_bytes(blake3::hash(text.as_bytes()).as_bytes()),
        HashLaw::Crc32 => format!("{:08x}", crc32_ieee(text.as_bytes())),
    };
    Value::try_string(&output).map_err(|_| EngineRunError::allocation_failure())
}

/// One keyed-hash (HMAC) law over a string input (the message) keyed by a string argument (the key). Output is the hex
/// digest, mirroring the plain hash family's spelling.
pub fn hmac_law(
    law: HmacLaw,
    input: &Value,
    key: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let Value::String(text) = input.untagged() else {
        return Err(raise("hmac requires string input", resources));
    };
    let Value::String(key_text) = key.untagged() else {
        return Err(raise("hmac key must be a string", resources));
    };
    let output = match law {
        HmacLaw::Sha256 => hex_encode_bytes(&hmac_sha256(key_text.as_bytes(), text.as_bytes())),
        HmacLaw::Sha1 => hex_encode_bytes(&hmac_sha1(key_text.as_bytes(), text.as_bytes())),
        HmacLaw::Sha512 => hex_encode_bytes(&hmac_sha512(key_text.as_bytes(), text.as_bytes())),
        HmacLaw::Sha1Base64url => base64url_encode(&hmac_sha1(key_text.as_bytes(), text.as_bytes())[..]),
        HmacLaw::Sha256Base64url => base64url_encode(&hmac_sha256(key_text.as_bytes(), text.as_bytes())[..]),
        HmacLaw::Sha512Base64url => base64url_encode(&hmac_sha512(key_text.as_bytes(), text.as_bytes())[..]),
    };
    Value::try_string(&output).map_err(|_| EngineRunError::allocation_failure())
}

/// The registered name of one compression law, for its own error messages.
fn compression_name(law: CompressLaw) -> &'static str {
    match law {
        CompressLaw::GzipCompress => "gzip_compress",
        CompressLaw::GzipDecompress => "gzip_decompress",
        CompressLaw::DeflateCompress => "deflate_compress",
        CompressLaw::DeflateDecompress => "deflate_decompress",
        CompressLaw::ZlibCompress => "zlib_compress",
        CompressLaw::ZlibDecompress => "zlib_decompress",
    }
}

/// One compression/decompression law over a string input.
///
/// The source payload is the input string's UTF-8 bytes, and the compressed payload travels as a STANDARD BASE64 string
/// — the tree's one byte-carrying idiom, the same spelling `base64_encode`/`base64_decode` publish (a JSON document
/// has no native byte kind, so a bare byte string could not be observed at all). A compress law therefore answers
/// base64(compressed bytes) and a decompress law accepts base64 and answers the UTF-8 text of the decompressed bytes,
/// refusing when that text is not valid UTF-8.
///
/// The compressors are DETERMINISTIC by construction: `miniz_oxide`'s deflate is a pure function of the payload, and
/// the gzip member fixes its header's MTIME to 0 (RFC 1952's optional wall-clock stamp would otherwise make the same
/// input answer differently from one run to the next).
pub fn compression_law(
    law: CompressLaw,
    input: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    use base64::Engine as _;
    let name = compression_name(law);
    let Value::String(text) = input.untagged() else {
        return Err(raise(&format!("{name} requires string input"), resources));
    };
    let compressed = matches!(
        law,
        CompressLaw::GzipCompress | CompressLaw::DeflateCompress | CompressLaw::ZlibCompress
    );
    if compressed {
        let payload = match law {
            CompressLaw::GzipCompress => gzip_member(&deflate_compress_bytes(text.as_bytes()), text.as_bytes()),
            CompressLaw::DeflateCompress => deflate_compress_bytes(text.as_bytes()),
            CompressLaw::ZlibCompress => zlib_compress_bytes(text.as_bytes()),
            _ => unreachable!("decompress laws handled below"),
        };
        return Value::try_string(&base64::engine::general_purpose::STANDARD.encode(payload))
            .map_err(|_| EngineRunError::allocation_failure());
    }
    let encoded = base64::engine::general_purpose::STANDARD
        .decode(text.as_bytes())
        .map_err(|_| raise(&format!("{name} expects base64 input"), resources))?;
    let bytes = match law {
        CompressLaw::GzipDecompress => {
            gzip_decompress(&encoded).map_err(|()| raise(&format!("{name} rejected the gzip stream"), resources))?
        }
        CompressLaw::DeflateDecompress => deflate_decompress_bytes(&encoded)
            .map_err(|()| raise(&format!("{name} rejected the deflate stream"), resources))?,
        CompressLaw::ZlibDecompress => zlib_decompress_bytes(&encoded)
            .map_err(|()| raise(&format!("{name} rejected the zlib stream"), resources))?,
        _ => unreachable!("compress laws handled above"),
    };
    String::from_utf8(bytes)
        .map_err(|_| raise(&format!("{name} decompressed data is not UTF-8"), resources))
        .and_then(|text| Value::try_string(&text).map_err(|_| EngineRunError::allocation_failure()))
}

/// The raw DEFLATE stream of `bytes` (RFC 1951), at miniz's default level.
///
/// `CompressionLevel::DefaultLevel` (6) is named rather than a bare literal so the determinism contract reads from the
/// API: the same level is the same pure function, and every compression law here always passes it.
fn deflate_compress_bytes(bytes: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(bytes, miniz_oxide::deflate::CompressionLevel::DefaultLevel as u8)
}

/// The zlib stream of `bytes` (RFC 1950): a 2-byte header, the raw DEFLATE body, and the Adler-32 trailer, all produced
/// by `miniz_oxide`.
fn zlib_compress_bytes(bytes: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(bytes, miniz_oxide::deflate::CompressionLevel::DefaultLevel as u8)
}

/// One gzip member (RFC 1952) around the raw DEFLATE stream of `bytes`.
///
/// The 10-byte header is FIXED — magic `1f 8b`, CM=8 (deflate), FLG=0 (no optional fields), a FOUR-BYTE MTIME OF
/// ZERO, XFL=0, OS=255 — so the member is a pure function of the payload with no wall-clock stamp (the determinism
/// pin: a gzip writer that stamped the current time into MTIME would fail byte-deterministic acceptance). The 8-byte
/// trailer is the CRC-32 of the UNCOMPRESSED data (little-endian) then ISIZE, the uncompressed length mod 2^32
/// (little-endian).
///
/// The ISIZE cast is the RFC's own truncation: the field is the length mod 2^32, so a real payload's length is
/// truncated by construction.
#[allow(
    clippy::cast_possible_truncation,
    reason = "ISIZE is the uncompressed length mod 2^32 by RFC 1952, a truncation the format itself defines"
)]
fn gzip_member(deflate_body: &[u8], uncompressed: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + deflate_body.len() + 8);
    out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255]);
    out.extend_from_slice(deflate_body);
    out.extend_from_slice(&crc32_ieee(uncompressed).to_le_bytes());
    out.extend_from_slice(&(uncompressed.len() as u32).to_le_bytes());
    out
}

/// One raw DEFLATE stream back to its original bytes.
fn deflate_decompress_bytes(bytes: &[u8]) -> Result<Vec<u8>, ()> {
    miniz_oxide::inflate::decompress_to_vec(bytes).map_err(|_| ())
}

/// One zlib stream back to its original bytes.
fn zlib_decompress_bytes(bytes: &[u8]) -> Result<Vec<u8>, ()> {
    miniz_oxide::inflate::decompress_to_vec_zlib(bytes).map_err(|_| ())
}

/// One gzip member back to its original bytes, validating the member's own checksums.
///
/// The fixed header is required, the FLG bits that add optional fields (FEXTRA/FNAME/FCOMMENT/FHCRC) are skipped with
/// bounds-checked walks, and the trailing CRC-32 and ISIZE must both match the inflated payload — a corrupt member is
/// refused rather than half-trusted.
fn gzip_decompress(bytes: &[u8]) -> Result<Vec<u8>, ()> {
    if bytes.len() < 18 || bytes[0] != 0x1f || bytes[1] != 0x8b || bytes[2] != 8 {
        return Err(());
    }
    let mut offset = 10usize;
    let flags = bytes[3];
    if flags & 0x04 != 0 {
        let Some(len) = bytes.get(offset..offset.saturating_add(2)) else {
            return Err(());
        };
        offset += 2 + usize::from(u16::from_le_bytes([len[0], len[1]]));
    }
    // FNAME (0x08) and FCOMMENT (0x10) are each a NUL-terminated string, walked and consumed in that order.
    for flag in [0x08, 0x10] {
        if flags & flag != 0 {
            while bytes.get(offset).is_some_and(|&byte| byte != 0) {
                offset += 1;
            }
            let Some(&0) = bytes.get(offset) else {
                return Err(());
            };
            offset += 1;
        }
    }
    if flags & 0x02 != 0 {
        offset += 2;
    }
    let Some(body) = bytes.get(offset..bytes.len().saturating_sub(8)) else {
        return Err(());
    };
    let out = miniz_oxide::inflate::decompress_to_vec(body).map_err(|_| ())?;
    let Some(crc) = bytes.get(bytes.len() - 8..bytes.len() - 4) else {
        return Err(());
    };
    if u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) != crc32_ieee(&out) {
        return Err(());
    }
    let Some(isize) = bytes.get(bytes.len() - 4..) else {
        return Err(());
    };
    let isize = u32::from_le_bytes([isize[0], isize[1], isize[2], isize[3]]);
    if (out.len() as u64 & 0xffff_ffff) != u64::from(isize) {
        return Err(());
    }
    Ok(out)
}

/// One number-FORMATTING law: `numfmt(format)` renders the piped number through a printf-style directive.
///
/// The format string carries EXACTLY one conversion directive surrounded by optional literal text. The directive is
/// `%[flags][width][.precision]spec`, where the flags are `,` (thousands grouping of the integer part) and `0`
/// (zero-pad to the width), the width is a minimum field width (space-padded, right-aligned), the precision is the
/// digit count after the point for `f`/`e`
/// (default 6), and the specifier is `d` (integer, truncated toward zero), `f`
/// (fixed-point), or `e` (scientific, `d.ddde±XX`).
///
/// The value model is the D1 exact-decimal one: an integer or exact decimal formats from its OWN digits (no binary64
/// wobble — `1.005 | numfmt(".2f")` is `"1.01"`), and only a binary64 input is converted through its shortest
/// round-trip spelling first. Rounding to the precision is HALF-AWAY-FROM-ZERO, the same law `round/1,2` pins; a
/// half-even consumer composes `round_even` first. A non-finite input is refused.
pub fn numfmt_law(subject: &Value, format: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::String(format_text) = format.untagged() else {
        return Err(raise("numfmt requires a string format", resources));
    };
    let Value::Number(number) = subject.untagged() else {
        return Err(raise("numfmt requires a number input", resources));
    };
    let spec = parse_numfmt(format_text.as_str()).map_err(|()| {
        raise(
            &format!("numfmt: invalid format \"{}\"", format_text.as_str()),
            resources,
        )
    })?;
    let formatted = format_number(spec, number)?.ok_or_else(|| raise("numfmt requires a finite number", resources))?;
    Value::try_string(&formatted).map_err(|_| EngineRunError::allocation_failure())
}

/// One parsed numfmt directive plus its surrounding literal text.
#[derive(Clone, Copy)]
struct NumFmtSpec<'format> {
    prefix: &'format str,
    suffix: &'format str,
    grouping: bool,
    zero_pad: bool,
    width: Option<usize>,
    precision: Option<usize>,
    conversion: NumFmtConversion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumFmtConversion {
    Integer,
    Fixed,
    Scientific,
}

/// Parses `%[flags][width][.precision]spec` from a format string that contains exactly one directive.
fn parse_numfmt(spec: &str) -> Result<NumFmtSpec<'_>, ()> {
    let bytes = spec.as_bytes();
    let pct = spec.find('%').ok_or(())?;
    if bytes[pct + 1..].contains(&b'%') {
        return Err(());
    }
    let prefix = &spec[..pct];
    let mut i = pct + 1;
    let mut grouping = false;
    let mut zero_pad = false;
    loop {
        match bytes.get(i) {
            Some(b',') => {
                grouping = true;
                i += 1;
            }
            Some(b'0') => {
                zero_pad = true;
                i += 1;
            }
            _ => break,
        }
    }
    let mut width = None;
    let width_start = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if i > width_start {
        let text = core::str::from_utf8(&bytes[width_start..i]).map_err(|_| ())?;
        width = Some(text.parse().map_err(|_| ())?);
    }
    let mut precision = None;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let precision_start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == precision_start {
            return Err(());
        }
        let text = core::str::from_utf8(&bytes[precision_start..i]).map_err(|_| ())?;
        precision = Some(text.parse().map_err(|_| ())?);
    }
    let conversion = match bytes.get(i) {
        Some(b'd') => NumFmtConversion::Integer,
        Some(b'f') => NumFmtConversion::Fixed,
        Some(b'e') => NumFmtConversion::Scientific,
        _ => return Err(()),
    };
    i += 1;
    let suffix = &spec[i..];
    if matches!(conversion, NumFmtConversion::Integer) && precision.is_some() {
        return Err(());
    }
    Ok(NumFmtSpec {
        prefix,
        suffix,
        grouping,
        zero_pad,
        width,
        precision,
        conversion,
    })
}

/// Renders one number through one parsed spec. `Ok(None)` for a non-finite binary64 (the only number kind with no exact
/// decimal spelling); an allocation failure propagates as the machine class.
fn format_number(spec: NumFmtSpec<'_>, number: &Number) -> Result<Option<String>, EngineRunError> {
    let Some((negative, digits, scale)) = number_parts(number) else {
        return Ok(None);
    };
    let negative = negative && digits.iter().any(|&byte| byte != b'0');
    let body = match spec.conversion {
        NumFmtConversion::Integer => fmt_integer(&digits, scale, spec.grouping)?,
        NumFmtConversion::Fixed => fmt_fixed(&digits, scale, spec.precision.unwrap_or(6), spec.grouping),
        NumFmtConversion::Scientific => fmt_scientific(&digits, scale, spec.precision.unwrap_or(6)),
    };
    let mut value = String::new();
    if negative {
        value.push('-');
    }
    value.push_str(&body);
    if let Some(width) = spec.width
        && let Some(pad) = width.checked_sub(value.len())
        && pad > 0
    {
        let mut padded = String::with_capacity(width);
        let sign_len = usize::from(negative);
        padded.push_str(&value[..sign_len]);
        for _ in 0..pad {
            padded.push(if spec.zero_pad { '0' } else { ' ' });
        }
        padded.push_str(&value[sign_len..]);
        value = padded;
    }
    let mut out = String::with_capacity(spec.prefix.len() + value.len() + spec.suffix.len());
    out.push_str(spec.prefix);
    out.push_str(&value);
    out.push_str(spec.suffix);
    Ok(Some(out))
}

/// The (sign, unsigned digit bytes, `scale`) of a number, or `None` for a non-finite binary64. An exact decimal keeps
/// its own coefficient/scale; a binary64 is read through its shortest round-trip spelling first.
fn number_parts(number: &Number) -> Option<(bool, Vec<u8>, i64)> {
    if let Some(integer) = number.to_integer() {
        let text = integer.as_str();
        let (negative, digits) = text.strip_prefix('-').map_or((false, text), |rest| (true, rest));
        return Some((negative, digits.bytes().collect(), 0));
    }
    if let Some(decimal) = number.as_decimal() {
        let text = decimal.coefficient().as_str();
        let (negative, digits) = text.strip_prefix('-').map_or((false, text), |rest| (true, rest));
        return Some((negative, digits.bytes().collect(), decimal.scale()));
    }
    if let Some(float) = number.as_float() {
        let value = float.get();
        if !value.is_finite() {
            return None;
        }
        let text = jqf_data::format_binary64(value)?;
        let decimal = jqf_data::Decimal::parse(text.as_str()).ok()?;
        let digits = decimal.coefficient().as_str();
        let (negative, digits) = digits.strip_prefix('-').map_or((false, digits), |rest| (true, rest));
        return Some((negative, digits.bytes().collect(), decimal.scale()));
    }
    None
}

/// `%d`: the value truncated toward zero to an integer.
///
/// The zero pad is the value's own NEGATIVE scale — an exact-decimal arithmetic result carries one of unbounded
/// magnitude — so the pad is try_reserve-disciplined: exhaustion answers the allocation-failure class instead of
/// aborting the process.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the kept-prefix cut is a subtraction against the digit count already in memory"
)]
fn fmt_integer(digits: &[u8], scale: i64, grouping: bool) -> Result<String, EngineRunError> {
    let mut int_digits: Vec<u8> = if scale > 0 {
        let keep = digits.len().saturating_sub(scale as usize);
        digits[..keep].to_vec()
    } else {
        let mut whole = digits.to_vec();
        let pad = i128::from(scale)
            .checked_neg()
            .ok_or_else(EngineRunError::allocation_failure)
            .and_then(|magnitude| usize::try_from(magnitude).map_err(|_| EngineRunError::allocation_failure()))?;
        whole
            .try_reserve_exact(pad)
            .map_err(|_| EngineRunError::allocation_failure())?;
        whole.resize(whole.len() + pad, b'0');
        whole
    };
    if int_digits.is_empty() {
        int_digits.push(b'0');
    }
    Ok(String::from_utf8(group_if(int_digits, grouping)).unwrap_or_default())
}

/// `%f`: the value at `places` decimal places, rounded half-away-from-zero, with the integer part optionally
/// thousands-grouped.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "the scale-to-places delta is a small fraction count on any real input"
)]
fn fmt_fixed(digits: &[u8], scale: i64, places: usize, grouping: bool) -> String {
    let mut d = digits.to_vec();
    let places_i = places as i64;
    if scale > places_i {
        let delta = (scale - places_i) as usize;
        let keep = d.len().saturating_sub(delta);
        let mut kept = d[..keep].to_vec();
        let dropped = &d[keep..];
        if round_up(dropped, delta) {
            increment_digits(&mut kept);
        }
        d = kept;
    } else {
        d.resize(d.len() + (places_i - scale) as usize, b'0');
    }
    let (mut integer, fraction) = split_fixed(&d, places);
    if integer.is_empty() {
        integer.push(b'0');
    }
    let mut out = String::new();
    for byte in group_if(integer, grouping) {
        out.push(char::from(byte));
    }
    if places > 0 {
        out.push('.');
        for byte in fraction {
            out.push(char::from(byte));
        }
    }
    out
}

/// Splits an already-places-scaled digit run into its integer and fraction halves (the fraction is exactly `places`
/// digits, left-padded with zeros).
fn split_fixed(d: &[u8], places: usize) -> (Vec<u8>, Vec<u8>) {
    if d.len() >= places {
        let split = d.len() - places;
        (d[..split].to_vec(), d[split..].to_vec())
    } else {
        let mut fraction = alloc::vec![b'0'; places - d.len()];
        fraction.extend_from_slice(d);
        (Vec::new(), fraction)
    }
}

/// Inserts a `,` every three digits from the right of an integer digit run.
fn group_if(int_digits: Vec<u8>, grouping: bool) -> Vec<u8> {
    if !grouping {
        return int_digits;
    }
    let mut out = Vec::with_capacity(int_digits.len() + int_digits.len() / 3);
    for (index, &byte) in int_digits.iter().enumerate() {
        if index > 0 && (int_digits.len() - index).is_multiple_of(3) {
            out.push(b',');
        }
        out.push(byte);
    }
    out
}

/// `%e`: `d.ddde±XX` with `places` mantissa fraction digits (default 6), the exponent sign carried and padded to at
/// least two digits.
fn fmt_scientific(digits: &[u8], scale: i64, places: usize) -> String {
    use core::fmt::Write as _;
    let total = places + 1;
    if digits.iter().all(|&byte| byte == b'0') {
        let mut out = String::with_capacity(places + 8);
        out.push_str("0.");
        out.extend(core::iter::repeat_n('0', places));
        out.push_str("e+00");
        return out;
    }
    let mut d = digits.to_vec();
    let mut exponent = d.len() as i128 - i128::from(scale) - 1;
    if d.len() > total {
        let delta = d.len() - total;
        let mut kept = d[..total].to_vec();
        if round_up(&d[total..], delta) {
            increment_digits(&mut kept);
            if kept.len() > total {
                exponent += 1;
            }
        }
        d = kept;
    } else {
        d.resize(total, b'0');
    }
    let mut out = String::with_capacity(places + 8);
    out.push(char::from(d[0]));
    out.push('.');
    for &byte in d.iter().take(1 + places).skip(1) {
        out.push(char::from(byte));
    }
    out.push('e');
    if exponent < 0 {
        out.push('-');
        let _ = write!(out, "{:02}", exponent.unsigned_abs());
    } else {
        out.push('+');
        let _ = write!(out, "{exponent:02}");
    }
    out
}

/// Whether the dropped digit suffix rounds UP under half-away-from-zero: the dropped value is at least `5 *
/// 10^(delta-1)`.
fn round_up(dropped: &[u8], delta: usize) -> bool {
    let mut digits = dropped;
    while let [b'0', rest @ ..] = digits {
        digits = rest;
    }
    if digits.is_empty() {
        return false;
    }
    if digits.len() != delta {
        return digits.len() > delta;
    }
    for (index, &byte) in digits.iter().enumerate() {
        let half = if index == 0 { b'5' } else { b'0' };
        if byte != half {
            return byte > half;
        }
    }
    true
}

/// Adds one to an unsigned digit run, carrying (empty runs become `"1"`).
fn increment_digits(digits: &mut Vec<u8>) {
    let mut index = digits.len();
    loop {
        if index == 0 {
            digits.insert(0, b'1');
            break;
        }
        index -= 1;
        if digits[index] == b'9' {
            digits[index] = b'0';
        } else {
            digits[index] += 1;
            break;
        }
    }
}

/// HMAC-SHA256 (RFC 2104) of `message` keyed by `key`, as raw bytes.
///
/// Implemented directly on the ALREADY-PRESENT `sha2` crate rather than pulling in an `hmac` dependency, for the same
/// reason the family hand-ports hex encoding (see [`hex_encode_bytes`]) and the tree's crypto crates resolve on
/// `digest` 0.11: the same bytes, with none of the version-resolution surface.
/// `pub(crate)` so the redact family's keyed mode reuses the machinery instead of re-implementing it.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; HMAC_SHA256_DIGEST] {
    use sha2::{Digest, Sha256};
    let mut key_pad = [0u8; HMAC_SHA256_BLOCK];
    if key.len() > HMAC_SHA256_BLOCK {
        key_pad[..HMAC_SHA256_DIGEST].copy_from_slice(Sha256::digest(key).as_slice());
    } else {
        key_pad[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    let mut outer = Sha256::new();
    inner.update(key_pad.map(|b| b ^ 0x36));
    outer.update(key_pad.map(|b| b ^ 0x5c));
    inner.update(message);
    let inner_hash = inner.finalize();
    outer.update(inner_hash);
    let mut out = [0u8; HMAC_SHA256_DIGEST];
    out.copy_from_slice(outer.finalize().as_slice());
    out
}

/// The SHA-256 compression-block width — HMAC's ipad/opad size (RFC 2104 §2).
const HMAC_SHA256_BLOCK: usize = 64;

/// The SHA-256 digest width — what a key longer than one block folds down to, and the width of the returned tag.
/// Named so an HMAC-SHA512 sibling cannot inherit a hardcoded 32 and silently truncate.
const HMAC_SHA256_DIGEST: usize = 32;

/// The lowercase hex alphabet, indexed by nibble.
const HEX_ALPHABET: &[u8; 16] = b"0123456789abcdef";

/// Encodes bytes as lowercase hex with a direct nibble table.
pub(crate) fn hex_encode_bytes(bytes: &[u8]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX_ALPHABET[usize::from(byte >> 4)]));
        out.push(char::from(HEX_ALPHABET[usize::from(byte & 0x0f)]));
    }
    out
}

/// Decodes hex in either case, matching the `hex` crate's accept set and its odd-length/invalid-digit refusals.
fn hex_decode_bytes(bytes: &[u8]) -> Result<alloc::vec::Vec<u8>, ()> {
    if !bytes.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = alloc::vec::Vec::new();
    out.try_reserve(bytes.len() / 2).map_err(|_| ())?;
    for pair in bytes.chunks_exact(2) {
        let Some(high) = hex_nibble(pair[0]) else {
            return Err(());
        };
        let Some(low) = hex_nibble(pair[1]) else {
            return Err(());
        };
        out.push(high << 4 | low);
    }
    Ok(out)
}

/// One hex digit's value, accepting both cases like the `hex` crate — the stdlib spelling `jsonpath.rs` already uses.
fn hex_nibble(byte: u8) -> Option<u8> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a base-16 digit is at most 15, so the u32 -> u8 cast cannot truncate"
    )]
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

const HMAC_SHA1_BLOCK: usize = 64;
const HMAC_SHA1_DIGEST: usize = 20;

fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; HMAC_SHA1_DIGEST] {
    use sha1::Digest;
    let mut key_pad = [0u8; HMAC_SHA1_BLOCK];
    if key.len() > HMAC_SHA1_BLOCK {
        key_pad[..HMAC_SHA1_DIGEST].copy_from_slice(sha1::Sha1::digest(key).as_slice());
    } else {
        key_pad[..key.len()].copy_from_slice(key);
    }
    let mut inner = sha1::Sha1::new();
    let mut outer = sha1::Sha1::new();
    inner.update(key_pad.map(|b| b ^ 0x36));
    outer.update(key_pad.map(|b| b ^ 0x5c));
    inner.update(message);
    let inner_hash = inner.finalize();
    outer.update(inner_hash);
    let mut out = [0u8; HMAC_SHA1_DIGEST];
    out.copy_from_slice(outer.finalize().as_slice());
    out
}

const HMAC_SHA512_BLOCK: usize = 128;
const HMAC_SHA512_DIGEST: usize = 64;

fn hmac_sha512(key: &[u8], message: &[u8]) -> [u8; HMAC_SHA512_DIGEST] {
    use sha2::Digest;
    let mut key_pad = [0u8; HMAC_SHA512_BLOCK];
    if key.len() > HMAC_SHA512_BLOCK {
        key_pad[..HMAC_SHA512_DIGEST].copy_from_slice(sha2::Sha512::digest(key).as_slice());
    } else {
        key_pad[..key.len()].copy_from_slice(key);
    }
    let mut inner = sha2::Sha512::new();
    let mut outer = sha2::Sha512::new();
    inner.update(key_pad.map(|b| b ^ 0x36));
    outer.update(key_pad.map(|b| b ^ 0x5c));
    inner.update(message);
    let inner_hash = inner.finalize();
    outer.update(inner_hash);
    let mut out = [0u8; HMAC_SHA512_DIGEST];
    out.copy_from_slice(outer.finalize().as_slice());
    out
}

const BASE64URL_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(bytes: &[u8]) -> alloc::string::String {
    let len = bytes.len();
    let mut out = alloc::string::String::with_capacity(len.div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let triple = u32::from(b0) << 16 | u32::from(b1) << 8 | u32::from(b2);
        out.push(char::from(BASE64URL_ALPHABET[((triple >> 18) & 0x3f) as usize]));
        out.push(char::from(BASE64URL_ALPHABET[((triple >> 12) & 0x3f) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(BASE64URL_ALPHABET[((triple >> 6) & 0x3f) as usize])
        } else {
            break;
        });
        out.push(if chunk.len() > 2 {
            char::from(BASE64URL_ALPHABET[(triple & 0x3f) as usize])
        } else {
            break;
        });
    }
    out
}

#[allow(clippy::cast_possible_truncation)]
fn base64url_decode(input: &[u8]) -> Result<alloc::vec::Vec<u8>, ()> {
    let mut out = alloc::vec::Vec::new();
    out.try_reserve(input.len().div_ceil(4) * 3).map_err(|_| ())?;
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in input {
        if byte == b'=' {
            break;
        }
        let sextet = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err(()),
        };
        buf = (buf << 6) | u32::from(sextet);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

fn percent_encode(s: &str) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(char::from(HEX_ALPHABET[usize::from(byte >> 4)]));
            out.push(char::from(HEX_ALPHABET[usize::from(byte & 0x0f)]));
        }
    }
    out
}

fn percent_decode(s: &str) -> Result<alloc::string::String, ()> {
    let bytes = s.as_bytes();
    let mut out = alloc::vec::Vec::new();
    out.try_reserve(bytes.len()).map_err(|_| ())?;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(());
            }
            let Some(high) = hex_nibble(bytes[i + 1]) else {
                return Err(());
            };
            let Some(low) = hex_nibble(bytes[i + 2]) else {
                return Err(());
            };
            out.push(high << 4 | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

#[allow(clippy::cast_possible_truncation)]
fn base32_encode(bytes: &[u8]) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(bytes.len().div_ceil(5) * 8);
    for chunk in bytes.chunks(5) {
        let mut buf: u64 = 0;
        for &b in chunk {
            buf = (buf << 8) | u64::from(b);
        }
        let byte_count = chunk.len();
        let bit_count = byte_count * 8;
        buf <<= 40 - bit_count;
        let full_quintets = bit_count / 5;
        let remainder_bits = bit_count % 5;
        for i in 0..full_quintets {
            let shift = 35 - i * 5;
            out.push(char::from(BASE32_ALPHABET[((buf >> shift) & 0x1f) as usize]));
        }
        if remainder_bits > 0 {
            let shift = 35 - full_quintets * 5;
            out.push(char::from(BASE32_ALPHABET[((buf >> shift) & 0x1f) as usize]));
        }
        let pad_needed = (8 - (full_quintets + usize::from(remainder_bits > 0))) % 8;
        for _ in 0..pad_needed {
            out.push('=');
        }
    }
    out
}

#[allow(clippy::cast_possible_truncation)]
fn base32_decode(input: &[u8]) -> Result<alloc::vec::Vec<u8>, ()> {
    let mut out = alloc::vec::Vec::new();
    out.try_reserve(input.len().div_ceil(8) * 5).map_err(|_| ())?;
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;
    for &byte in input {
        if byte == b'=' {
            continue;
        }
        let quintet = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a'),
            b'2'..=b'7' => u32::from(byte - b'2' + 26),
            _ => return Err(()),
        };
        buf = (buf << 5) | u64::from(quintet);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

fn quoted_printable_encode(s: &str) -> alloc::string::String {
    let bytes = s.as_bytes();
    let mut out = alloc::string::String::with_capacity(bytes.len());
    let mut line_len: usize = 0;
    for i in 0..bytes.len() {
        if line_len >= 75 {
            out.push('=');
            out.push('\r');
            out.push('\n');
            line_len = 0;
        }
        let byte = bytes[i];
        let encode_this = !(0x20..=0x7e).contains(&byte)
            || byte == b'='
            || (byte == b' ' || byte == b'\t')
                && (i + 1 >= bytes.len() || bytes[i + 1] == b'\r' || bytes[i + 1] == b'\n');
        if encode_this {
            out.push('=');
            out.push(char::from(HEX_ALPHABET[(usize::from(byte) >> 4) & 0xf]));
            out.push(char::from(HEX_ALPHABET[usize::from(byte) & 0xf]));
            line_len += 3;
        } else {
            out.push(byte as char);
            line_len += 1;
        }
    }
    out
}

fn quoted_printable_decode(s: &str) -> Result<alloc::string::String, ()> {
    let bytes = s.as_bytes();
    let mut out = alloc::vec::Vec::new();
    out.try_reserve(bytes.len()).map_err(|_| ())?;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'=' if i + 1 < bytes.len() => {
                if bytes[i + 1] == b'\r' && i + 2 < bytes.len() && bytes[i + 2] == b'\n' {
                    i += 3;
                } else if bytes[i + 1] == b'\n' {
                    i += 2;
                } else {
                    if i + 2 >= bytes.len() {
                        return Err(());
                    }
                    let Some(high) = hex_nibble(bytes[i + 1]) else {
                        return Err(());
                    };
                    let Some(low) = hex_nibble(bytes[i + 2]) else {
                        return Err(());
                    };
                    out.push(high << 4 | low);
                    i += 3;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

#[allow(clippy::cast_possible_truncation)]
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = 0xedb8_8320 ^ (crc >> 1);
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc: u32 = !0;
    for &byte in bytes {
        crc = CRC32_TABLE[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

fn number_input(input: &Value, resources: &ResourceContext<'_>) -> Result<f64, EngineRunError> {
    let Value::Number(number) = input.untagged() else {
        let text = message::number_required(input)?;
        return Err(raise(&text, resources));
    };
    Ok(order::to_f64(number))
}

/// One extension law over its piped subject and its ALREADY-EVALUATED filter arguments, one value per declared
/// parameter in source order.
///
/// Which parameter a law reads is the LAW's own fact, not the executor's:
/// `log/2` takes its VALUE from the first parameter and its base from the second, `round/1,2` its digit count from the
/// last, `hmac` its key, the set laws both of theirs, and the unary hashes none. That mapping lives here so the
/// engine's two drives — the graph machine's lazy per-argument frames and path mode's eager owned run — read ONE
/// table instead of two transcriptions of it, which is what keeps a builtin from behaving differently inside a path
/// expression than outside one.
///
/// The caller owns argument EVALUATION: it runs each parameter's filter over the call's input and calls this law once
/// per COMBINATION of their outputs, so a law here answers for exactly one tuple and never reasons about cardinality.
/// A slot this law needs but the caller did not supply is an arity contract the compiler already fixed, never user
/// input.
///
/// # Errors
///
/// Returns the law's own catchable refusal (a non-array set operand, a non-numeric math operand, an out-of-range
/// quantile) or an allocation failure.
pub fn extension_law(
    law: ExtensionLaw,
    subject: &Value,
    args: &[Value],
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let argument = |index: usize| -> Result<&Value, EngineRunError> {
        args.get(index)
            .ok_or_else(|| EngineRunError::internal_contract("an extension law argument slot is unfilled"))
    };
    let owned = |index: usize| -> Result<Value, EngineRunError> { Ok(argument(index)?.clone()) };
    match law {
        ExtensionLaw::Set(set) => set_law(set, owned(0)?, owned(1)?, resources),
        ExtensionLaw::Uuid(law) => uuid_law(law, subject, resources),
        ExtensionLaw::Hash(law) => hash_law(law, subject, resources),
        ExtensionLaw::Hmac(law) => hmac_law(law, subject, argument(0)?, resources),
        ExtensionLaw::Compress(law) => compression_law(law, subject, resources),
        ExtensionLaw::NumFmt => numfmt_law(subject, argument(0)?, resources),
        ExtensionLaw::Stats(law) => {
            let q = match law {
                StatsLaw::Quantile => {
                    let q = number_input(argument(1)?, resources)?;
                    if !q.is_finite() || !(0.0..=1.0).contains(&q) {
                        return Err(raise("quantile q must be a finite number in [0,1]", resources));
                    }
                    Some(q)
                }
                _ => None,
            };
            stats_law(law, owned(0)?, q, resources)
        }
        ExtensionLaw::Melt => melt_law(
            subject,
            argument(0)?,
            argument(1)?,
            argument(2)?,
            argument(3)?,
            resources,
        ),
        ExtensionLaw::Pivot => pivot_law(
            subject,
            argument(0)?,
            argument(1)?,
            argument(2)?,
            argument(3)?,
            resources,
        ),
        ExtensionLaw::Frequency => frequency_law(&owned(0)?, resources),
        // The four PARAMETERIZED math extensions share ONE argument law, and it is the whole contract: the LAST
        // parameter is the modifier (`log`'s base, `round`'s digit count), and the VALUE is the FIRST parameter at
        // arity 2 and the SUBJECT at arity 1. EVERY parameter is read — `log(2)` on `8` is `3`, not `log10(8)`, and
        // `round(3.14159;2)` is `3.14` whatever the subject is: a discarded argument is the one failure mode whose
        // examples can all pass by accident, because every registered example agrees with a law that ignores the
        // parameter.
        // Every other math extension is unary over the subject.
        ExtensionLaw::Math(
            law @ (MathExtLaw::LogOne | MathExtLaw::LogTwo | MathExtLaw::RoundOne | MathExtLaw::RoundTwo),
        ) => {
            let parameterized = matches!(law, MathExtLaw::LogTwo | MathExtLaw::RoundTwo);
            let value = if parameterized {
                number_input(argument(0)?, resources)?
            } else {
                number_input(subject, resources)?
            };
            let modifier = number_input(argument(usize::from(parameterized))?, resources)?;
            let result = if matches!(law, MathExtLaw::LogOne | MathExtLaw::LogTwo) {
                log_base(value, modifier)
            } else {
                // C's truncation is the law (`round(1.5; 1.9)` rounds to one digit).
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "round's digit count is C's double->int truncation by pinned law"
                )]
                let digits = modifier as i64;
                round_digits(value, digits)
            };
            Ok(Value::Number(Number::float(jqf_data::Float::new(result))))
        }
        ExtensionLaw::Math(law) => math_ext_law(law, subject, resources),
        #[cfg(feature = "ext-redact")]
        ExtensionLaw::Redact(law) => super::redact::redact_law(law, subject, args, resources),
        #[cfg(feature = "ext-fuzzy")]
        ExtensionLaw::Fuzzy(law) => super::fuzzy::fuzzy_law(law, subject, args, resources),
    }
}

/// One unary math-extension law over a numeric input.
pub fn math_ext_law(law: MathExtLaw, input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let result = match law {
        MathExtLaw::E => core::f64::consts::E,
        MathExtLaw::Pi => core::f64::consts::PI,
        MathExtLaw::Tau => core::f64::consts::TAU,
        MathExtLaw::Degrees => number_input(input, resources)?.to_degrees(),
        MathExtLaw::Radians => number_input(input, resources)?.to_radians(),
        MathExtLaw::Pow10 => 10.0_f64.powf(number_input(input, resources)?),
        MathExtLaw::Recip => 1.0 / number_input(input, resources)?,
        MathExtLaw::RoundEven => number_input(input, resources)?.round_ties_even(),
        MathExtLaw::Signum => {
            let value = number_input(input, resources)?;
            if value == 0.0 { 0.0 } else { value.signum() }
        }
        MathExtLaw::Fract => {
            let value = number_input(input, resources)?;
            value - value.trunc()
        }
        MathExtLaw::LogOne | MathExtLaw::LogTwo | MathExtLaw::RoundOne | MathExtLaw::RoundTwo => {
            return Err(EngineRunError::internal_contract(
                "argument-taking math extension reached the unary law",
            ));
        }
    };
    Ok(Value::Number(Number::float(jqf_data::Float::new(result))))
}

/// `round/1,2`: round half away from zero to `digits` places.
pub fn round_digits(value: f64, digits: i64) -> f64 {
    let factor = 10.0_f64.powi(i32::try_from(digits).unwrap_or(i32::MAX));
    (value * factor).round() / factor
}

/// `log/2`: `log(value; base)`.
///
/// Base 10 and base 2 route to libm's own `log10`/`log2` rather than to the generic `ln(x)/ln(base)` ratio, because the
/// ratio loses the last digit on exact powers: `1000 | log(10)` is `2.9999999999999996` through the ratio and `3`
/// through `log10`. The reference's `log10` is the direct call, so the ratio is a DIVERGENCE, not a rounding taste. The
/// reference reproduces jqf's old answer exactly when spelled the same ratio way (`1000 | log/(10|log)`), which is what
/// identified the mechanism.
#[allow(
    clippy::float_cmp,
    reason = "the exactness IS the condition: only a base that IS 10.0 (or 2.0) may take the \
              direct call, and a base near-but-not-equal must keep the general ratio, so an \
              error margin would silently reroute it"
)]
pub fn log_base(value: f64, base: f64) -> f64 {
    if base <= 0.0 {
        f64::NAN
    } else if base == 10.0 {
        value.log10()
    } else if base == 2.0 {
        value.log2()
    } else {
        value.log(base)
    }
}

/// One stats law over an ARRAY source: numeric members are collected (converted to f64), an empty result answers
/// `null`, and the law's value is computed per the law: population variance, an f64 midpoint median, a
/// linear-interpolated quantile.
///
/// The casts are the law, not accidents: the arithmetic is f64 by design, the array length is bounded far below 2^52
/// for any real input, and the quantile position rounds toward the sorted neighbours.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::needless_pass_by_value,
    reason = "the stats laws are f64 arithmetic by design; len() can never approach the f64 \
              mantissa's 2^52 on a real array, the quantile index rounding is the law, and the \
              array source is read through a borrowed untag while the caller keeps the owned value"
)]
pub fn stats_law(
    law: StatsLaw,
    source: Value,
    q: Option<f64>,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let name = match law {
        StatsLaw::Sum => "sum",
        StatsLaw::Avg => "avg",
        StatsLaw::Median => "median",
        StatsLaw::Quantile => "quantile",
        StatsLaw::Stddev => "stddev",
        StatsLaw::Variance => "variance",
        StatsLaw::Count => "count",
    };
    let Value::Array(array) = source.untagged() else {
        // The hint is one clause on every stats-law mismatch: on a RECORD stream the filter runs once per record, so a
        // stream-counting idiom like `count(select(...))` fails per record with no clue why — the fix is to collect
        // the records first (`-s`), then apply the law to the whole array. The stream-counting idiom errors once per
        // record, exit 5.
        return Err(raise(
            &format!(
                "{name} expects an array (on a record stream the program runs \
                 once per record — collect the records first with -s, then \
                 apply {name} to the whole array)"
            ),
            resources,
        ));
    };
    if let StatsLaw::Count = law {
        return Ok(Value::Number(Number::integer(jqf_data::Integer::from_i64(
            i64::try_from(array.len()).unwrap_or(i64::MAX),
        ))));
    }
    let mut values: Vec<f64> = Vec::new();
    for item in array {
        if let Value::Number(number) = item.untagged() {
            values.push(order::to_f64(number));
        }
    }
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let result = match law {
        StatsLaw::Sum => values.into_iter().sum(),
        StatsLaw::Avg => values.iter().sum::<f64>() / values.len() as f64,
        StatsLaw::Variance => {
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            values
                .iter()
                .map(|value| {
                    let delta = value - mean;
                    delta * delta
                })
                .sum::<f64>()
                / values.len() as f64
        }
        StatsLaw::Stddev => {
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            (values
                .iter()
                .map(|value| {
                    let delta = value - mean;
                    delta * delta
                })
                .sum::<f64>()
                / values.len() as f64)
                .sqrt()
        }
        StatsLaw::Median => {
            values.sort_by(f64::total_cmp);
            let mid = values.len() / 2;
            if values.len().is_multiple_of(2) {
                f64::midpoint(values[mid - 1], values[mid])
            } else {
                values[mid]
            }
        }
        StatsLaw::Quantile => {
            values.sort_by(f64::total_cmp);
            let q = q.expect("quantile q is provided by the caller");
            if values.len() == 1 {
                values[0]
            } else {
                let position = (values.len() - 1) as f64 * q;
                let lower = position.floor() as usize;
                let upper = position.ceil() as usize;
                if lower == upper {
                    values[lower]
                } else {
                    let fraction = position - lower as f64;
                    values[lower] + (values[upper] - values[lower]) * fraction
                }
            }
        }
        StatsLaw::Count => unreachable!("count handled before numeric collection"),
    };
    Ok(Value::Number(Number::float(jqf_data::Float::new(result))))
}

/// A direct core string. Tagged strings are not unwrapped: a tag is an intrinsic of that node, so a tagged string is
/// not a key name.
fn core_string(value: &Value) -> Option<&str> {
    match value {
        Value::String(text) => Some(text.as_str()),
        _ => None,
    }
}

fn array_expected(name: &str, resources: &ResourceContext<'_>) -> EngineRunError {
    raise(
        &format!(
            "{name} expects an array (on a record stream the program runs \
             once per record — collect the records first with -s, then \
             apply {name} to the whole array)"
        ),
        resources,
    )
}

fn expect_array<'a>(
    value: &'a Value,
    name: &str,
    resources: &ResourceContext<'_>,
) -> Result<&'a Array, EngineRunError> {
    match value.untagged() {
        Value::Array(array) => Ok(array),
        _ => Err(array_expected(name, resources)),
    }
}

fn expect_object<'a>(
    value: &'a Value,
    name: &str,
    resources: &ResourceContext<'_>,
) -> Result<&'a Object, EngineRunError> {
    match value.untagged() {
        Value::Object(object) => Ok(object),
        _ => Err(raise(&format!("{name} expects an array of objects"), resources)),
    }
}

fn expect_string<'a>(value: &'a Value, name: &str, resources: &ResourceContext<'_>) -> Result<&'a str, EngineRunError> {
    core_string(value).ok_or_else(|| raise(&format!("{name} must be a string"), resources))
}

fn expect_string_list<'a>(
    value: &'a Value,
    name: &str,
    resources: &ResourceContext<'_>,
) -> Result<Vec<&'a str>, EngineRunError> {
    let Value::Array(array) = value.untagged() else {
        return Err(raise(&format!("{name} must be an array of strings"), resources));
    };
    let mut out = Vec::new();
    for item in array {
        let Some(text) = core_string(item) else {
            return Err(raise(&format!("{name} must be an array of strings"), resources));
        };
        out.push(text);
    }
    Ok(out)
}

fn insert_field(builder: &mut ObjectBuilder, name: &str, value: Value) -> Result<(), EngineRunError> {
    let key = ObjectKey::try_from_str(name).map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(key, value)
        .map_err(|_| EngineRunError::allocation_failure())
}

fn finish_object(builder: ObjectBuilder, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    builder
        .try_finish()
        .map(Value::Object)
        .map_err(|_| EngineRunError::allocation_failure())
}

fn finish_array(values: Vec<Value>, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    Array::try_from_vec(values)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// `melt($id_keys; $value_keys; $var_name; $value_name)`: wide→long.
///
/// The piped subject is an array of objects. Each row emits one output object per value key that is present (a missing
/// value key is skipped). An output object is the id keys copied (a missing id key becomes `null`) plus `{($var_name):
/// field_name, ($value_name): field_value}`. Row order is input order × `$value_keys` order. An empty subject answers
/// `[]`.
fn melt_law(
    subject: &Value,
    id_keys: &Value,
    value_keys: &Value,
    var_name: &Value,
    value_name: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let array = expect_array(subject, "melt", resources)?;
    let id_keys = expect_string_list(id_keys, "melt id keys", resources)?;
    let value_keys = expect_string_list(value_keys, "melt value keys", resources)?;
    let var_name = expect_string(var_name, "melt var name", resources)?;
    let value_name = expect_string(value_name, "melt value name", resources)?;
    let mut out = Vec::new();
    out.try_reserve(array.len().saturating_mul(value_keys.len()))
        .map_err(|_| EngineRunError::allocation_failure())?;
    for row in array {
        let object = expect_object(row, "melt", resources)?;
        for value_key in &value_keys {
            let Some(field_value) = object.get(value_key) else {
                continue;
            };
            let mut builder = ObjectBuilder::try_with_capacity(id_keys.len() + 2)
                .map_err(|_| EngineRunError::allocation_failure())?;
            for id_key in &id_keys {
                insert_field(&mut builder, id_key, object.get(id_key).cloned().unwrap_or(Value::Null))?;
            }
            insert_field(
                &mut builder,
                var_name,
                Value::try_string(value_key).map_err(|_| EngineRunError::allocation_failure())?,
            )?;
            insert_field(&mut builder, value_name, field_value.clone())?;
            out.push(finish_object(builder, resources)?);
        }
    }
    finish_array(out, resources)
}

fn id_tuple_eq(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(a, b)| semantic_eq(a, b).unwrap_or(false))
}

struct PivotGroup {
    ids: Vec<Value>,
    cells: Vec<Option<Value>>,
}

struct FrequencyRow {
    value: Value,
    count: i64,
}

/// `pivot($id_keys; $var_name; $value_name; $value_keys)`: long→wide.
///
/// Groups by the id-key tuple under `semantic_eq`, first-seen group order.
/// A duplicate `(id, var)` keeps the last value, the same last-write law `INDEX` uses. `$value_keys` as an array of
/// strings is the wide-column order; `null` discovers columns in first-seen var order. A missing cell is `null`.
///
/// The fast path buckets rows by sorting row indices under the tuple `total_cmp` — the keyed-primitive law
/// (`semantics::keyed`, the join index): for NaN-free values, `total_cmp == Equal` IS `semantic_eq`, first-seen group
/// order comes from each equal-key run's minimum original position, and every decline (a carried NaN anywhere in an id
/// tuple, a comparison past the depth ceiling) falls back to the linear walk, whose answer is the answer.
/// One pivot input's per-row plan: every row's flat id tuple (row-major at stride `id_key_names.len()`), the column
/// each row will write (`None`: this row writes nothing), and the discovered columns in first-seen var order.
/// `None` from the planner is the DECLINE — an id tuple carried a NaN, so the bucketed law cannot speak and the
/// linear walk answers.
struct PivotPlan<'a> {
    flat_ids: Vec<Value>,
    cells_of_row: Vec<Option<usize>>,
    discovered: Vec<&'a str>,
}

fn plan_pivot_rows<'a>(
    array: &'a Array,
    id_key_names: &[&str],
    var_key: &str,
    specified: bool,
    specified_columns: &[&str],
    resources: &ResourceContext<'_>,
) -> Result<Option<PivotPlan<'a>>, EngineRunError> {
    let stride = id_key_names.len();
    let mut flat_ids: Vec<Value> = Vec::new();
    if flat_ids.try_reserve_exact(array.len() * stride).is_err() {
        return Err(EngineRunError::allocation_failure());
    }
    let mut cells_of_row: Vec<Option<usize>> = Vec::new();
    if cells_of_row.try_reserve_exact(array.len()).is_err() {
        return Err(EngineRunError::allocation_failure());
    }
    let mut discovered: Vec<&'a str> = Vec::new();
    let mut discovered_index: BTreeMap<&str, usize> = BTreeMap::new();
    for row in array {
        let object = expect_object(row, "pivot", resources)?;
        for key in id_key_names {
            let id = object.get(key).cloned().unwrap_or(Value::Null);
            if order::carries_nan(&id) != Ok(false) {
                return Ok(None);
            }
            flat_ids.push(id);
        }
        let column = match object.get(var_key) {
            None => None,
            Some(var_value) => {
                let Some(var) = core_string(var_value) else {
                    return Err(raise("pivot var must be a string", resources));
                };
                if specified {
                    specified_columns.iter().position(|name| *name == var)
                } else {
                    let next = discovered.len();
                    let index = match discovered_index.entry(var) {
                        Entry::Occupied(slot) => *slot.get(),
                        Entry::Vacant(slot) => {
                            slot.insert(next);
                            discovered.push(var);
                            next
                        }
                    };
                    Some(index)
                }
            }
        };
        cells_of_row.push(column);
    }
    Ok(Some(PivotPlan {
        flat_ids,
        cells_of_row,
        discovered,
    }))
}

/// The bucketed pivot's grouping: one group per distinct id tuple in first-seen order, beside each row's group index
/// for the replay pass.
struct PivotBuckets {
    groups: Vec<PivotGroup>,
    group_of_row: Vec<usize>,
}

/// Buckets planned rows into groups: row indices sorted under the lexicographic tuple `total_cmp`, equal-key runs
/// folded into one [`PivotGroup`] each in FIRST-SEEN order (a stable sort keeps each run's earliest original row
/// first), beside the row→group index the replay writes through. `None` is the DECLINE — a comparison passed the
/// depth ceiling, so no order was invented and the linear walk answers.
fn bucket_pivot_groups(
    flat_ids: &[Value],
    row_count: usize,
    stride: usize,
    column_count: usize,
) -> Result<Option<PivotBuckets>, EngineRunError> {
    let mut row_order: Vec<usize> = Vec::new();
    if row_order.try_reserve_exact(row_count).is_err() {
        return Err(EngineRunError::allocation_failure());
    }
    row_order.extend(0..row_count);
    let mut too_deep = false;
    row_order.sort_by(|&a, &b| {
        let left = &flat_ids[a * stride..a * stride + stride];
        let right = &flat_ids[b * stride..b * stride + stride];
        for (l, r) in left.iter().zip(right) {
            match total_cmp(l, r) {
                Ok(Ordering::Equal) => {}
                Ok(other) => return other,
                Err(_) => {
                    too_deep = true;
                    return Ordering::Equal;
                }
            }
        }
        Ordering::Equal
    });
    if too_deep {
        return Ok(None);
    }

    // Walk equal-key runs; each run's FIRST member is its earliest original row (the sort is stable), so ordering the
    // groups by that head restores first-seen group order. The sort just proved this weak order total over NaN-free
    // tuples, so an adjacency compare cannot raise here.
    let mut run_heads: Vec<usize> = Vec::new();
    if run_heads.try_reserve_exact(row_count).is_err() {
        return Err(EngineRunError::allocation_failure());
    }
    for (index, &row) in row_order.iter().enumerate() {
        let starts_run = index == 0 || {
            let (prev, curr) = (row_order[index - 1] * stride, row * stride);
            let (left, right) = (&flat_ids[prev..prev + stride], &flat_ids[curr..curr + stride]);
            !left
                .iter()
                .zip(right)
                .all(|(l, r)| total_cmp(l, r) == Ok(Ordering::Equal))
        };
        if starts_run {
            run_heads.push(index);
        }
    }

    let mut groups: Vec<PivotGroup> = Vec::new();
    if groups.try_reserve_exact(run_heads.len()).is_err() {
        return Err(EngineRunError::allocation_failure());
    }
    // Runs are contiguous in the sorted-key space; the GROUPS themselves go out in first-seen order, so visit the runs
    // by ascending original row of each run's head.
    let mut head_order: Vec<usize> = Vec::new();
    if head_order.try_reserve_exact(run_heads.len()).is_err() {
        return Err(EngineRunError::allocation_failure());
    }
    head_order.extend(0..run_heads.len());
    head_order.sort_by_key(|&slot| row_order[run_heads[slot]]);
    let mut group_of_row: Vec<usize> = vec![0; row_count];
    for (group_index, &head_slot) in head_order.iter().enumerate() {
        let head_position = run_heads[head_slot];
        let head_row = row_order[head_position];
        let ids = flat_ids[head_row * stride..head_row * stride + stride].to_vec();
        groups.push(PivotGroup {
            ids,
            cells: vec![None; column_count],
        });
        let run_end = run_heads.get(head_slot + 1).copied().unwrap_or(row_order.len());
        for position in head_position..run_end {
            group_of_row[row_order[position]] = group_index;
        }
    }
    Ok(Some(PivotBuckets { groups, group_of_row }))
}

fn pivot_law(
    subject: &Value,
    id_keys: &Value,
    var_name: &Value,
    value_name: &Value,
    value_keys: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let array = expect_array(subject, "pivot", resources)?;
    let id_key_names = expect_string_list(id_keys, "pivot id keys", resources)?;
    let var_key = expect_string(var_name, "pivot var name", resources)?;
    let value_key = expect_string(value_name, "pivot value name", resources)?;
    let specified = !matches!(value_keys, Value::Null);
    let specified_columns: Vec<&str> = if specified {
        expect_string_list(value_keys, "pivot value keys", resources)?
    } else {
        Vec::new()
    };

    // Pass 1 — validate and plan every row IN INPUT ORDER, raising exactly where the linear walk raises. Each row
    // contributes its id tuple (the same `.cloned().unwrap_or(Null)` extraction) and, when its var resolves, the column
    // index it will write.
    let Some(plan) = plan_pivot_rows(array, &id_key_names, var_key, specified, &specified_columns, resources)? else {
        return pivot_linear(subject, id_keys, var_name, value_name, value_keys, resources);
    };

    let column_count = if specified {
        specified_columns.len()
    } else {
        plan.discovered.len()
    };

    // Bucket rows into first-seen-order groups over the NaN-free id tuples.
    let Some(buckets) = bucket_pivot_groups(&plan.flat_ids, array.len(), id_key_names.len(), column_count)? else {
        return pivot_linear(subject, id_keys, var_name, value_name, value_keys, resources);
    };
    let mut groups = buckets.groups;

    // Pass 2 — replay the writes in input order: the last `(id, var)` wins, exactly the law the single-pass walk
    // executed.
    for (row_index, row) in array.iter().enumerate() {
        let Some(column) = plan.cells_of_row[row_index] else {
            continue;
        };
        let object = expect_object(row, "pivot", resources)?;
        groups[buckets.group_of_row[row_index]].cells[column] =
            Some(object.get(value_key).cloned().unwrap_or(Value::Null));
    }

    let columns: &[&str] = if specified {
        &specified_columns
    } else {
        &plan.discovered
    };
    finish_pivot_groups(groups, &id_key_names, columns, resources)
}

/// The shared pivot emit: one object per group, the id keys first (aligned 1:1 with the group's stored ids), then the
/// wide columns in order; a missing cell is `null`.
fn finish_pivot_groups(
    groups: Vec<PivotGroup>,
    id_key_names: &[&str],
    columns: &[&str],
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut out = Vec::new();
    out.try_reserve_exact(groups.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for group in groups {
        let mut builder = ObjectBuilder::try_with_capacity(id_key_names.len() + columns.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        for (id_key, id_value) in id_key_names.iter().copied().zip(group.ids) {
            insert_field(&mut builder, id_key, id_value)?;
        }
        for (column, cell) in columns.iter().copied().zip(group.cells) {
            insert_field(&mut builder, column, cell.unwrap_or(Value::Null))?;
        }
        out.push(finish_object(builder, resources)?);
    }
    finish_array(out, resources)
}

/// The linear pivot walk — the bucketed path's decline target, kept verbatim.
#[allow(clippy::too_many_arguments)]
fn pivot_linear(
    subject: &Value,
    id_keys: &Value,
    var_name: &Value,
    value_name: &Value,
    value_keys: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let array = expect_array(subject, "pivot", resources)?;
    let id_keys = expect_string_list(id_keys, "pivot id keys", resources)?;
    let var_name = expect_string(var_name, "pivot var name", resources)?;
    let value_name = expect_string(value_name, "pivot value name", resources)?;
    let specified = !matches!(value_keys, Value::Null);
    let mut columns: Vec<String> = if specified {
        expect_string_list(value_keys, "pivot value keys", resources)?
            .into_iter()
            .map(String::from)
            .collect()
    } else {
        Vec::new()
    };
    let mut groups: Vec<PivotGroup> = Vec::new();
    for row in array {
        let object = expect_object(row, "pivot", resources)?;
        let ids: Vec<Value> = id_keys
            .iter()
            .map(|key| object.get(key).cloned().unwrap_or(Value::Null))
            .collect();
        let group_index = if let Some(index) = groups.iter().position(|group| id_tuple_eq(&group.ids, &ids)) {
            index
        } else {
            let index = groups.len();
            groups.push(PivotGroup {
                ids,
                cells: vec![None; columns.len()],
            });
            index
        };
        let Some(var_value) = object.get(var_name) else {
            continue;
        };
        let Some(var) = core_string(var_value) else {
            return Err(raise("pivot var must be a string", resources));
        };
        let column = if let Some(index) = columns.iter().position(|name| name == var) {
            index
        } else if specified {
            continue;
        } else {
            let index = columns.len();
            columns.push(String::from(var));
            for group in &mut groups {
                group.cells.push(None);
            }
            index
        };
        groups[group_index].cells[column] = Some(object.get(value_name).cloned().unwrap_or(Value::Null));
    }
    let mut out = Vec::new();
    out.try_reserve_exact(groups.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for group in groups {
        let mut builder = ObjectBuilder::try_with_capacity(id_keys.len() + columns.len())
            .map_err(|_| EngineRunError::allocation_failure())?;
        for (id_key, id_value) in id_keys.iter().zip(group.ids) {
            insert_field(&mut builder, id_key, id_value)?;
        }
        for (column, cell) in columns.iter().zip(group.cells) {
            insert_field(&mut builder, column, cell.unwrap_or(Value::Null))?;
        }
        out.push(finish_object(builder, resources)?);
    }
    finish_array(out, resources)
}

/// `frequency($source)`: one `{"value":…,"count":N}` per distinct source element. Distinctness is `semantic_eq`, so
/// two NaNs stay two rows. Empty answers `[]`. Order is descending count, then `total_cmp` on the value.
///
/// The fast path buckets by sorting cloned elements under [`total_cmp`] — the keyed-primitive law
/// (`semantics::keyed`, the join index): for NaN-free values, `total_cmp == Equal` IS `semantic_eq`, so equal-key runs
/// are exactly the linear walk's distinct rows. Every decline (a carried NaN anywhere in the source, a comparison past
/// the depth ceiling) falls back to the linear walk, whose answer is the answer.
fn frequency_law(source: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let array = expect_array(source, "frequency", resources)?;
    for item in array {
        if order::carries_nan(item) != Ok(false) {
            return frequency_linear(array, resources);
        }
    }
    // Clone EVERY element before sorting so no entry shares an allocation with a source slot: the semantic_eq identity
    // short-circuit must never see a pair the linear walk would have compared structurally.
    let mut sorted: Vec<Value> = Vec::new();
    sorted
        .try_reserve_exact(array.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for item in array {
        sorted.push(item.clone());
    }
    let mut too_deep = false;
    sorted.sort_by(|left, right| {
        if let Ok(ordering) = total_cmp(left, right) {
            ordering
        } else {
            too_deep = true;
            Ordering::Equal
        }
    });
    if too_deep {
        return frequency_linear(array, resources);
    }
    let mut rows: Vec<FrequencyRow> = Vec::new();
    rows.try_reserve_exact(sorted.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    let mut index = 0;
    while index < sorted.len() {
        let value = sorted[index].clone();
        let mut count = 0i64;
        while index < sorted.len() && total_cmp(&sorted[index], &value) == Ok(Ordering::Equal) {
            index += 1;
            count += 1;
        }
        rows.push(FrequencyRow { value, count });
    }
    rows.sort_by(|left, right| match right.count.cmp(&left.count) {
        Ordering::Equal => total_cmp(&left.value, &right.value).unwrap_or(Ordering::Equal),
        other => other,
    });
    let mut out = Vec::new();
    out.try_reserve_exact(rows.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for row in rows {
        let mut builder = ObjectBuilder::try_with_capacity(2).map_err(|_| EngineRunError::allocation_failure())?;
        insert_field(&mut builder, "value", row.value)?;
        insert_field(
            &mut builder,
            "count",
            Value::Number(Number::integer(jqf_data::Integer::from_i64(row.count))),
        )?;
        out.push(finish_object(builder, resources)?);
    }
    finish_array(out, resources)
}

/// The linear frequency walk — the bucketed path's decline target, kept verbatim.
fn frequency_linear(array: &Array, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut rows: Vec<FrequencyRow> = Vec::new();
    for item in array {
        if let Some(existing) = rows
            .iter_mut()
            .find(|row| semantic_eq(&row.value, item).unwrap_or(false))
        {
            existing.count = existing.count.saturating_add(1);
        } else {
            rows.push(FrequencyRow {
                value: item.clone(),
                count: 1,
            });
        }
    }
    rows.sort_by(|left, right| match right.count.cmp(&left.count) {
        Ordering::Equal => total_cmp(&left.value, &right.value).unwrap_or(Ordering::Equal),
        other => other,
    });
    let mut out = Vec::new();
    out.try_reserve_exact(rows.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for row in rows {
        let mut builder = ObjectBuilder::try_with_capacity(2).map_err(|_| EngineRunError::allocation_failure())?;
        insert_field(&mut builder, "value", row.value)?;
        insert_field(
            &mut builder,
            "count",
            Value::Number(Number::integer(jqf_data::Integer::from_i64(row.count))),
        )?;
        out.push(finish_object(builder, resources)?);
    }
    finish_array(out, resources)
}

// ------------------------------------------------------------------------
// Registry records.

const fn family(id: u16, name: &'static str, summary: &'static str, detail: &'static str) -> BuiltinFamilyRecord {
    BuiltinFamilyRecord {
        id: BuiltinFamilyId::new(id),
        canonical_name: name,
        category: "jqf-extension",
        summary,
        detail,
    }
}

const ONE_FILTER: &[ParameterKind] = &[ParameterKind::Filter];
const TWO_FILTERS: &[ParameterKind] = &[ParameterKind::Filter, ParameterKind::Filter];
const FOUR_FILTERS: &[ParameterKind] = &[
    ParameterKind::Filter,
    ParameterKind::Filter,
    ParameterKind::Filter,
    ParameterKind::Filter,
];

const fn example(program: &'static str, input: &'static str, expected: &'static str) -> BuiltinExample {
    BuiltinExample {
        program,
        input,
        expected,
    }
}

const fn overload0(
    id: u16,
    family_id: u16,
    name: &'static str,
    examples: &'static [BuiltinExample],
    impure: bool,
) -> BuiltinOverloadRecord {
    BuiltinOverloadRecord {
        id: BuiltinOverloadId::new(id),
        family: BuiltinFamilyId::new(family_id),
        canonical_name: name,
        arity: 0,
        parameters: &[],
        execution: BuiltinExecution::Evaluator,
        demand_transfer: DemandTransfer::Subtree,
        semantic_revision: SemanticRevision::new(1),
        effects: if impure { Effects::Impure } else { Effects::Pure },
        examples,
    }
}

const fn overload_filter(
    id: u16,
    family_id: u16,
    name: &'static str,
    arity: u8,
    parameters: &'static [ParameterKind],
    examples: &'static [BuiltinExample],
    impure: bool,
) -> BuiltinOverloadRecord {
    BuiltinOverloadRecord {
        id: BuiltinOverloadId::new(id),
        family: BuiltinFamilyId::new(family_id),
        canonical_name: name,
        arity,
        parameters,
        execution: BuiltinExecution::Evaluator,
        demand_transfer: DemandTransfer::Subtree,
        semantic_revision: SemanticRevision::new(1),
        effects: if impure { Effects::Impure } else { Effects::Pure },
        examples,
    }
}

/// The analytics law over the piped array.
///
/// `sample` draws WITHOUT replacement by partial Fisher-Yates on a cloned entry vector (a reservoir with a membership
/// set would be cheaper in memory for tiny draws but needs a hash structure this module does not carry); `shuffle` is
/// the same walk over the whole vector. Both are IMPURE effects — each draw runs through [`with_prng`], which draws
/// from fresh uuid v4 entropy UNLESS the host primed `--seed`, in which case two runs with the same seed answer
/// identically (the CLI's reproducibility contract; the engine's own deterministic-output contract still does not cover
/// the unseeded default). `fill_forward` is a pure copy law: every `null` element is replaced by the nearest preceding
/// non-null, and a leading run of nulls stays null (qsv's fill / mlr's fill-down law).
pub fn analytics_law(
    law: AnalyticsLaw,
    input: &Value,
    count: Option<&Value>,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let Value::Array(array) = input.untagged() else {
        let name = match law {
            AnalyticsLaw::Sample => "sample",
            AnalyticsLaw::Shuffle => "shuffle",
            AnalyticsLaw::FillForward => "fill_forward",
        };
        return Err(raise(&format!("{name} expects an array"), resources));
    };
    match law {
        AnalyticsLaw::Sample => {
            let n = match count {
                Some(Value::Number(number)) => number
                    .to_i64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| raise("sample count must be a nonnegative integer", resources))?,
                _ => {
                    return Err(raise("sample count must be a nonnegative integer", resources));
                }
            };
            let n = n.min(array.len());
            if n <= array.len() / 2 {
                // A small draw: Floyd's distinct-position selection over the BORROWED array — O(n log n), no full
                // clone. A full-copy Fisher-Yates would cost O(len) to draw ten elements, which the slice-vs-sample
                // comparison showed at ~450 ms over a 1M-row array.
                let drawn = with_prng(resources, |rng| draw_positions(rng, array.len(), n))?;
                let mut out = Vec::new();
                out.try_reserve_exact(n)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                for position in drawn {
                    out.push(
                        array
                            .get(position)
                            .map_or_else(|| Err(EngineRunError::allocation_failure()), |value| Ok(value.clone()))?,
                    );
                }
                return Array::try_from_vec(out)
                    .map(Value::Array)
                    .map_err(|_| EngineRunError::allocation_failure());
            }
            // A draw of most of the array: partial Fisher-Yates on a full clone (the clone is what the draw would need
            // anyway).
            let mut entries = clone_entries(array)?;
            with_prng(resources, |rng| {
                for index in 0..n {
                    let position = index + rng.below(entries.len() - index);
                    entries.swap(index, position);
                }
            });
            entries.truncate(n);
            Array::try_from_vec(entries)
                .map(Value::Array)
                .map_err(|_| EngineRunError::allocation_failure())
        }
        AnalyticsLaw::Shuffle => {
            let mut entries = clone_entries(array)?;
            with_prng(resources, |rng| {
                for index in (1..entries.len()).rev() {
                    let position = rng.below(index + 1);
                    entries.swap(index, position);
                }
            });
            Array::try_from_vec(entries)
                .map(Value::Array)
                .map_err(|_| EngineRunError::allocation_failure())
        }
        AnalyticsLaw::FillForward => {
            let mut out = Vec::new();
            out.try_reserve_exact(array.len())
                .map_err(|_| EngineRunError::allocation_failure())?;
            // `last` borrows from the input, which outlives the walk, so a non-null entry is cloned once (for the
            // output) instead of twice (output plus carry). Only a null actually needs the carry cloned.
            let mut last: Option<&Value> = None;
            for entry in array {
                if matches!(entry.untagged(), Value::Null) {
                    out.push(match last {
                        Some(filled) => filled.clone(),
                        None => Value::Null,
                    });
                } else {
                    out.push(entry.clone());
                    last = Some(entry);
                }
            }
            Array::try_from_vec(out)
                .map(Value::Array)
                .map_err(|_| EngineRunError::allocation_failure())
        }
    }
}

/// Clones every entry of a borrowed array into a fresh owned vector.
///
/// Takes no [`ResourceContext`]: the clones are accounted by `try_clone` and the vector by `try_reserve_exact`, so
/// there is nothing left for a context to meter here.
fn clone_entries(array: &jqf_data::Array) -> Result<Vec<Value>, EngineRunError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(array.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for entry in array {
        entries.push(entry.clone());
    }
    Ok(entries)
}

/// Draws `n` distinct positions in `0..len`, uniformly — Floyd's selection. The sole caller is the owned `sample` arm
/// above.
///
/// Both reservations report failure rather than degrading into unchecked growth: the draw runs inside the request's
/// accounting, so an exhausted budget has to surface as the family's ordinary allocation error.
pub fn draw_positions(rng: &mut Prng, len: usize, n: usize) -> Result<Vec<usize>, EngineRunError> {
    let mut drawn = Vec::new();
    drawn
        .try_reserve_exact(n)
        .map_err(|_| EngineRunError::allocation_failure())?;
    let mut seen = alloc::collections::BTreeSet::new();
    for step in len - n..len {
        let position = rng.below(step + 1);
        if seen.insert(position) {
            drawn.push(position);
        } else {
            drawn.push(step);
            seen.insert(step);
        }
    }
    Ok(drawn)
}

/// The rand-family law over its already-evaluated arguments.
///
/// The unseeded forms (`Uniform`, `RandintOne`, `RandintTwo`, `Choice`) are IMPURE effects — each draw runs through
/// [`with_prng`], which draws from fresh uuid v4 entropy UNLESS the host primed `--seed`, in which case two runs with
/// the same seed answer identically. `UniformSeeded` (`rand(seed)`) is a separate, deliberately PURE exception: it
/// seeds a one-shot xoshiro256** from its own integer ARGUMENT rather than the request's draw state, so the same seed
/// always answers the same float regardless of `--seed` or call order — the CLI flag never touches it.
pub fn rand_law(law: RandLaw, args: &[Value], resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    match law {
        RandLaw::Uniform => Ok(with_prng(resources, rand_float)),
        RandLaw::UniformSeeded => {
            let seed = integer_arg(args.first(), "rand seed", resources)?;
            Ok(rand_float(&mut Prng::from_seed(u64::from_ne_bytes(seed.to_ne_bytes()))))
        }
        RandLaw::RandintOne => {
            let bound = integer_arg(args.first(), "randint bound", resources)?;
            if bound <= 0 {
                return Err(raise("randint bound must be a positive integer", resources));
            }
            // `bound > 0` is already checked, so the signedness cast is exact and the draw stays in `[0, bound)`, which
            // fits an i64.
            #[allow(
                clippy::cast_sign_loss,
                clippy::cast_possible_wrap,
                reason = "bound > 0 was checked above; the draw is < bound, so both casts are exact"
            )]
            let value = {
                let draw = with_prng(resources, |rng| rng.below_u64(bound as u64));
                draw as i64
            };
            Ok(Value::Number(Number::integer(jqf_data::Integer::from_i64(value))))
        }
        RandLaw::RandintTwo => {
            let lower = integer_arg(args.first(), "randint lower bound", resources)?;
            let upper = integer_arg(args.get(1), "randint upper bound", resources)?;
            if upper <= lower {
                return Err(raise("randint upper bound must exceed the lower bound", resources));
            }
            // Both bounds are i64 and upper > lower, so the width fits u64.
            // The offset is added in u64 (two's complement): the true result lies in [lower, upper), which fits i64, so
            // the wrapped u64 sum is exactly that result re-read as i64.
            let width = u64::from_ne_bytes(upper.to_ne_bytes()).wrapping_sub(u64::from_ne_bytes(lower.to_ne_bytes()));
            let offset = with_prng(resources, |rng| rng.below_u64(width));
            let value = u64::from_ne_bytes(lower.to_ne_bytes()).wrapping_add(offset);
            Ok(Value::Number(Number::integer(jqf_data::Integer::from_i64(
                i64::from_ne_bytes(value.to_ne_bytes()),
            ))))
        }
        RandLaw::Choice => {
            let Some(source) = args.first() else {
                return Err(raise("choice needs an array", resources));
            };
            let Value::Array(array) = source.untagged() else {
                return Err(raise("choice expects an array", resources));
            };
            if array.is_empty() {
                return Err(raise("choice cannot pick from an empty array", resources));
            }
            let position = with_prng(resources, |rng| rng.below(array.len()));
            array
                .get(position)
                .map_or_else(|| Err(EngineRunError::allocation_failure()), |value| Ok(value.clone()))
        }
    }
}

/// One float uniform in `[0, 1)`: the high 53 bits of one xoshiro256** output scaled into the unit interval (the
/// standard double-draw, uniform up to the usual 2^-53 grid).
///
/// Reads one argument as an exact integer that fits an i64, or raises.
fn integer_arg(value: Option<&Value>, name: &str, resources: &ResourceContext<'_>) -> Result<i64, EngineRunError> {
    let Some(Value::Number(number)) = value.map(Value::untagged) else {
        return Err(raise(&format!("{name} must be an integer"), resources));
    };
    number
        .to_i64()
        .ok_or_else(|| raise(&format!("{name} must be an integer"), resources))
}

pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    family(
        id::SAMPLE_FAMILY_ID,
        "sample",
        "Draws elements from an array without replacement.",
        "",
    ),
    family(
        id::SHUFFLE_FAMILY_ID,
        "shuffle",
        "One uniform permutation of the input array.",
        "",
    ),
    family(
        id::FILL_FORWARD_FAMILY_ID,
        "fill_forward",
        "Replaces every null with the nearest preceding non-null.",
        "",
    ),
    family(id::UNION_FAMILY_ID, "union", "The sorted union of two arrays.", ""),
    family(
        id::INTERSECT_FAMILY_ID,
        "intersect",
        "The sorted intersection of two arrays.",
        "",
    ),
    family(
        id::EXCEPT_FAMILY_ID,
        "except",
        "The sorted difference of two arrays.",
        "",
    ),
    family(id::UUID_FAMILY_ID, "uuid", "Parses and normalizes a UUID string.", ""),
    family(id::UUID_V4_FAMILY_ID, "uuid_v4", "Generates a random UUID v4.", ""),
    family(
        id::UUID_V7_FAMILY_ID,
        "uuid_v7",
        "Generates a time-ordered UUID v7.",
        "",
    ),
    family(id::MD5_FAMILY_ID, "md5", "The MD5 digest of a string, hex.", ""),
    family(id::SHA1_FAMILY_ID, "sha1", "The SHA-1 digest of a string, hex.", ""),
    family(
        id::SHA256_FAMILY_ID,
        "sha256",
        "The SHA-256 digest of a string, hex.",
        "",
    ),
    family(
        id::SHA512_FAMILY_ID,
        "sha512",
        "The SHA-512 digest of a string, hex.",
        "",
    ),
    family(
        id::HMAC_FAMILY_ID,
        "hmac",
        "The HMAC-SHA256 of the input string keyed by the argument, hex.",
        "",
    ),
    family(id::XXHASH_FAMILY_ID, "xxhash", "The xxHash64 of a string, hex.", ""),
    family(id::HEX_ENCODE_FAMILY_ID, "hex_encode", "Hex-encodes a string.", ""),
    family(
        id::HEX_DECODE_FAMILY_ID,
        "hex_decode",
        "Decodes a hex string.",
        "The decoded payload must be valid UTF-8, matching the base64 decode \
         family's string-domain law.",
    ),
    family(
        id::BASE64_ENCODE_FAMILY_ID,
        "base64_encode",
        "Base64-encodes a string.",
        "",
    ),
    family(
        id::BASE64_DECODE_FAMILY_ID,
        "base64_decode",
        "Decodes a base64 string.",
        "The decoded payload must be valid UTF-8: the family answers strings, \
         so a payload of arbitrary bytes raises `invalid UTF-8 after base64 decode`.",
    ),
    family(
        id::BASE64URL_ENCODE_FAMILY_ID,
        "base64url_encode",
        "Base64url-encodes a string (unpadded).",
        "",
    ),
    family(
        id::BASE64URL_DECODE_FAMILY_ID,
        "base64url_decode",
        "Decodes a base64url string.",
        "Same string-domain law as `base64_decode`: the decoded payload must be \
         valid UTF-8.",
    ),
    family(
        id::PERCENT_ENCODE_FAMILY_ID,
        "percent_encode",
        "Percent-encodes a string per RFC 3986.",
        "",
    ),
    family(
        id::PERCENT_DECODE_FAMILY_ID,
        "percent_decode",
        "Decodes a percent-encoded string.",
        "",
    ),
    family(
        id::BASE32_ENCODE_FAMILY_ID,
        "base32_encode",
        "Base32-encodes a string per RFC 4648.",
        "",
    ),
    family(
        id::BASE32_DECODE_FAMILY_ID,
        "base32_decode",
        "Decodes a base32 string.",
        "",
    ),
    family(
        id::QUOTED_PRINTABLE_ENCODE_FAMILY_ID,
        "quoted_printable_encode",
        "Quoted-printable encodes a string per RFC 2045.",
        "",
    ),
    family(
        id::QUOTED_PRINTABLE_DECODE_FAMILY_ID,
        "quoted_printable_decode",
        "Decodes a quoted-printable string.",
        "",
    ),
    family(
        id::HMAC_SHA1_FAMILY_ID,
        "hmac_sha1",
        "The HMAC-SHA1 of the input string keyed by the argument, hex.",
        "",
    ),
    family(
        id::HMAC_SHA512_FAMILY_ID,
        "hmac_sha512",
        "The HMAC-SHA512 of the input string keyed by the argument, hex.",
        "",
    ),
    family(
        id::HMAC_SHA256_FAMILY_ID,
        "hmac_sha256",
        "The HMAC-SHA256 of the input string keyed by the argument, hex.",
        "",
    ),
    family(
        id::HMAC_SHA1_BASE64URL_FAMILY_ID,
        "hmac_sha1_base64url",
        "The HMAC-SHA1 of the input string keyed by the argument, base64url.",
        "",
    ),
    family(
        id::HMAC_SHA256_BASE64URL_FAMILY_ID,
        "hmac_sha256_base64url",
        "The HMAC-SHA256 of the input string keyed by the argument, base64url.",
        "",
    ),
    family(
        id::HMAC_SHA512_BASE64URL_FAMILY_ID,
        "hmac_sha512_base64url",
        "The HMAC-SHA512 of the input string keyed by the argument, base64url.",
        "",
    ),
    family(
        id::BLAKE3_FAMILY_ID,
        "blake3",
        "The BLAKE3 digest of a string, hex.",
        "",
    ),
    family(id::CRC32_FAMILY_ID, "crc32", "The CRC32 checksum of a string, hex.", ""),
    family(
        id::GZIP_COMPRESS_FAMILY_ID,
        "gzip_compress",
        "Compresses a string to base64-carrying gzip.",
        "",
    ),
    family(
        id::GZIP_DECOMPRESS_FAMILY_ID,
        "gzip_decompress",
        "Decompresses a base64-carrying gzip member to a string.",
        "",
    ),
    family(
        id::DEFLATE_COMPRESS_FAMILY_ID,
        "deflate_compress",
        "Compresses a string to base64-carrying raw DEFLATE.",
        "",
    ),
    family(
        id::DEFLATE_DECOMPRESS_FAMILY_ID,
        "deflate_decompress",
        "Decompresses a base64-carrying raw DEFLATE stream to a string.",
        "",
    ),
    family(
        id::ZLIB_COMPRESS_FAMILY_ID,
        "zlib_compress",
        "Compresses a string to base64-carrying zlib.",
        "",
    ),
    family(
        id::ZLIB_DECOMPRESS_FAMILY_ID,
        "zlib_decompress",
        "Decompresses a base64-carrying zlib stream to a string.",
        "",
    ),
    family(
        id::NUMFMT_FAMILY_ID,
        "numfmt",
        "Formats a number with printf-style controls.",
        "",
    ),
    family(id::E_FAMILY_ID, "e", "Euler's number.", ""),
    family(id::PI_FAMILY_ID, "pi", "The circle constant.", ""),
    family(id::TAU_FAMILY_ID, "tau", "Two pi.", ""),
    family(id::DEGREES_FAMILY_ID, "degrees", "Radians to degrees.", ""),
    family(id::RADIANS_FAMILY_ID, "radians", "Degrees to radians.", ""),
    family(id::POW10_FAMILY_ID, "pow10", "Ten to the input's power.", ""),
    family(id::RECIP_FAMILY_ID, "recip", "The reciprocal.", ""),
    family(id::ROUND_EVEN_FAMILY_ID, "round_even", "Round half to even.", ""),
    family(id::SIGNUM_FAMILY_ID, "signum", "The sign of the input.", ""),
    family(id::FRACT_FAMILY_ID, "fract", "The fractional part.", ""),
    family(id::SUM_FAMILY_ID, "sum", "The sum of an array's numbers.", ""),
    family(id::AVG_FAMILY_ID, "avg", "The mean of an array's numbers.", ""),
    family(id::MEDIAN_FAMILY_ID, "median", "The median of an array's numbers.", ""),
    family(
        id::QUANTILE_FAMILY_ID,
        "quantile",
        "A linear-interpolated quantile.",
        "",
    ),
    family(id::STDDEV_FAMILY_ID, "stddev", "The population standard deviation.", ""),
    family(id::VARIANCE_FAMILY_ID, "variance", "The population variance.", ""),
    family(id::COUNT_FAMILY_ID, "count", "The element count of an array.", ""),
    family(
        id::FREQUENCY_FAMILY_ID,
        "frequency",
        "Value-count rows of an array, descending count then total order.",
        "",
    ),
    family(
        id::MELT_FAMILY_ID,
        "melt",
        "Wide-to-long reshape of an array of objects.",
        "",
    ),
    family(
        id::PIVOT_FAMILY_ID,
        "pivot",
        "Long-to-wide reshape of an array of objects.",
        "",
    ),
    family(
        id::RAND_FAMILY_ID,
        "rand",
        "A float uniform in [0, 1), unseeded or seeded.",
        "",
    ),
    family(
        id::RANDINT_FAMILY_ID,
        "randint",
        "An integer uniform in a bounded range.",
        "",
    ),
    family(id::CHOICE_FAMILY_ID, "choice", "One uniform element of an array.", ""),
];

const UNION_EXAMPLES: &[BuiltinExample] = &[BuiltinExample {
    program: "union([1,2];[2,3])",
    input: "null",
    expected: "[1,2,3]\n",
}];
const INTERSECT_EXAMPLES: &[BuiltinExample] = &[BuiltinExample {
    program: "intersect([1,2];[2,3])",
    input: "null",
    expected: "[2]\n",
}];
const EXCEPT_EXAMPLES: &[BuiltinExample] = &[BuiltinExample {
    program: "except([1,2,3];[2])",
    input: "null",
    expected: "[1,3]\n",
}];

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    overload_filter(
        id::SAMPLE_1,
        id::SAMPLE_FAMILY_ID,
        "sample",
        1,
        &[ParameterKind::Value],
        // A FULL draw is a permutation, so `sample(n) | sort` over an n-element array is deterministic — the pinned
        // example for an impure law.
        &[example("sample(5) | sort", "[3,1,4,1,5]", "[1,1,3,4,5]\n")],
        true,
    ),
    overload0(
        id::SHUFFLE_0,
        id::SHUFFLE_FAMILY_ID,
        "shuffle",
        &[example("shuffle | sort", "[3,1,2]", "[1,2,3]\n")],
        true,
    ),
    overload0(
        id::FILL_FORWARD_0,
        id::FILL_FORWARD_FAMILY_ID,
        "fill_forward",
        &[example("fill_forward", "[1,null,2,null,null,3]", "[1,1,2,2,2,3]\n")],
        false,
    ),
    overload_filter(
        id::UNION,
        id::UNION_FAMILY_ID,
        "union",
        2,
        TWO_FILTERS,
        UNION_EXAMPLES,
        false,
    ),
    overload_filter(
        id::INTERSECT,
        id::INTERSECT_FAMILY_ID,
        "intersect",
        2,
        TWO_FILTERS,
        INTERSECT_EXAMPLES,
        false,
    ),
    overload_filter(
        id::EXCEPT,
        id::EXCEPT_FAMILY_ID,
        "except",
        2,
        TWO_FILTERS,
        EXCEPT_EXAMPLES,
        false,
    ),
    overload0(
        id::UUID,
        id::UUID_FAMILY_ID,
        "uuid",
        &[example(
            "uuid",
            "\"123e4567-e89b-12d3-a456-426614174000\"",
            "\"123e4567-e89b-12d3-a456-426614174000\"\n",
        )],
        false,
    ),
    overload0(
        id::UUID_V4,
        id::UUID_V4_FAMILY_ID,
        "uuid_v4",
        &[example("uuid_v4 | length", "null", "36\n")],
        true,
    ),
    overload0(
        id::UUID_V7,
        id::UUID_V7_FAMILY_ID,
        "uuid_v7",
        &[example("uuid_v7 | length", "null", "36\n")],
        true,
    ),
    overload0(
        id::MD5,
        id::MD5_FAMILY_ID,
        "md5",
        &[example("md5", "\"abc\"", "\"900150983cd24fb0d6963f7d28e17f72\"\n")],
        false,
    ),
    overload0(
        id::SHA1,
        id::SHA1_FAMILY_ID,
        "sha1",
        &[example(
            "sha1",
            "\"abc\"",
            "\"a9993e364706816aba3e25717850c26c9cd0d89d\"\n",
        )],
        false,
    ),
    overload0(
        id::SHA256,
        id::SHA256_FAMILY_ID,
        "sha256",
        &[example(
            "sha256",
            "\"abc\"",
            "\"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\"\n",
        )],
        false,
    ),
    overload0(
        id::SHA512,
        id::SHA512_FAMILY_ID,
        "sha512",
        &[example("sha512 | length", "\"abc\"", "128\n")],
        false,
    ),
    // `hmac/1`: the input is the MESSAGE, the one filter argument is the KEY — RFC 4231 test vector #2 (key \"Jefe\",
    // data \"what do ya want for nothing?\").
    overload_filter(
        id::HMAC,
        id::HMAC_FAMILY_ID,
        "hmac",
        1,
        ONE_FILTER,
        &[example(
            "hmac(\"Jefe\")",
            "\"what do ya want for nothing?\"",
            "\"5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843\"\n",
        )],
        false,
    ),
    overload0(
        id::XXHASH,
        id::XXHASH_FAMILY_ID,
        "xxhash",
        &[example("xxhash | length", "\"abc\"", "16\n")],
        false,
    ),
    overload0(
        id::HEX_ENCODE,
        id::HEX_ENCODE_FAMILY_ID,
        "hex_encode",
        &[example("hex_encode", "\"abc\"", "\"616263\"\n")],
        false,
    ),
    overload0(
        id::HEX_DECODE,
        id::HEX_DECODE_FAMILY_ID,
        "hex_decode",
        &[example("hex_decode", "\"616263\"", "\"abc\"\n")],
        false,
    ),
    overload0(
        id::BASE64_ENCODE,
        id::BASE64_ENCODE_FAMILY_ID,
        "base64_encode",
        &[example("base64_encode", "\"hello\"", "\"aGVsbG8=\"\n")],
        false,
    ),
    overload0(
        id::BASE64_DECODE,
        id::BASE64_DECODE_FAMILY_ID,
        "base64_decode",
        &[example("base64_decode", "\"aGVsbG8=\"", "\"hello\"\n")],
        false,
    ),
    overload0(
        id::BASE64URL_ENCODE,
        id::BASE64URL_ENCODE_FAMILY_ID,
        "base64url_encode",
        &[example("base64url_encode", "\"f\"", "\"Zg\"\n")],
        false,
    ),
    overload0(
        id::BASE64URL_DECODE,
        id::BASE64URL_DECODE_FAMILY_ID,
        "base64url_decode",
        &[example("base64url_decode", "\"Zg\"", "\"f\"\n")],
        false,
    ),
    overload0(
        id::PERCENT_ENCODE,
        id::PERCENT_ENCODE_FAMILY_ID,
        "percent_encode",
        &[example("percent_encode", "\"hello world\"", "\"hello%20world\"\n")],
        false,
    ),
    overload0(
        id::PERCENT_DECODE,
        id::PERCENT_DECODE_FAMILY_ID,
        "percent_decode",
        &[example("percent_decode", "\"hello%20world\"", "\"hello world\"\n")],
        false,
    ),
    overload0(
        id::BASE32_ENCODE,
        id::BASE32_ENCODE_FAMILY_ID,
        "base32_encode",
        &[example("base32_encode", "\"f\"", "\"MY======\"\n")],
        false,
    ),
    overload0(
        id::BASE32_DECODE,
        id::BASE32_DECODE_FAMILY_ID,
        "base32_decode",
        &[example("base32_decode", "\"MY======\"", "\"f\"\n")],
        false,
    ),
    overload0(
        id::QUOTED_PRINTABLE_ENCODE,
        id::QUOTED_PRINTABLE_ENCODE_FAMILY_ID,
        "quoted_printable_encode",
        &[example("quoted_printable_encode", "\"a test\"", "\"a test\"\n")],
        false,
    ),
    overload0(
        id::QUOTED_PRINTABLE_DECODE,
        id::QUOTED_PRINTABLE_DECODE_FAMILY_ID,
        "quoted_printable_decode",
        &[example("quoted_printable_decode", "\"a test\"", "\"a test\"\n")],
        false,
    ),
    overload_filter(
        id::HMAC_SHA1,
        id::HMAC_SHA1_FAMILY_ID,
        "hmac_sha1",
        1,
        ONE_FILTER,
        &[example(
            "hmac_sha1(\"key\")",
            "\"The quick brown fox jumps over the lazy dog\"",
            "\"de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9\"\n",
        )],
        false,
    ),
    overload_filter(
        id::HMAC_SHA512,
        id::HMAC_SHA512_FAMILY_ID,
        "hmac_sha512",
        1,
        ONE_FILTER,
        &[example(
            "hmac_sha512(\"key\")",
            "\"The quick brown fox jumps over the lazy dog\"",
            "\"b42af09057bac1e2d41708e48a902e09b5ff7f12ab428a4fe86653c73dd248fb82f948a549f7b791a5b41915ee4d1ec3935357e4e2317250d0372afa2ebeeb3a\"\n",
        )],
        false,
    ),
    overload_filter(
        id::HMAC_SHA256,
        id::HMAC_SHA256_FAMILY_ID,
        "hmac_sha256",
        1,
        ONE_FILTER,
        &[example(
            "hmac_sha256(\"key\")",
            "\"The quick brown fox jumps over the lazy dog\"",
            "\"f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8\"\n",
        )],
        false,
    ),
    overload_filter(
        id::HMAC_SHA1_BASE64URL,
        id::HMAC_SHA1_BASE64URL_FAMILY_ID,
        "hmac_sha1_base64url",
        1,
        ONE_FILTER,
        &[example(
            "hmac_sha1_base64url(\"key\")",
            "\"The quick brown fox jumps over the lazy dog\"",
            "\"3nybhbi3iqa8ino29wqQcBydtNk\"\n",
        )],
        false,
    ),
    overload_filter(
        id::HMAC_SHA256_BASE64URL,
        id::HMAC_SHA256_BASE64URL_FAMILY_ID,
        "hmac_sha256_base64url",
        1,
        ONE_FILTER,
        &[example(
            "hmac_sha256_base64url(\"key\")",
            "\"The quick brown fox jumps over the lazy dog\"",
            "\"97yD9DBThCSxMpjmqm-xQ-9NWaFJRhdZl0edvC0aPNg\"\n",
        )],
        false,
    ),
    overload_filter(
        id::HMAC_SHA512_BASE64URL,
        id::HMAC_SHA512_BASE64URL_FAMILY_ID,
        "hmac_sha512_base64url",
        1,
        ONE_FILTER,
        &[example(
            "hmac_sha512_base64url(\"key\")",
            "\"The quick brown fox jumps over the lazy dog\"",
            "\"tCrwkFe6weLUFwjkipAuCbX_fxKrQopP6GZTxz3SSPuC-UilSfe3kaW0GRXuTR7Dk1NX5OIxclDQNyr6Lr7rOg\"\n",
        )],
        false,
    ),
    overload0(
        id::BLAKE3,
        id::BLAKE3_FAMILY_ID,
        "blake3",
        &[example(
            "blake3",
            "\"abc\"",
            "\"6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85\"\n",
        )],
        false,
    ),
    overload0(
        id::CRC32,
        id::CRC32_FAMILY_ID,
        "crc32",
        &[example("crc32", "\"123456789\"", "\"cbf43926\"\n")],
        false,
    ),
    // The compression family. Every compress law is DETERMINISTIC: the pinned bytes below are a pure function of the
    // input (the gzip member carries an MTIME of zero — the determinism pin), so an example that answers differently
    // from run to run is a real break.
    // The round-trip pins chain the compress law into its own decompress law, and the error pins keep a non-string
    // input and a corrupt/cross-codec stream distinguishable from each other.
    overload0(
        id::GZIP_COMPRESS,
        id::GZIP_COMPRESS_FAMILY_ID,
        "gzip_compress",
        &[
            example(
                "gzip_compress",
                "\"hello\"",
                "\"H4sIAAAAAAAA/8tIzcnJBwCGphA2BQAAAA==\"\n",
            ),
            example("gzip_compress | gzip_decompress", "\"hello\"", "\"hello\"\n"),
            example(
                "try gzip_compress catch .",
                "null",
                "\"gzip_compress requires string input\"\n",
            ),
        ],
        false,
    ),
    overload0(
        id::GZIP_DECOMPRESS,
        id::GZIP_DECOMPRESS_FAMILY_ID,
        "gzip_decompress",
        &[
            example(
                "gzip_decompress",
                "\"H4sIAAAAAAAA/8tIzcnJBwCGphA2BQAAAA==\"",
                "\"hello\"\n",
            ),
            example(
                "try gzip_decompress catch .",
                "\"bm90IGd6aXA=\"",
                "\"gzip_decompress rejected the gzip stream\"\n",
            ),
        ],
        false,
    ),
    overload0(
        id::DEFLATE_COMPRESS,
        id::DEFLATE_COMPRESS_FAMILY_ID,
        "deflate_compress",
        &[
            example("deflate_compress", "\"hello\"", "\"y0jNyckHAA==\"\n"),
            example("deflate_compress | deflate_decompress", "\"hello\"", "\"hello\"\n"),
        ],
        false,
    ),
    overload0(
        id::DEFLATE_DECOMPRESS,
        id::DEFLATE_DECOMPRESS_FAMILY_ID,
        "deflate_decompress",
        &[example("deflate_decompress", "\"y0jNyckHAA==\"", "\"hello\"\n")],
        false,
    ),
    overload0(
        id::ZLIB_COMPRESS,
        id::ZLIB_COMPRESS_FAMILY_ID,
        "zlib_compress",
        &[
            example("zlib_compress", "\"hello\"", "\"eJzLSM3JyQcABiwCFQ==\"\n"),
            example("zlib_compress | zlib_decompress", "\"hello\"", "\"hello\"\n"),
        ],
        false,
    ),
    overload0(
        id::ZLIB_DECOMPRESS,
        id::ZLIB_DECOMPRESS_FAMILY_ID,
        "zlib_decompress",
        &[example("zlib_decompress", "\"eJzLSM3JyQcABiwCFQ==\"", "\"hello\"\n")],
        false,
    ),
    // numfmt: the FORMAT is the one filter argument, the piped NUMBER the subject — the value law reads the format
    // from parameter 0 exactly as `hmac` reads its key. The pins cover the four named controls (decimal places,
    // thousands separators, fixed-width, rounding = half-away-from-zero), the exact-decimal path (`1.005` has no
    // binary64 wobble), literal text, and the argument-iteration law.
    overload_filter(
        id::NUMFMT,
        id::NUMFMT_FAMILY_ID,
        "numfmt",
        1,
        ONE_FILTER,
        &[
            example("numfmt(\"%.2f\")", "3.14159", "\"3.14\"\n"),
            example("numfmt(\"%,.2f\")", "1234567.891", "\"1,234,567.89\"\n"),
            example("numfmt(\"%10.2f\")", "3.14159", "\"      3.14\"\n"),
            example("numfmt(\"%010.2f\")", "3.14159", "\"0000003.14\"\n"),
            example("numfmt(\"%.0f\")", "2.5", "\"3\"\n"),
            example("numfmt(\"%.0f\")", "-2.5", "\"-3\"\n"),
            example("numfmt(\"%d\")", "3.9", "\"3\"\n"),
            example("numfmt(\"%e\")", "12345.678", "\"1.234568e+04\"\n"),
            example("numfmt(\"$%.2f\")", "3.1", "\"$3.10\"\n"),
            example("1.005 | numfmt(\"%.2f\")", "null", "\"1.01\"\n"),
            example(
                "try numfmt(\"%.2f\") catch .",
                "\"abc\"",
                "\"numfmt requires a number input\"\n",
            ),
            example("try numfmt(1) catch .", "3.1", "\"numfmt requires a string format\"\n"),
            example(
                "try numfmt(\"%x\") catch .",
                "3.1",
                "\"numfmt: invalid format \\\"%x\\\"\"\n",
            ),
            example("[numfmt((\"%.1f\",\"%.2f\"))]", "3.14159", "[\"3.1\",\"3.14\"]\n"),
        ],
        false,
    ),
    overload0(
        id::E,
        id::E_FAMILY_ID,
        "e",
        &[example("e", "null", "2.718281828459045\n")],
        false,
    ),
    overload0(
        id::PI,
        id::PI_FAMILY_ID,
        "pi",
        &[example("pi", "null", "3.141592653589793\n")],
        false,
    ),
    overload0(
        id::TAU,
        id::TAU_FAMILY_ID,
        "tau",
        &[example("tau", "null", "6.283185307179586\n")],
        false,
    ),
    overload0(
        id::DEGREES,
        id::DEGREES_FAMILY_ID,
        "degrees",
        &[example("degrees", "3.141592653589793", "180\n")],
        false,
    ),
    overload0(
        id::RADIANS,
        id::RADIANS_FAMILY_ID,
        "radians",
        &[example("radians", "180", "3.141592653589793\n")],
        false,
    ),
    overload0(
        id::POW10,
        id::POW10_FAMILY_ID,
        "pow10",
        &[example("pow10", "2", "100\n")],
        false,
    ),
    overload0(
        id::RECIP,
        id::RECIP_FAMILY_ID,
        "recip",
        &[example("recip", "2", "0.5\n")],
        false,
    ),
    overload0(
        id::ROUND_EVEN,
        id::ROUND_EVEN_FAMILY_ID,
        "round_even",
        &[example("round_even", "2.5", "2\n")],
        false,
    ),
    overload0(
        id::SIGNUM,
        id::SIGNUM_FAMILY_ID,
        "signum",
        &[example("signum", "-4", "-1\n")],
        false,
    ),
    overload0(
        id::FRACT,
        id::FRACT_FAMILY_ID,
        "fract",
        &[example("fract", "3.75", "0.75\n")],
        false,
    ),
    overload_filter(
        id::LOG_1,
        super::id::LOG_FAMILY_ID,
        "log",
        1,
        ONE_FILTER,
        // `log(2)` on `8` is the DISCRIMINATING example: a base-10 law that ignores its argument answers
        // `0.9030899869919435` here, while every base-10 spelling (`log(10)` on `100`) agrees with it by accident.
        &[
            example("log(2)", "8", "3\n"),
            example("log(10)", "100", "2\n"),
            // Base 10 and base 2 take libm's direct `log10`/`log2`, not the `ln(x)/ln(base)` ratio: the ratio answers
            // `2.9999999999999996` here, which is a divergence from the reference's `log10`.
            example("log(10)", "1000", "3\n"),
            example("log(2)", "1024", "10\n"),
            // A filter argument ITERATES, and an argument with no outputs makes the whole call publish nothing — the
            // law every extension family shares, and the one they all used to break.
            example("[log(2,4)]", "8", "[3,1.5]\n"),
            example("[log(empty)]", "8", "[]\n"),
            example(
                "try log(\"ten\") catch .",
                "100",
                "\"string (\\\"ten\\\") number required\"\n",
            ),
        ],
        false,
    ),
    overload_filter(
        id::LOG_2,
        super::id::LOG_FAMILY_ID,
        "log",
        2,
        TWO_FILTERS,
        // `log(8;2)` reads NEITHER operand from the input, so an evaluator that substitutes the input for the VALUE
        // argument cannot pass it.
        &[example("log(8;2)", "null", "3\n"), example("log(.;10)", "100", "2\n")],
        false,
    ),
    overload_filter(
        id::ROUND_1,
        super::id::ROUND_FAMILY_ID,
        "round",
        1,
        ONE_FILTER,
        &[
            example("round(2)", "3.14159", "3.14\n"),
            example("round(0)", "3.14159", "3\n"),
            // A non-number is the reference's `number required`, never an internal contract violation: the digit count
            // and the input are both ordinary user data.
            example("try round(2) catch .", "null", "\"null (null) number required\"\n"),
            example(
                "try round(\"two\") catch .",
                "3.14159",
                "\"string (\\\"two\\\") number required\"\n",
            ),
        ],
        false,
    ),
    overload_filter(
        id::ROUND_2,
        super::id::ROUND_FAMILY_ID,
        "round",
        2,
        TWO_FILTERS,
        // `round(3.14159;2)` reads NEITHER operand from the input, so an evaluator that substitutes the input for the
        // VALUE argument cannot pass it.
        &[
            example("round(3.14159;2)", "100", "3.14\n"),
            example("round(.;2)", "3.14159", "3.14\n"),
        ],
        false,
    ),
    overload_filter(
        id::SUM_1,
        id::SUM_FAMILY_ID,
        "sum",
        1,
        ONE_FILTER,
        &[example("sum([1,2,3])", "null", "6\n")],
        false,
    ),
    overload_filter(
        id::AVG_1,
        id::AVG_FAMILY_ID,
        "avg",
        1,
        ONE_FILTER,
        &[example("avg([1,2,3])", "null", "2\n")],
        false,
    ),
    overload_filter(
        id::MEDIAN_1,
        id::MEDIAN_FAMILY_ID,
        "median",
        1,
        ONE_FILTER,
        &[example("median([1,2,9])", "null", "2\n")],
        false,
    ),
    overload_filter(
        id::QUANTILE_2,
        id::QUANTILE_FAMILY_ID,
        "quantile",
        2,
        TWO_FILTERS,
        // The second example is the ORDER pin for the whole extension roster:
        // arguments iterate under the ONE argument law (the right-outer order, the LAST argument's outputs varying
        // fastest), so the q argument varies fastest and each source answers q=0 then q=1.
        &[
            example("quantile([1,2,3];0.5)", "null", "2\n"),
            example("[quantile([1,2,3,4],[10,20]; 0.0,1.0)]", "null", "[1,4,10,20]\n"),
        ],
        false,
    ),
    overload_filter(
        id::STDDEV_1,
        id::STDDEV_FAMILY_ID,
        "stddev",
        1,
        ONE_FILTER,
        &[example("stddev([2,4])", "null", "1\n")],
        false,
    ),
    overload_filter(
        id::VARIANCE_1,
        id::VARIANCE_FAMILY_ID,
        "variance",
        1,
        ONE_FILTER,
        &[example("variance([2,4])", "null", "1\n")],
        false,
    ),
    overload_filter(
        id::COUNT_1,
        id::COUNT_FAMILY_ID,
        "count",
        1,
        ONE_FILTER,
        &[example("count([1,2,3])", "null", "3\n")],
        false,
    ),
    overload_filter(
        id::FREQUENCY_1,
        id::FREQUENCY_FAMILY_ID,
        "frequency",
        1,
        ONE_FILTER,
        &[
            example(
                "frequency([1,2,1,3])",
                "null",
                "[{\"value\":1,\"count\":2},{\"value\":2,\"count\":1},{\"value\":3,\"count\":1}]\n",
            ),
            example("frequency([])", "null", "[]\n"),
        ],
        false,
    ),
    overload_filter(
        id::MELT,
        id::MELT_FAMILY_ID,
        "melt",
        4,
        FOUR_FILTERS,
        &[
            example(
                "melt([\"id\"]; [\"a\",\"b\"]; \"k\"; \"v\")",
                "[{\"id\":1,\"a\":10,\"b\":20}]",
                "[{\"id\":1,\"k\":\"a\",\"v\":10},{\"id\":1,\"k\":\"b\",\"v\":20}]\n",
            ),
            example(
                "melt([\"id\"]; [\"a\",\"b\"]; \"v\"; \"k\")",
                "[{\"id\":1,\"a\":10,\"b\":20}]",
                "[{\"id\":1,\"v\":\"a\",\"k\":10},{\"id\":1,\"v\":\"b\",\"k\":20}]\n",
            ),
            example("melt([\"id\"]; [\"a\"]; \"k\"; \"v\")", "[]", "[]\n"),
        ],
        false,
    ),
    overload_filter(
        id::PIVOT,
        id::PIVOT_FAMILY_ID,
        "pivot",
        4,
        FOUR_FILTERS,
        &[
            example(
                "pivot([\"id\"]; \"k\"; \"v\"; [\"a\",\"b\"])",
                "[{\"id\":1,\"k\":\"a\",\"v\":10},{\"id\":1,\"k\":\"b\",\"v\":20}]",
                "[{\"id\":1,\"a\":10,\"b\":20}]\n",
            ),
            example(
                "pivot([\"id\"]; \"v\"; \"k\"; [\"a\",\"b\"])",
                "[{\"id\":1,\"v\":\"a\",\"k\":10},{\"id\":1,\"v\":\"b\",\"k\":20}]",
                "[{\"id\":1,\"a\":10,\"b\":20}]\n",
            ),
            example("pivot([\"id\"]; \"k\"; \"v\"; [\"a\"])", "[]", "[]\n"),
        ],
        false,
    ),
    // The rand family. The unseeded forms are IMPURE effects (a fresh entropy seed per draw), so their pinned examples
    // are deterministic INVARIANTS — a float in [0,1), a bounded integer, and a singleton choice. `rand/1` is the
    // deliberate exception: the seeded form is deterministic given the seed, so its example pins the exact byte the
    // seed 42 produces.
    overload0(
        id::RAND_0,
        id::RAND_FAMILY_ID,
        "rand",
        &[example("rand >= 0 and rand < 1", "null", "true\n")],
        true,
    ),
    overload_filter(
        id::RAND_1,
        id::RAND_FAMILY_ID,
        "rand",
        1,
        ONE_FILTER,
        // Deterministic given the seed: the pinned byte for seed 42.
        &[example("rand(42)", "null", "0.08386297105988216\n")],
        false,
    ),
    overload_filter(
        id::RANDINT_1,
        id::RANDINT_FAMILY_ID,
        "randint",
        1,
        ONE_FILTER,
        &[example("randint(10) >= 0 and randint(10) < 10", "null", "true\n")],
        true,
    ),
    overload_filter(
        id::RANDINT_2,
        id::RANDINT_FAMILY_ID,
        "randint",
        2,
        TWO_FILTERS,
        &[example("randint(5; 10) >= 5 and randint(5; 10) < 10", "null", "true\n")],
        true,
    ),
    overload_filter(
        id::CHOICE_1,
        id::CHOICE_FAMILY_ID,
        "choice",
        1,
        ONE_FILTER,
        &[example("choice([42])", "null", "42\n")],
        true,
    ),
];

/// The extension families' execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// The family spans three law enums (the extension laws proper, the analytics laws, and the rand laws), so the slice
/// needs one wrapper type; `registry::dispatch` unwraps it at table build time. Every entry carries its overload id so
/// the const coverage walk there can prove pairwise alignment.
#[cfg(feature = "ext-hash")]
#[derive(Clone, Copy, Debug)]
pub enum ExtPayload {
    /// An extension law proper.
    Extension(ExtensionLaw),
    /// An analytics law (`sample`, `shuffle`, `fill_forward`).
    Analytics(AnalyticsLaw),
    /// A rand law (`rand`, `randint`, `choice`).
    Rand(RandLaw),
}

/// See [`ExtPayload`]: the id-carrying payload slice over [`OVERLOADS`].
#[cfg(feature = "ext-hash")]
pub const PAYLOADS: &[(u16, ExtPayload)] = &[
    (id::SAMPLE_1, ExtPayload::Analytics(AnalyticsLaw::Sample)),
    (id::SHUFFLE_0, ExtPayload::Analytics(AnalyticsLaw::Shuffle)),
    (id::FILL_FORWARD_0, ExtPayload::Analytics(AnalyticsLaw::FillForward)),
    (id::UNION, ExtPayload::Extension(ExtensionLaw::Set(SetLaw::Union))),
    (
        id::INTERSECT,
        ExtPayload::Extension(ExtensionLaw::Set(SetLaw::Intersect)),
    ),
    (id::EXCEPT, ExtPayload::Extension(ExtensionLaw::Set(SetLaw::Except))),
    (id::UUID, ExtPayload::Extension(ExtensionLaw::Uuid(UuidLaw::Parse))),
    (id::UUID_V4, ExtPayload::Extension(ExtensionLaw::Uuid(UuidLaw::V4))),
    (id::UUID_V7, ExtPayload::Extension(ExtensionLaw::Uuid(UuidLaw::V7))),
    (id::MD5, ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Md5))),
    (id::SHA1, ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Sha1))),
    (id::SHA256, ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Sha256))),
    (id::SHA512, ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Sha512))),
    // `hmac/1` defaults to sha256, and `hmac_sha256/1` is its explicit hex spelling — one law, two names.
    (id::HMAC, ExtPayload::Extension(ExtensionLaw::Hmac(HmacLaw::Sha256))),
    (id::XXHASH, ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Xxhash))),
    (
        id::HEX_ENCODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::HexEncode)),
    ),
    (
        id::HEX_DECODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::HexDecode)),
    ),
    (
        id::BASE64_ENCODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Base64Encode)),
    ),
    (
        id::BASE64_DECODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Base64Decode)),
    ),
    (
        id::BASE64URL_ENCODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Base64urlEncode)),
    ),
    (
        id::BASE64URL_DECODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Base64urlDecode)),
    ),
    (
        id::PERCENT_ENCODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::PercentEncode)),
    ),
    (
        id::PERCENT_DECODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::PercentDecode)),
    ),
    (
        id::BASE32_ENCODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Base32Encode)),
    ),
    (
        id::BASE32_DECODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Base32Decode)),
    ),
    (
        id::QUOTED_PRINTABLE_ENCODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::QuotedPrintableEncode)),
    ),
    (
        id::QUOTED_PRINTABLE_DECODE,
        ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::QuotedPrintableDecode)),
    ),
    (id::HMAC_SHA1, ExtPayload::Extension(ExtensionLaw::Hmac(HmacLaw::Sha1))),
    (
        id::HMAC_SHA512,
        ExtPayload::Extension(ExtensionLaw::Hmac(HmacLaw::Sha512)),
    ),
    (
        id::HMAC_SHA256,
        ExtPayload::Extension(ExtensionLaw::Hmac(HmacLaw::Sha256)),
    ),
    (
        id::HMAC_SHA1_BASE64URL,
        ExtPayload::Extension(ExtensionLaw::Hmac(HmacLaw::Sha1Base64url)),
    ),
    (
        id::HMAC_SHA256_BASE64URL,
        ExtPayload::Extension(ExtensionLaw::Hmac(HmacLaw::Sha256Base64url)),
    ),
    (
        id::HMAC_SHA512_BASE64URL,
        ExtPayload::Extension(ExtensionLaw::Hmac(HmacLaw::Sha512Base64url)),
    ),
    (id::BLAKE3, ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Blake3))),
    (id::CRC32, ExtPayload::Extension(ExtensionLaw::Hash(HashLaw::Crc32))),
    (
        id::GZIP_COMPRESS,
        ExtPayload::Extension(ExtensionLaw::Compress(CompressLaw::GzipCompress)),
    ),
    (
        id::GZIP_DECOMPRESS,
        ExtPayload::Extension(ExtensionLaw::Compress(CompressLaw::GzipDecompress)),
    ),
    (
        id::DEFLATE_COMPRESS,
        ExtPayload::Extension(ExtensionLaw::Compress(CompressLaw::DeflateCompress)),
    ),
    (
        id::DEFLATE_DECOMPRESS,
        ExtPayload::Extension(ExtensionLaw::Compress(CompressLaw::DeflateDecompress)),
    ),
    (
        id::ZLIB_COMPRESS,
        ExtPayload::Extension(ExtensionLaw::Compress(CompressLaw::ZlibCompress)),
    ),
    (
        id::ZLIB_DECOMPRESS,
        ExtPayload::Extension(ExtensionLaw::Compress(CompressLaw::ZlibDecompress)),
    ),
    (id::NUMFMT, ExtPayload::Extension(ExtensionLaw::NumFmt)),
    (id::E, ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::E))),
    (id::PI, ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::Pi))),
    (id::TAU, ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::Tau))),
    (
        id::DEGREES,
        ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::Degrees)),
    ),
    (
        id::RADIANS,
        ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::Radians)),
    ),
    (id::POW10, ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::Pow10))),
    (id::RECIP, ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::Recip))),
    (
        id::ROUND_EVEN,
        ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::RoundEven)),
    ),
    (
        id::SIGNUM,
        ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::Signum)),
    ),
    (id::FRACT, ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::Fract))),
    (id::LOG_1, ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::LogOne))),
    (id::LOG_2, ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::LogTwo))),
    (
        id::ROUND_1,
        ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::RoundOne)),
    ),
    (
        id::ROUND_2,
        ExtPayload::Extension(ExtensionLaw::Math(MathExtLaw::RoundTwo)),
    ),
    (id::SUM_1, ExtPayload::Extension(ExtensionLaw::Stats(StatsLaw::Sum))),
    (id::AVG_1, ExtPayload::Extension(ExtensionLaw::Stats(StatsLaw::Avg))),
    (
        id::MEDIAN_1,
        ExtPayload::Extension(ExtensionLaw::Stats(StatsLaw::Median)),
    ),
    (
        id::QUANTILE_2,
        ExtPayload::Extension(ExtensionLaw::Stats(StatsLaw::Quantile)),
    ),
    (
        id::STDDEV_1,
        ExtPayload::Extension(ExtensionLaw::Stats(StatsLaw::Stddev)),
    ),
    (
        id::VARIANCE_1,
        ExtPayload::Extension(ExtensionLaw::Stats(StatsLaw::Variance)),
    ),
    (id::COUNT_1, ExtPayload::Extension(ExtensionLaw::Stats(StatsLaw::Count))),
    (id::FREQUENCY_1, ExtPayload::Extension(ExtensionLaw::Frequency)),
    (id::MELT, ExtPayload::Extension(ExtensionLaw::Melt)),
    (id::PIVOT, ExtPayload::Extension(ExtensionLaw::Pivot)),
    (id::RAND_0, ExtPayload::Rand(RandLaw::Uniform)),
    (id::RAND_1, ExtPayload::Rand(RandLaw::UniformSeeded)),
    (id::RANDINT_1, ExtPayload::Rand(RandLaw::RandintOne)),
    (id::RANDINT_2, ExtPayload::Rand(RandLaw::RandintTwo)),
    (id::CHOICE_1, ExtPayload::Rand(RandLaw::Choice)),
];
