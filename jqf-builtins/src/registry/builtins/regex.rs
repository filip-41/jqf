//! The regex family: the reference's pattern builtins (`test match capture scan splits split/2 sub gsub`), over the
//! translation layer the old base carried.
//!
//! One job: own the family/overload records AND the per-law evaluators the executor dispatches through. The executor
//! evaluates the pattern/flags argument filters over the same input, and for `sub`/`gsub` it evaluates the REPLACEMENT
//! filter once per match with dot = the match's capture object — the reference's law, transcribed here as a two-phase
//! contract: the executor asks for the match set, evaluates the replacement per match, and hands the assembled outputs
//! back for publication.
//!
//! The reference compiles patterns with its bundled regex engine; jqf uses TWO Rust engines behind the SAME translation
//! layer the old base verified: the reference's flag string (`g n l i m p s x`) maps onto the engines' builders
//! (`m`/`p` enable dot-matches-newline, `s` is a no-op, `l` is the longest-match search), and the reference's
//! `(?<name>…)` named-capture spelling is rewritten while the capture-name list is tracked for the match/capture
//! objects. Error TEXT for a pattern compile failure is the Rust engine's, not the reference engine's — a catalogued
//! divergence the compat corpus pins with exit-class rows rather than `stderrparity` rows.
//!
//! **The tier is a property of the PATTERN, decided once when it compiles.** `regex_automata` stays the engine for
//! every pattern it can express, which is all but a handful — its performance is why the compiled-regex cache exists
//! at all. `analyze_pattern` classifies the pattern's syntax, and only the constructs that engine cannot express AT ALL
//! (lookaround, backreferences, atomic groups, possessive quantifiers, subroutine calls, `\K` — the
//! `FallbackConstruct` list) route to `fancy-regex`'s backtracking engine. Every one of those is a construct the fast
//! tier REFUSES to compile, pinned by `every_routed_construct_is_one_the_fast_tier_refuses`, so routing can only ever
//! turn an error into an answer — never one answer into another. The decision never depends on the INPUT, so a scan
//! cannot change engines mid-flight.
//!
//! The SECOND catalogued divergence is the zero-width advance: the reference walks a global scan forward by BYTE, so
//! inside a multi-byte character it re-finds the same empty match at each interior byte and emits it once per byte
//! (`"aé漢b" | gsub("";"X")` is `XaXéXX漢XXXbX`). jqf advances by CODEPOINT — one empty match per character —
//! and the compat corpus pins both readings as `intdiff` rows.
//!
//! What the reference spells as a standard-library definition rather than C is law here too, because the wrapper's
//! checks run BEFORE the matcher's: `test`/`match`/`capture` take a `[regex, flags]` array as their /1 argument and
//! reject a bad argument before the input is read, while `scan`/`splits`/`split`/`sub`/`gsub` hand their `$re` straight
//! through. The `"g"` those definitions concatenate is visible in their rejection sentences, which is why the flag
//! errors differ by name.

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicPtr, Ordering};
use fancy_regex::{Captures as FancyCaptures, Regex as FancyRegex};
use jqf_data::{Array, Number, ObjectBuilder, Value};
use jqf_resource::ResourceContext;
use regex::{Regex, RegexBuilder};
use regex_automata::{
    Anchored, Input as RegexAutomataInput, MatchKind, Span as RegexAutomataSpan, meta::Regex as AutomataRegex,
};

use super::id;
use crate::error::{EngineRunError, message};
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::path::raise;

// ------------------------------------------------------------------------
// POSIX bracket class → Unicode property mapping.
//
// The `regex` crate defines `[[:alpha:]]` and its siblings as ASCII-only by design; the reference engine's are
// Unicode-aware under UTF-8. This table maps each POSIX class name to a Unicode-aware expression that both `regex` and
// `fancy-regex` accept inside a character class. Every entry was derived from the reference engine's documented Unicode
// property set per class.
//
// POSIX class → Unicode-aware expression: [:alpha:]  → \p{Alphabetic}        (Letter) [:alnum:]  →
// \p{Alphabetic}\p{Nd} [:ascii:]  → \x00-\x7f              (identity) [:blank:]  → [\t\p{Zs}]             (tab +
// Space_Separator) [:cntrl:]  → \p{Cc}                 (Control) [:digit:]  → \p{Nd}
// (Decimal_Number) [:graph:]  → [\pL\pM\pN\pP\pS]     (visible = not Z/C) [:lower:]  → \p{Lowercase} [:print:]  →
// [\pL\pM\pN\pP\pS\p{Zs}\t]
//                                       (graph + space + tab)
// [:punct:]  → \pP                    (Punctuation) [:space:]  → \p{White_Space} [:upper:]  → \p{Uppercase}
// [:word:]   → [\p{Alphabetic}\p{Nd}\pM\p{Pc}\p{JoinC}] [:xdigit:] → [0-9a-fA-F]            (identity)
//
// Each value is emitted inside the existing character class, so `[[:alpha:]0-9_]` becomes `[\p{Alphabetic}0-9_]` —
// the properties compose.

const POSIX_TO_UNICODE: &[(&str, &str)] = &[
    ("alpha", r"\p{Alphabetic}"),
    ("alnum", r"\p{Alphabetic}\p{Nd}"),
    ("ascii", r"\x00-\x7f"),
    ("blank", r"[\t\p{Zs}]"),
    ("cntrl", r"\p{Cc}"),
    ("digit", r"\p{Nd}"),
    ("graph", r"[\pL\pM\pN\pP\pS]"),
    ("lower", r"\p{Lowercase}"),
    ("print", r"[\pL\pM\pN\pP\pS\p{Zs}\t]"),
    ("punct", r"\pP"),
    ("space", r"\p{White_Space}"),
    ("upper", r"\p{Uppercase}"),
    ("word", r"[\p{Alphabetic}\p{Nd}\pM\p{Pc}\p{JoinC}]"),
    ("xdigit", r"[0-9a-fA-F]"),
];

/// Rewrite a `[:name:]` POSIX class found inside a character class into its Unicode-property equivalent.
fn posix_class_replacement(rest: &str) -> Option<&'static str> {
    let body = rest.strip_prefix("[:")?;
    let end = body.find(":]")?;
    let name = &body[..end];
    POSIX_TO_UNICODE
        .iter()
        .find(|(posix, _)| *posix == name)
        .map(|(_, unicode)| *unicode)
}

// ------------------------------------------------------------------------
// Law discriminants.

/// One regex law per reference overload. The arity is part of the law because what the ARGUMENTS mean differs per name
/// and arity, not per arity alone: a /1 `test`/`match`/`capture` reads pattern AND flags from its single argument, a /1
/// `scan`/`splits` is global by default, and the replace-all default belongs to the `sub`/`gsub` spelling.
#[derive(Clone, Copy, Debug)]
pub enum RegexLaw {
    Test1,
    Test2,
    Match1,
    Match2,
    Capture1,
    Capture2,
    Scan1,
    Scan2,
    Splits1,
    Splits2,
    Split2,
    Sub2,
    Sub3,
    Gsub2,
    Gsub3,
}

// ------------------------------------------------------------------------
// Compilation and matching (port of the old base's ops.rs).

/// The flag letters that steer the SCAN: which matches the laws see and how many.
#[derive(Clone, Copy, Debug)]
struct RegexOptions {
    global: bool,
    no_empty: bool,
    longest: bool,
}

/// The flag letters that steer COMPILATION: what the pattern itself means.
///
/// They are held apart from `RegexOptions` because the two tiers apply them differently — the fast tier through its
/// builders, the backtracking tier through an inline group prefix — while the scan options are the same law on both.
#[derive(Clone, Copy, Debug, Default)]
struct RegexSyntax {
    case_insensitive: bool,
    dot_matches_new_line: bool,
    ignore_whitespace: bool,
}

/// The compiled pattern, on the tier its own syntax demands.
#[derive(Clone, Debug)]
enum ReferenceCompiledRegex {
    /// `regex_automata`'s leftmost-first engine — every pattern that engine can express — plus the anchored
    /// longest-match twin the `l` flag needs and the min-len-1 twin the `n` flag's empty-match fallback needs.
    Fast {
        regex: Regex,
        longest: Option<AutomataRegex>,
        /// A variant of the pattern where every zero-repetition quantifier requires at least one repetition: `*?` →
        /// `+?`, `*` → `+`, standalone `?` removed, `{0,N}` → `{1,N}`. Compiled only when the `n` (no-empty) flag
        /// is set; used to re-search anchored at a start whose primary match was empty — the first non-empty match in
        /// preference order.
        min_len_1: Option<Regex>,
    },
    /// `fancy-regex`'s backtracking engine, for the constructs the fast tier cannot express at all.
    Fallback { regex: FancyRegex },
}

#[derive(Debug)]
struct RegexMatch {
    start: usize,
    end: usize,
    captures: Vec<Option<(usize, usize)>>,
}

/// One cache slot's compiled payload. The VARIANT is part of the key match: a request for one shape never consumes an
/// entry another shape published under the same key string — a variant mismatch is a miss (the per-call compile
/// fallback), never a wrong answer.
enum RegexCacheValue {
    /// A reference (pattern, flags) compile: the two-tier compiled regex plus the reference option and capture-name
    /// facts.
    Reference {
        regex: ReferenceCompiledRegex,
        options: RegexOptions,
        capture_names: Vec<Option<String>>,
    },
    /// A plain `regex` crate compile of the key string with default flags — the shape the parse and schema families
    /// request.
    Plain(Regex),
    /// `parse_grok`'s assembly, keyed on the USER's grok pattern (not the assembled text): the compiled assembled regex
    /// plus its capture names in match order. The assembly is part of the value, so a hit skips the
    /// tokenize-and-rebuild loop as well as the compile.
    Grok { regex: Regex, capture_names: Vec<String> },
}

/// One fill-once cache slot's compiled entry.
///
/// The cache is process-lifetime and immutable after construction: the `Box` is leaked, so every entry lives for the
/// process and no slot is ever replaced. That bounds the total memory at `REGEX_CACHE_SLOTS` compiled patterns and
/// makes the entries safe to share — `regex::Regex` is `Sync`, and the pointer is published with `Release`/read with
/// `Acquire`.
struct RegexCacheEntry {
    pattern: &'static str,
    flags: &'static str,
    value: RegexCacheValue,
}

/// The compiled-regex cache. The reference recompiles a pattern on EVERY call, and the bench lanes (and real log
/// filtering) run one pattern over every record — `RegexBuilder::build` is ~10 µs where `is_match` is ~0.2 µs, so a
/// hit turns a 100 ms lane into a ~15 ms one. The first (pattern, flags) pair to claim a slot fills it; later distinct
/// pairs miss and compile per call (no replacement, so the cache cannot grow). The table is the engine's ONE
/// compiled-regex cache: the reference pattern builtins, `parse_grok`'s assembled patterns, and the schema family's
/// pattern keywords all route through it, so the parse family competes for slots with `test`/`match`/`capture`.
const REGEX_CACHE_SLOTS: usize = 16;
static REGEX_CACHE: [AtomicPtr<RegexCacheEntry>; REGEX_CACHE_SLOTS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; REGEX_CACHE_SLOTS];

/// FNV-1a over the pattern+flags bytes, for slot selection only (hits still compare the full strings).
#[allow(
    clippy::cast_possible_truncation,
    reason = "slot selection folds the 64-bit hash onto the small fixed cache; the full key is \
              still compared before a hit is served"
)]
fn cache_slot(pattern: &str, flags: &str) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in pattern.bytes().chain(flags.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    (hash as usize) % REGEX_CACHE_SLOTS
}

/// One slot load plus the full (pattern, flags) key compare, shared by every lookup. Returns the entry when the slot
/// holds exactly that pair.
fn cache_lookup(pattern: &str, flags: &str) -> Option<&'static RegexCacheEntry> {
    let slot = &REGEX_CACHE[cache_slot(pattern, flags)];
    let entry = slot.load(Ordering::Acquire);
    if entry.is_null() {
        return None;
    }
    // SAFETY: the entry is leaked and immutable once published, so the
    // reference outlives this function and the field reads are race-free
    // after the Acquire load.
    let entry = unsafe { &*entry };
    (entry.pattern == pattern && entry.flags == flags).then_some(entry)
}

/// The cached reference compile for one (pattern, flags), if its slot was claimed by exactly that pair.
fn cached_regex(pattern: &str, flags: &str) -> Option<(ReferenceCompiledRegex, RegexOptions, Vec<Option<String>>)> {
    let entry = cache_lookup(pattern, flags)?;
    match &entry.value {
        RegexCacheValue::Reference {
            regex,
            options,
            capture_names,
        } => Some((regex.clone(), *options, capture_names.clone())),
        _ => None,
    }
}

/// The ambient scope of a throwaway unlimited ledger, for process-lifetime allocations. An entry published while a
/// request (worse, a worker's child grant) is ambient stays charged to that ledger forever — the leak is the design
/// — and a worker's detach-time quiescence check then sees live `Working` residency it can never release.
pub(crate) fn unlimited_ambient_scope() -> Option<jqf_resource::ScopeGuard> {
    let account = jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u32::MAX,
    ))
    .ok()?;
    Some(jqf_resource::install(account))
}

/// Runs one probe match over every compiled component so the engines' lazy per-regex state (DFA caches, pool slots)
/// initializes HERE — under the process-lifetime ledger — instead of inside the first request that uses the
/// pattern, whose ledger would otherwise carry that state for the life of the process (the cache never frees it, so the
/// charge is never released, and a worker's detach-time quiescence check fails on it).
fn warm_reference_regex(compiled: &ReferenceCompiledRegex) {
    match compiled {
        ReferenceCompiledRegex::Fast {
            regex,
            longest,
            min_len_1,
        } => {
            let _ = regex.is_match("");
            if let Some(longest) = longest {
                let _ = longest.is_match("");
            }
            if let Some(min_len_1) = min_len_1 {
                let _ = min_len_1.is_match("");
            }
        }
        ReferenceCompiledRegex::Fallback { regex } => {
            let _ = regex.is_match("");
        }
    }
}

/// Publishes one compiled entry into its slot if the slot is still empty.
///
/// The entry is process-lifetime by design, so it is built and leaked under [`unlimited_ambient_scope`]: the charge
/// lands on a throwaway ledger that dies here, never on the calling request. `make` runs INSIDE that scope — the
/// value's own allocations (the regex clone, the capture-name strings) are part of the leaked entry, so they must be
/// charged where the entry is, not at the caller.
///
/// A LOSING publisher — one whose slot another thread filled between the load and the exchange — drops its whole
/// entry and leaks its two pattern/flags strings with it: they were built for a process-lifetime slot, there is no safe
/// owner to hand them to, and the leak is bounded by how often two threads compile the same (pattern, flags) pair in
/// the same window.
fn publish_regex(pattern: &str, flags: &str, make: impl FnOnce() -> RegexCacheValue) {
    let slot = &REGEX_CACHE[cache_slot(pattern, flags)];
    if slot.load(Ordering::Relaxed).is_null() {
        let _process_ledger = unlimited_ambient_scope();
        let entry = Box::new(RegexCacheEntry {
            pattern: Box::leak(pattern.to_owned().into_boxed_str()),
            flags: Box::leak(flags.to_owned().into_boxed_str()),
            value: make(),
        });
        let _ = slot.compare_exchange(
            core::ptr::null_mut(),
            Box::into_raw(entry),
            Ordering::Release,
            Ordering::Relaxed,
        );
    }
}

