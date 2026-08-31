//! Embedded prelude gate and process-wide parse+bind cache.
//!
//! [`scan_prelude_gate`] is a token-aware pre-parse scan over user source.
//! [`stdlib_prelude`] / [`extension_prelude`] load embedded prelude ASTs when
//! the gate hits; parse+bind is paid once per process. Parallel cold starts
//! serialize on one winner.
//!
//! Invariants:
//! - False gate hits only waste a prelude parse; false misses are unsound.
//! - Module directives (`import` / `include` / `module`) force both preludes.
//! - Identifiers inside `"` strings and after `#` line comments are skipped;
//!   `\(…)` interpolation holes are CODE and scanned (a hole may contain a
//!   string that contains a hole).
//! - Prelude call names and directive spellings live in [`super::stdlib`];
//!   the name lists stay in step with prelude text via compile tests.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

use super::EngineCompileError;
use super::parse::{bind_syntax, into_valid_syntax, parse_query_input};
use super::prelude::*;
use super::stdlib::{EXTENSION_NAMES, EXTENSION_PRELUDE, STDLIB_NAMES, STDLIB_PRELUDE};

/// Whether embedded stdlib and/or extension preludes must be parsed.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreludeGate {
    pub needs_stdlib: bool,
    pub needs_extension: bool,
}

impl PreludeGate {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            needs_stdlib: false,
            needs_extension: false,
        }
    }
}

/// Scans `source` once for module directives and prelude identifier tokens.
#[doc(hidden)]
#[must_use]
pub fn scan_prelude_gate(source: &str) -> PreludeGate {
    let mut gate = PreludeGate::empty();
    if source.is_empty() {
        return gate;
    }
    scan_tokens(source, 0, false, &mut gate);
    gate
}

/// One identifier-token scan over `source[index..]`, mutating `gate`, until
/// end of input or — when `stop_at_paren` — the balanced `)` closing an
/// interpolation hole. Returns the index one past that `)` (or the end).
///
/// A `"` starts a string, which [`skip_double_quoted`] walks without
/// consulting as tokens; `#` starts a line comment. A `\(` inside a string is
/// an INTERPOLATION HOLE — an expression, not string text — so the string
/// walker hands the hole back to this scanner, which is why strings and holes
/// recurse through each other (a hole can contain a string that contains a
/// hole).
fn scan_tokens(source: &str, mut index: usize, stop_at_paren: bool, gate: &mut PreludeGate) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    while index < bytes.len() {
        if gate.needs_stdlib && gate.needs_extension {
            return bytes.len();
        }
        let byte = bytes[index];
        if byte == b'"' {
            index = skip_double_quoted(bytes, source, index + 1, gate);
            continue;
        }
        if byte == b'#' {
            index = skip_line_comment(bytes, index + 1);
            continue;
        }
        if stop_at_paren && (byte == b'(' || byte == b')') {
            if byte == b'(' {
                depth += 1;
            } else if depth == 0 {
                return index + 1;
            } else {
                depth -= 1;
            }
            index += 1;
            continue;
        }
        if is_ident_byte(byte) {
            let start = index;
            index += 1;
            while index < bytes.len() && is_ident_byte(bytes[index]) {
                index += 1;
            }
            let ident = &source[start..index];
            match ident {
                "import" | "include" | "module" => {
                    gate.needs_stdlib = true;
                    gate.needs_extension = true;
                    return bytes.len();
                }
                name if STDLIB_NAMES.contains(&name) => gate.needs_stdlib = true,
                name if EXTENSION_NAMES.contains(&name) => gate.needs_extension = true,
                _ => {}
            }
            continue;
        }
        index += 1;
    }
    bytes.len()
}

fn skip_line_comment(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] != b'\n' {
        index += 1;
    }
    index
}

