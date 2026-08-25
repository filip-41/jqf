//! `render.shell@1`: one POSIX `sh` assignment per flattened leaf.
//!
//! Every leaf of a value tree becomes one `name=value` line. Paths flatten with a configurable separator (default `_`),
//! array indices are ordinary path components (`b_d_0`), a root scalar sits under the key `value`, and a root array
//! under the empty prefix (`_0`, `_1`). A string single-quotes per POSIX `sh` with `'\''` for an embedded quote —
//! byte-identical to the `@sh` builtin's word law (`jqf-builtins/src/registry/builtins/format.rs`) — and every other
//! scalar renders its JSON text unquoted (`a=1`, `b=true`, `c=null`). A NON-FINITE float is the one number divergence:
//! it spells the render package's shared law — `nan(0x…)`, `inf`, `-inf`, quoted as one word because the NaN
//! spelling's parentheses are not legal bare assignment text — never JSON's `null`, which would be indistinguishable
//! from a real null.
//!
//! Three refusals are terminal (zero bytes published): a key that is not a valid shell name (`[A-Za-z_][A-Za-z0-9_]*`),
//! two distinct document paths that flatten to the same variable, and a flattened name that is caller-significant
//! (`IFS`, `PATH`, `LD_PRELOAD`, and the rest of the sourced-shell trap set). The output is evaluated as a shell
//! program, so a collision is never a silent overwrite, a mangled key is never a silently different name, and sourcing
//! the frame cannot clobber the caller's environment. An EMPTY container has no leaves to flatten and is never dropped:
//! its assignment spells its JSON literal (`a='[]'`, `b='{}'`), so a sourced consumer reads a variable holding it.
//! Tags, bytes, and the temporal kinds have no shell spelling and refuse with the path named.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, TagLayer, value_tag_layer};
use jqf_data::{DecimalText, Number, Value, format_binary64};

use super::error::{contract, unsupported_owned};
use super::options::RenderEncodeOptions;

/// The one shell-name law: every variable must match `[A-Za-z_][A-Za-z0-9_]*`.
const NAME_RULE: &str = "[A-Za-z_][A-Za-z0-9_]*";

/// Flattened names that must never appear as a bare assignment. Sorted for `binary_search`. Sourcing the frame would
/// clobber the caller's field splitting, command lookup, linker preload, and the rest of the POSIX / bash trap set.
/// Refusal, not prefix: a mangled key is never a silently different name.
const RESERVED_NAMES: &[&str] = &[
    "BASHOPTS",
    "BASH_ENV",
    "CDPATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "ENV",
    "FPATH",
    "GCONV_PATH",
    "GLOBIGNORE",
    "IFS",
    "LD_AUDIT",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "NLSPATH",
    "PATH",
    "PROMPT_COMMAND",
    "PS4",
    "SHELLOPTS",
];

/// Renders one item as one `name=value` line per flattened leaf.
///
/// The frame carries no trailing LF (the facade appends the single final LF, as every renderer's does). Any refusal
/// publishes zero bytes.
///
/// # Errors
///
/// Returns an `UnsupportedRepresentation` reject for a key that is not a valid shell name, for two paths that flatten
/// to the same variable, for a flattened name in the sourced-shell trap set, for a value kind with no shell spelling,
/// or for nesting past the crate's depth ceiling; an allocation failure; or an internal-contract error.
pub(crate) fn render(value: &Value, options: RenderEncodeOptions) -> Result<String, CodecError> {
    let mut walk = Walk {
        out: String::new(),
        separator: options.shell_separator,
        seen: BTreeMap::new(),
        started: false,
    };
    // A root scalar AND a root EMPTY container emit under the fixed key `value` (an empty container has no leaves to
    // flatten); a non-empty root container starts with an empty prefix (a root array's first element is `_0`).
    let root = match value.untagged() {
        Value::Object(object) if !object.is_empty() => String::new(),
        Value::Array(array) if !array.is_empty() => String::new(),
        _ => "value".to_owned(),
    };
    walk.node(value, &root, &mut Vec::new(), 0)?;
    Ok(walk.out)
}

/// One component of the flattened document path, for the refusal prose.
enum Component {
    /// One object key, appended as `.key`.
    Key(String),
    /// One array index, appended as `[i]`.
    Index(u64),
}

struct Walk<'a> {
    out: String,
    separator: &'a str,
    /// Every variable name emitted so far with the FIRST document path that produced it, for the collision refusal's
    /// two-path prose. The map keeps membership at O(log n) per leaf; the first-inserted path wins.
    seen: BTreeMap<String, String>,
    /// Whether a line has already been emitted (lines join with `\n`; the facade appends the single final LF, so the
    /// frame has no trailing one).
    started: bool,
}