/// The cached plain compile of `pattern` (default flags), if its slot was claimed by exactly that pair.
pub fn cached_plain_regex(pattern: &str) -> Option<Regex> {
    let entry = cache_lookup(pattern, "")?;
    match &entry.value {
        RegexCacheValue::Plain(regex) => Some(regex.clone()),
        _ => None,
    }
}

/// Publishes a plain compile into its slot if the slot is still empty.
pub fn publish_plain_regex(pattern: &str, regex: &Regex) {
    publish_regex(pattern, "", || RegexCacheValue::Plain(regex.clone()));
}

/// The cached plain compile of `pattern`, compiling and publishing on a miss. The caller owns the error mapping, so the
/// `regex` crate's own error is returned untouched when a miss fails to compile.
pub fn compile_plain_regex(pattern: &str) -> Result<Regex, regex::Error> {
    if let Some(cached) = cached_plain_regex(pattern) {
        return Ok(cached);
    }
    // Same process-lifetime law as [`compile_regex`]: build, warm, and publish under the throwaway ledger.
    let _process_ledger = unlimited_ambient_scope();
    let compiled = Regex::new(pattern)?;
    let _ = compiled.is_match("");
    publish_plain_regex(pattern, &compiled);
    Ok(compiled)
}

/// `parse_grok`'s cached assembly, keyed on the USER's grok pattern: the compiled assembled regex plus its capture
/// names in match order, if the slot was claimed by exactly that pattern.
pub fn cached_grok_regex(pattern: &str) -> Option<(Regex, Vec<String>)> {
    let entry = cache_lookup(pattern, "")?;
    match &entry.value {
        RegexCacheValue::Grok { regex, capture_names } => Some((regex.clone(), capture_names.clone())),
        _ => None,
    }
}

/// Publishes a grok assembly into its slot if the slot is still empty.
pub fn publish_grok_regex(pattern: &str, regex: Regex, capture_names: Vec<String>) {
    publish_regex(pattern, "", || RegexCacheValue::Grok { regex, capture_names });
}

/// One `sub`/`gsub` match, with its capture object already built for the replacement filter's dot.
pub struct SubstitutionMatch {
    pub start: usize,
    pub end: usize,
    pub captures: Value,
}

/// One reference pattern+flags pair compiled onto whichever tier its syntax demands.
fn compile_regex(
    pattern: &str,
    flags: &str,
    resources: &ResourceContext<'_>,
) -> Result<(ReferenceCompiledRegex, RegexOptions, Vec<Option<String>>), EngineRunError> {
    let cache_pattern = pattern;
    let cache_flags = flags;
    if let Some(cached) = cached_regex(pattern, flags) {
        return Ok(cached);
    }
    // The miss path builds a PROCESS-LIFETIME artifact: the published entry shares its engine internals with every
    // later clone, and those internals' lazy state initializes on first use and outlives the request. Build, warm, and
    // publish under a throwaway unlimited ledger so no request — worse, no worker child grant — keeps a permanent
    // charge it can never release.
    let process_ledger = unlimited_ambient_scope();
    let analysis = analyze_pattern(pattern, resources, false)?;
    let (options, syntax) = parse_regex_flags(flags, resources)?;
    let compiled = match analysis.fallback {
        None => build_fast_regex(
            &analysis.rewritten,
            syntax,
            options.longest,
            options.no_empty,
            resources,
        )?,
        Some(_) => build_fallback_regex(&analysis.rewritten, syntax, resources)?,
    };
    warm_reference_regex(&compiled);
    publish_regex(cache_pattern, cache_flags, || RegexCacheValue::Reference {
        regex: compiled.clone(),
        options,
        capture_names: analysis.capture_names.clone(),
    });
    drop(process_ledger);
    Ok((compiled, options, analysis.capture_names))
}

/// The reference's flag string split into the two things it decides: `g` global, `n` no-empty and `l` longest steer the
/// SCAN, while `i` case-insensitive, `m`/`p` dot-matches-newline (the reference engine's "multi-line" IS dotall:
/// `"a\nb" | test("a.b"; "m")` is true while the plain form is false) and `x` ignore-whitespace steer COMPILATION. `s`
/// is a no-op in the reference's configuration.
fn parse_regex_flags(
    flags: &str,
    resources: &ResourceContext<'_>,
) -> Result<(RegexOptions, RegexSyntax), EngineRunError> {
    let mut options = RegexOptions {
        global: false,
        no_empty: false,
        longest: false,
    };
    let mut syntax = RegexSyntax::default();
    for flag in flags.chars() {
        match flag {
            'g' => options.global = true,
            'n' => options.no_empty = true,
            'l' => options.longest = true,
            'i' => syntax.case_insensitive = true,
            'm' | 'p' => syntax.dot_matches_new_line = true,
            's' => {}
            'x' => syntax.ignore_whitespace = true,
            _ => {
                return Err(raise(&format!("{flag} is not a valid modifier string"), resources));
            }
        }
    }
    Ok((options, syntax))
}

/// The fast tier: the `regex` crate's leftmost-first engine, plus — only when the `l` flag asks for it — the
/// `MatchKind::All` twin whose ANCHORED search answers "the longest match starting exactly here", and — only when the
/// `n` flag asks for it — the min-len-1 twin whose anchored search answers "the first non-empty match at this start
/// in preference order."
fn build_fast_regex(
    pattern: &str,
    syntax: RegexSyntax,
    longest: bool,
    no_empty: bool,
    resources: &ResourceContext<'_>,
) -> Result<ReferenceCompiledRegex, EngineRunError> {
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(syntax.case_insensitive)
        .dot_matches_new_line(syntax.dot_matches_new_line)
        .ignore_whitespace(syntax.ignore_whitespace)
        .build()
        .map_err(|error| raise(&format!("Regex failure: {error}"), resources))?;
    let longest = if longest {
        let syntax_config = regex_automata::util::syntax::Config::new()
            .case_insensitive(syntax.case_insensitive)
            .dot_matches_new_line(syntax.dot_matches_new_line)
            .ignore_whitespace(syntax.ignore_whitespace);
        Some(
            AutomataRegex::builder()
                .syntax(syntax_config)
                .configure(AutomataRegex::config().match_kind(MatchKind::All))
                .build(pattern)
                .map_err(|error| raise(&format!("Regex failure: {error}"), resources))?,
        )
    } else {
        None
    };
    let min_len_1 = if no_empty {
        let ml = analyze_pattern(pattern, resources, true)?;
        Some(
            RegexBuilder::new(&ml.rewritten)
                .case_insensitive(syntax.case_insensitive)
                .dot_matches_new_line(syntax.dot_matches_new_line)
                .ignore_whitespace(syntax.ignore_whitespace)
                .build()
                .map_err(|error| raise(&format!("Regex failure: {error}"), resources))?,
        )
    } else {
        None
    };
    Ok(ReferenceCompiledRegex::Fast {
        regex,
        longest,
        min_len_1,
    })
}

/// The backtracking tier: fancy-regex, for the constructs the fast tier cannot express.
///
/// Its builder exposes only case folding, so the other two syntax flags travel as an inline group prefix — the same
/// setting, spelled in the pattern. Setting them all one way keeps ONE mechanism on this tier.
fn build_fallback_regex(
    pattern: &str,
    syntax: RegexSyntax,
    resources: &ResourceContext<'_>,
) -> Result<ReferenceCompiledRegex, EngineRunError> {
    let mut letters = String::new();
    if syntax.case_insensitive {
        letters.push('i');
    }
    // The reference's `m`/`p` is dot-matches-newline, spelled `s` here.
    if syntax.dot_matches_new_line {
        letters.push('s');
    }
    if syntax.ignore_whitespace {
        letters.push('x');
    }
    let spelled = if letters.is_empty() {
        pattern.to_owned()
    } else {
        format!("(?{letters}){pattern}")
    };
    let regex = FancyRegex::new(&spelled).map_err(|error| raise(&format!("Regex failure: {error}"), resources))?;
    Ok(ReferenceCompiledRegex::Fallback { regex })
}

/// A construct `regex_automata` cannot express at all, and which therefore decides the pattern's tier.
///
/// Every variant names a construct the reference's engine ACCEPTS and the `regex` crate REFUSES to compile — pinned
/// by `every_routed_construct_is_one_the_fast_tier_refuses` — so routing one can only turn an error into an answer.
/// Constructs the reference itself rejects are deliberately absent: `(?(cond)…)` stays on the fast tier, which
/// refuses it exactly as the reference does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FallbackConstruct {
    Lookahead,
    Lookbehind,
    AtomicGroup,
    PossessiveQuantifier,
    Backreference,
    SubroutineCall,
    KeepOut,
}

/// What one reference pattern needs from the engines: the rewritten spelling, its capture names in group order, and the
/// tier its syntax demands.
#[derive(Debug)]
struct PatternAnalysis {
    rewritten: String,
    capture_names: Vec<Option<String>>,
    fallback: Option<FallbackConstruct>,
}

