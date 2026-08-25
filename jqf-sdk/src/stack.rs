//! The request-thread stack size the CLI and the FFI handle share.
//!
//! Parse and lowering recurse over the program tree on the call stack. The
//! documented `10_000` nesting refusal needs a large stack; a default thread
//! aborts far sooner. [`request_stack_bytes`] is the one parser for
//! `JQF_REQUEST_STACK_BYTES`: default 256 MiB, floor 64 KiB, fail-loud on
//! bad UTF-8 or a value below the floor. [`Request`](crate::Request) and
//! [`ResourceContext`](crate::ResourceContext) are `!Send`, so a caller
//! cannot hop an existing request onto a sized thread — spawn first, then
//! construct the request on that thread. See the crate-level stack caveat.

use std::env;

/// Default request-thread stack: sized to hold the documented nesting
/// ceiling in an unoptimized build with room to spare. The reservation is
/// virtual address space, not committed memory.
pub const DEFAULT_REQUEST_STACK_BYTES: usize = 256 << 20;

/// Below this the process cannot reliably reach the request at all, and a
/// lane that dies before its recursion starts proves nothing about a depth
/// guard.
pub const MIN_REQUEST_STACK_BYTES: usize = 64 << 10;

/// Environment variable that sizes the request thread. A test instrument
/// (the stack-depth gate and its teeth probe are the intended callers) and
/// fail-loud on purpose: a malformed value or a value below the floor is an
/// error rather than a silent fallback.
pub const REQUEST_STACK_BYTES_VAR: &str = "JQF_REQUEST_STACK_BYTES";

/// Parses [`REQUEST_STACK_BYTES_VAR`] into a stack size in bytes.
///
/// Unset → [`DEFAULT_REQUEST_STACK_BYTES`]. Non-UTF-8, a non-integer, or a
/// value below [`MIN_REQUEST_STACK_BYTES`] is an error naming the variable
/// and the problem, the same messages the CLI prints at exit 2.
pub fn request_stack_bytes() -> Result<usize, String> {
    let requested = match env::var(REQUEST_STACK_BYTES_VAR) {
        Ok(text) => text,
        Err(env::VarError::NotPresent) => return Ok(DEFAULT_REQUEST_STACK_BYTES),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(format!("{REQUEST_STACK_BYTES_VAR} is not valid UTF-8"));
        }
    };
    let trimmed = requested.trim();
    // `usize::from_str` accepts a leading `+`; a byte count has no sign.
    if trimmed.starts_with('+') {
        return Err(format!(
            "{REQUEST_STACK_BYTES_VAR}: expected a byte count, got {requested:?}"
        ));
    }
    let bytes: usize = trimmed.parse().map_err(|error: std::num::ParseIntError| {
        // An overflowing literal IS a byte count — name the range, not the
        // syntax.
        match error.kind() {
            std::num::IntErrorKind::PosOverflow => {
                format!("{REQUEST_STACK_BYTES_VAR}: {requested} is above the largest supported byte count")
            }
            _ => format!("{REQUEST_STACK_BYTES_VAR}: expected a byte count, got {requested:?}"),
        }
    })?;
    if bytes < MIN_REQUEST_STACK_BYTES {
        return Err(format!(
            "{REQUEST_STACK_BYTES_VAR}: {bytes} is below the {MIN_REQUEST_STACK_BYTES}-byte floor"
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_returns_the_default() {
        // The helper is process-global; this test only asserts the documented
        // constants so a silent default change is a compile-visible edit.
        assert_eq!(DEFAULT_REQUEST_STACK_BYTES, 256 << 20);
        assert_eq!(MIN_REQUEST_STACK_BYTES, 64 << 10);
    }
}