impl Walk<'_> {
    fn node(&mut self, value: &Value, prefix: &str, path: &mut Vec<Component>, depth: usize) -> Result<(), CodecError> {
        // The crate's shared nesting law: a document can nest arbitrarily deep and this walk recurses once per
        // container level, so past the ceiling it refuses by name instead of overflowing the stack. See
        // [`crate::MAX_NESTING_DEPTH`].
        if depth >= crate::MAX_NESTING_DEPTH {
            return Err(unsupported_owned(
                "shell-depth",
                "the value nests past the render package's depth ceiling",
            ));
        }
        if let TagLayer::Tagged(_) = value_tag_layer(value) {
            return Err(unsupported_owned(
                "shell-tag",
                &format!(
                    "a tagged value at document path {} has no shell spelling",
                    render_path(path)
                ),
            ));
        }
        match value.untagged() {
            Value::Object(object) => {
                if object.is_empty() {
                    self.emit(prefix, value, path)?;
                    return Ok(());
                }
                for index in 0..object.len() {
                    let entry = object
                        .get_index(index)
                        .ok_or_else(|| contract("an object index past its length"))?;
                    let key = entry.key();
                    path.push(Component::Key(key.to_owned()));
                    if !is_valid_name(key) {
                        return Err(unsupported_owned(
                            "shell-key",
                            &format!(
                                "the key at document path {} is not a valid shell variable name \
                                 ({NAME_RULE}); rename the key or pre-flatten with with_entries",
                                render_path(path)
                            ),
                        ));
                    }
                    let mut child_prefix = String::new();
                    join(&mut child_prefix, prefix, self.separator, key);
                    self.node(entry.value(), &child_prefix, path, depth + 1)?;
                    path.pop();
                }
            }
            Value::Array(array) => {
                if array.is_empty() {
                    // An empty container has no leaves to flatten, which used to DROP it silently; the faithful
                    // spelling is its JSON literal as the quoted word (a sourced consumer reads a variable holding `[]`
                    // / `{}`).
                    self.emit(prefix, value, path)?;
                    return Ok(());
                }
                for index in 0..array.len() {
                    let child = array
                        .get(index)
                        .ok_or_else(|| contract("an array index past its length"))?;
                    let text = index.to_string();
                    let mut child_prefix = String::new();
                    push_array_child(&mut child_prefix, prefix, self.separator, &text);
                    path.push(Component::Index(index as u64));
                    self.node(child, &child_prefix, path, depth + 1)?;
                    path.pop();
                }
            }
            _ => self.emit(prefix, value, path)?,
        }
        Ok(())
    }

    /// Appends one `name=value` line, refusing on a reserved name, a collision, or an unspellable value BEFORE any byte
    /// of the line is staged.
    ///
    /// An empty container arrives here with the CONTAINER itself (from the walk's empty arms): its spelling is the
    /// quoted JSON literal — `'[]'` or `'{}'` — so nothing is silently dropped.
    fn emit(&mut self, name: &str, value: &Value, path: &[Component]) -> Result<(), CodecError> {
        if is_reserved_name(name) {
            return Err(unsupported_owned(
                "shell-special",
                &format!(
                    "the shell variable \"{name}\" at document path {} is \
                     caller-significant (IFS, PATH, LD_PRELOAD, ENV, and the \
                     sourced-shell trap set) and cannot be assigned; rename \
                     the key or pre-flatten with with_entries",
                    render_path(path)
                ),
            ));
        }
        if let Some(earlier) = self.seen.get(name) {
            return Err(unsupported_owned(
                "shell-collision",
                &format!(
                    "shell variable \"{name}\" collides: document paths \"{earlier}\" and \
                     \"{}\" both flatten to it",
                    render_path(path)
                ),
            ));
        }
        let mut spelling = String::new();
        match value.untagged() {
            Value::Null => spelling.push_str("null"),
            Value::Bool(true) => spelling.push_str("true"),
            Value::Bool(false) => spelling.push_str("false"),
            Value::Number(number) => write_number(&mut spelling, number)?,
            // An empty string renders `key=` with nothing after the `=`; every non-empty string single-quotes as one
            // word. The empty spelling is the ONE deliberate divergence from `@sh` (which writes `''`).
            Value::String(text) if text.is_empty() => {}
            Value::String(text) => push_word(&mut spelling, text),
            // An EMPTY container (the walk routes it here): its JSON literal, single-quoted like any word.
            container @ (Value::Array(_) | Value::Object(_)) => {
                push_word(
                    &mut spelling,
                    if matches!(container, Value::Array(_)) {
                        "[]"
                    } else {
                        "{}"
                    },
                );
            }
            other => {
                return Err(unsupported_owned(
                    "shell-value",
                    &format!(
                        "a {} value at document path {} has no shell spelling",
                        kind_name(other),
                        render_path(path)
                    ),
                ));
            }
        }
        if self.started {
            self.out.push('\n');
        }
        self.out.push_str(name);
        self.out.push('=');
        self.out.push_str(&spelling);
        self.started = true;
        self.seen.entry(name.to_owned()).or_insert_with(|| render_path(path));
        Ok(())
    }
}