/// One walk of a reference pattern that answers everything the compile needs.
///
/// The reference's `(?<name>…)` spelling is rewritten to the internal one both engines accept, with the capture-name
/// list tracked in group order; a `(?P<` spelling — the other named form, which the reference itself refuses — is
/// refused exactly as the old base refused it. The SAME walk classifies the pattern's tier, because the escape and
/// character-class state that makes a `(` a group is exactly the state that makes a `\1` a backreference: two walks
/// would be two chances to disagree about which is which.
///
/// When `force_non_empty` is true the walk rewrites zero-repetition quantifiers to require at least one repetition —
/// `*?` → `+?`, `*` → `+`, `?` (as quantifier) → removed, `{0,N}` → `{1,N}`. This produces a min-len-1 pattern
/// that the `n` flag's empty-match fallback searches at the SAME anchored start to find the first non-empty match in
/// preference order. The rewrite is syntactic rather than semantic — it is only ever used for a fallback re-search at
/// a start whose primary match was empty, so a missed transformation defers to a wrong answer rather than creating one.
///
/// The same walk translates the reference engine's extra escape vocabulary into spellings the tiers accept: property
/// classes and braced hex pass through verbatim, braced octal and an unbraced digit run that names no group become hex
/// escapes, and a `\Q…\E` span becomes its quoted literals — so a pattern the reference answers never dies in the
/// rewrite.
#[allow(
    clippy::too_many_lines,
    reason = "one reason this function exists is so that the capture-name rewrite and the tier \
              classification share one walk; breaking it into sub-functions would need a \
              mirroring state struct whose fields cross the same 143 lines"
)]
fn analyze_pattern(
    pattern: &str,
    resources: &ResourceContext<'_>,
    force_non_empty: bool,
) -> Result<PatternAnalysis, EngineRunError> {
    let mut out = String::with_capacity(pattern.len());
    let mut capture_names = vec![None];
    let mut fallback = None;
    let mut index = 0;
    let mut escaped = false;
    let mut in_class = false;
    let mut after_quantifier = false;
    let mut quoting = false;
    while index < pattern.len() {
        let rest = &pattern[index..];
        let ch = rest.chars().next().expect("index is on a char boundary");
        // Inside a `\Q` span every character is LITERAL — escape pairs included (`\Q\n\E` quotes a backslash and an
        // `n`) — so the whole special-character machinery below is bypassed until `\E`. The reference binds a
        // quantifier after `\E` to the last quoted character (`^\Qabc\E+$` matches only `abcc`), which is exactly the
        // atom the output already ends with.
        if quoting {
            if rest.starts_with("\\E") {
                quoting = false;
                after_quantifier = true;
                index += "\\E".len();
                continue;
            }
            push_quoted_literal(&mut out, ch);
            index += ch.len_utf8();
            continue;
        }
        if escaped {
            if let Some(reference) = parse_group_reference(rest) {
                fallback = fallback.or(Some(reference.construct));
                out.push(reference.kind);
                out.push(reference.open);
                out.push_str(&resolve_capture_reference(&capture_names, reference.name));
                out.push(reference.close);
                escaped = false;
                after_quantifier = false;
                index += reference.length;
                continue;
            }
            // C4: `\0` is the reference engine's octal NUL escape (with up to two following octal digits, `\01` …
            // `\077`). Both Rust tiers read a bare `\0` as a BACKREFERENCE and refuse to compile it, so jqf rejected a
            // pattern the reference accepts (`test("a\0b")` is false in the reference, a raise here — the catalogue's
            // C4 class). The rewrite emits the `\xNN` spelling both tiers accept, keeping the pattern on the fast tier.
            // Checked before `escaped_fallback_construct`: `\0` must not be classified with `\1`…`\9`.
            if let Some(octal) = octal_nul_escape(rest) {
                // The escape's backslash was already pushed when `\` set the escaped flag; the rewrite REPLACES it (the
                // `\xNN` spelling carries its own backslash), so the pushed one is popped first — otherwise the
                // pattern would gain a doubled backslash and match a literal `\x00` text instead of NUL.
                out.pop();
                out.push_str(&octal.spelling);
                escaped = false;
                after_quantifier = false;
                index += octal.consumed;
                continue;
            }
            // `\Q` quotes meta characters through the next `\E` (or the end of the pattern). The rewrite spells every
            // quoted character so the tiers read the bare character — including `]`, `-`, and `^` inside a class,
            // whitespace under the `x` flag, and backslash pairs, which the quote law treats as two literal characters
            // rather than an escape. The pushed backslash is popped: `\Q` itself emits nothing.
            if ch == 'Q' {
                out.pop();
                quoting = true;
                escaped = false;
                after_quantifier = false;
                index += ch.len_utf8();
                continue;
            }
            // The braced property classes (`\p{Han}`, `\P{L}`) and the braced hex escape (`\x{1F600}`) are copied
            // VERBATIM — their braces are part of the spelling, and escaping them as literal braces (the bare-brace
            // arm below) corrupts the class into something both tiers refuse.
            if let Some(consumed) = verbatim_braced_escape(rest) {
                out.push_str(&rest[..consumed]);
                escaped = false;
                after_quantifier = false;
                index += consumed;
                continue;
            }
            // The braced octal escape becomes the braced-hex spelling the fast tier accepts; like `\0`, the pushed
            // backslash is popped.
            if let Some(escaped_octal) = octal_braced_escape(rest) {
                out.pop();
                out.push_str(&escaped_octal.spelling);
                escaped = false;
                after_quantifier = false;
                index += escaped_octal.consumed;
                continue;
            }
            // A `\N` digit run is a BACKREFERENCE only when group N exists somewhere in the pattern — counting groups
            // this walk has not reached yet, because the reference reads a FORWARD reference as a backreference (one
            // that fails at match time), never as an escape. A run naming no group is an OCTAL escape when its digits
            // allow one (`\101` is `A`; `(a)\12` is U+000A), spelled as hex so the fast tier keeps it; a run that is
            // neither (`\8` with no group 8) stays authored for the fallback tier to refuse exactly as the reference
            // refuses it.
            if ch.is_ascii_digit() && ch != '0' {
                let run_end = index + pattern[index..].bytes().take_while(u8::is_ascii_digit).count();
                let run = &pattern[index..run_end];
                let total_groups = capture_names.len() - 1 + capturing_group_opens(&pattern[run_end..]);
                let names_a_group = run
                    .parse::<u32>()
                    .is_ok_and(|number| number <= u32::try_from(total_groups).unwrap_or(u32::MAX));
                if !names_a_group && let Some(escaped_octal) = unmatched_octal_run(run) {
                    out.pop();
                    out.push_str(&escaped_octal.spelling);
                    escaped = false;
                    after_quantifier = false;
                    index += escaped_octal.consumed;
                    continue;
                }
            }
            fallback = fallback.or_else(|| escaped_fallback_construct(rest));
            out.push(ch);
            escaped = false;
            after_quantifier = false;
            index += ch.len_utf8();
            continue;
        }
        if ch == '\\' {
            out.push(ch);
            escaped = true;
            index += ch.len_utf8();
            continue;
        }
        if in_class {
            if let Some(replacement) = posix_class_replacement(rest) {
                out.push_str(replacement);
                let skipped = "[:".len() + rest["[:".len()..].find(":]").unwrap() + ":]".len();
                index += skipped;
                continue;
            }
            if ch == '[' {
                // The reference engine treats a `[` INSIDE a character class as a literal member (`test("[[1,2]")` is
                // false in the reference — the class is `[`, `1`, `,`, `2`); the tiers reject the nested brace. The
                // escape makes both read the literal, which is the leniency reached by a DATA-driven pattern: the
                // reference's own error-message text (`number (2) and array ([[1,2],…])`) carries the nested
                // brackets.
                out.push('\\');
            }
            out.push(ch);
            if ch == ']' {
                in_class = false;
                after_quantifier = false;
            }
            index += ch.len_utf8();
            continue;
        }
        if ch == '[' {
            out.push(ch);
            in_class = true;
            index += ch.len_utf8();
            continue;
        }
        if let Some(length) = bounded_repeat_length(rest) {
            if force_non_empty {
                let body = &rest["{".len()..];
                let end = body.find('}').unwrap();
                let inner = &body[..end];
                let rewritten = rewrite_zero_repeat_range(inner);
                out.push('{');
                out.push_str(&rewritten);
                out.push('}');
            } else {
                out.push_str(&rest[..length]);
            }
            after_quantifier = true;
            index += length;
            continue;
        }
        if ch == '{' {
            // The reference engine treats a `{` that does not open a valid interval as a LITERAL (`test("{a}")` is true
            // in the reference — the pattern matches the text `{a}` itself); both tiers reject the bare brace with
            // `repetition operator missing expression`, so the rewrite escapes it. A `{` that DOES open a valid
            // interval was consumed above and still reaches the tier's own `target of repeat op` refusal,
            // exit-class-identical to the reference's.
            out.push('\\');
            out.push('{');
            after_quantifier = false;
            index += ch.len_utf8();
            continue;
        }
        if let Some((name, end_index)) = parse_named_capture(pattern, index) {
            let internal_name = format!("__jqf_capture_{}", capture_names.len());
            capture_names.push(Some(name.to_owned()));
            out.push_str("(?<");
            out.push_str(&internal_name);
            out.push('>');
            after_quantifier = false;
            index = end_index;
            continue;
        }
        if rest.starts_with("(?P<") {
            return Err(raise("Regex failure: undefined group option", resources));
        }
        if ch == '(' {
            fallback = fallback.or_else(|| group_fallback_construct(rest));
            if !rest.starts_with("(?") {
                capture_names.push(None);
            }
        }
        if ch == '+' && after_quantifier {
            fallback = fallback.or(Some(FallbackConstruct::PossessiveQuantifier));
        }
        if force_non_empty {
            if ch == '*' {
                // * → +; if followed by ? (lazy) or + (possessive), carry it.
                out.push('+');
                if let Some(next) = rest[ch.len_utf8()..].chars().next()
                    && (next == '?' || next == '+')
                {
                    out.push(next);
                    index += next.len_utf8();
                }
                after_quantifier = true;
                index += ch.len_utf8();
                continue;
            }
            if ch == '?' && !after_quantifier {
                // Standalone ? is zero-or-one — skip it in min-len-1 mode. If the next char is also ?, that's the
                // lazy suffix of a quantifier we already skipped; skip that too.
                if let Some(next) = rest[ch.len_utf8()..].chars().next()
                    && next == '?'
                {
                    index += next.len_utf8();
                }
                after_quantifier = false;
                index += ch.len_utf8();
                continue;
            }
        }
        after_quantifier = match ch {
            '*' | '+' => true,
            '?' => !after_quantifier,
            _ => false,
        };
        out.push(ch);
        index += ch.len_utf8();
    }
    Ok(PatternAnalysis {
        rewritten: out,
        capture_names,
        fallback,
    })
}

/// Rewrite the interior of a `{n,m}` repetition: `0` → `1`, `0,` → `1,`, `0,m` → `1,m`. Everything else stays.
fn rewrite_zero_repeat_range(inner: &str) -> String {
    if let Some((low, high)) = inner.split_once(',') {
        if low.trim() == "0" {
            return format!("1,{high}");
        }
    } else if inner.trim() == "0" {
        return "1".to_owned();
    }
    inner.to_owned()
}

/// the reference engine's octal NUL escape as the hex spelling both Rust tiers accept.
///
/// `rest` begins at the escaped character (the backslash already consumed). Matches `\0` plus up to two following octal
/// digits (`\01` … `\077`, the octal range the reference engine assigns); the value is masked to a byte, matching the
/// reference engine's wrap for out-of-range spellings. A `0` followed by `8`/`9` or a non-digit is just `\0` (NUL) with
/// the digit left as a literal — the reference engine's own reading. Returns the `\xNN` spelling and how many pattern
/// bytes the escape consumed.
fn octal_nul_escape(rest: &str) -> Option<HexEscape> {
    let mut chars = rest.chars();
    if chars.next()? != '0' {
        return None;
    }
    let mut digits = String::from("0");
    let mut consumed = 1;
    for _ in 0..2 {
        match chars.next() {
            Some(c @ '0'..='7') => {
                digits.push(c);
                consumed += 1;
            }
            _ => break,
        }
    }
    let value = u32::from_str_radix(&digits, 8).ok()? & 0xFF;
    Some(HexEscape {
        spelling: format!("\\x{value:02x}"),
        consumed,
    })
}

/// The braced escapes the fast tier accepts VERBATIM: the Unicode property classes (`\p{Han}`, `\P{L}`) and the braced
/// hex codepoint escape (`\x{1F600}`). `rest` begins at the escaped character. Returns the span length only when the
/// brace also CLOSES; without a closer the spelling stays with the ordinary walk, whose literal-brace rewrite raises
/// the same refusal the reference raises for an unterminated class.
fn verbatim_braced_escape(rest: &str) -> Option<usize> {
    let lead = rest.chars().next()?;
    if !matches!(lead, 'p' | 'P' | 'x') || rest.as_bytes().get(1) != Some(&b'{') {
        return None;
    }
    let close = rest.find('}')?;
    Some(close + '}'.len_utf8())
}

/// The reference engine's braced octal escape `\o{nnn}` as the braced-hex spelling the fast tier accepts. Unlike an
/// UNBRACED run, the braced form is codepoint-valued in the reference (`\o{377}` matches U+00FF), so the translation is
/// exact at every value the engines share. A malformed or oversized spelling returns `None` and stays with the ordinary
/// walk, whose tier refusal matches the reference's own.
fn octal_braced_escape(rest: &str) -> Option<HexEscape> {
    let body = rest.strip_prefix('o')?.strip_prefix('{')?;
    let end = body.find('}')?;
    let digits = &body[..end];
    if digits.is_empty() || !digits.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return None;
    }
    let value = u32::from_str_radix(digits, 8).ok()?;
    if value > 0x10_FFFF {
        return None;
    }
    Some(HexEscape {
        spelling: format!("\\x{{{value:x}}}"),
        consumed: digits.len() + 3, // `o`, `{`, the digits, `}`
    })
}

/// The octal reading of a `\N` digit run that names NO group. `run` begins at the first digit (the backslash already
/// consumed by the walk).
///
/// The reference reads such a run as an escape only when every digit is octal AND the value fits one byte below U+0080;
/// anything else — an eight-or-nine digit (`\8` with no group 8), or a byte-semantic value at or above 0x80, which
/// matches no UTF-8 text in the reference — is left authored, because the fast tier cannot spell that answer (`\xff`
/// refuses to compile in Unicode mode and `\x{ff}` would name the CODEPOINT) and both tiers refuse the authored form as
/// the shared closest answer.
fn unmatched_octal_run(run: &str) -> Option<HexEscape> {
    if !run.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return None;
    }
    let value = u32::from_str_radix(run, 8).ok()?;
    if value > 0x7F {
        return None;
    }
    Some(HexEscape {
        spelling: format!("\\x{value:02x}"),
        consumed: run.len(),
    })
}

/// The result of [`octal_nul_escape`] and of the other escape translations that emit a hex spelling both tiers accept.
struct HexEscape {
    spelling: String,
    consumed: usize,
}

/// The literal spelling of one `\Q`-quoted character: word characters pass through bare, everything else carries a
/// backslash — the one spelling both tiers read as the bare character in every context, including `]`, `-`, and `^`
/// inside a character class and whitespace under the `x` flag.
fn push_quoted_literal(out: &mut String, ch: char) {
    if ch.is_alphanumeric() || ch == '_' {
        out.push(ch);
    } else {
        out.push('\\');
        out.push(ch);
    }
}

/// Counts the capturing-group openings ahead of a `\N` reference: plain `(…)` opens and valid named `(?<name>` opens,
/// under the same escape and character-class state rules the main walk applies. This is the forward half of the
/// group-existence question — a reference may name a group the walk has not reached yet, and the reference engine
/// still calls that a backreference, never an escape.
fn capturing_group_opens(rest: &str) -> usize {
    let mut opens = 0;
    let mut index = 0;
    let mut escaped = false;
    let mut in_class = false;
    while index < rest.len() {
        let ch = rest[index..].chars().next().expect("index is on a char boundary");
        if escaped {
            escaped = false;
            index += ch.len_utf8();
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '[' => in_class = true,
            ']' => in_class = false,
            '(' if !in_class => {
                // Only a VALID named capture opens a group here; every other `(?…` form — including the lookbehind
                // twins, whose `=`/`!` first names fail the capture-name grammar — opens none.
                if rest[index + 1..].starts_with('?') {
                    opens += usize::from(parse_named_capture(rest, index).is_some());
                } else {
                    opens += 1;
                }
            }
            _ => {}
        }
        index += ch.len_utf8();
    }
    opens
}

/// The backtracking-tier constructs spelled as a GROUP opening. `rest` begins at the `(`.
fn group_fallback_construct(rest: &str) -> Option<FallbackConstruct> {
    // Lookbehind is tested before lookahead: `(?<!` shares its first three characters with nothing else, but `(?<`
    // alone is a named group.
    if rest.starts_with("(?<=") || rest.starts_with("(?<!") {
        return Some(FallbackConstruct::Lookbehind);
    }
    if rest.starts_with("(?=") || rest.starts_with("(?!") {
        return Some(FallbackConstruct::Lookahead);
    }
    if rest.starts_with("(?>") {
        return Some(FallbackConstruct::AtomicGroup);
    }
    None
}

/// The backtracking-tier constructs spelled as a bare ESCAPE. `rest` begins at the escaped character, the backslash
/// already consumed. The NAMED forms (`\k<name>`, `\g<name>`) are consumed by `parse_group_reference`, which has to
/// rewrite them as well as classify them, and a `\N` digit run that names a group reaches the `'1'..='9'` arm here
/// after the octal rewrite above declined it.
fn escaped_fallback_construct(rest: &str) -> Option<FallbackConstruct> {
    match rest.chars().next()? {
        '1'..='9' => Some(FallbackConstruct::Backreference),
        'K' => Some(FallbackConstruct::KeepOut),
        _ => None,
    }
}

/// A reference to a named group: `\k<name>` / `\k'name'` (a backreference) or `\g<name>` / `\g'name'` (a subroutine
/// call).
struct GroupReference<'a> {
    kind: char,
    open: char,
    close: char,
    name: &'a str,
    length: usize,
    construct: FallbackConstruct,
}

/// The named group reference at the head of `rest`, which begins at the escaped character.
fn parse_group_reference(rest: &str) -> Option<GroupReference<'_>> {
    let mut chars = rest.chars();
    let (kind, construct) = match chars.next()? {
        'k' => ('k', FallbackConstruct::Backreference),
        'g' => ('g', FallbackConstruct::SubroutineCall),
        _ => return None,
    };
    let (open, close) = match chars.next()? {
        '<' => ('<', '>'),
        '\'' => ('\'', '\''),
        _ => return None,
    };
    let body = rest.get("k<".len()..)?;
    let end = body.find(close)?;
    Some(GroupReference {
        kind,
        open,
        close,
        name: &body[..end],
        length: "k<".len() + end + close.len_utf8(),
        construct,
    })
}

/// The internal spelling of the group a reference names, or the name unchanged when no group carries it — an
/// all-digit reference is a group NUMBER, which the rewrite never moves, and a forward reference names a group this
/// walk has not reached yet. Both stay as authored and let the engine judge them.
fn resolve_capture_reference(capture_names: &[Option<String>], name: &str) -> String {
    capture_names
        .iter()
        .rposition(|known| known.as_deref() == Some(name))
        .map_or_else(|| name.to_owned(), |index| format!("__jqf_capture_{index}"))
}

