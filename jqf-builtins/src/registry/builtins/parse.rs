//! The jqf PARSERS extension family.
//!
//! Six string→object parsers: `parse_url`, `parse_query_string`, `parse_logfmt`, `parse_syslog`, `parse_user_agent`,
//! and `parse_grok`. They are PURE value laws over the piped string (grok takes its pattern as a filter argument, the
//! argument-evaluation law), with no engine or codec entanglement — the log-crunching wedge.
//!
//! Behavior notes, all deliberate and documented against the reference:
//! - `null` input answers `null` (the reference's `string_arg` law); any other non-string input raises a catch-eligible
//!   error.
//! - Parse failures raise catch-eligible errors with the reference's messages (`parse_url input is invalid`,
//!   `PARSE_LOGFMT expects key=value pairs`, …) — a failure is an alternate outcome of the string, and users expect
//!   `try parse_url(.) catch …` to observe it.
//! - `parse_url` is a HAND-ROLLED RFC 3986 splitter (jqf's dependency policy adds no `url` crate):
//!   `scheme://[userinfo@]host[:port]/path?query# fragment`. The reference normalized through the `url` crate; this
//!   port keeps every component AS AUTHORED (no host lowercasing, no IDNA), which is the deterministic spelling a
//!   byte-oriented pipeline wants.
//! - `parse_user_agent` is the reference's marker table (browser/os/device), not a real UA parser — it is a
//!   classifier, and its `raw` field keeps the input for the cases the table does not know.
//! - `parse_grok` assembles and compiles its pattern ONCE per distinct pattern string — a fill-once cache in the
//!   `regex` module keys the assembled regex and its capture names on the user's grok pattern, so the
//!   tokenize-and-rebuild loop is cached too, not just the compile. The token table is the reference's eight kinds:
//!   WORD, NUMBER/INT, DATA, GREEDYDATA, IP/IPV4, HOSTNAME, UUID, NOTSPACE. An unknown kind raises.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use jqf_data::{Integer, Number, ObjectBuilder, ObjectKey, Value};
use jqf_resource::ResourceContext;

use super::id;
use super::regex::{cached_grok_regex, compile_plain_regex, publish_grok_regex};
use crate::error::EngineRunError;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};

/// One parser law, one evaluator shape.
#[derive(Clone, Copy, Debug)]
pub enum ParseLaw {
    Url,
    QueryString,
    Logfmt,
    Syslog,
    UserAgent,
    Grok,
}

const fn family(id: u16, name: &'static str, summary: &'static str, detail: &'static str) -> BuiltinFamilyRecord {
    BuiltinFamilyRecord {
        id: BuiltinFamilyId::new(id),
        canonical_name: name,
        category: "jqf-extension",
        summary,
        detail,
    }
}

/// The six parser family records — one per builtin, each named after its canonical name (the registration used to
/// list a phantom `"parse"` family that no overload referenced while the six overloads named six UNREGISTERED ids,
/// which `validate()` could not see).
pub const PARSE_URL_FAMILY: BuiltinFamilyRecord = family(
    id::PARSE_URL_FAMILY_ID,
    "parse_url",
    "RFC 3986 URL parsing.",
    "Splits a URL into its scheme, authority, path, query, and fragment parts.",
);
pub const PARSE_QUERY_STRING_FAMILY: BuiltinFamilyRecord = family(
    id::PARSE_QUERY_STRING_FAMILY_ID,
    "parse_query_string",
    "Query-string parsing.",
    "Parses an application/x-www-form-urlencoded query string into an object.",
);
pub const PARSE_LOGFMT_FAMILY: BuiltinFamilyRecord = family(
    id::PARSE_LOGFMT_FAMILY_ID,
    "parse_logfmt",
    "Logfmt parsing.",
    "Parses a logfmt key=value line into an object.",
);
pub const PARSE_SYSLOG_FAMILY: BuiltinFamilyRecord = family(
    id::PARSE_SYSLOG_FAMILY_ID,
    "parse_syslog",
    "Syslog message parsing.",
    "Parses an RFC 3164-style syslog message into its parts.",
);
pub const PARSE_USER_AGENT_FAMILY: BuiltinFamilyRecord = family(
    id::PARSE_USER_AGENT_FAMILY_ID,
    "parse_user_agent",
    "User-agent parsing.",
    "Splits a User-Agent header into its browser, os, and device markers.",
);
pub const PARSE_GROK_FAMILY: BuiltinFamilyRecord = family(
    id::PARSE_GROK_FAMILY_ID,
    "parse_grok",
    "Grok pattern matching.",
    "Matches a string against a grok pattern and returns the captures.",
);