/// Skips one double-quoted string whose opening `"` is at `index - 1`,
/// mutating `gate` for every interpolation hole the string contains.
/// Returns the index one past the closing quote (or the end when unterminated).
///
/// A `\(` opens an interpolation hole: its body is an expression, so it is
/// handed back to [`scan_tokens`] — a prelude name there is a real call, and a
/// false miss (skipping it as string text) would leave the name undefined.
fn skip_double_quoted(bytes: &[u8], source: &str, mut index: usize, gate: &mut PreludeGate) -> usize {
    while index < bytes.len() {
        if gate.needs_stdlib && gate.needs_extension {
            return bytes.len();
        }
        match bytes[index] {
            b'\\' => match bytes.get(index + 1) {
                // `\(` starts an interpolation hole; the hole ends at its
                // balanced `)` no matter how it nests.
                Some(b'(') => index = scan_tokens(source, index + 2, true, gate),
                Some(_) => index = index.saturating_add(2).min(bytes.len()),
                None => return bytes.len(),
            },
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

struct CachedPrelude {
    root: *const Expr,
    source: *const SyntaxSource<'static>,
}

impl CachedPrelude {
    fn pair(&self) -> (&'static Expr, &'static SyntaxSource<'static>) {
        // SAFETY: the leaked bound program outlives every compile request.
        unsafe { (&*self.root, &*self.source) }
    }
}

const INIT_EMPTY: u8 = 0;
const INIT_RUNNING: u8 = 1;
const INIT_READY: u8 = 2;
const INIT_FAILED: u8 = 3;

struct InitGuard<'a> {
    state: &'a AtomicU8,
    disarmed: bool,
}

impl Drop for InitGuard<'_> {
    fn drop(&mut self) {
        if !self.disarmed {
            self.state.store(INIT_FAILED, Ordering::Release);
        }
    }
}

struct InitOnce<T> {
    state: AtomicU8,
    value: AtomicPtr<T>,
}

impl<T> InitOnce<T> {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(INIT_EMPTY),
            value: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn get_or_try_init(&self, init: impl FnOnce() -> Result<T, EngineCompileError>) -> Result<&T, EngineCompileError> {
        loop {
            match self.state.load(Ordering::Acquire) {
                INIT_READY => {
                    let value = self.value.load(Ordering::Acquire);
                    debug_assert!(!value.is_null());
                    // SAFETY: READY implies a prior successful init stored `value`.
                    return Ok(unsafe { &*value });
                }
                INIT_FAILED => return Err(EngineCompileError::PreludeInitFailed),
                INIT_EMPTY => {
                    if self
                        .state
                        .compare_exchange(INIT_EMPTY, INIT_RUNNING, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        let mut guard = InitGuard {
                            state: &self.state,
                            disarmed: false,
                        };
                        return match init() {
                            Ok(value) => {
                                let leaked = Box::leak(Box::new(value));
                                self.value.store(leaked, Ordering::Release);
                                self.state.store(INIT_READY, Ordering::Release);
                                guard.disarmed = true;
                                Ok(leaked)
                            }
                            Err(error) => {
                                self.state.store(INIT_EMPTY, Ordering::Release);
                                guard.disarmed = true;
                                Err(error)
                            }
                        };
                    }
                }
                _ => core::hint::spin_loop(),
            }
        }
    }
}

static STDLIB: InitOnce<CachedPrelude> = InitOnce::new();
static EXTENSION: InitOnce<CachedPrelude> = InitOnce::new();

fn leak_prelude(
    label: &'static str,
    text: &'static str,
    source_ref: SourceRef,
) -> Result<CachedPrelude, EngineCompileError> {
    let parse = parse_query_input(source_ref, text)?;
    let syntax = into_valid_syntax(parse)?;
    let syntax = Box::new(syntax);
    let (root, source) = {
        let bound = bind_syntax(&syntax, source_ref, label, text)?;
        (core::ptr::from_ref(bound.root()), *bound.source())
    };
    let _ = Box::leak(syntax);
    let source = Box::leak(Box::new(source));
    Ok(CachedPrelude {
        root,
        source: core::ptr::from_ref(source),
    })
}

fn stdlib_once() -> Result<&'static CachedPrelude, EngineCompileError> {
    STDLIB.get_or_try_init(|| {
        let prelude_ref = SourceRef::new(SourceId::new(1), SourceKind::Query);
        leak_prelude("<stdlib>", STDLIB_PRELUDE, prelude_ref)
    })
}

fn extension_once() -> Result<&'static CachedPrelude, EngineCompileError> {
    EXTENSION.get_or_try_init(|| {
        let extension_ref = SourceRef::new(SourceId::new(2), SourceKind::Query);
        leak_prelude("<extension>", EXTENSION_PRELUDE, extension_ref)
    })
}

pub(crate) fn stdlib_prelude() -> Result<(&'static Expr, &'static SyntaxSource<'static>), EngineCompileError> {
    Ok(stdlib_once()?.pair())
}

pub(crate) fn extension_prelude() -> Result<(&'static Expr, &'static SyntaxSource<'static>), EngineCompileError> {
    Ok(extension_once()?.pair())
}