/// The length of a `{n}` / `{n,}` / `{n,m}` repetition at the head of `rest`, if that is what it is.
///
/// A brace that is not a repetition is an ordinary literal in both engines, so recognizing the form is what keeps
/// `"a}+"` (a literal brace, then one or more of it) from being read as a possessive quantifier.
fn bounded_repeat_length(rest: &str) -> Option<usize> {
    let body = rest.strip_prefix('{')?;
    let end = body.find('}')?;
    let (low, high) = match body[..end].split_once(',') {
        Some((low, high)) => (low, Some(high)),
        None => (&body[..end], None),
    };
    let bounded = !low.is_empty()
        && low.chars().all(|ch| ch.is_ascii_digit())
        && high.is_none_or(|high| high.chars().all(|ch| ch.is_ascii_digit()));
    bounded.then_some(end + "{}".len())
}

fn parse_named_capture(pattern: &str, index: usize) -> Option<(&str, usize)> {
    let rest = pattern.get(index..)?;
    let name_start = index + "(?<".len();
    let name_rest = rest.strip_prefix("(?<")?;
    let name_len = name_rest.find('>')?;
    let name = &pattern[name_start..name_start + name_len];
    is_valid_regex_capture_name(name).then_some((name, name_start + name_len + 1))
}

fn is_valid_regex_capture_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic()) && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Where the next search of a global scan starts, the law: a NON-EMPTY match resumes at its own end, and a ZERO-WIDTH
/// one resumes one codepoint PAST the match — never one past the SEARCH START, which re-finds an end-anchored match
/// once per remaining position (`"abc" | gsub("$";"X")` is `abcX`, not `abcXXXX`). Past the subject the answer is
/// `len + 1`, which ends the scan; the search AT `len` still runs, because the reference's last match of
/// `"abcc" | match("c*";"g")` is the empty one at offset 4. The reference advances by BYTE here and so emits duplicate
/// matches inside a multi-byte character; jqf advances by CODEPOINT (the divergence the compat corpus pins as an
/// `intdiff` row).
fn next_search_start(input: &str, matched: &RegexMatch) -> usize {
    if matched.start == matched.end {
        next_char_boundary_after(input, matched.end)
    } else {
        matched.end
    }
}

/// Every match the laws see, under ONE scan loop.
///
/// The loop is the reference's: search from the cursor, publish, resume at `next_search_start`, and stop at the first
/// search that finds nothing. `n` (no-empty) SKIPS a zero-width match and keeps scanning rather than ending the scan,
/// which is what makes `"a1b" | [match("[0-9]*";"gn")]` find the `1` after the empty match at offset 0. The `l` flag
/// changes what ONE search answers and never the loop, and the tier changes neither.
fn regex_matches(
    input: &str,
    regex: &ReferenceCompiledRegex,
    options: RegexOptions,
    global: bool,
    resources: &ResourceContext<'_>,
) -> Result<Vec<RegexMatch>, EngineRunError> {
    let mut out = Vec::new();
    let mut search_start = 0;
    while search_start <= input.len() {
        let found = if options.longest {
            longest_match_in_window(input, regex, search_start, resources)?
        } else {
            regex.first_match_at_or_after(input, search_start, resources)?
        };
        let Some(matched) = found else {
            break;
        };
        let empty = matched.start == matched.end;
        if options.no_empty && empty {
            let non_empty = regex.first_non_empty_match_at(input, search_start);
            if let Some(non_empty_match) = non_empty {
                search_start = next_search_start(input, &non_empty_match);
                out.push(non_empty_match);
            } else {
                search_start = next_search_start(input, &matched);
            }
        } else {
            search_start = next_search_start(input, &matched);
            out.push(matched);
        }
        if !global {
            break;
        }
    }
    Ok(out)
}

/// The `l` (longest) law: ONE search answers with the LONGEST match anywhere at or after `from`, ties broken LEFTMOST.
///
/// This is the reference engine's longest-match option, and it is NOT leftmost-longest: `"abb" | match("a|bb";"l")` is
/// `bb` at offset 1, the `a` at offset 0 LOSING to a longer match further right, and `"abbxa" |
/// [match("a|bb";"gl")|.offset]` is `[1,4]` with the offset-0 `a` never emitted at all. So the walk cannot stop at the
/// first start that matches: it enumerates candidate STARTS leftmost-first and keeps the longest answer, replacing only
/// on a STRICTLY longer one so that a tie keeps the leftmost (`"aaa" | match("a";"l")` is offset 0).
///
/// Two things keep the walk off every position. The leftmost-first engine names the next start that can match AT ALL,
/// so a run of dead positions costs one ordinary search rather than one per byte; and once the best match is at least
/// as long as the input that REMAINS, no later start can beat it and the walk stops.
fn longest_match_in_window(
    input: &str,
    regex: &ReferenceCompiledRegex,
    from: usize,
    resources: &ResourceContext<'_>,
) -> Result<Option<RegexMatch>, EngineRunError> {
    let mut best: Option<RegexMatch> = None;
    let mut cursor = from;
    while cursor <= input.len() {
        let Some(candidate) = regex.first_match_at_or_after(input, cursor, resources)? else {
            break;
        };
        let start = candidate.start;
        let matched = match regex.longest_match_at(input, start) {
            Some(longest) => longest,
            None => candidate,
        };
        cursor = next_char_boundary_after(input, start);
        let length = matched.end - matched.start;
        if best.as_ref().is_none_or(|found| length > found.end - found.start) {
            best = Some(matched);
        }
        if best
            .as_ref()
            .is_some_and(|found| found.end - found.start >= input.len().saturating_sub(cursor))
        {
            break;
        }
    }
    Ok(best)
}

impl ReferenceCompiledRegex {
    /// The leftmost match starting at or after `from`, in the engine's own preference order — the reference's
    /// ordinary (non-`l`) search on both tiers.
    ///
    /// Neither engine is handed a SLICE: both search the whole input from an offset, so `^`, `\b` and lookbehind read
    /// the bytes before `from` as the reference engine does.
    fn first_match_at_or_after(
        &self,
        input: &str,
        from: usize,
        resources: &ResourceContext<'_>,
    ) -> Result<Option<RegexMatch>, EngineRunError> {
        match self {
            Self::Fast { regex, .. } => Ok(regex.captures_at(input, from).as_ref().and_then(match_from_captures)),
            Self::Fallback { regex } => {
                // A backtracking engine can exhaust its step budget; that is a failure to answer, never an answer of
                // "no match".
                let captures = regex
                    .captures_from_pos(input, from)
                    .map_err(|error| raise(&format!("Regex failure: {error}"), resources))?;
                Ok(captures.as_ref().and_then(match_from_fancy_captures))
            }
        }
    }