/// Drives one parser law over the piped value.
///
/// `pattern` is the evaluated filter argument for `parse_grok` and `None` for the unary laws.
pub fn parse_law(
    law: ParseLaw,
    input: &Value,
    pattern: Option<&Value>,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    match input.untagged() {
        Value::Null => Ok(Value::Null),
        Value::String(text) => {
            let text = text.as_str();
            match law {
                ParseLaw::Url => parse_url(text, resources),
                ParseLaw::QueryString => parse_query_string(text, resources),
                ParseLaw::Logfmt => parse_logfmt(text, resources),
                ParseLaw::Syslog => parse_syslog(text, resources),
                ParseLaw::UserAgent => parse_user_agent(text, resources),
                ParseLaw::Grok => {
                    let Some(pattern) = pattern else {
                        return Err(EngineRunError::internal_contract(
                            "parse_grok reached its law without a pattern argument",
                        ));
                    };
                    let Value::String(pattern) = pattern.untagged() else {
                        return Err(crate::semantics::path::raise(
                            "parse_grok requires a string pattern",
                            resources,
                        ));
                    };
                    parse_grok(text, pattern.as_str(), resources)
                }
            }
        }
        _ => Err(crate::semantics::path::raise("parser requires string input", resources)),
    }
}

// ---------------------------------------------------------------------------
// Shared object-building helpers
// ---------------------------------------------------------------------------

/// Builds one owned object from insertion-ordered string keys and values.
fn build_object<'k>(
    entries: impl IntoIterator<Item = (&'k str, Value)>,
    capacity: usize,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut builder = ObjectBuilder::try_with_capacity(capacity).map_err(|_| EngineRunError::allocation_failure())?;
    for (key, value) in entries {
        let key = ObjectKey::try_from_str(key).map_err(|_| EngineRunError::allocation_failure())?;
        builder
            .try_insert_last(key, value)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    builder
        .try_finish()
        .map(Value::Object)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// One owned string value.
fn owned(text: &str, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    Value::try_string(text).map_err(|_| EngineRunError::allocation_failure())
}

fn owned_null() -> Value {
    Value::Null
}

fn owned_int(value: i64) -> Value {
    Value::Number(Number::integer(Integer::from_i64(value)))
}

// ---------------------------------------------------------------------------
// parse_url
// ---------------------------------------------------------------------------

/// Splits `scheme://[userinfo@]host[:port]/path?query#fragment` into its six documented components, every component as
/// authored. No normalization: the host is not lowercased, percent-escapes are not decoded, and the port is the number
/// when it parses and `null` when it is absent or non-numeric.
fn parse_url(input: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Some(scheme_end) = input.find("://") else {
        return Err(crate::semantics::path::raise("parse_url input is invalid", resources));
    };
    let scheme = &input[..scheme_end];
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'-' || byte == b'.')
    {
        return Err(crate::semantics::path::raise("parse_url input is invalid", resources));
    }
    let rest = &input[scheme_end + 3..];

    // authority ends at the first '/', '?', or '#'
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);

    // path ends at the first '?' or '#'
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let (path, tail) = tail.split_at(path_end);
    let (query, fragment): (Option<&str>, Option<&str>) = match tail.strip_prefix('?') {
        Some(after) => match after.find('#') {
            Some(index) => (Some(&after[..index]), Some(&after[index + 1..])),
            None => (Some(after), None),
        },
        None => (None, tail.strip_prefix('#')),
    };

    // `[userinfo@]host[:port]`, with the userinfo stripped FIRST — it can hold a raw colon, which would otherwise be
    // mistaken for the port separator (`user:pass@example.com` must answer host "example.com", not "user"). A bracketed
    // IPv6 host keeps its brackets, and its port splits at the LAST colon, which sits AFTER the closing bracket:
    // splitting INSIDE the brackets is excluded by `port_text.contains(']')`.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, bare)| bare);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port_text)) if !port_text.contains(']') && !port_text.is_empty() => {
            (host, port_text.parse::<i64>().ok())
        }
        // A trailing colon with no port digits is not a port separator; the host is everything before it.
        Some((host, _)) if authority.ends_with(':') => (host, None),
        _ => (authority, None),
    };

    build_object(
        [
            ("scheme", owned(scheme, resources)?),
            (
                "host",
                if host.is_empty() {
                    owned_null()
                } else {
                    owned(host, resources)?
                },
            ),
            ("port", port.map_or_else(owned_null, owned_int)),
            (
                "path",
                if path.is_empty() {
                    owned_null()
                } else {
                    owned(path, resources)?
                },
            ),
            (
                "query",
                match query {
                    None => owned_null(),
                    Some(q) => owned(q, resources)?,
                },
            ),
            (
                "fragment",
                match fragment {
                    None => owned_null(),
                    Some(f) => owned(f, resources)?,
                },
            ),
        ],
        6,
        resources,
    )
}

