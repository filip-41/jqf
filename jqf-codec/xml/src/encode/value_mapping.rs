//! The §1 value-mapping grammar shared by the two encode products.
//!
//! [`super::owned_value`] lowers values into a synthetic document and renders
//! through [`super::DeterministicSerializer`] (document frame, namespaces,
//! trailing newline). [`super::edit_splice`] renders the same child grammar as
//! bare splice bytes (no frame, no trailing newline). The mapping law is
//! identical; the wrappers are not.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;

use jqf_codec_core::CodecError;
use jqf_data::DecimalText;

use super::unsupported;

/// Document element name for owned-value encode.
pub(crate) const VALUE_ROOT_NAME: &str = "root";
/// Array-item element name under the §1 mapping.
pub(crate) const VALUE_ITEM_NAME: &str = "item";

/// Whether `name` is a valid XML `Name` per the decoder's own rule
/// (`is_name_start`/`is_name_char`, the exact §4.9 law).
pub(crate) fn valid_element_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    crate::parse::is_name_start(first) && chars.all(crate::parse::is_name_char)
}

/// The canonical number text of a value: an integer's own digits,
/// a decimal's scientific-string form, and a binary64's
/// float rendering (NaN renders `null`, an infinity the clamped
/// widest finite — the shared number-text law, not a codec decision).
pub(crate) fn value_number_text(number: &jqf_data::Number) -> Option<String> {
    // The inline machine arm renders its canonical spelling on demand;
    // the boxed arm borrows its retained one.
    if let Some(machine) = number.as_machine() {
        let integer = jqf_data::Integer::from_i64(machine);
        return Some(integer.as_str().to_owned());
    }
    if let Some(integer) = number.as_integer() {
        return Some(integer.as_str().to_owned());
    }
    if let Some(decimal) = number.as_decimal() {
        let rendered = DecimalText::new(decimal.coefficient().as_str(), decimal.scale())?;
        let mut out = String::new();
        for piece in rendered.pieces() {
            out.push_str(core::str::from_utf8(piece).ok()?);
        }
        return Some(out);
    }
    let float = number.as_float()?;
    Some(jqf_data::format_binary64(float.get())?.as_str().to_owned())
}

/// Refusal when an object key is not a valid element name on the owned-value
/// encode path.
pub(crate) fn invalid_element_key_for_encode(key: &str) -> CodecError {
    unsupported(&format!(
        "object key {key:?} is not a valid XML element name; the value cannot \
         be represented as XML (the value mapping never renames keys)"
    ))
}

/// Refusal when an object key is not a valid element name on the edit-splice
/// child path.
pub(crate) fn invalid_element_key_for_splice(key: &str) -> CodecError {
    unsupported(&format!(
        "object key {key:?} is not a valid XML element name; the value cannot \
         be spliced into XML (the value mapping never renames keys)"
    ))
}