    /// Whether a match exists at or after the input's start, without building captures.
    ///
    /// `test` needs existence only; the full scan's match vector and capture objects are the whole price of a
    /// per-record predicate lane. Existence does not depend on `l` (which chooses WHICH match) or `g` (which loops),
    /// and the fast tier answers it with the regex crate's `is_match` — no capture machinery at all. The `n`
    /// (no-empty) flag DOES change existence: an empty primary match only counts when a non-empty match exists at that
    /// same start or a later start finds one, so `n` keeps the exact scan.
    fn has_match_at_or_after(
        &self,
        input: &str,
        options: RegexOptions,
        resources: &ResourceContext<'_>,
    ) -> Result<bool, EngineRunError> {
        if !options.no_empty
            && let Self::Fast { regex, .. } = self
        {
            return Ok(regex.is_match(input));
        }
        let mut search_start = 0;
        while search_start <= input.len() {
            let found = if options.longest {
                longest_match_in_window(input, self, search_start, resources)?
            } else {
                self.first_match_at_or_after(input, search_start, resources)?
            };
            let Some(matched) = found else {
                return Ok(false);
            };
            if options.no_empty && matched.start == matched.end {
                if self.first_non_empty_match_at(input, search_start).is_some() {
                    return Ok(true);
                }
                search_start = next_search_start(input, &matched);
                continue;
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// The first non-empty match in preference order starting exactly at `at`.
    ///
    /// Called when the primary search at `at` returned an empty match and the `n` (no-empty) flag is set — the
    /// reference engine's no-empty option backtracks to the first NON-EMPTY match in preference order at that same
    /// start. Neither Rust engine exposes that directly, so this method uses the min-len-1 variant compiled alongside
    /// the primary regex: it searches anchored at `at` (the input sliced from `at`) and returns the match only when it
    /// starts at the slice's first byte — if the min-len-1 variant matches further right, no non-empty match exists
    /// at `at`.
    ///
    /// On the fast tier the min-len-1 variant is a rewritten pattern where zero-repetition quantifiers require at least
    /// one repetition (`*?` → `+?`, `*` → `+`, `?` removed, `{0,N}` → `{1,N}`). On the backtracking tier no
    /// min-len-1 twin exists, so this returns `None` — the `n` flag on a backtracking-tier pattern keeps the current
    /// post-filter behavior. `n` is rare; a per-empty-match fallback re-search is acceptable.
    fn first_non_empty_match_at(&self, input: &str, at: usize) -> Option<RegexMatch> {
        match self {
            Self::Fast {
                min_len_1: Some(ml), ..
            } => {
                let found = ml.captures(&input[at..]);
                let caps = found.as_ref()?;
                let matched = caps.get(0).expect("group 0 exists");
                if matched.start() == 0 && matched.start() != matched.end() {
                    Some(RegexMatch {
                        start: at + matched.start(),
                        end: at + matched.end(),
                        captures: (0..caps.len())
                            .map(|index| caps.get(index).map(|c| (at + c.start(), at + c.end())))
                            .collect(),
                    })
                } else {
                    None
                }
            }
            Self::Fast { min_len_1: None, .. } | Self::Fallback { .. } => None,
        }
    }

    /// The LONGEST match starting exactly at `at`, when the engine can answer that question at all.
    ///
    /// The fast tier can: an ANCHORED search on the `MatchKind::All` twin reports the longest match at the anchor,
    /// captures and all. The backtracking tier cannot — fancy-regex has no longest-match mode — so it answers
    /// `None` and the walk keeps that start's ordinary first-preference match. That is the one place the tiers do not
    /// agree, and it is catalogued (`.docs-intenal/regex-divergence-catalogue-2026-08-04.md`).
    fn longest_match_at(&self, input: &str, at: usize) -> Option<RegexMatch> {
        match self {
            Self::Fast {
                longest: Some(longest), ..
            } => {
                let mut captures = longest.create_captures();
                longest.captures(
                    RegexAutomataInput::new(input)
                        .span(at..input.len())
                        .anchored(Anchored::Yes),
                    &mut captures,
                );
                match_from_automata_captures(&captures)
            }
            Self::Fast { longest: None, .. } | Self::Fallback { .. } => None,
        }
    }
}

fn match_from_captures(captures: &regex::Captures<'_>) -> Option<RegexMatch> {
    let matched = captures.get(0)?;
    Some(RegexMatch {
        start: matched.start(),
        end: matched.end(),
        captures: (0..captures.len())
            .map(|index| {
                // An UNMATCHED group answers `null` even inside an EMPTY match (`"b" | scan("(?<x>a)?")` streams
                // `[null]`), so a missing capture is `None` whatever the match span is.
                captures.get(index).map(|capture| (capture.start(), capture.end()))
            })
            .collect(),
    })
}

fn match_from_fancy_captures(captures: &FancyCaptures<'_, str>) -> Option<RegexMatch> {
    let matched = captures.get(0)?;
    Some(RegexMatch {
        start: matched.start(),
        end: matched.end(),
        captures: (0..captures.len())
            .map(|index| captures.get(index).map(|group| (group.start(), group.end())))
            .collect(),
    })
}

fn match_from_automata_captures(captures: &regex_automata::util::captures::Captures) -> Option<RegexMatch> {
    let matched = captures.get_match()?;
    Some(RegexMatch {
        start: matched.start(),
        end: matched.end(),
        captures: captures
            .iter()
            .map(|span| span.map(regex_automata_span_range))
            .collect(),
    })
}

fn regex_automata_span_range(span: RegexAutomataSpan) -> (usize, usize) {
    (span.start, span.end)
}

fn next_char_boundary_after(input: &str, index: usize) -> usize {
    if index >= input.len() {
        return input.len() + 1;
    }
    let mut next = index + 1;
    while next < input.len() && !input.is_char_boundary(next) {
        next += 1;
    }
    next
}

fn regex_match_spans(
    input: &str,
    regex: &ReferenceCompiledRegex,
    options: RegexOptions,
    global: bool,
    resources: &ResourceContext<'_>,
) -> Result<Vec<(usize, usize)>, EngineRunError> {
    Ok(regex_matches(input, regex, options, global, resources)?
        .into_iter()
        .map(|matched| (matched.start, matched.end))
        .collect())
}

fn split_by_match_spans(input: &str, matches: &[(usize, usize)]) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part_start = 0;
    for (start, end) in matches {
        if *start < part_start {
            if *end == part_start {
                parts.push(String::new());
            }
            continue;
        }
        parts.push(input[part_start..*start].to_owned());
        part_start = *end;
    }
    parts.push(input[part_start..].to_owned());
    parts
}

// ------------------------------------------------------------------------
// The laws.

/// One pattern+flags combination's answer, for every non-substitution law.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per law family: the law table IS the dispatch, and splitting it would \
              hide which shape each refusal and default belongs to"
)]
pub fn apply_combo(
    law: RegexLaw,
    input: &Value,
    pattern: &Value,
    flags: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Vec<Value>, EngineRunError> {
    match law {
        RegexLaw::Test1 | RegexLaw::Test2 => {
            let (input, pattern, flags) = matcher_arguments(law, input, pattern, flags, resources)?;
            let (regex, options, _) = compile_regex(pattern, flags, resources)?;
            Ok(vec![Value::Bool(
                regex.has_match_at_or_after(input, options, resources)?,
            )])
        }
        RegexLaw::Match1 | RegexLaw::Match2 => {
            let (input, pattern, flags) = matcher_arguments(law, input, pattern, flags, resources)?;
            let (regex, options, capture_names) = compile_regex(pattern, flags, resources)?;
            let matches = regex_matches(input, &regex, options, options.global, resources)?;
            let mut tally = CodepointTally::default();
            Ok(matches
                .iter()
                .map(|matched| build_match_object(input, matched, &capture_names, &mut tally, resources))
                .collect::<Result<Vec<_>, _>>()?)
        }
        RegexLaw::Capture1 | RegexLaw::Capture2 => {
            let (input, pattern, flags) = matcher_arguments(law, input, pattern, flags, resources)?;
            let (regex, options, capture_names) = compile_regex(pattern, flags, resources)?;
            Ok(regex_matches(input, &regex, options, options.global, resources)?
                .into_iter()
                .map(|matched| build_capture_object(input, &matched, &capture_names, resources))
                .collect::<Result<Vec<_>, _>>()?)
        }
        RegexLaw::Scan1 | RegexLaw::Scan2 => {
            let input = expect_matchable_string(input, resources)?;
            let flags = if matches!(law, RegexLaw::Scan1) {
                "g".to_owned()
            } else {
                flags_with_global(flags, GlobalFlag::Leading, resources)?
            };
            let (regex, options, _) = compile_regex(expect_string(pattern, resources)?, &flags, resources)?;
            Ok(regex_matches(input, &regex, options, true, resources)?
                .into_iter()
                .map(|matched| build_scan_value(input, &matched, resources))
                .collect::<Result<Vec<_>, _>>()?)
        }
        RegexLaw::Splits1 | RegexLaw::Splits2 => {
            let input = expect_matchable_string(input, resources)?;
            let flags = if matches!(law, RegexLaw::Splits1) {
                "g".to_owned()
            } else {
                flags_with_global(flags, GlobalFlag::Trailing, resources)?
            };
            let (regex, options, _) = compile_regex(expect_string(pattern, resources)?, &flags, resources)?;
            let spans = regex_match_spans(input, &regex, options, true, resources)?;
            Ok(split_by_match_spans(input, &spans)
                .into_iter()
                .map(|part| string_value(&part, resources))
                .collect::<Result<Vec<_>, _>>()?)
        }
        RegexLaw::Split2 => {
            let input = expect_matchable_string(input, resources)?;
            let pattern = expect_string(pattern, resources)?;
            let flags = flags_with_global(flags, GlobalFlag::Trailing, resources)?;
            let (regex, options, _) = compile_regex(pattern, &flags, resources)?;
            // `split/2` IS `[splits($re; $flags)]` — the same span list held in an array. A pattern that names no
            // match at all (including an empty one whose matches the `n` flag suppresses) splits into ONE part, the
            // whole input, which an empty span list already yields.
            let matches = regex_match_spans(input, &regex, options, true, resources)?;
            Ok(vec![array_value(split_by_match_spans(input, &matches), resources)?])
        }
        RegexLaw::Sub2 | RegexLaw::Sub3 | RegexLaw::Gsub2 | RegexLaw::Gsub3 => Err(EngineRunError::internal_contract(
            "substitution laws are driven by the executor's two-phase drive",
        )),
    }
}

/// The match set one `sub`/`gsub` pattern+flags combination names, with the capture objects built for the replacement
/// filter's dot.
///
/// The law carries the flag default, so the `g` that makes a substitution replace ALL matches is read off the compiled
/// flag string — `gsub` forces it on, and `sub($re; s; "g")` asks for it by name.
pub fn substitution_matches(
    law: RegexLaw,
    input: &Value,
    pattern: &Value,
    flags: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Vec<SubstitutionMatch>, EngineRunError> {
    let input = expect_matchable_string(input, resources)?;
    let flags = substitution_flags(law, flags, resources)?;
    let (regex, options, capture_names) = compile_regex(expect_string(pattern, resources)?, &flags, resources)?;
    let mut out = Vec::new();
    for matched in regex_matches(input, &regex, options, options.global, resources)? {
        out.push(SubstitutionMatch {
            start: matched.start,
            end: matched.end,
            captures: build_capture_object(input, &matched, &capture_names, resources)?,
        });
    }
    Ok(out)
}

/// The span list one `sub`/`gsub` pattern+flags combination names, WITHOUT the capture objects a replacement filter
/// would need.
///
/// A compile-time-literal replacement never reads dot, so the executor can splice it straight from spans; building a
/// capture object per match is the whole extra price a per-record `gsub` lane pays (the scrub lane's `"#"`).
pub fn substitution_match_spans(
    law: RegexLaw,
    input: &Value,
    pattern: &Value,
    flags: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Vec<(usize, usize)>, EngineRunError> {
    let input = expect_matchable_string(input, resources)?;
    let flags = substitution_flags(law, flags, resources)?;
    let (regex, options, _) = compile_regex(expect_string(pattern, resources)?, &flags, resources)?;
    regex_match_spans(input, &regex, options, options.global, resources)
}

/// Assembles one substituted string for a literal replacement, splicing the constant text at every span.
///
/// This is the single-output special case of [`substitute_assembled`]: a literal replacement filter answers one string
/// per match, so the zip law has exactly one output index and the capture objects are never built.
pub fn substitute_assembled_literal(
    input: &Value,
    spans: &[(usize, usize)],
    replacement: &str,
    resources: &ResourceContext<'_>,
) -> Result<Vec<Value>, EngineRunError> {
    let Value::String(input) = input.untagged() else {
        return Err(EngineRunError::internal_contract(
            "literal substitution assembly over a non-string input",
        ));
    };
    if spans.is_empty() {
        return Ok(vec![string_value(input.as_str(), resources)?]);
    }
    let mut text = String::new();
    text.try_reserve(input.as_str().len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    let mut previous_end = 0;
    for (start, end) in spans {
        text.push_str(&input[previous_end..*start]);
        text.push_str(replacement);
        previous_end = *end;
    }
    text.push_str(&input[previous_end..]);
    Ok(vec![string_value(&text, resources)?])
}

/// Assembles the substituted string(s) from one match set and the replacement filter's outputs per match (the old
/// base's `substitute_all` zip law: each output index rebuilds the whole string with that index's replacement per
/// match, and a match whose replacement produced nothing contributes nothing).
pub fn substitute_assembled(
    input: &Value,
    matches: &[SubstitutionMatch],
    replacements: &[Vec<Value>],
    resources: &ResourceContext<'_>,
) -> Result<Vec<Value>, EngineRunError> {
    let Value::String(input) = input.untagged() else {
        return Err(EngineRunError::internal_contract(
            "substitution assembly over a non-string input",
        ));
    };
    if matches.is_empty() {
        return Ok(vec![string_value(input.as_str(), resources)?]);
    }
    let mut streams = Vec::new();
    let mut max_len = 0;
    for (replacement, matched) in replacements.iter().zip(matches) {
        let texts = replacement
            .iter()
            .map(|value| substitution_replacement_text(value, &input[..matched.start], resources))
            .collect::<Result<Vec<_>, EngineRunError>>()?;
        max_len = max_len.max(texts.len());
        streams.push(texts);
    }
    if max_len == 0 {
        return Ok(vec![string_value(input.as_str(), resources)?]);
    }
    let mut out = Vec::new();
    for index in 0..max_len {
        let mut text = String::new();
        let mut previous_end = 0;
        for (matched, replacements) in matches.iter().zip(&streams) {
            text.push_str(&input[previous_end..matched.start]);
            if let Some(replacement) = replacements.get(index) {
                text.push_str(replacement);
            }
            previous_end = matched.end;
        }
        text.push_str(&input[previous_end..]);
        out.push(string_value(&text, resources)?);
    }
    Ok(out)
}

fn substitution_replacement_text(
    value: &Value,
    prefix: &str,
    resources: &ResourceContext<'_>,
) -> Result<String, EngineRunError> {
    match value.untagged() {
        Value::String(text) => Ok(text.as_str().to_owned()),
        Value::Null => Ok(String::new()),
        value => {
            let operand = message::dump_trunc_owned(value)?;
            let left = message::dump_trunc_owned(
                &Value::try_string(prefix).map_err(|_| EngineRunError::allocation_failure())?,
            )?;
            let text = message::join(&[
                "string (",
                &left,
                ") and ",
                message::kind_name(value.kind()),
                " (",
                &operand,
                ") cannot be added",
            ])?;
            Err(raise(&text, resources))
        }
    }
}

// ------------------------------------------------------------------------
// Value construction and rejections.

fn string_value(text: &str, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    Value::try_string(text).map_err(|_| EngineRunError::allocation_failure())
}

fn array_value(parts: Vec<String>, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    for part in parts {
        array
            .try_push(string_value(&part, resources)?)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(array))
}

fn expect_string<'a>(value: &'a Value, resources: &ResourceContext<'_>) -> Result<&'a str, EngineRunError> {
    if let Value::String(text) = value.untagged() {
        Ok(text.as_str())
    } else {
        let operand = message::dump_trunc_owned(value)?;
        let text = message::join(&[message::kind_name(value.kind()), " (", &operand, ") is not a string"])?;
        Err(raise(&text, resources))
    }
}

fn expect_matchable_string<'a>(value: &'a Value, resources: &ResourceContext<'_>) -> Result<&'a str, EngineRunError> {
    if let Value::String(text) = value.untagged() {
        Ok(text.as_str())
    } else {
        let operand = message::dump_trunc_owned(value)?;
        let text = message::join(&[
            message::kind_name(value.kind()),
            " (",
            &operand,
            ") cannot be matched, as it is not a string",
        ])?;
        Err(raise(&text, resources))
    }
}

/// The three strings one `test`/`match`/`capture` call runs on — subject, pattern, flags — read in the reference's
/// own ORDER. A /1 form reads pattern and flags from its single argument, and that argument law runs BEFORE the
/// subject: `1 | match(1)` names the argument (`number not a string or array`). A /2 form reaches the matcher with the
/// subject checked first: `1 | match(1; "g")` names the subject.
fn matcher_arguments<'a>(
    law: RegexLaw,
    input: &'a Value,
    pattern: &'a Value,
    flags: &'a Value,
    resources: &ResourceContext<'_>,
) -> Result<(&'a str, &'a str, &'a str), EngineRunError> {
    if matches!(law, RegexLaw::Test1 | RegexLaw::Match1 | RegexLaw::Capture1) {
        let (pattern, flags) = regex_flags_argument(pattern, resources)?;
        Ok((expect_matchable_string(input, resources)?, pattern, flags))
    } else {
        let input = expect_matchable_string(input, resources)?;
        Ok((
            input,
            expect_string(pattern, resources)?,
            expect_flags(flags, resources)?,
        ))
    }
}

/// The /1 argument law for `test`/`match`/`capture`: a STRING is the pattern with no flags, and a NON-EMPTY ARRAY is
/// `[regex]` or `[regex, flags]` with later elements ignored. Every other kind — an EMPTY array included — raises
/// `<kind> not a string or array`, which is why that sentence names a type and no operand. Inside the array the
/// ordinary rejections apply (`match([1,2])` is `number (1) is not a string`).
///
/// `scan`/`splits`/`split`/`sub`/`gsub` do NOT take the array form: their reference definitions hand `$re` straight to
/// the matcher, so an array there is `array ([…]) is not a string`.
fn regex_flags_argument<'a>(
    argument: &'a Value,
    resources: &ResourceContext<'_>,
) -> Result<(&'a str, &'a str), EngineRunError> {
    match argument.untagged() {
        Value::String(text) => Ok((text.as_str(), "")),
        Value::Array(array) if !array.is_empty() => {
            let pattern = array
                .get(0)
                .ok_or_else(|| EngineRunError::internal_contract("a non-empty array has a first element"))?;
            // The REGEX is read first: `match([1,2])` names `number (1)`.
            let pattern = expect_string(pattern, resources)?;
            let flags = match array.get(1) {
                Some(flags) => expect_flags(flags, resources)?,
                None => "",
            };
            Ok((pattern, flags))
        }
        _ => {
            let text = message::join(&[message::kind_name(argument.kind()), " not a string or array"])?;
            Err(raise(&text, resources))
        }
    }
}

/// The flags argument as the reference's flag string. A STRING is itself and `null` is NO FLAGS: the /1 forms pass
/// `null` for it (`def scan($re): scan($re; null);`) and `null` is the identity for the reference's `+`, so ported code
/// depends on it. Every other kind raises the operand rejection `_match_impl` raises (`number (1) is not a string`).
fn expect_flags<'a>(flags: &'a Value, resources: &ResourceContext<'_>) -> Result<&'a str, EngineRunError> {
    if matches!(flags.untagged(), Value::Null) {
        Ok("")
    } else {
        expect_string(flags, resources)
    }
}

/// Where a reference definition concatenates the `"g"` it forces on: `scan/2` spells `"g" + $flags` while `splits/2`,
/// `split/2` and `gsub/3` spell `$flags + "g"`. The position decides the flag string AND the operand order of the
/// rejection sentence.
#[derive(Clone, Copy, Debug)]
enum GlobalFlag {
    Leading,
    Trailing,
}

/// The flag string a forced-global law compiles with, and the concatenation sentence a non-string flags argument raises
/// — the reference's own, because the `"g"` really is concatenated by `+` and the error is `+`'s (`number (1) and
/// string ("g") cannot be added` for `splits`/`split`/`gsub`, `string ("g") and number (1) cannot be added` for
/// `scan`). `null` concatenates as the identity, leaving just the `"g"`.
fn flags_with_global(
    flags: &Value,
    position: GlobalFlag,
    resources: &ResourceContext<'_>,
) -> Result<String, EngineRunError> {
    let authored = match flags.untagged() {
        Value::Null => "",
        Value::String(text) => text.as_str(),
        _ => {
            let operand = message::dump_trunc_owned(flags)?;
            let kind = message::kind_name(flags.kind());
            let text = match position {
                GlobalFlag::Leading => {
                    message::join(&["string (\"g\") and ", kind, " (", &operand, ") cannot be added"])?
                }
                GlobalFlag::Trailing => message::join(&[kind, " (", &operand, ") and string (\"g\") cannot be added"])?,
            };
            return Err(raise(&text, resources));
        }
    };
    Ok(match position {
        GlobalFlag::Leading => format!("g{authored}"),
        GlobalFlag::Trailing => format!("{authored}g"),
    })
}

/// The flags string one substitution law compiles with, and with it the replace-all decision (the `g` the law forces on
/// is the same `g` the scan reads). The reference defines `sub($re; s)` as `sub($re; s; "")`, `gsub($re; s)` as
/// `sub($re; s; "g")` and `gsub($re; s; flags)` as `sub($re; s; flags + "g")`, so a `gsub/3` flags rejection is the
/// ADDITION sentence while a `sub/3` one is `is not a string`.
fn substitution_flags(law: RegexLaw, flags: &Value, resources: &ResourceContext<'_>) -> Result<String, EngineRunError> {
    match law {
        RegexLaw::Sub2 => Ok(String::new()),
        RegexLaw::Gsub2 => Ok("g".to_owned()),
        RegexLaw::Sub3 => Ok(expect_flags(flags, resources)?.to_owned()),
        RegexLaw::Gsub3 => flags_with_global(flags, GlobalFlag::Trailing, resources),
        _ => Err(EngineRunError::internal_contract(
            "substitution flags asked for a non-substitution law",
        )),
    }
}

/// A running codepoint count across one successive-match scan.
///
/// `regex_matches` publishes matches in ascending, non-overlapping order (`next_search_start` never rewinds), so every
/// `.offset`/`.length` query of one scan walks the input FORWARD from where the last query stopped instead of
/// rescanning `input[0..start]` per match object — one pass over the input per scan rather than one pass per match.
/// Capture spans inside one match may interleave out of array order; a query below the cursor answers subtractively
/// over the span between and never rewinds.
#[derive(Default)]
struct CodepointTally {
    counted_to: usize,
    count: i64,
}

impl CodepointTally {
    /// Codepoints in `input[0..pos]`.
    fn count_at(&mut self, input: &str, pos: usize) -> i64 {
        if pos < self.counted_to {
            #[allow(
                clippy::cast_possible_wrap,
                reason = "a string longer than i64::MAX codepoints cannot exist in a decoded document"
            )]
            let behind = input[pos..self.counted_to].chars().count() as i64;
            return self.count - behind;
        }
        #[allow(
            clippy::cast_possible_wrap,
            reason = "a string longer than i64::MAX codepoints cannot exist in a decoded document"
        )]
        {
            self.count += input[self.counted_to..pos].chars().count() as i64;
        }
        self.counted_to = pos;
        self.count
    }
}