// ---------------------------------------------------------------------------
// parse_query_string
// ---------------------------------------------------------------------------

/// Parses `a=1&b=2` into an object; a repeated key accumulates its values into an array (the reference's
/// `insert_query_value` law). `+` decodes to space and `%XX` percent-escapes decode strictly — an incomplete or
/// invalid escape, or non-UTF-8 after decoding, raises.
fn parse_query_string(input: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let query = input.strip_prefix('?').unwrap_or(input);
    let mut keys: Vec<String> = Vec::new();
    let mut values: Vec<Vec<String>> = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = strict_form_decode("parse_query_string key", key, resources)?;
        let value = strict_form_decode("parse_query_string value", value, resources)?;
        if let Some(index) = keys.iter().position(|existing| existing == &key) {
            values[index].push(value);
        } else {
            keys.push(key);
            values.push(vec![value]);
        }
    }
    let mut entries: Vec<(&str, Value)> = Vec::with_capacity(keys.len());
    for (key, values) in keys.iter().zip(values.iter()) {
        let value = if values.len() == 1 {
            owned(&values[0], resources)?
        } else {
            let mut array =
                jqf_data::Array::try_with_capacity(values.len()).map_err(|_| EngineRunError::allocation_failure())?;
            for value in values {
                array
                    .try_push(owned(value, resources)?)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            Value::Array(array)
        };
        entries.push((key, value));
    }
    build_object(entries, keys.len(), resources)
}

fn strict_form_decode(name: &str, input: &str, resources: &ResourceContext<'_>) -> Result<String, EngineRunError> {
    let mut bytes = Vec::with_capacity(input.len());
    let raw = input.as_bytes();
    let mut index = 0usize;
    while index < raw.len() {
        match raw[index] {
            b'+' => {
                bytes.push(b' ');
                index += 1;
            }
            b'%' => {
                let Some(&hi) = raw.get(index + 1) else {
                    return Err(crate::semantics::path::raise(
                        &format!("{name} has an incomplete percent escape"),
                        resources,
                    ));
                };
                let Some(&lo) = raw.get(index + 2) else {
                    return Err(crate::semantics::path::raise(
                        &format!("{name} has an incomplete percent escape"),
                        resources,
                    ));
                };
                let Some(hi) = hex_digit(hi) else {
                    return Err(crate::semantics::path::raise(
                        &format!("{name} has an invalid percent escape"),
                        resources,
                    ));
                };
                let Some(lo) = hex_digit(lo) else {
                    return Err(crate::semantics::path::raise(
                        &format!("{name} has an invalid percent escape"),
                        resources,
                    ));
                };
                bytes.push((hi << 4) | lo);
                index += 3;
            }
            byte => {
                bytes.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(bytes).map_err(|_| {
        crate::semantics::path::raise(&format!("{name} is not valid UTF-8 after percent decoding"), resources)
    })
}

fn hex_digit(byte: u8) -> Option<u8> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a base-16 digit is at most 15, so the u32 -> u8 cast cannot truncate"
    )]
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

// ---------------------------------------------------------------------------
// parse_logfmt
// ---------------------------------------------------------------------------

/// Parses `key=value key2="quoted value"` pairs; a repeated key keeps its LAST value (the reference's
/// `ObjectMap::insert` law). Quoted values support the reference's escape set; a token without `=` or an unterminated
/// quoted value raises.
fn parse_logfmt(input: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let bytes = input.as_bytes();
    let mut index = 0usize;
    let mut keys: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::new();

    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }
        let key_start = index;
        while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'=' {
            index += 1;
        }
        if key_start == index || index == bytes.len() || bytes[index] != b'=' {
            return Err(crate::semantics::path::raise(
                "parse_logfmt expects key=value pairs",
                resources,
            ));
        }
        let key = &input[key_start..index];
        index += 1;

        let value = if index < bytes.len() && bytes[index] == b'"' {
            index += 1;
            parse_logfmt_quoted(input, bytes, &mut index, resources)?
        } else {
            let value_start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            input[value_start..index].to_string()
        };

        if let Some(position) = keys.iter().position(|existing| existing == key) {
            values[position] = value;
        } else {
            keys.push(key.to_string());
            values.push(value);
        }
    }

    let entries: Vec<(&str, Value)> = keys
        .iter()
        .zip(values.iter())
        .map(|(key, value)| Ok((key.as_str(), owned(value, resources)?)))
        .collect::<Result<_, EngineRunError>>()?;
    build_object(entries, keys.len(), resources)
}