/// Whether `key` is a valid shell variable name: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_valid_name(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Whether the flattened variable is in the sourced-shell trap set.
fn is_reserved_name(name: &str) -> bool {
    RESERVED_NAMES.binary_search(&name).is_ok()
}

/// Appends `prefix` + `separator` + `component`, omitting the separator when the prefix is empty (a root object's first
/// key starts the name bare).
fn join(out: &mut String, prefix: &str, separator: &str, component: &str) {
    out.push_str(prefix);
    if !prefix.is_empty() {
        out.push_str(separator);
    }
    out.push_str(component);
}

/// Appends `prefix` + `separator` + `component` ALWAYS: a root array's first element is `_0`, never a bare `0`.
fn push_array_child(out: &mut String, prefix: &str, separator: &str, component: &str) {
    out.push_str(prefix);
    out.push_str(separator);
    out.push_str(component);
}

/// The refusal prose for one document path: `.a.b`, `.a[0]`.
fn render_path(path: &[Component]) -> String {
    let mut text = String::new();
    for component in path {
        match component {
            Component::Key(key) => {
                text.push('.');
                text.push_str(key);
            }
            Component::Index(index) => {
                text.push('[');
                text.push_str(&index.to_string());
                text.push(']');
            }
        }
    }
    text
}

/// Appends one string as a POSIX single-quoted word: `'...'` with each `'` written `'\''`. Nothing else is escaped —
/// inside single quotes `$`, backticks, backslashes, newlines, tabs, and every non-ASCII byte are literal.
/// Byte-identical to the `@sh` builtin's word law.
fn push_word(out: &mut String, text: &str) {
    out.push('\'');
    for ch in text.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
}

/// Appends one number's JSON text: an integer verbatim, an exact decimal in its scientific-string form, a finite
/// binary64 through the shortest-round-trip placement — the same three spellings the `@sh` builtin's non-string arm
/// reads from `jqf-builtins`' JSON writer. A non-finite binary64 spells the render package's shared non-finite law
/// instead (quoted; see the module doc).
fn write_number(out: &mut String, number: &Number) -> Result<(), CodecError> {
    if let Some(integer) = number.to_integer() {
        out.push_str(integer.as_str());
        return Ok(());
    }
    if let Some(decimal) = number.as_decimal() {
        let text = DecimalText::new(decimal.coefficient().as_str(), decimal.scale())
            .ok_or_else(|| contract("a decimal with no scientific-string form"))?;
        for piece in text.pieces() {
            let piece = core::str::from_utf8(piece).map_err(|_| contract("a decimal rendering that is not UTF-8"))?;
            out.push_str(piece);
        }
        return Ok(());
    }
    match number.as_float() {
        Some(value) => {
            let raw = value.get();
            // The render package's non-finite law owns these spellings (`nan(0x…)`/`inf`/`-inf`) — routing around
            // it would spell NaN as `null`, indistinguishable from a real null and inconsistent with every sibling
            // renderer. The NaN spelling carries parentheses, which are not legal bare assignment text, so the
            // non-finite word is single-quoted like any other word.
            if raw.is_nan() || raw.is_infinite() {
                let mut text = String::new();
                super::scalar::write_float(&mut text, raw)?;
                push_word(out, &text);
                return Ok(());
            }
            let text = format_binary64(raw).ok_or_else(|| contract("a binary64 with no shortest-round-trip form"))?;
            out.push_str(text.as_str());
        }
        None => return Err(contract("a number carrying no representation")),
    }
    Ok(())
}

/// The prose kind name for the value-refusal message.
fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Bytes(_) => "bytes",
        Value::LocalDate(_) => "date",
        Value::LocalTime(_) => "time",
        Value::LocalDateTime(_) => "date-time",
        Value::OffsetDateTime(_) => "offset date-time",
        _ => "value",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reserved_names_are_sorted_for_binary_search() {
        let mut sorted = super::RESERVED_NAMES.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, super::RESERVED_NAMES);
    }
}