fn integer_value(value: i64) -> Value {
    Value::Number(Number::integer(jqf_data::Integer::from_i64(value)))
}

fn build_scan_value(
    input: &str,
    matched: &RegexMatch,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    if matched.captures.len() <= 1 {
        return string_value(&input[matched.start..matched.end], resources);
    }
    let mut array = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    for capture in &matched.captures[1..] {
        let value = match capture {
            Some((start, end)) => string_value(&input[*start..*end], resources)?,
            None => Value::Null,
        };
        array
            .try_push(value)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(array))
}

fn build_match_object(
    input: &str,
    matched: &RegexMatch,
    capture_names: &[Option<String>],
    tally: &mut CodepointTally,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let offset = tally.count_at(input, matched.start);
    // Captures are resolved BEFORE the walk advances to the match's own end: their spans live inside `[matched.start,
    // matched.end]`, so resolving them first keeps the tally's walk forward-only in the common case.
    let mut captures = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    for index in 1..matched.captures.len() {
        let capture = build_match_capture_object(
            input,
            matched.captures[index],
            capture_names.get(index).and_then(Option::as_deref),
            tally,
            resources,
        )?;
        captures
            .try_push(capture)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    let length = tally.count_at(input, matched.end) - offset;
    let mut builder = ObjectBuilder::try_with_capacity(4).map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(object_key("offset")?, integer_value(offset))
        .map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(object_key("length")?, integer_value(length))
        .map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(
            object_key("string")?,
            string_value(&input[matched.start..matched.end], resources)?,
        )
        .map_err(|_| EngineRunError::allocation_failure())?;
    builder
        .try_insert_last(object_key("captures")?, Value::Array(captures))
        .map_err(|_| EngineRunError::allocation_failure())?;
    Ok(Value::Object(
        builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?,
    ))
}

fn build_match_capture_object(
    input: &str,
    matched: Option<(usize, usize)>,
    name: Option<&str>,
    tally: &mut CodepointTally,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut builder = ObjectBuilder::try_with_capacity(3).map_err(|_| EngineRunError::allocation_failure())?;
    if let Some((start, end)) = matched {
        let offset = tally.count_at(input, start);
        builder
            .try_insert_last(object_key("offset")?, integer_value(offset))
            .map_err(|_| EngineRunError::allocation_failure())?;
        if start == end {
            builder
                .try_insert_last(object_key("string")?, string_value("", resources)?)
                .map_err(|_| EngineRunError::allocation_failure())?;
            builder
                .try_insert_last(object_key("length")?, integer_value(0))
                .map_err(|_| EngineRunError::allocation_failure())?;
        } else {
            let length = tally.count_at(input, end) - offset;
            builder
                .try_insert_last(object_key("length")?, integer_value(length))
                .map_err(|_| EngineRunError::allocation_failure())?;
            builder
                .try_insert_last(object_key("string")?, string_value(&input[start..end], resources)?)
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
    } else {
        builder
            .try_insert_last(object_key("offset")?, integer_value(-1))
            .map_err(|_| EngineRunError::allocation_failure())?;
        builder
            .try_insert_last(object_key("string")?, Value::Null)
            .map_err(|_| EngineRunError::allocation_failure())?;
        builder
            .try_insert_last(object_key("length")?, integer_value(0))
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    builder
        .try_insert_last(
            object_key("name")?,
            match name {
                Some(name) => string_value(name, resources)?,
                None => Value::Null,
            },
        )
        .map_err(|_| EngineRunError::allocation_failure())?;
    Ok(Value::Object(
        builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?,
    ))
}

fn build_capture_object(
    input: &str,
    matched: &RegexMatch,
    capture_names: &[Option<String>],
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut entries: Vec<(String, Value)> = Vec::new();
    for index in 1..matched.captures.len() {
        if let Some(Some(name)) = capture_names.get(index) {
            let value = match matched.captures[index] {
                Some((start, end)) => string_value(&input[start..end], resources)?,
                None => Value::Null,
            };
            if let Some((_, existing)) = entries.iter_mut().find(|(existing_name, _)| existing_name == name) {
                *existing = value;
            } else {
                entries.push((name.clone(), value));
            }
        }
    }
    let mut builder =
        ObjectBuilder::try_with_capacity(entries.len()).map_err(|_| EngineRunError::allocation_failure())?;
    for (name, value) in entries {
        builder
            .try_insert_last(object_key(&name)?, value)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Object(
        builder.try_finish().map_err(|_| EngineRunError::allocation_failure())?,
    ))
}

fn object_key(name: &str) -> Result<jqf_data::ObjectKey, EngineRunError> {
    jqf_data::ObjectKey::try_from_str(name).map_err(|_| EngineRunError::allocation_failure())
}

// ------------------------------------------------------------------------
// Registry records.

const ONE_FILTER: &[ParameterKind] = &[ParameterKind::Filter];
const TWO_FILTERS: &[ParameterKind] = &[ParameterKind::Filter, ParameterKind::Filter];
const THREE_FILTERS: &[ParameterKind] = &[ParameterKind::Filter, ParameterKind::Filter, ParameterKind::Filter];

macro_rules! family {
    ($id:expr, $canonical_name:literal) => {
        BuiltinFamilyRecord {
            id: BuiltinFamilyId::new($id),
            canonical_name: $canonical_name,
            category: "regex",
            summary: concat!("The ", $canonical_name, " family."),
            detail: "",
        }
    };
}

macro_rules! overload {
    ($id:expr, $name:literal, $family_id:expr, $arity:expr, $parameters:expr, $program:literal, $input:literal, $expected:literal) => {
        BuiltinOverloadRecord {
            id: BuiltinOverloadId::new($id),
            family: BuiltinFamilyId::new($family_id),
            canonical_name: $name,
            arity: $arity,
            parameters: $parameters,
            execution: BuiltinExecution::Evaluator,
            demand_transfer: DemandTransfer::Subtree,
            semantic_revision: SemanticRevision::new(1),
            effects: Effects::Pure,
            examples: &[BuiltinExample {
                program: $program,
                input: $input,
                expected: $expected,
            }],
        }
    };
}

const TEST_FAMILY: BuiltinFamilyRecord = family!(id::TEST_FAMILY_ID, "test");
const MATCH_FAMILY: BuiltinFamilyRecord = family!(id::MATCH_FAMILY_ID, "match");
const CAPTURE_FAMILY: BuiltinFamilyRecord = family!(id::CAPTURE_FAMILY_ID, "capture");
const SCAN_FAMILY: BuiltinFamilyRecord = family!(id::SCAN_FAMILY_ID, "scan");
const SPLITS_FAMILY: BuiltinFamilyRecord = family!(id::SPLITS_FAMILY_ID, "splits");
const SUB_FAMILY: BuiltinFamilyRecord = family!(id::SUB_FAMILY_ID, "sub");
const GSUB_FAMILY: BuiltinFamilyRecord = family!(id::GSUB_FAMILY_ID, "gsub");

pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    TEST_FAMILY,
    MATCH_FAMILY,
    CAPTURE_FAMILY,
    SCAN_FAMILY,
    SPLITS_FAMILY,
    SUB_FAMILY,
    GSUB_FAMILY,
];

const TEST_1_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::TEST_1,
    "test",
    id::TEST_FAMILY_ID,
    1,
    ONE_FILTER,
    "test(\"b\")",
    "\"abc\"",
    "true\n"
);
const TEST_2_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::TEST_2,
    "test",
    id::TEST_FAMILY_ID,
    2,
    TWO_FILTERS,
    "test(\"B\"; \"i\")",
    "\"abc\"",
    "true\n"
);
const MATCH_1_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::MATCH_1,
    "match",
    id::MATCH_FAMILY_ID,
    1,
    ONE_FILTER,
    "match(\"b\")",
    "\"abc\"",
    "{\"offset\":1,\"length\":1,\"string\":\"b\",\"captures\":[]}\n"
);
const MATCH_2_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::MATCH_2,
    "match",
    id::MATCH_FAMILY_ID,
    2,
    TWO_FILTERS,
    "match(\"(?<x>a)(?<y>b)\"; \"\")",
    "\"abc\"",
    "{\"offset\":0,\"length\":2,\"string\":\"ab\",\"captures\":[{\"offset\":0,\"length\":1,\"string\":\"a\",\"name\":\"x\"},{\"offset\":1,\"length\":1,\"string\":\"b\",\"name\":\"y\"}]}\n"
);
const CAPTURE_1_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::CAPTURE_1,
    "capture",
    id::CAPTURE_FAMILY_ID,
    1,
    ONE_FILTER,
    "capture(\"(?<x>a)(?<y>b)\")",
    "\"ab\"",
    "{\"x\":\"a\",\"y\":\"b\"}\n"
);
const CAPTURE_2_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::CAPTURE_2,
    "capture",
    id::CAPTURE_FAMILY_ID,
    2,
    TWO_FILTERS,
    "capture(\"(?<x>a)\"; \"\")",
    "\"ab\"",
    "{\"x\":\"a\"}\n"
);
const SCAN_1_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::SCAN_1,
    "scan",
    id::SCAN_FAMILY_ID,
    1,
    ONE_FILTER,
    "[scan(\"a\")]",
    "\"aaa\"",
    "[\"a\",\"a\",\"a\"]\n"
);
const SCAN_2_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::SCAN_2,
    "scan",
    id::SCAN_FAMILY_ID,
    2,
    TWO_FILTERS,
    // The example CALLS scan/2. `[scan("(?<x>a)?")]` used to sit here — a scan/1 call, so the arity-2 overload had no
    // example of ITSELF and the examples-as-tests lane was green on the wrong overload.
    "[scan(\"A\"; \"i\")]",
    "\"aA\"",
    "[\"a\",\"A\"]\n"
);
const SPLITS_1_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::SPLITS_1,
    "splits",
    id::SPLITS_FAMILY_ID,
    1,
    ONE_FILTER,
    "[splits(\",\")]",
    "\"a,b,c\"",
    "[\"a\",\"b\",\"c\"]\n"
);
const SPLITS_2_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::SPLITS_2,
    "splits",
    id::SPLITS_FAMILY_ID,
    2,
    TWO_FILTERS,
    "[splits(\",\"; \"\")]",
    "\"a,b,c\"",
    "[\"a\",\"b\",\"c\"]\n"
);
const SPLIT_2_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::SPLIT_2,
    "split",
    id::SPLIT,
    2,
    TWO_FILTERS,
    "split(\",\"; \"g\")",
    "\"a,b,c\"",
    "[\"a\",\"b\",\"c\"]\n"
);
const SUB_2_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::SUB_2,
    "sub",
    id::SUB_FAMILY_ID,
    2,
    TWO_FILTERS,
    "sub(\"a\"; \"X\")",
    "\"aaa\"",
    "\"Xaa\"\n"
);
const SUB_3_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::SUB_3,
    "sub",
    id::SUB_FAMILY_ID,
    3,
    THREE_FILTERS,
    "sub(\"a\"; \"X\"; \"g\")",
    "\"aaa\"",
    "\"XXX\"\n"
);
const GSUB_2_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::GSUB_2,
    "gsub",
    id::GSUB_FAMILY_ID,
    2,
    TWO_FILTERS,
    "gsub(\"a\"; \"X\")",
    "\"aaa\"",
    "\"XXX\"\n"
);
const GSUB_3_OVERLOAD: BuiltinOverloadRecord = overload!(
    id::GSUB_3,
    "gsub",
    id::GSUB_FAMILY_ID,
    3,
    THREE_FILTERS,
    // The example CALLS gsub/3. `gsub("(?<x>.)"; .x)` used to sit here — a gsub/2 call, so the arity-3 overload had
    // no example of ITSELF.
    "gsub(\"A\"; \"x\"; \"i\")",
    "\"aA\"",
    "\"xx\"\n"
);

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    TEST_1_OVERLOAD,
    TEST_2_OVERLOAD,
    MATCH_1_OVERLOAD,
    MATCH_2_OVERLOAD,
    CAPTURE_1_OVERLOAD,
    CAPTURE_2_OVERLOAD,
    SCAN_1_OVERLOAD,
    SCAN_2_OVERLOAD,
    SPLITS_1_OVERLOAD,
    SPLITS_2_OVERLOAD,
    SPLIT_2_OVERLOAD,
    SUB_2_OVERLOAD,
    SUB_3_OVERLOAD,
    GSUB_2_OVERLOAD,
    GSUB_3_OVERLOAD,
];

/// The regex execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, RegexLaw)] = &[
    (id::TEST_1, RegexLaw::Test1),
    (id::TEST_2, RegexLaw::Test2),
    (id::MATCH_1, RegexLaw::Match1),
    (id::MATCH_2, RegexLaw::Match2),
    (id::CAPTURE_1, RegexLaw::Capture1),
    (id::CAPTURE_2, RegexLaw::Capture2),
    (id::SCAN_1, RegexLaw::Scan1),
    (id::SCAN_2, RegexLaw::Scan2),
    (id::SPLITS_1, RegexLaw::Splits1),
    (id::SPLITS_2, RegexLaw::Splits2),
    (id::SPLIT_2, RegexLaw::Split2),
    (id::SUB_2, RegexLaw::Sub2),
    (id::SUB_3, RegexLaw::Sub3),
    (id::GSUB_2, RegexLaw::Gsub2),
    (id::GSUB_3, RegexLaw::Gsub3),
];

#[cfg(test)]
mod tests {
    use super::{
        Array, EngineRunError, FallbackConstruct, ReferenceCompiledRegex, RegexLaw, SubstitutionMatch, Value,
        analyze_pattern, apply_combo, compile_regex, message, regex_match_spans, substitute_assembled,
        substitute_assembled_literal, substitution_match_spans,
    };
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    /// The byte spans one global scan names, the shape every `g` law reads.
    fn spans(pattern: &str, flags: &str, input: &str) -> Vec<(usize, usize)> {
        let resources = resources();
        let (regex, options, _) = compile_regex(pattern, flags, &resources).expect("pattern compiles");
        assert!(options.global, "the probe drives the GLOBAL scan");
        regex_match_spans(input, &regex, options, true, &resources).expect("the scan answers")
    }