fn parse_logfmt_quoted(
    input: &str,
    bytes: &[u8],
    index: &mut usize,
    resources: &ResourceContext<'_>,
) -> Result<String, EngineRunError> {
    let mut value = String::new();
    while *index < bytes.len() {
        match bytes[*index] {
            b'"' => {
                *index += 1;
                return Ok(value);
            }
            b'\\' => {
                *index += 1;
                if *index == bytes.len() {
                    return Err(crate::semantics::path::raise(
                        "parse_logfmt quoted value has a trailing escape",
                        resources,
                    ));
                }
                let ch = parse_logfmt_char(input, *index, resources)?;
                value.push(ch);
                *index += ch.len_utf8();
            }
            _ => {
                let ch = parse_logfmt_char(input, *index, resources)?;
                value.push(ch);
                *index += ch.len_utf8();
            }
        }
    }
    Err(crate::semantics::path::raise(
        "parse_logfmt quoted value is unterminated",
        resources,
    ))
}

fn parse_logfmt_char(input: &str, byte_index: usize, resources: &ResourceContext<'_>) -> Result<char, EngineRunError> {
    input
        .get(byte_index..)
        .and_then(|rest| rest.chars().next())
        .ok_or_else(|| {
            crate::semantics::path::raise("parse_logfmt quoted value has an invalid character boundary", resources)
        })
}

// ---------------------------------------------------------------------------
// parse_syslog
// ---------------------------------------------------------------------------

/// Parses one RFC 3164-style line (with the optional RFC 5424 version field the reference admits): `<PRI>VERSION?
/// TIMESTAMP HOST APP[PID]?: MESSAGE`. The priority decomposes into facility (pri/8) and severity (pri%8). The pattern
/// is a compile-time constant, so it compiles ONCE per process through the shared compiled-regex cache rather than once
/// per record.
fn parse_syslog(input: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    const SYSLOG_PATTERN: &str = r"^<(?P<pri>\d{1,3})>(?:(?P<version>\d)\s+)?(?P<ts>\S+(?:\s+\d{1,2}\s+\d\d:\d\d:\d\d)?)\s+(?P<host>\S+)\s+(?P<app>[^\s:\[]+)(?:\[(?P<pid>[^\]]+)\])?(?::\s*|\s+)(?P<msg>.*)$";
    let Ok(re) = compile_plain_regex(SYSLOG_PATTERN) else {
        return Err(EngineRunError::internal_contract(
            "parse_syslog pattern failed to compile",
        ));
    };
    let Some(caps) = re.captures(input) else {
        return Err(crate::semantics::path::raise(
            "parse_syslog input is not a supported syslog line",
            resources,
        ));
    };
    let Some(priority) = caps
        .name("pri")
        .and_then(|matched| matched.as_str().parse::<i64>().ok())
    else {
        return Err(crate::semantics::path::raise(
            "parse_syslog priority is invalid",
            resources,
        ));
    };
    let version = caps
        .name("version")
        .and_then(|matched| matched.as_str().parse::<i64>().ok());
    let field = |name: &str| -> Result<Value, EngineRunError> {
        match caps.name(name) {
            Some(matched) => owned(matched.as_str(), resources),
            // A missing capture is null by law; an allocation refusal propagates.
            None => Ok(owned_null()),
        }
    };
    build_object(
        [
            ("priority", owned_int(priority)),
            ("facility", owned_int(priority / 8)),
            ("severity", owned_int(priority % 8)),
            ("version", version.map_or_else(owned_null, owned_int)),
            ("timestamp", field("ts")?),
            ("host", field("host")?),
            ("appname", field("app")?),
            ("procid", field("pid")?),
            ("message", field("msg")?),
        ],
        9,
        resources,
    )
}

