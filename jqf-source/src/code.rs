//! Static identifiers stored by [`crate::Diagnostic`].
//!
//! A namespace is one non-empty `[a-z0-9_-]` segment; a code name is one or more such segments joined by dots. They
//! print as `namespace.name`, and construction panics on a spelling outside that grammar.

use core::fmt;

/// A diagnostic-code namespace: a static non-empty lowercase ASCII segment (`[a-z0-9_-]`). ASCII-only so codes stay
/// searchable in logs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Namespace {
    name: &'static str,
}

impl Namespace {
    /// Create a namespace.
    ///
    /// # Panics
    ///
    /// Panics when `name` is empty or contains a byte outside `[a-z0-9_-]`.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        assert!(is_valid_segment(name.as_bytes()), "invalid diagnostic namespace");
        Self { name }
    }

    /// Namespace text.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Create a diagnostic code in this namespace.
    ///
    /// # Panics
    ///
    /// Panics when `name` is empty, has an empty `.`-separated segment, or contains a byte outside `[a-z0-9_.-]`.
    #[must_use]
    pub const fn code(self, name: &'static str) -> Code {
        assert!(is_valid_code_name(name.as_bytes()), "invalid diagnostic code name");
        Code { namespace: self, name }
    }
}

/// A diagnostic code. Prints as `namespace.name`.
///
/// The name may contain `.` to group related codes under one namespace.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Code {
    namespace: Namespace,
    name: &'static str,
}

impl Code {
    /// Code namespace.
    #[must_use]
    pub const fn namespace(self) -> Namespace {
        self.namespace
    }

    /// Code name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.namespace.name(), self.name)
    }
}
const fn is_valid_code_name(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }

    let mut index = 0;
    let mut segment_len = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'.' {
            if segment_len == 0 {
                return false;
            }
            segment_len = 0;
        } else if is_segment_byte(byte) {
            segment_len += 1;
        } else {
            return false;
        }
        index += 1;
    }

    segment_len != 0
}

const fn is_valid_segment(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }

    let mut index = 0;
    while index < bytes.len() {
        if !is_segment_byte(bytes[index]) {
            return false;
        }
        index += 1;
    }

    true
}

const fn is_segment_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
}