    /// A zero-width match at the subject's END is emitted ONCE and ends the scan (the reference: `"abc" |
    /// [match("$";"g")|.offset]` is `[3]`). The scan used to resume one codepoint past the SEARCH START rather than
    /// past the MATCH, so an end-anchored pattern re-found the same match once per remaining position —
    /// `gsub("$";"X")` answered `abcXXXX`.
    #[test]
    fn a_zero_width_match_at_the_end_is_emitted_once() {
        assert_eq!(spans("$", "g", "abc"), vec![(3, 3)]);
        assert_eq!(spans(r"\b", "g", "abc"), vec![(0, 0), (3, 3)]);
        assert_eq!(spans(r"\b", "g", "ab cd"), vec![(0, 0), (2, 2), (3, 3), (5, 5)]);
        assert_eq!(spans("$", "g", ""), vec![(0, 0)]);
        assert_eq!(spans(r"\b", "g", ""), Vec::new());
    }

    /// The advance law itself: a NON-EMPTY match resumes at its end, a zero-width one resumes one CODEPOINT past the
    /// match, and the search AT the subject's end still runs (the reference: `"abcc" | [match("c*";"g")]` ends with an
    /// empty match at offset 4).
    #[test]
    fn the_global_scan_advances_past_the_match_not_past_the_search_start() {
        assert_eq!(spans("", "g", "abc"), vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
        assert_eq!(spans("c*", "g", "abcc"), vec![(0, 0), (1, 1), (2, 4), (4, 4)]);
        assert_eq!(spans("b|$", "g", "abc"), vec![(1, 2), (3, 3)]);
        assert_eq!(spans("", "g", ""), vec![(0, 0)]);
    }

    /// The `n` (no-empty) flag changes `test`'s EXISTENCE question: an empty primary match only counts when a non-empty
    /// match exists at that same start or a later start finds one. The reference: `"b" | test("a*"; "n")` is false
    /// (only the empty match exists), `"ab"` is true (the non-empty `a` at offset 0), and `"bb"` is false again.
    #[test]
    fn test_n_flag_existence_matches_the_reference() {
        let resources = resources();
        let answer = |input: &str, pattern: &str, flags: &str| -> bool {
            let values = apply_combo(
                RegexLaw::Test2,
                &Value::try_string(input).expect("input"),
                &Value::try_string(pattern).expect("pattern"),
                &Value::try_string(flags).expect("flags"),
                &resources,
            )
            .expect("test answers");
            match values.as_slice() {
                [Value::Bool(answer)] => *answer,
                _ => panic!("test answers one boolean"),
            }
        };
        assert!(!answer("b", "a*", "n"));
        assert!(answer("ab", "a*", "n"));
        assert!(!answer("bb", "a*", "n"));
        assert!(answer("b", "a*", ""), "without n an empty match exists");
    }

    /// The literal substitution splice rebuilds the string with the constant text at every span, byte-identically to
    /// the capture-object assembly it replaces (one replacement output per match, `"X"` here), including the zero-width
    /// case at every codepoint boundary.
    #[test]
    fn literal_substitution_splices_the_same_bytes_as_the_filter_path() {
        let resources = resources();
        let input = Value::try_string("aba").expect("input");
        let pattern = Value::try_string("a").expect("pattern");
        let spans =
            substitution_match_spans(RegexLaw::Gsub2, &input, &pattern, &Value::Null, &resources).expect("spans");
        assert_eq!(spans, vec![(0, 1), (2, 3)]);
        let spliced = substitute_assembled_literal(&input, &spans, "X", &resources).expect("literal assembly");
        let Value::String(text) = spliced[0].untagged() else {
            panic!("literal assembly must answer a string");
        };
        assert_eq!(text.as_str(), "XbX");
        let matches = spans
            .iter()
            .map(|&(start, end)| SubstitutionMatch {
                start,
                end,
                captures: Value::Null,
            })
            .collect::<Vec<_>>();
        let replacements = spans
            .iter()
            .map(|_| vec![Value::try_string("X").expect("replacement")])
            .collect::<Vec<_>>();
        let assembled = substitute_assembled(&input, &matches, &replacements, &resources).expect("filter assembly");
        let Value::String(filtered) = assembled[0].untagged() else {
            panic!("filter assembly must answer a string");
        };
        assert_eq!(text.as_str(), filtered.as_str());
        let untouched = substitute_assembled_literal(&input, &[], "X", &resources).expect("empty-span echo");
        let Value::String(echo) = untouched[0].untagged() else {
            panic!("empty-span echo must be a string");
        };
        assert_eq!(echo.as_str(), "aba");
        let empty_at_every_boundary = substitute_assembled_literal(
            &Value::try_string("ab").expect("input"),
            &[(0, 0), (1, 1), (2, 2)],
            "X",
            &resources,
        )
        .expect("zero-width splice");
        let Value::String(spliced_text) = empty_at_every_boundary[0].untagged() else {
            panic!("zero-width splice must answer a string");
        };
        assert_eq!(spliced_text.as_str(), "XaXbX");
    }

    /// The reference engine's octal NUL escape: `\0` is a BACKREFERENCE to both Rust tiers, so jqf used to refuse a
    /// pattern the reference compiles (`"ab" | test("a\\0b")` is false in the reference, a raise here). The rewrite
    /// turns `\0` plus up to two following octal digits into the `\xNN` spelling both tiers accept, keeping the pattern
    /// on the FAST tier.
    #[test]
    fn an_octal_nul_escape_is_rewritten_not_refused() {
        let resources = resources();
        // `\0` alone: NUL.
        let analysis = analyze_pattern(r"a\0b", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"a\x00b");
        assert!(analysis.fallback.is_none(), "the fast tier must keep it");
        // `\01` … `\077`: the octal range the reference engine assigns.
        let analysis = analyze_pattern(r"a\01b", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"a\x01b");
        let analysis = analyze_pattern(r"a\077b", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"a\x3fb");
        // A non-octal digit after `\0` is a literal: `\08` is NUL then `8`.
        let analysis = analyze_pattern(r"a\08b", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"a\x008b");
        // The end-to-end law: `"ab" | test("a\\0b")` answers false in BOTH (a NUL cannot appear in the ASCII input).
        let matches = spans(r"a\0b", "g", "ab");
        assert!(matches.is_empty(), "NUL never matches ASCII");
        // And the in-class form compiles too (`[a\0b]` matches a NUL).
        assert_eq!(spans(r"[a\0b]", "g", "a\u{0}b"), vec![(0, 1), (1, 2), (2, 3)]);
    }

    /// A Unicode property class opens its `{name}` as part of the ESCAPE, so the brace must reach the engine verbatim:
    /// the bare-brace leniency rewrite used to escape it (`\p{Han}` → `\p\{Han}`) and both tiers refused a pattern
    /// the reference answers (`"日本語" | test("\\p{Han}")` is true there). The same holds for the negated `\P`
    /// spelling and the in-class form.
    #[test]
    fn a_unicode_property_class_brace_is_copied_verbatim() {
        let resources = resources();
        let analysis = analyze_pattern(r"\p{Han}", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"\p{Han}");
        assert!(analysis.fallback.is_none(), "the fast tier keeps it");
        let analysis = analyze_pattern(r"\P{L}", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"\P{L}");
        // End to end: the class really matches Han script text.
        assert_eq!(spans(r"\p{Han}", "g", "日本語"), vec![(0, 3), (3, 6), (6, 9)]);
        // The negated spelling excludes it.
        assert_eq!(spans(r"\P{Han}", "g", "日x"), vec![(3, 4)]);
        // The in-class form compiles and matches too.
        assert_eq!(spans(r"[\p{Nd}]+", "g", "a42b"), vec![(1, 3)]);
    }

    /// The reference engine's remaining escape spellings answer where they used to raise: braced hex (`\x{1F600}` —
    /// the brace was corrupted by the same bare-brace rewrite as `\p`), the octal spell `\o{n}`, an octal byte reading
    /// of `\NNN` when no group carries that number, and the `\Q…\E` literal quote. A `\N` that DOES name a seen group
    /// is still routed as a backreference.
    #[test]
    fn the_reference_engines_escape_spellings_answer_like_the_reference() {
        let resources = resources();
        // Braced hex, copied verbatim onto the fast tier.
        let analysis = analyze_pattern(r"\x{1F600}", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"\x{1F600}");
        assert!(analysis.fallback.is_none(), "the fast tier keeps it");
        assert_eq!(spans(r"\x{1F600}", "g", "a😀"), vec![(1, 5)]);
        // The octal spell: `\o{101}` is 0x41 = "A".
        assert_eq!(spans(r"\o{101}", "g", "xAy"), vec![(1, 2)]);
        // The braced spell is CODEPOINT-valued in the reference, so `\o{401}` is U+0101, spelled as the braced hex both
        // tiers accept.
        let analysis = analyze_pattern(r"\o{401}", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"\x{101}");
        assert_eq!(spans(r"\o{401}", "g", "xā"), vec![(1, 3)]);
        // Octal NOT a group: `\101` names no group, so it reads as 0x41.
        let analysis = analyze_pattern(r"\101", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"\x41");
        assert!(analysis.fallback.is_none());
        assert_eq!(spans(r"\101", "g", "A"), vec![(0, 1)]);
        // …but a digit run naming an EXISTING group stays a backreference, routed to the backtracking tier unchanged.
        let analysis = analyze_pattern(r"(a)\1", &resources, false).expect("analysis");
        assert_eq!(
            analysis.fallback,
            Some(FallbackConstruct::Backreference),
            "an existing group's number is a backreference"
        );
        assert_eq!(spans(r"(a|b)\1", "g", "aa"), vec![(0, 2)]);
        // A FORWARD reference names a group the walk has not reached yet, and the reference still reads it as a
        // backreference there — one that fails at match time — never as an octal escape (`(q)\2(y)` refuses to
        // match a U+0002 between the q and the y).
        let analysis = analyze_pattern(r"(q)\2(y)", &resources, false).expect("analysis");
        assert_eq!(
            analysis.fallback,
            Some(FallbackConstruct::Backreference),
            "a forward reference is a backreference, not an escape"
        );
        assert!(spans(r"(q)\2(y)", "g", "q\u{2}y").is_empty());
        // A run that is neither reading — `\8` can spell no octal byte and names no group — stays authored for the
        // tier refusal, which is exit-class-identical to the reference's own.
        let analysis = analyze_pattern(r"\8", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"\8");
        assert_eq!(analysis.fallback, Some(FallbackConstruct::Backreference));
        // `\Q…\E` quotes every metacharacter through the closing `\E`.
        assert_eq!(spans(r"\Qa.b\E", "g", "xa.by"), vec![(1, 4)]);
        // A dot inside the quote is LITERAL, not any-character.
        assert!(spans(r"\Qa.b\E", "g", "axby").is_empty());
        // Backslashes inside the quote are literal too.
        assert_eq!(spans(r"\Q\d\E", "g", "a\\d"), vec![(1, 3)]);
        // An unterminated quote runs to the end of the pattern.
        let analysis = analyze_pattern(r"\Qa.b", &resources, false).expect("analysis");
        assert_eq!(analysis.rewritten, r"a\.b");
    }

    /// The reference engine's malformed-pattern leniency: an invalid interval `{` and a `[` inside a character class
    /// are LITERALS in the reference (`"z{a}z" | test("{a}")` is true — the pattern matches the text itself), where
    /// the regex tiers rejected both. The rewrite now escapes the two shapes; a `{` that DOES open a valid interval
    /// still raises (`test("{2}")` is the reference's `target of repeat op`, exit-class-identical).
    #[test]
    fn an_invalid_interval_and_a_nested_class_are_literal() {
        let resources = resources();
        let test = |pattern: &str, input: &str| -> Result<bool, EngineRunError> {
            let values = apply_combo(
                RegexLaw::Test1,
                &Value::try_string(input).expect("input"),
                &Value::try_string(pattern).expect("pattern"),
                &Value::Null,
                &resources,
            )?;
            match values.as_slice() {
                [Value::Bool(truth)] => Ok(*truth),
                _ => Err(EngineRunError::internal_contract("test answers one boolean")),
            }
        };
        assert!(test("{a}", "z{a}z").expect("the literal brace matches"));
        assert!(test("{", "z{z}z").expect("a lone brace matches"));
        assert!(!test("{}", "x").expect("an empty interval is literal"));
        assert!(!test("[[1,2]", "x").expect("a nested class compiles"));
        assert!(test("[[]", "[").expect("a class of brackets matches"));
        assert!(test("{2}", "x").is_err(), "a real interval with no target raises");
    }

    /// The advance is by CODEPOINT, so a multi-byte subject names one empty match per character rather than one per
    /// byte. (The reference advances by BYTE here and emits duplicate offsets inside a multi-byte character — the
    /// catalogued divergence the compat corpus pins as an `intdiff` row.)
    #[test]
    fn the_zero_width_advance_is_by_codepoint() {
        assert_eq!(spans("", "g", "aé漢b"), vec![(0, 0), (1, 1), (3, 3), (6, 6), (7, 7)]);
        assert_eq!(spans("$", "g", "aé漢b"), vec![(7, 7)]);
    }

    /// The `l` (longest) engine follows the same advance law.
    #[test]
    fn the_longest_engine_shares_the_advance_law() {
        assert_eq!(spans("$", "gl", "abc"), vec![(3, 3)]);
        assert_eq!(spans("", "gl", "aé"), vec![(0, 0), (1, 1), (3, 3)]);
    }

    /// The `n` (no-empty) flag skips a zero-width match without stalling the scan: the reference's `"abcc" |
    /// [match("c*";"gn")]` is the single `cc` match.
    #[test]
    fn the_no_empty_flag_skips_zero_width_matches() {
        assert_eq!(spans("c*", "gn", "abcc"), vec![(2, 4)]);
        assert_eq!(spans("x*", "gn", "abc"), Vec::new());
    }

    /// The `l` (longest) flag is the reference engine's longest-match option, which is GLOBALLY longest with ties
    /// broken LEFTMOST — not leftmost-longest.
    ///
    /// Every expectation here pins the reference. The `a|bb` rows are the ones that tell the two readings apart: a
    /// leftmost-longest search answers `a` at offset 0 on `"abb"`, and the reference answers `bb` at offset 1. The
    /// engine used to answer with `MatchKind::All`'s LAST maximal match, so `"aaa" | match("a";"l")` was offset 2 where
    /// the reference says 0.
    #[test]
    fn the_longest_law_is_globally_longest_with_leftmost_ties() {
        assert_eq!(spans("a", "gl", "aaa"), vec![(0, 1), (1, 2), (2, 3)]);
        assert_eq!(spans("a|bb", "gl", "abb"), vec![(1, 3)]);
        assert_eq!(spans("a|bb", "gl", "aabb"), vec![(2, 4)]);
        assert_eq!(spans("a|bb", "gl", "bba"), vec![(0, 2), (2, 3)]);
        // The `a` at offset 0 is not merely reordered, it is never emitted: the search that skipped it resumes past the
        // match it preferred.
        assert_eq!(spans("a|bb", "gl", "abbxa"), vec![(1, 3), (4, 5)]);
        // A zero-width tie still goes leftmost, and a longer match anywhere in the window beats every empty one before
        // it.
        assert_eq!(spans("c*", "gl", "abcc"), vec![(2, 4), (4, 4)]);
        assert_eq!(spans("[0-9]*", "gnl", "a1b"), vec![(1, 2)]);
    }

    /// Which tier a pattern lands on is decided by its SYNTAX alone, and this is the seam: a pattern routed to the fast
    /// tier that needed the backtracking one would be a silent wrong answer.
    #[test]
    fn the_classifier_names_the_construct_that_forces_the_backtracking_tier() {
        let resources = resources();
        let routed = [
            ("(?=a)", FallbackConstruct::Lookahead),
            ("(?!a)", FallbackConstruct::Lookahead),
            ("a(?=b)c", FallbackConstruct::Lookahead),
            ("(?<=a)b", FallbackConstruct::Lookbehind),
            ("(?<!a)b", FallbackConstruct::Lookbehind),
            ("(?>a*)a", FallbackConstruct::AtomicGroup),
            ("a*+", FallbackConstruct::PossessiveQuantifier),
            ("a++", FallbackConstruct::PossessiveQuantifier),
            ("a?+", FallbackConstruct::PossessiveQuantifier),
            ("a{2,3}+", FallbackConstruct::PossessiveQuantifier),
            (r"(a)\1", FallbackConstruct::Backreference),
            (r"(?<x>a)\k<x>", FallbackConstruct::Backreference),
            (r"(?<x>a)\k'x'", FallbackConstruct::Backreference),
            (r"(?<x>a)\g<x>", FallbackConstruct::SubroutineCall),
            (r"a\Kb", FallbackConstruct::KeepOut),
        ];
        for (pattern, construct) in routed {
            let analysis = analyze_pattern(pattern, &resources, false).expect("pattern analyzes");
            assert_eq!(
                analysis.fallback,
                Some(construct),
                "{pattern} must route to the backtracking tier"
            );
        }
    }

    /// ...and the other direction, which is the ordinary-pattern half: every ordinary pattern keeps the fast engines,
    /// including the shapes that only LOOK like a routed construct — a lookaround spelling inside a character class
    /// or behind an escape, a named group (which shares `(?<` with lookbehind), a lazy quantifier, and a literal brace
    /// followed by `+`.
    #[test]
    fn an_ordinary_pattern_never_reaches_the_backtracking_tier() {
        let resources = resources();
        let fast = [
            "",
            "a",
            "a|bb",
            "(a)(b)",
            "(?:ab)+",
            "(?i)ab",
            "(?<name>a)",
            "(?<name>a)(?<other>b)",
            "[?=]",
            "[(?=a)]",
            "[a-z]{2,3}",
            "a{2,3}",
            "a}+",
            "a*?",
            "a+?",
            "a??",
            r"\(?=a\)",
            r"\\1",
            r"\bfoo\b",
            r"\d+\s*",
            "^a$",
            "(?<x>[a-z]+)-(?<n>[0-9]+)",
        ];
        for pattern in fast {
            let analysis = analyze_pattern(pattern, &resources, false).expect("pattern analyzes");
            assert_eq!(analysis.fallback, None, "{pattern} must keep the fast engines");
            let (compiled, _, _) = compile_regex(pattern, "", &resources).expect("pattern compiles");
            assert!(
                matches!(compiled, ReferenceCompiledRegex::Fast { .. }),
                "{pattern} must compile onto the fast tier"
            );
        }
    }

    /// The routing law that makes the two-tier engine safe: with ONE declared exception (the test below), every
    /// construct that routes is one the FAST tier refuses to compile at all.
    ///
    /// So routing turns an error into an answer — it does not change one answer into another, which is the risk of
    /// putting a second engine behind a pattern the first could already run. A construct both engines accept may only
    /// join `FallbackConstruct` with reference evidence that the new answer is the reference's, a corpus row pinning
    /// it, and a divergence-catalogue line.
    #[test]
    fn every_routed_construct_is_one_the_fast_tier_refuses() {
        let resources = resources();
        let routed = [
            "(?=a)",
            "(?!a)",
            "(?<=a)b",
            "(?<!a)b",
            "(?>a*)a",
            r"(a)\1",
            r"(?<x>a)\k<x>",
            r"(?<x>a)\g<x>",
            r"a\Kb",
        ];
        for pattern in routed {
            let analysis = analyze_pattern(pattern, &resources, false).expect("pattern analyzes");
            assert!(analysis.fallback.is_some(), "{pattern} is the routed half of this test");
            assert!(
                regex::Regex::new(&analysis.rewritten).is_err(),
                "{pattern} compiles on the fast tier, so routing it changes an ANSWER"
            );
            let (compiled, _, _) = compile_regex(pattern, "", &resources).expect("pattern compiles");
            assert!(
                matches!(compiled, ReferenceCompiledRegex::Fallback { .. }),
                "{pattern} must compile onto the backtracking tier"
            );
        }
    }

    /// The ONE declared exception to the law above, and the reason it is declared rather than hidden: the possessive
    /// quantifier is a construct the fast tier ACCEPTS with a different meaning.
    ///
    /// The `regex` crate reads `a*+` as the nested repetition `(a*)+`, so `"aaa" | test("a*+a")` was TRUE where the
    /// reference (whose engine treats `*+` as a real possessive) says false — a silent wrong answer, found by this
    /// test rather than by a user. Routing possessive quantifiers moves that answer to the reference's. The reference:
    /// `"aaa" | [match("a*+a";"g")]` is `[]`, and `[match("a?+";"g")|.offset]` is `[0,1,2,3]` rather than the fast
    /// tier's single whole-string match.
    #[test]
    fn the_possessive_quantifier_is_the_declared_answer_changing_route() {
        let resources = resources();
        for pattern in ["a*+", "a++", "a?+", "a{2,3}+"] {
            let analysis = analyze_pattern(pattern, &resources, false).expect("pattern analyzes");
            assert_eq!(
                analysis.fallback,
                Some(FallbackConstruct::PossessiveQuantifier),
                "{pattern} routes"
            );
            assert!(
                regex::Regex::new(&analysis.rewritten).is_ok(),
                "{pattern} is the exception BECAUSE the fast tier accepts it"
            );
        }
        // The reference's answers, now jqf's.
        assert_eq!(spans("a*+a", "g", "aaa"), Vec::new());
        assert_eq!(spans("a?+", "g", "aaa"), vec![(0, 1), (1, 2), (2, 3), (3, 3)]);
        assert_eq!(spans("a*+", "g", "aaa"), vec![(0, 3), (3, 3)]);
    }

    /// A construct the reference ITSELF rejects stays on the fast tier, which rejects it too. Routing it would make jqf
    /// accept a program the reference refuses.
    #[test]
    fn a_construct_the_reference_rejects_is_not_routed() {
        let resources = resources();
        // The reference: `(?(1)a|b)` is `Regex failure: invalid backref number/name`, and `(?P<x>a)` is `undefined
        // group option`.
        let analysis = analyze_pattern("(?(1)a|b)", &resources, false).expect("pattern analyzes");
        assert_eq!(analysis.fallback, None);
        assert!(compile_regex("(?(1)a|b)", "", &resources).is_err());
        assert!(compile_regex("(?P<x>a)", "", &resources).is_err());
    }

    fn text(value: &str, _resources: &ResourceContext<'_>) -> Value {
        Value::try_string(value).expect("string value")
    }

    fn number(literal: &str) -> Value {
        Value::Number(jqf_data::Number::try_json_literal(literal).expect("number literal"))
    }

    fn array(items: Vec<Value>, _resources: &ResourceContext<'_>) -> Value {
        let mut array = Array::try_new().expect("array");
        for item in items {
            array.try_push(item).expect("push");
        }
        Value::Array(array)
    }

    /// One law's outputs over `input`, rendered as the compact JSON the diagnostics render — the shape an assertion
    /// can read at a glance.
    fn combo(law: RegexLaw, input: &str, pattern: &Value, flags: &Value) -> Result<String, EngineRunError> {
        let resources = resources();
        let input = text(input, &resources);
        let values = apply_combo(law, &input, pattern, flags, &resources)?;
        let rendered = values
            .iter()
            .map(|value| message::dump_trunc_owned(value).expect("render"))
            .collect::<Vec<_>>();
        Ok(rendered.join(" "))
    }

    /// The message a law raises, for the rejection laws.
    fn combo_message(law: RegexLaw, input: &str, pattern: &Value, flags: &Value) -> String {
        match combo(law, input, pattern, flags) {
            Err(EngineRunError::Raised(Value::String(message))) => String::from(message.as_str()),
            other => panic!("expected a raised message, got {other:?}"),
        }
    }

    /// The /1 forms pass `null` for the flags argument (`def scan($re): scan($re; null);`), and `null` is the identity
    /// for `+`, so a null flags argument means NO FLAGS everywhere in the family — `"a,b" | split(","; null)` is
    /// `["a","b"]`, not an error.
    #[test]
    fn a_null_flags_argument_means_no_flags() {
        let resources = resources();
        let comma = text(",", &resources);
        let empty = text("", &resources);
        for law in [
            RegexLaw::Test2,
            RegexLaw::Match2,
            RegexLaw::Capture2,
            RegexLaw::Scan2,
            RegexLaw::Splits2,
            RegexLaw::Split2,
        ] {
            let with_null = combo(law, "a,b", &comma, &Value::Null).expect("null flags are legal");
            let with_empty = combo(law, "a,b", &comma, &empty).expect("empty flags are legal");
            assert_eq!(with_null, with_empty, "law {law:?} disagreed on null flags");
        }
    }

    /// The four flag sentences: `test`/`match`/ `capture`/`sub` pass the flags straight through, `scan` renders `"g" +
    /// $flags`, and `splits`/`split`/`gsub` render `$flags + "g"` — so the operand ORDER differs by name, and the
    /// rendered KIND is the argument's own.
    #[test]
    fn a_non_string_flags_argument_raises_the_reference_s_sentence() {
        let resources = resources();
        let comma = text(",", &resources);
        let one = number("1");
        let flags_array = array(vec![text("g", &resources)], &resources);
        assert_eq!(
            combo_message(RegexLaw::Test2, "a,b", &comma, &one),
            "number (1) is not a string"
        );
        assert_eq!(
            combo_message(RegexLaw::Scan2, "a,b", &comma, &one),
            "string (\"g\") and number (1) cannot be added"
        );
        assert_eq!(
            combo_message(RegexLaw::Splits2, "a,b", &comma, &one),
            "number (1) and string (\"g\") cannot be added"
        );
        assert_eq!(
            combo_message(RegexLaw::Split2, "a,b", &comma, &flags_array),
            "array ([\"g\"]) and string (\"g\") cannot be added"
        );
    }

    /// The /1 forms of `test`/`match`/`capture` take a `[regex, flags]` array as well as a bare pattern (the
    /// reference's array-kind arms), with a one-element array meaning no flags and later elements ignored.
    #[test]
    fn the_one_arity_matchers_take_a_regex_flags_array() {
        let resources = resources();
        let global = array(vec![text("b", &resources), text("g", &resources)], &resources);
        let bare = array(vec![text("b", &resources)], &resources);
        let extra = array(
            vec![text("b", &resources), text("g", &resources), text("junk", &resources)],
            &resources,
        );
        let insensitive = array(vec![text("B", &resources), text("i", &resources)], &resources);
        // `match([$re, $flags])` IS `match($re; $flags)`, and a one-element array IS `match($re; null)`.
        let matched = combo(RegexLaw::Match1, "abcb", &global, &Value::Null).expect("array form");
        assert_eq!(
            matched,
            combo(RegexLaw::Match2, "abcb", &text("b", &resources), &text("g", &resources)).expect("two-argument form")
        );
        assert_eq!(
            combo(RegexLaw::Match1, "abcb", &extra, &Value::Null).expect("extra elements"),
            matched
        );
        assert_eq!(
            combo(RegexLaw::Match1, "abcb", &bare, &Value::Null).expect("one-element array"),
            combo(RegexLaw::Match2, "abcb", &text("b", &resources), &Value::Null).expect("two-argument form")
        );
        assert_ne!(matched, "", "the array form matched nothing");
        assert_eq!(
            combo(RegexLaw::Test1, "abc", &insensitive, &Value::Null).expect("array form"),
            "true"
        );
    }

    /// The two pattern rejections the reference spells differently: the /1 forms name the argument law (`number not a
    /// string or array`, and an EMPTY array is not the array form), while the /2 forms and everything inside the array
    /// name the operand (`number (1) is not a string`).
    #[test]
    fn a_pattern_argument_is_rejected_the_way_its_arity_spells_it() {
        let resources = resources();
        let one = number("1");
        let empty = array(Vec::new(), &resources);
        let numeric_pattern = array(vec![number("1"), number("2")], &resources);
        let flags = text("g", &resources);
        assert_eq!(
            combo_message(RegexLaw::Match1, "abc", &one, &Value::Null),
            "number not a string or array"
        );
        assert_eq!(
            combo_message(RegexLaw::Match1, "abc", &empty, &Value::Null),
            "array not a string or array"
        );
        assert_eq!(
            combo_message(RegexLaw::Match1, "abc", &numeric_pattern, &Value::Null),
            "number (1) is not a string"
        );
        assert_eq!(
            combo_message(RegexLaw::Match2, "abc", &one, &flags),
            "number (1) is not a string"
        );
        assert_eq!(
            combo_message(RegexLaw::Scan1, "abc", &empty, &Value::Null),
            "array ([]) is not a string"
        );
    }

    /// The `[regex, flags]` argument law runs BEFORE the input is matched: a bad ARGUMENT is named even when the input
    /// is not a string, which is the opposite of the /2 forms' order.
    #[test]
    fn the_one_arity_argument_law_is_read_before_the_input() {
        let resources = resources();
        let one = number("1");
        let flags = text("g", &resources);
        let input = number("7");
        let message = match apply_combo(RegexLaw::Match1, &input, &one, &Value::Null, &resources) {
            Err(EngineRunError::Raised(Value::String(message))) => String::from(message.as_str()),
            other => panic!("expected a raised message, got {other:?}"),
        };
        assert_eq!(message, "number not a string or array");
        let ordered = match apply_combo(RegexLaw::Match2, &input, &one, &flags, &resources) {
            Err(EngineRunError::Raised(Value::String(message))) => String::from(message.as_str()),
            other => panic!("expected a raised message, got {other:?}"),
        };
        assert_eq!(ordered, "number (7) cannot be matched, as it is not a string");
    }
}