// ---------------------------------------------------------------------------
// parse_user_agent
// ---------------------------------------------------------------------------

/// The reference's marker classifier: browser (Edg/OPR/Chrome/Firefox/ Version), OS (Windows NT/Android/iPhone OS/CPU
/// OS/Mac OS X/Linux), and device (mobile/tablet/desktop). The `raw` field carries the input.
fn parse_user_agent(input: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let browser = browser_info(input, resources)?;
    let os = os_info(input, resources)?;
    let device = device_info(input, resources)?;
    build_object(
        [
            ("browser", browser),
            ("os", os),
            ("device", device),
            ("raw", owned(input, resources)?),
        ],
        4,
        resources,
    )
}

fn browser_info(input: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    const CANDIDATES: [(&str, &str); 5] = [
        ("Edg/", "Edge"),
        ("OPR/", "Opera"),
        ("Chrome/", "Chrome"),
        ("Firefox/", "Firefox"),
        ("Version/", "Safari"),
    ];
    for (marker, name) in CANDIDATES {
        if let Some(version) = version_after(input, marker) {
            return build_object(
                [
                    ("name", owned(name, resources)?),
                    ("version", owned(&version, resources)?),
                ],
                2,
                resources,
            );
        }
    }
    build_object(
        [("name", owned("Other", resources)?), ("version", owned_null())],
        2,
        resources,
    )
}

fn os_info(input: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let (name, version) = if let Some(version) = version_after(input, "Windows NT ") {
        ("Windows", Some(version.replace('_', ".")))
    } else if let Some(version) = version_after(input, "Android ") {
        ("Android", Some(version.replace('_', ".")))
    } else if let Some(version) = version_after(input, "iPhone OS ") {
        ("iOS", Some(version.replace('_', ".")))
    } else if let Some(version) = version_after(input, "CPU OS ") {
        ("iOS", Some(version.replace('_', ".")))
    } else if let Some(version) = version_after(input, "Mac OS X ") {
        ("macOS", Some(version.replace('_', ".")))
    } else if input.contains("Linux") {
        ("Linux", None)
    } else {
        ("Other", None)
    };
    build_object(
        [
            ("name", owned(name, resources)?),
            (
                "version",
                match version {
                    Some(v) => owned(&v, resources)?,
                    None => owned_null(),
                },
            ),
        ],
        2,
        resources,
    )
}

fn device_info(input: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let name = if input.contains("Mobile") || input.contains("iPhone") || input.contains("Android") {
        "mobile"
    } else if input.contains("iPad") || input.contains("Tablet") {
        "tablet"
    } else {
        "desktop"
    };
    build_object([("type", owned(name, resources)?)], 1, resources)
}

fn version_after(input: &str, marker: &str) -> Option<String> {
    let start = input.find(marker)? + marker.len();
    let rest = &input[start..];
    let end = rest
        .find(|ch: char| ch.is_ascii_whitespace() || ch == ';' || ch == ')')
        .unwrap_or(rest.len());
    (!rest[..end].is_empty()).then(|| rest[..end].to_string())
}

// ---------------------------------------------------------------------------
// parse_grok
// ---------------------------------------------------------------------------

/// The grok token scanner's pattern: `%{KIND:name}`. A compile-time constant, so it compiles ONCE per process through
/// the shared compiled-regex cache rather than once per record.
const GROK_TOKEN_PATTERN: &str = r"%\{(?P<kind>[A-Z0-9_]+):(?P<name>[A-Za-z_][A-Za-z0-9_]*)\}";

/// The reference's grok token table, one kind to one regex body.
fn grok_kind_body(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "WORD" => r"\b\w+\b",
        "NUMBER" | "INT" => r"[-+]?(?:\d+(?:\.\d+)?)",
        "DATA" => r".*?",
        "GREEDYDATA" => r".*",
        "IP" | "IPV4" => r"(?:\d{1,3}\.){3}\d{1,3}",
        "HOSTNAME" => r"[A-Za-z0-9][A-Za-z0-9._-]*",
        "UUID" => r"[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}",
        "NOTSPACE" => r"\S+",
        _ => return None,
    })
}

/// Tokenizes a grok pattern and assembles the anchored regex text plus the capture names in match order. Runs once per
/// DISTINCT pattern string: `parse_grok` caches the result keyed on the pattern, so the tokenize-and-rebuild loop is
/// part of the cached value, never per-record work.
fn assemble_grok_pattern(
    pattern: &str,
    resources: &ResourceContext<'_>,
) -> Result<(String, Vec<String>), EngineRunError> {
    let Ok(token) = compile_plain_regex(GROK_TOKEN_PATTERN) else {
        return Err(EngineRunError::internal_contract(
            "parse_grok token pattern failed to compile",
        ));
    };
    let mut regex_text = String::from("^");
    let mut names = Vec::new();
    let mut last = 0usize;
    for caps in token.captures_iter(pattern) {
        let Some(full) = caps.get(0) else {
            return Err(crate::semantics::path::raise(
                "parse_grok token match is missing the full capture",
                resources,
            ));
        };
        regex_text.push_str(&regex::escape(&pattern[last..full.start()]));
        let Some(kind) = caps.name("kind").map(|matched| matched.as_str()) else {
            return Err(crate::semantics::path::raise(
                "parse_grok token match is missing the pattern kind",
                resources,
            ));
        };
        let Some(name) = caps.name("name").map(|matched| matched.as_str()) else {
            return Err(crate::semantics::path::raise(
                "parse_grok token match is missing the capture name",
                resources,
            ));
        };
        let Some(body) = grok_kind_body(kind) else {
            return Err(crate::semantics::path::raise(
                &format!("parse_grok does not support %{{{kind}}}"),
                resources,
            ));
        };
        regex_text.push_str("(?P<");
        regex_text.push_str(name);
        regex_text.push('>');
        regex_text.push_str(body);
        regex_text.push(')');
        names.push(name.to_owned());
        last = full.end();
    }
    regex_text.push_str(&regex::escape(&pattern[last..]));
    regex_text.push('$');
    Ok((regex_text, names))
}

/// Parses `input` with a grok pattern (`%{KIND:name}` tokens) into an object of named captures. An unknown kind, a
/// malformed token, or a non-match raises.
fn parse_grok(input: &str, pattern: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let (compiled, names) = if let Some(hit) = cached_grok_regex(pattern) {
        hit
    } else {
        // Same process-lifetime law as the regex family's own compile: the assembled pattern is published for the life
        // of the process and its engine internals' lazy state initializes on first use, so build, warm, and publish
        // under a throwaway unlimited ledger — never on the calling request's (a worker's charge would never be
        // released).
        let _process_ledger = super::regex::unlimited_ambient_scope();
        let (regex_text, names) = assemble_grok_pattern(pattern, resources)?;
        let compiled = regex::Regex::new(&regex_text)
            .map_err(|_| crate::semantics::path::raise("parse_grok pattern did not compile", resources))?;
        let _ = compiled.is_match("");
        publish_grok_regex(pattern, compiled.clone(), names.clone());
        (compiled, names)
    };
    let Some(caps) = compiled.captures(input) else {
        return Err(crate::semantics::path::raise(
            "parse_grok input did not match pattern",
            resources,
        ));
    };
    let mut entries: Vec<(&str, Value)> = Vec::with_capacity(names.len());
    for name in &names {
        let value = match caps.name(name) {
            Some(matched) => owned(matched.as_str(), resources)?,
            None => owned_null(),
        };
        entries.push((name.as_str(), value));
    }
    let capacity = entries.len();
    build_object(entries, capacity, resources)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

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
        effects: Effects::Pure,
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
        effects: Effects::Pure,
        examples,
    }
}

const URL_EXAMPLES: &[BuiltinExample] = &[
    example(
        "parse_url",
        r#""https://example.com/path?q=1""#,
        concat!(
            r#"{"scheme":"https","host":"example.com","port":null,"path":"/path","query":"q=1","fragment":null}"#,
            "\n"
        ),
    ),
    example(
        "parse_url",
        r#""http://user@example.com:8080/a/b#frag""#,
        concat!(
            r#"{"scheme":"http","host":"example.com","port":8080,"path":"/a/b","query":null,"fragment":"frag"}"#,
            "\n"
        ),
    ),
    // A trailing colon with no port digits: the host keeps its name and the port stays null — the colon is not part
    // of the host.
    example(
        "parse_url",
        r#""http://example.com:""#,
        concat!(
            r#"{"scheme":"http","host":"example.com","port":null,"path":null,"query":null,"fragment":null}"#,
            "\n"
        ),
    ),
];

const QUERY_STRING_EXAMPLES: &[BuiltinExample] = &[
    example(
        "parse_query_string",
        r#""a=1&b=2""#,
        concat!(r#"{"a":"1","b":"2"}"#, "\n"),
    ),
    example(
        "parse_query_string",
        r#""a=1&a=2""#,
        concat!(r#"{"a":["1","2"]}"#, "\n"),
    ),
    example(
        "parse_query_string",
        r#""q=hello+world&pct=%41""#,
        concat!(r#"{"q":"hello world","pct":"A"}"#, "\n"),
    ),
];

const LOGFMT_EXAMPLES: &[BuiltinExample] = &[
    example(
        "parse_logfmt",
        r#""key=val foo=bar""#,
        concat!(r#"{"key":"val","foo":"bar"}"#, "\n"),
    ),
    example(
        "parse_logfmt",
        r#""msg=\"hello world\" n=1""#,
        concat!(r#"{"msg":"hello world","n":"1"}"#, "\n"),
    ),
];

const SYSLOG_EXAMPLES: &[BuiltinExample] = &[example(
    "parse_syslog",
    r#""<34>Oct 11 22:14:15 host su: failed""#,
    concat!(
        r#"{"priority":34,"facility":4,"severity":2,"version":null,"timestamp":"Oct 11 22:14:15","host":"host","appname":"su","procid":null,"message":"failed"}"#,
        "\n"
    ),
)];

const USER_AGENT_EXAMPLES: &[BuiltinExample] = &[example(
    "parse_user_agent",
    r#""Mozilla/5.0 (X11; Linux x86_64) Firefox/121.0""#,
    concat!(
        r#"{"browser":{"name":"Firefox","version":"121.0"},"os":{"name":"Linux","version":null},"device":{"type":"desktop"},"raw":"Mozilla/5.0 (X11; Linux x86_64) Firefox/121.0"}"#,
        "\n"
    ),
)];

const GROK_EXAMPLES: &[BuiltinExample] = &[example(
    "parse_grok(\"%{WORD:level} %{INT:code}\")",
    r#""INFO 200""#,
    concat!(r#"{"level":"INFO","code":"200"}"#, "\n"),
)];

pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    PARSE_URL_FAMILY,
    PARSE_QUERY_STRING_FAMILY,
    PARSE_LOGFMT_FAMILY,
    PARSE_SYSLOG_FAMILY,
    PARSE_USER_AGENT_FAMILY,
    PARSE_GROK_FAMILY,
];

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    overload0(id::PARSE_URL, id::PARSE_URL_FAMILY_ID, "parse_url", URL_EXAMPLES),
    overload0(
        id::PARSE_QUERY_STRING,
        id::PARSE_QUERY_STRING_FAMILY_ID,
        "parse_query_string",
        QUERY_STRING_EXAMPLES,
    ),
    overload0(
        id::PARSE_LOGFMT,
        id::PARSE_LOGFMT_FAMILY_ID,
        "parse_logfmt",
        LOGFMT_EXAMPLES,
    ),
    overload0(
        id::PARSE_SYSLOG,
        id::PARSE_SYSLOG_FAMILY_ID,
        "parse_syslog",
        SYSLOG_EXAMPLES,
    ),
    overload0(
        id::PARSE_USER_AGENT,
        id::PARSE_USER_AGENT_FAMILY_ID,
        "parse_user_agent",
        USER_AGENT_EXAMPLES,
    ),
    overload_filter(
        id::PARSE_GROK,
        id::PARSE_GROK_FAMILY_ID,
        "parse_grok",
        1,
        &[ParameterKind::Filter],
        GROK_EXAMPLES,
    ),
];

/// The parser execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, ParseLaw)] = &[
    (id::PARSE_URL, ParseLaw::Url),
    (id::PARSE_QUERY_STRING, ParseLaw::QueryString),
    (id::PARSE_LOGFMT, ParseLaw::Logfmt),
    (id::PARSE_SYSLOG, ParseLaw::Syslog),
    (id::PARSE_USER_AGENT, ParseLaw::UserAgent),
    (id::PARSE_GROK, ParseLaw::Grok),
];

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_data::Value;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(1).expect("work"),
        )
        .expect("resources")
    }

    /// `parse_url` splits `scheme://[userinfo@]host[:port]/path?query#fragment`, with the userinfo stripped BEFORE the
    /// host:port split — a userinfo holding a raw colon (`user:pass@`) used to be misread as the port separator,
    /// answering host "user" — and a bracketed IPv6 host keeps its brackets while its port splits at the last colon,
    /// which sits AFTER the closing bracket (`[::1]:8080` answered host `"[::1]:8080"`, port null before).
    #[test]
    fn parse_url_strips_userinfo_and_splits_bracketed_ipv6_ports() {
        let r = resources();
        let host_of = |url: &str| match parse_url(url, &r).expect("parse_url succeeds") {
            Value::Object(object) => match object.get("host") {
                Some(Value::String(s)) => s.as_str().to_owned(),
                other => panic!("expected a string host, got {other:?}"),
            },
            other => panic!("expected an object, got {other:?}"),
        };
        let port_of = |url: &str| match parse_url(url, &r).expect("parse_url succeeds") {
            Value::Object(object) => match object.get("port") {
                Some(Value::Number(n)) => n.to_i64(),
                Some(Value::Null) => None,
                other => panic!("expected a number or null port, got {other:?}"),
            },
            other => panic!("expected an object, got {other:?}"),
        };
        // Userinfo with a colon must not become the host or the port.
        assert_eq!(host_of("http://user:pass@example.com/path"), "example.com");
        assert_eq!(port_of("http://user:pass@example.com/path"), None);
        // The IPv6 bracket law: host keeps its brackets, the port splits.
        assert_eq!(host_of("http://[::1]:8080/x"), "[::1]");
        assert_eq!(port_of("http://[::1]:8080/x"), Some(8080));
        // A bracketed host with no port keeps brackets and a null port.
        assert_eq!(host_of("http://[::1]/x"), "[::1]");
        assert_eq!(port_of("http://[::1]/x"), None);
        // Ordinary hosts keep working: port splits, absent port is null.
        assert_eq!(port_of("http://example.com:80"), Some(80));
        assert_eq!(port_of("http://example.com"), None);
    }

    /// A parsed record answers `null` ONLY for a genuinely missing capture; every owned-field site propagates an
    /// allocation refusal through `?` (the `Result` return types leave no silent-null fallback to compile). This
    /// binary's unit tests do not install the counting allocator, so the refusal arm itself cannot be forced here —
    /// these assertions pin the reachable side of the law: present captures keep their text, absent ones answer null,
    /// and the records still render.
    #[test]
    fn parse_records_answer_null_only_for_missing_captures() {
        let r = resources();
        let object_of = |value: Result<Value, EngineRunError>| match value.expect("record parses") {
            Value::Object(object) => object,
            other => panic!("expected an object, got {other:?}"),
        };
        let string_of = |value: Value| match value {
            Value::String(text) => String::from(text.as_str()),
            other => panic!("expected a string, got {other:?}"),
        };

        // logfmt keeps quoting through the owned-field path.
        let logfmt = object_of(parse_logfmt("a=1 b=\"two c\"", &r));
        assert_eq!(string_of(logfmt.get("a").cloned().expect("a")), "1");
        assert_eq!(string_of(logfmt.get("b").cloned().expect("b")), "two c");

        // syslog without an RFC version token and without a `[pid]`: both captures are genuinely absent, and each null
        // site is distinct code (the version arm and the named-field closure).
        let syslog = object_of(parse_syslog("<34>Jan 5 08:12:60 host app: booted", &r));
        assert_eq!(string_of(syslog.get("host").cloned().expect("host")), "host");
        assert!(matches!(syslog.get("version"), Some(Value::Null)));
        assert!(matches!(syslog.get("procid"), Some(Value::Null)));
    }
}
