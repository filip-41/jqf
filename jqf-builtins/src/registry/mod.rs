//! The engine-owned builtin catalog seam: one record, many consumers.
//!
//! One job: own the const builtin inventories and resolve a `(name, arity)` call to a stable overload record. The same
//! records later feed dispatch, the declaration compiler, generated reference docs, CLI help, and SDK introspection, so
//! documentation cannot drift from behavior. The inventories are validated at compile time by [`validate`]: a duplicate
//! id, a duplicate `(name, arity)`, or a missing required doc field is a build error.
//!
//! The inventories are the concatenation of the per-family const slices in [`builtins`], so the file split is the
//! source of record ownership: each family module owns exactly the families its name assigns it (`core` owns
//! `length`/`keys`, `control` owns `select`, `collection` owns `map`, `order` owns the sorting vocabulary, `text` owns
//! the stringifiers, `format` owns the ten `@name` transforms, `strings` owns the arity-0 scalar laws (the two parsers,
//! the codepoint pair, the ASCII case pair, the byte length and the three trims), `search` owns the text builtins that
//! take an ARGUMENT, `entries` owns the object/pair-array pair, `generate` owns the value SOURCES, and `reshape` owns
//! the restructuring vocabulary and the index idioms). The crate-private [`dispatch`] table is the executing half —
//! pure records here, execution payloads there, keyed by the same stable [`BuiltinOverloadId`].
//!
//! Public catalog accessor note: the read-only catalog accessors below ([`builtin_families`], [`builtin_overloads`],
//! [`resolve_builtin`]) are a deliberate, documented extension of the design doc's §5 "no public API additions" line.
//! `docs/architecture/builtin-library.md` pins that engine registration, generated documentation, CLI help, and SDK
//! introspection all consume the same resolved records; the read-only catalog IS that pinned shared surface. The
//! executing internals (the [`dispatch`] payloads) stay private to the crate.
//!
//! Negative space: the records store no executable payloads and author no `language/v1.json` declaration manifest. The
//! deferred resolved-record fields (errors, state/host requirements, optimization metadata,
//! `JqRelation`-as-machine-field) and the declaration manifest that finally carries them land with the
//! mass-import/manifest vertical, not here.

pub mod builtins;
mod dispatch;
mod record;
mod validate;

pub use dispatch::{BuiltinDispatch, Evaluator, Lowering, dispatch};
pub use record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};

/// Sums the element counts of a slice of slices at compile time.
const fn total_len<T>(slices: &[&[T]]) -> usize {
    let mut total = 0;
    let mut i = 0;
    while i < slices.len() {
        total += slices[i].len();
        i += 1;
    }
    total
}

/// Concatenates the per-family record slices into one fixed-size array, in the module order the split pins. `filler`
/// seeds the array; every slot is then overwritten in order, so the filler never survives.
const fn concat<T: Copy, const N: usize>(slices: &[&[T]], filler: T) -> [T; N] {
    let mut out = [filler; N];
    let mut cursor = 0;
    let mut i = 0;
    while i < slices.len() {
        let mut j = 0;
        while j < slices[i].len() {
            out[cursor] = slices[i][j];
            cursor += 1;
            j += 1;
        }
        i += 1;
    }
    out
}

/// The per-family family-record slices, in split order.
/// The prelude-backed names `builtins/0` enumerates alongside the registry:
/// every definition the inlined preludes provide — the reference's own transcribed `STDLIB_PRELUDE` plus the
/// jqf-extension window family — plus the `empty/0` expression form (the reference lists it; jqf spells it as syntax,
/// not a call). Pinned against the two preludes by a compile test, so a prelude change cannot silently desync the
/// enumeration.
///
/// The collision law is the same one every jqf extension follows: a name the reference owns is never re-registered
/// here. These names are deliberately NOT registry overloads — `values`/`nulls` stay prelude defs because their law
/// is the engine's EQUALITY law, `empty` is a syntax form, and the window family is prelude source on purpose (no
/// engine work) — so the enumeration is the only surface that mentions them.
///
/// Public because `--list-builtins` (the CLI's discovery surface) must enumerate the SAME two sources the `builtins/0`
/// builtin does — one law, two doors — and the CLI is a different crate than the registry.
pub const PRELUDE_ENUMERATED: &[(&str, u8)] = &[
    ("all", 0),
    ("all", 1),
    ("all", 2),
    ("any", 0),
    ("any", 1),
    ("any", 2),
    ("counter", 1),
    ("deltas", 1),
    ("empty", 0),
    ("ewma", 2),
    ("first", 0),
    ("isempty", 1),
    ("lag", 1),
    ("last", 0),
    ("last", 1),
    ("moving_avg", 2),
    ("moving_max", 2),
    ("moving_min", 2),
    ("moving_sum", 2),
    ("nulls", 0),
    ("running", 2),
    ("values", 0),
    ("windows", 2),
    // The `~` engine-constructor family: the constructors are engine-surface syntax, not registry overloads, but the
    // collision-check duty is that `--list-builtins` and `builtins/0` carry the family — the `~` namespace is outside
    // the value namespace by construction, so the tilde stays part of the spelled name and no registry overload can
    // collide with it.
    ("~cursor", 1),
    ("~generator", 3),
    ("~inputs", 0),
    ("~rng", 1),
];

const FAMILY_SLICES: &[&[BuiltinFamilyRecord]] = &[
    builtins::core::FAMILIES,
    builtins::kinds::FAMILIES,
    builtins::control::FAMILIES,
    builtins::collection::FAMILIES,
    builtins::paths::FAMILIES,
    builtins::pointer::FAMILIES,
    #[cfg(feature = "ext-jsonpath")]
    builtins::jsonpath::FAMILIES,
    builtins::order::FAMILIES,
    builtins::top_k::FAMILIES,
    builtins::text::FAMILIES,
    builtins::format::FAMILIES,
    builtins::strings::FAMILIES,
    builtins::search::FAMILIES,
    builtins::math::FAMILIES,
    #[cfg(feature = "ext-net")]
    builtins::net::FAMILIES,
    builtins::time::FAMILIES,
    builtins::regex::FAMILIES,
    #[cfg(feature = "ext-redact")]
    builtins::redact::FAMILIES,
    #[cfg(feature = "ext-fuzzy")]
    builtins::fuzzy::FAMILIES,
    builtins::index::FAMILIES,
    builtins::rider::FAMILIES,
    builtins::process::FAMILIES,
    builtins::streams::FAMILIES,
    #[cfg(feature = "ext-hash")]
    builtins::extension::FAMILIES,
    builtins::facts::FAMILIES,
    builtins::diff::FAMILIES,
    builtins::parse::FAMILIES,
    #[cfg(feature = "ext-schema")]
    builtins::schema::FAMILIES,
    builtins::entries::FAMILIES,
    builtins::generate::FAMILIES,
    builtins::reshape::FAMILIES,
    builtins::selector::FAMILIES,
];

/// The per-family overload-record slices, in split order.
const OVERLOAD_SLICES: &[&[BuiltinOverloadRecord]] = &[
    builtins::core::OVERLOADS,
    builtins::kinds::OVERLOADS,
    builtins::control::OVERLOADS,
    builtins::collection::OVERLOADS,
    builtins::paths::OVERLOADS,
    builtins::pointer::OVERLOADS,
    #[cfg(feature = "ext-jsonpath")]
    builtins::jsonpath::OVERLOADS,
    builtins::order::OVERLOADS,
    builtins::top_k::OVERLOADS,
    builtins::text::OVERLOADS,
    builtins::format::OVERLOADS,
    builtins::strings::OVERLOADS,
    builtins::search::OVERLOADS,
    builtins::math::OVERLOADS,
    #[cfg(feature = "ext-net")]
    builtins::net::OVERLOADS,
    builtins::time::OVERLOADS,
    builtins::regex::OVERLOADS,
    #[cfg(feature = "ext-redact")]
    builtins::redact::OVERLOADS,
    #[cfg(feature = "ext-fuzzy")]
    builtins::fuzzy::OVERLOADS,
    builtins::index::OVERLOADS,
    builtins::rider::OVERLOADS,
    builtins::process::OVERLOADS,
    builtins::streams::OVERLOADS,
    #[cfg(feature = "ext-hash")]
    builtins::extension::OVERLOADS,
    builtins::facts::OVERLOADS,
    builtins::diff::OVERLOADS,
    builtins::parse::OVERLOADS,
    #[cfg(feature = "ext-schema")]
    builtins::schema::OVERLOADS,
    builtins::entries::OVERLOADS,
    builtins::generate::OVERLOADS,
    builtins::reshape::OVERLOADS,
    builtins::selector::OVERLOADS,
];

/// The registered builtin families, concatenated from the per-family slices.
static FAMILIES: [BuiltinFamilyRecord; total_len(FAMILY_SLICES)] = concat(FAMILY_SLICES, builtins::core::FAMILIES[0]);

/// The registered builtin overloads, concatenated from the per-family slices.
#[allow(
    clippy::large_const_arrays,
    reason = "the overload inventory IS a compile-time table; a runtime build would cost the \
              same bytes with no const guarantees"
)]
const OVERLOADS: [BuiltinOverloadRecord; total_len(OVERLOAD_SLICES)] =
    concat(OVERLOAD_SLICES, builtins::core::OVERLOADS[0]);

/// Compile-time integrity gate over the inventories. A duplicate id, a duplicate `(name, arity)`, or a docless
/// registration fails the build here.
const _: () = validate::validate(&FAMILIES, &OVERLOADS);

/// The read-only builtin family catalog.
#[must_use]
pub const fn builtin_families() -> &'static [BuiltinFamilyRecord] {
    &FAMILIES
}

/// The read-only builtin overload catalog.
#[must_use]
pub const fn builtin_overloads() -> &'static [BuiltinOverloadRecord] {
    &OVERLOADS
}

/// Resolves a `(name, arity)` call to its stable overload record.
///
/// Resolution is by `(canonical name, arity)`, per the compiler's resolve-before-lowering law, and returns the stable
/// record (never the source name) callers store by [`BuiltinOverloadId`]. Returns `None` when no overload matches —
/// the compiler then rejects the call with the reference's `name/arity is not defined` spelling.
#[must_use]
pub fn resolve_builtin(name: &str, arity: u8) -> Option<&'static BuiltinOverloadRecord> {
    OVERLOADS
        .iter()
        .find(|overload| overload.arity == arity && overload.canonical_name == name)
}

/// Resolves a builtin family's canonical name to its catalog record.
///
/// The CLI's `--help <builtin>` page and SDK introspection read [`BuiltinFamilyRecord::summary`] and
/// [`BuiltinFamilyRecord::detail`] through this lookup — the same registry the compiler resolves overloads from.
#[must_use]
pub fn resolve_family(name: &str) -> Option<&'static BuiltinFamilyRecord> {
    FAMILIES.iter().find(|family| family.canonical_name == name)
}

/// The demand transfer one overload id declares, for the demand-projection classifier's `Call` arm.
///
/// The lookup is by stored overload id, which is the only identity a compiled plan carries — the classifier never
/// sees a name. `None` means the id is not in this registry at all; that is unreachable by construction (the compiler
/// mints call ids only through [`resolve_builtin`]), so the classifier treats it as the conservative
/// [`DemandTransfer::Subtree`] rather than panicking on a state the type system already forbids.
///
/// Id-keyed lookups go through the const [`OVERLOAD_ID_INDEX`] position table rather than a linear scan: the classifier
/// consults this per `Call` node (and the morsel-eligibility fence consults [`effects`] the same way), so a call-heavy
/// program pays one table read instead of a walk over the whole overload inventory.
pub fn demand_transfer(overload: u16) -> Option<DemandTransfer> {
    let position = id_position(overload)?;
    Some(OVERLOADS[position].demand_transfer)
}

/// The effect class one overload id declares.
///
/// Read by the morsel-eligibility fence (the parallel relay's purity gate, consumed through `jqf_engine`'s
/// `CompiledProgram::is_morsel_eligible`): a per-record or per-shard run of an IMPURE call would observe a different
/// world than the same call under the serial drive, so an impure call declines morsel eligibility. Impure overloads
/// exist in the registry today (the input family, `env`, `now`, `stderr`, `halt`), so the fence is a LIVE filter —
/// and `None` (an id outside this registry, unreachable by construction) is treated as impure, the fail-closed
/// direction.
pub fn effects(overload: u16) -> Option<Effects> {
    let position = id_position(overload)?;
    Some(OVERLOADS[position].effects)
}

/// One registered id's position in [`OVERLOADS`], via the const index table.
///
/// `None` for an id outside the table or not in the inventory — both unreachable by construction (the compiler mints
/// ids only through [`resolve_builtin`]).
const fn id_position(overload: u16) -> Option<usize> {
    let index = overload as usize;
    // `<[T]>::get` is not const-callable on stable, so the bounds check is manual; `dispatch` stays a `const fn`
    // through this lookup.
    if index >= OVERLOAD_ID_INDEX.len() {
        return None;
    }
    let position = OVERLOAD_ID_INDEX[index];
    // `u16::MAX` is the table's unregistered marker; it can never be a real position, since positions index records and
    // `u16::MAX` far exceeds every registered id.
    if position == u16::MAX {
        None
    } else {
        Some(position as usize)
    }
}

/// Position of each registered overload id in [`OVERLOADS`], built once at compile time; `u16::MAX` marks an
/// unregistered id.
///
/// Sized by the highest registered id (plus one) so the table covers the whole id space, and the duplicate-id check in
/// [`crate::registry::validate`] guarantees the mapping is single-valued. Ids are unique but not dense or ordered, so
/// an id-keyed table is the only ordering-independent O(1) shape.
const OVERLOAD_ID_INDEX: [u16; overload_id_bound()] = build_overload_id_index();

/// One past the highest registered overload id.
const fn overload_id_bound() -> usize {
    let mut bound = 0;
    let mut i = 0;
    while i < OVERLOADS.len() {
        let id = OVERLOADS[i].id.get() as usize;
        if id >= bound {
            bound = id + 1;
        }
        i += 1;
    }
    bound
}

/// Fills the id→position table from [`OVERLOADS`].
#[allow(
    clippy::cast_possible_truncation,
    reason = "the table is sized by the overload-id bound and every id is a dense index below it, so the u16 position never truncates"
)]
const fn build_overload_id_index() -> [u16; overload_id_bound()] {
    let mut table = [u16::MAX; overload_id_bound()];
    let mut i = 0;
    while i < OVERLOADS.len() {
        table[OVERLOADS[i].id.get() as usize] = i as u16;
        i += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::{
        BuiltinExecution, DemandTransfer, Lowering, builtin_families, builtin_overloads, demand_transfer, dispatch,
        resolve_builtin,
    };

    #[test]
    fn the_registry_counts_match_the_enabled_surface() {
        // The always-on base (the six tiered families' contributions are added per feature below): 285 - (65+4+1+5+3+2)
        // families, 323 - (71+5+2+5+3+4) overloads on the default build.
        const BASE_FAMILIES: usize = 205;
        const BASE_OVERLOADS: usize = 233;
        let mut expected_families = BASE_FAMILIES;
        let mut expected_overloads = BASE_OVERLOADS;
        #[cfg(feature = "ext-hash")]
        {
            expected_families += 65;
            expected_overloads += 71;
        }
        #[cfg(feature = "ext-schema")]
        {
            expected_families += 4;
            expected_overloads += 5;
        }
        #[cfg(feature = "ext-jsonpath")]
        {
            expected_families += 1;
            expected_overloads += 2;
        }
        #[cfg(feature = "ext-net")]
        {
            expected_families += 5;
            expected_overloads += 5;
        }
        #[cfg(feature = "ext-fuzzy")]
        {
            expected_families += 3;
            expected_overloads += 3;
        }
        #[cfg(feature = "ext-redact")]
        {
            expected_families += 2;
            expected_overloads += 4;
        }
        assert_eq!(builtin_families().len(), expected_families);
        assert_eq!(builtin_overloads().len(), expected_overloads);
    }

    // The 285/323 pin is the FULL default surface; under a partial feature set
    // `the_registry_counts_match_the_enabled_surface` asserts the matching subset, so the seed-set pin runs only where
    // its numbers hold.
    #[cfg(all(
        feature = "ext-hash",
        feature = "ext-schema",
        feature = "ext-jsonpath",
        feature = "ext-net",
        feature = "ext-fuzzy",
        feature = "ext-redact"
    ))]
    #[test]
    fn inventories_hold_the_seed_set() {
        // 8 + `type/0` + the seven-strong kind-filter family (`booleans`, `numbers`, `strings`, `arrays`, `objects`,
        // `iterables`, `scalars`), which the stdlib prelude used to spell as `select(type == …)`.
        // …plus the six-family path vertical (`path`, `paths`, `getpath`, `setpath`, `delpaths`, `del`), whose
        // `paths` family owns two arities.
        // …plus the collections: eleven ordering families (`sort`, `sort_by`, `group_by`, `unique`, `unique_by`,
        // `min`, `max`, `min_by`, `max_by`, `reverse`, `bsearch`), three stringifying families (`tostring`, `tojson`,
        // `join`) and four entries families (`keys_unsorted`, `to_entries`, `from_entries`, `with_entries`), each
        // owning exactly one arity.
        // …plus the eight generator families (`range`, `while`, `until`, `repeat`, `recurse`, `combinations`, `nth`,
        // `skip`), of which `range` owns three arities, `recurse` three, `combinations` two and `nth` two.
        // …plus the eleven reshaping families (`add`, `flatten`, `transpose`, `has`, `in`, `walk`, `map_values`,
        // `pick`, `IN`, `INDEX`, `JOIN`), of which `add`, `flatten`, `IN` and `INDEX` own two arities and `JOIN` owns
        // three.
        // …plus the `format/1` and its eleven arity-0 scalar laws (`tonumber`, `toboolean`, `fromjson`, `explode`,
        // `implode`, `ascii_downcase`, `ascii_upcase`, `utf8bytelength`, `trim`, `ltrim`, `rtrim`), each owning exactly
        // one arity.
        // …plus the twelve argument-taking text families (`startswith`, `endswith`, `ltrimstr`, `rtrimstr`,
        // `trimstr`, `indices`, `index`, `rindex`, `_strindices`, `split`, `contains`, `inside`), each owning one
        // arity.
        // …plus the `_negate/0`, unary minus's value law under the name the reference gives it.
        // …plus the math stage: sixty-one families and sixty-two overloads, one per reference math builtin (`tgamma`
        // shares the `gamma` family, its alias; `lgamma_r` is its own family because its law publishes the `[log,
        // sign]` pair).
        // …plus the dates stage: eleven families and eleven overloads, one per reference date builtin
        // (`todate`/`fromdate` and the iso8601 pair are distinct names, distinct families, same laws).
        // …plus the regex stage: seven new families (`test match capture scan splits sub gsub`) with fifteen
        // overloads — `split/2` joins the the strings family's existing `split` family.
        // …plus the misc riders: `builtins`/`have_decnum`/`debug` (the debug family owns both arities), four
        // overloads.
        // …plus the two number filters (`finites`, `normals`), `have_literal_numbers`, the four host-state families
        // (`env`, `get_prog_origin`, `get_jq_origin`, `get_search_list`), the process-control families (`stderr`,
        // `halt`, which owns both `halt_error` arities), the six Bessel families, and the three streaming families
        // (`tostream`, `fromstream`, `truncate_stream`), and the four input-family laws (`input`, `inputs`,
        // `input_filename`, `input_line_number`) — twenty-two families, twenty-four overloads.
        // …plus the parity-gaps modules stage's `modulemeta` — twenty-three families, twenty-five overloads.
        // …plus the jqf extensions (set algebra, uuid, hashing/encodings, math extensions; `log/1,2` and `round/1,2`
        // extend the existing math families) — twenty-five new families, twenty-nine new overloads.
        // …plus the analytics (`sample`, `shuffle`, `fill_forward`) and the rand family (`rand/0,1`, `randint/1,2`,
        // `choice/1`) — three new families, three overloads, then three new families, five new overloads.
        // …plus the JSON-Pointer family (the `json_pointer/1,2` RFC 6901 port) — one new family, two new overloads.
        // …plus the schema family (the `schema_infer/1,2` and `schema_validate/2`/`schema_errors/2` JSON Schema
        // 2020-12 port) — three new families (one per name), three new overloads.
        // …plus the `tag/0`, the read side of the publish law — one new family, one new overload.
        // …plus the encoding-completion dispatch: `base64url_encode`, base64url_decode, percent_encode,
        // percent_decode, base32_encode, base32_decode, quoted_printable_encode, quoted_printable_decode — eight new
        // families, eight new overloads.
        // …plus the hashing-completion dispatch: `hmac_sha1`, hmac_sha512, hmac_sha1_base64url,
        // hmac_sha256_base64url, hmac_sha512_base64url, blake3, crc32 — seven new families, seven new overloads.
        // …plus the temporal-completion dispatch: `fromrfc3339` and `torfc3339` — two new families, two new
        // overloads.
        // …plus the IP/CIDR family: `ip_valid`, `ip_version`, `ip_class`, `ip_canonical`, `ip_in_cidr` — five new
        // families, five new overloads.
        // …plus the compression dispatch: gzip/deflate/ zlib compress+decompress — six new families, six new
        // overloads.
        // …plus numfmt, the printf-style number formatter — one new family, one new overload.
        // …plus the selector seam: `xpath/1` and `css/1`, the engine's two doors onto the codec-native selector
        // profiles — two new families, two new overloads.
        // …plus the redact/fuzzy families: `redact/0,1,2` and `redact_keyed/1` — two new families, four new
        // overloads — and `edit_distance/1`, `similarity/1`, `fuzzy_match/2` — three new families, three new
        // overloads.
        // …plus the JSONPath family `jsonpath/1` and `jsonpath/2` — one new family, two new overloads.
        // …plus the user-declared reusable index:
        // `declare_index/2` — one new family, one new overload.
        // …plus the value-schema vertical `schema_infer/2`, the strictness-options arity — one new overload, no new
        // family. the six parser families registered under their real ids (555-560) replace the former `"parse"` family
        // (565): +5 families.
        // `hmac_sha256/1`, the explicit hex spelling the HMAC family lacked — one new family, one new overload.
        // …plus the drift verb `schema_diff/2` — one new family, one new overload.
        // …plus the facts projection: `json_facts/0` — one new family, one new overload.
        // …plus `frequency/1`, `melt/4`, and `pivot/4` — three new families, three new overloads.
        assert_eq!(builtin_families().len(), 285);
        assert_eq!(builtin_overloads().len(), 323);
    }

    #[test]
    fn jqf_extension_names_never_collide_with_the_reference_surface() {
        // The extension families are jqf-only names. A name the reference owns is never re-registered as an extension
        // — and the standing rule for future growth: when the reference adds a builtin whose name collides with an
        // extension, the EXTENSION is renamed or namespaced, never shadowed. The registry's duplicate-name validation
        // would catch a same-name collision anyway; this test asserts the STRONGER disjointness (an extension name may
        // not share a name with ANY reference-family overload, even at a different arity — `log/1` and `round/1` are
        // the deliberate arity extensions of existing reference families and are the exception).
        let extension_names: alloc::collections::BTreeSet<&str> = builtin_families()
            .iter()
            .filter(|family| family.category == "jqf-extension")
            .map(|family| family.canonical_name)
            .collect();
        let reference_names: alloc::collections::BTreeSet<&str> = builtin_overloads()
            .iter()
            .filter(|overload| {
                builtin_families()
                    .iter()
                    .any(|family| family.id == overload.family && family.category != "jqf-extension")
            })
            .map(|overload| overload.canonical_name)
            .collect();
        for name in &extension_names {
            // `log`/`round` extend the EXISTING reference families with new arities, so their names legitimately appear
            // on both sides.
            if *name == "log" || *name == "round" {
                continue;
            }
            assert!(
                !reference_names.contains(name),
                "extension `{name}` collides with a reference-family builtin name"
            );
        }
    }

    #[test]
    fn resolution_matches_name_and_arity() {
        assert!(resolve_builtin("length", 0).is_some());
        assert!(resolve_builtin("keys", 0).is_some());
        assert!(resolve_builtin("select", 1).is_some());
        assert!(resolve_builtin("map", 1).is_some());
        assert!(resolve_builtin("not", 0).is_some());
        assert!(resolve_builtin("error", 0).is_some());
        assert!(resolve_builtin("error", 1).is_some());
        // Wrong arity or unknown name resolves to nothing.
        assert!(resolve_builtin("length", 1).is_none());
        assert!(resolve_builtin("select", 0).is_none());
        assert!(resolve_builtin("not", 1).is_none());
        assert!(resolve_builtin("error", 2).is_none());
        assert!(resolve_builtin("nonexistent", 0).is_none());
    }

    #[test]
    fn family_resolution_matches_canonical_name() {
        use super::resolve_family;

        let family = resolve_family("startswith").expect("startswith is registered");
        assert_eq!(family.canonical_name, "startswith");
        assert!(!family.summary.is_empty());
        assert!(!family.detail.is_empty());
        assert!(resolve_family("no-such-family").is_none());
    }

    #[test]
    #[allow(
        clippy::match_same_arms,
        reason = "`has/1` declares the same transfer as the zero-arity shallow readers but \
                  for a different reason — it takes an ARGUMENT, and the classifier's \
                  admission condition hangs on that — so the two arms stay apart with \
                  their own records"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "one assertion per seeded declaration: the receipt IS the table"
    )]
    fn every_registered_overload_declares_its_seeded_transfer() {
        // The seed declarations, by stored id. The `demand_transfer` field is required, so "every overload declares
        // one" is type-enforced; what this pins is WHICH one, since a declaration is the classifier's law.
        for overload in builtin_overloads() {
            let declared = demand_transfer(overload.id.get()).expect("registered id resolves");
            assert_eq!(declared, overload.demand_transfer);
            let expected = match (overload.canonical_name, overload.arity) {
                ("length", 0) => DemandTransfer::CountOfConstructedInput,
                ("select", 1) => DemandTransfer::ConditionUnionPassThrough,
                ("not", 0) => DemandTransfer::InputPassThrough,
                // Every lowering declares `ViaLowering`: the nodes it expands into carry the demand, so the call itself
                // never reaches the classifier. `first`/`limit` expand to the reference definitions.
                // `paths`/`del` likewise expand to the reference definitions, which are written in terms of `path`,
                // `select`, and `delpaths`.
                // The collection idioms expand to the reference definitions too, and two of them ("INDEX", "JOIN")
                // expand to a REBOUND spelling of it:
                // the reference indexes by an arbitrary expression where jqf indexes by a variable slot, so the key is
                // bound first. The rebinding is a spelling change and not a semantic one, which is what keeps the
                // declaration `ViaLowering` all the same. The strings family's `index`/`rindex` expand to the reference
                // definitions over `indices` plus a real index or slice step, so their demand is the expansion's too.
                (
                    "map" | "first" | "del" | "paths" | "join" | "with_entries" | "nth" | "add"
                    | "in" | "map_values" | "pick" | "IN" | "INDEX" | "index" | "rindex" | "inside",
                    1,
                )
                | ("limit" | "setpath" | "nth" | "skip" | "IN" | "INDEX" | "JOIN", 2)
                | ("paths" | "add", 0)
                | ("JOIN", 3 | 4) => match overload.execution {
                    BuiltinExecution::Lowering => DemandTransfer::ViaLowering,
                    BuiltinExecution::Evaluator
                    | BuiltinExecution::Definition
                    | BuiltinExecution::Operator => DemandTransfer::Subtree,
                },
                // `type`, `keys`, `tag` and `has` read only the value's kind, its member identities, and the one
                // intrinsic tag fact — no payload below them. They join the conservative default: the per-element
                // lattice has no shallower rung for an element's own kind and keys, so `Subtree` is the honest demand
                // in both worlds, and the codec's whole-document lazy binding answers them without materializing the
                // payload.
                ("keys" | "keys_unsorted" | "type" | "tag", 0) => DemandTransfer::Subtree,
                // `has/1` is the same claim with an ARGUMENT: it reads the container's kind and its member identities
                // and never a payload below them.
                ("has", 1) => DemandTransfer::Subtree,
                // `format/1` renders the WHOLE input — eight of its ten transforms stringify it first, and the other
                // two read every cell of it — so no shallower claim is honest. Its argument is a name and not a
                // reader, but the declaration is about the INPUT demand, and that demand is the subtree.
                ("format", 1) => DemandTransfer::Subtree,
                // Each arity-0 scalar law is a function of the WHOLE input:
                // nine of the eleven read every byte or every cell of it, and the two that answer from a pass-through
                // (`tonumber` on a number, `toboolean` on a boolean) still have to read the input's payload to know
                // that is what they are looking at.
                (
                    "tonumber" | "toboolean" | "fromjson" | "explode" | "implode"
                    | "ascii_downcase" | "ascii_upcase" | "utf8bytelength" | "trim" | "ltrim"
                    | "rtrim",
                    0,
                ) => DemandTransfer::Subtree,
                // The five argument-taking text laws read the whole input string and the whole argument string. An
                // upstream path narrows the class to that path's field, as it does for every text builtin.
                ("startswith" | "endswith" | "ltrimstr" | "rtrimstr" | "trimstr", 1) => {
                    DemandTransfer::Subtree
                }
                // A search reads the whole input — every codepoint of a string or every cell of an array — and
                // compares it against the whole argument, and `split` rebuilds the whole input as pieces.
                // `index`/`rindex` are SEARCHES with their own Subtree rows; the `indices`-class laws read the whole
                // input against the whole argument, and a lowering declares its transfer through its expansion.
                ("indices" | "_strindices" | "split", 1) => DemandTransfer::Subtree,
                // Unary minus reads the input's number PAYLOAD — its digits, its scale and its sign — and
                // republishes it re-signed, so the shallow fact its refusal keys off is not the whole demand.
                ("_negate", 0) => DemandTransfer::Subtree,
                // The containment relation walks BOTH operands to whatever depth the argument reaches, so nothing
                // shallower is honest.
                ("contains", 1) => DemandTransfer::Subtree,
                // A kind filter asks `type`'s own question but then passes the ADMITTED input through whole, so its
                // demand is the whole subtree, not the shallow fact.
                // ...and so is `error`, and every ordering, stringifying and entries overload: each publishes a value
                // rebuilt from, or a diagnostic naming, the WHOLE input. A path builtin's argument may navigate
                // arbitrarily deep and `..` visits the whole document, so no shallower transfer is honest for those
                // either.
                // A generator reads nothing of the input for `range`, but its ARGUMENTS are ordinary filters over that
                // same input and `recurse`/`combinations` walk it whole, so the honest declaration for the family is
                // the conservative one.
                (
                    "booleans" | "numbers" | "strings" | "arrays" | "objects" | "iterables"
                    | "scalars" | "error" | "sort" | "unique" | "min" | "max" | "reverse"
                    | "tostring" | "tojson" | "to_entries" | "from_entries" | "recurse"
                    | "combinations" | "flatten" | "transpose",
                    0,
                )
                | (
                    "error" | "sort_by" | "group_by" | "unique_by" | "min_by" | "max_by"
                    | "bsearch" | "path" | "getpath" | "delpaths" | "range" | "repeat" | "recurse"
                    | "combinations" | "flatten" | "walk",
                    1,
                )
                | ("while" | "until" | "recurse", 2)
                | ("range", 2 | 3) => DemandTransfer::Subtree,
                // top_k reads the whole input to produce the partial sort.
                ("top_k", 1..=4) => DemandTransfer::Subtree,
                // The scalar-tails math stage: every math overload is a pure function of its operand VALUES — a /0
                // form reads the whole piped number, a /2 or /3 form reads every byte of every argument filter's output
                // — so `Subtree` is the only honest declaration. The `isnan` quartet answers from the number's bits,
                // `nan`/`infinite` publish constants, and none of them can promise a shallower read of the piped input.
                (
                    "abs" | "fabs" | "floor" | "ceil" | "round" | "trunc" | "rint" | "nearbyint"
                    | "sqrt" | "cbrt" | "exp" | "expm1" | "exp2" | "exp10" | "log" | "log1p"
                    | "log2" | "log10" | "erf" | "erfc" | "sin" | "cos" | "tan" | "sinh" | "cosh"
                    | "tanh" | "asin" | "acos" | "atan" | "asinh" | "acosh" | "atanh" | "gamma"
                    | "tgamma" | "lgamma" | "lgamma_r" | "significand" | "logb" | "frexp" | "modf"
                    | "nan" | "infinite" | "isnan" | "isinfinite" | "isfinite" | "isnormal"
                    | "hypot" | "pow" | "atan2" | "fmod" | "copysign" | "remainder" | "drem"
                    | "fdim" | "fmin" | "fmax" | "ldexp" | "scalbln" | "scalb" | "nexttoward"
                    | "nextafter" | "fma" | "j0" | "j1" | "jn" | "y0" | "y1" | "yn",
                    0 | 2 | 3,
                ) => DemandTransfer::Subtree,
                // The dates stage: every date law reads the whole piped value (a timestamp number, a parsed-datetime
                // array, or a date string) or every byte of its format argument, so `Subtree` is the only honest
                // declaration. `now` publishes the wall clock and reads nothing, but its declared transfer stays the
                // conservative whole-document one — coarser is always sound.
                (
                    "now" | "gmtime" | "localtime" | "mktime" | "todate" | "fromdate"
                    | "todateiso8601" | "fromdateiso8601" | "fromrfc3339" | "torfc3339"
                    | "strftime" | "strflocaltime" | "strptime",
                    0 | 1,
                ) => DemandTransfer::Subtree,
                // The regex stage: every law reads the whole input string and the whole pattern/flags arguments (and
                // `sub`/`gsub` read every capture of every match), so `Subtree` is the only honest declaration.
                (
                    "test" | "match" | "capture" | "scan" | "splits" | "split" | "sub" | "gsub",
                    1..=3,
                ) => DemandTransfer::Subtree,
                // The misc riders: `builtins` enumerates the whole registry, `have_decnum` answers the number model's
                // own fact, and `debug` passes the whole piped value through — `Subtree` is sound for all of them.
                ("builtins" | "have_decnum" | "debug", 0 | 1) => DemandTransfer::Subtree,
                // The parity-gaps number filters decide from the value's bits, `have_literal_numbers` publishes a
                // constant, and the host-state laws read the injected snapshot — all whole-value (or whole-program)
                // reads, so `Subtree` is the only honest declaration.
                (
                    "finites"
                    | "normals"
                    | "have_literal_numbers"
                    | "env"
                    | "get_prog_origin"
                    | "get_jq_origin"
                    | "get_search_list",
                    0,
                ) => DemandTransfer::Subtree,
                // The parity-gaps host-state laws: `stderr` passes the whole piped value through, and the halt laws
                // read the whole input (or argument) before terminating — `Subtree` throughout.
                ("stderr" | "halt" | "halt_error", 0 | 1) => DemandTransfer::Subtree,
                // The parity-gaps streaming laws: `tostream` walks the whole piped value, and
                // `fromstream`/`truncate_stream` read the whole input AND every byte of their argument stream's outputs
                // — `Subtree` is the only honest declaration.
                ("tostream" | "fromstream" | "truncate_stream", 0 | 1) => DemandTransfer::Subtree,
                // The parity-gaps input family: every law reads the whole current input (or pulls the whole next one)
                // — `Subtree`.
                ("input" | "inputs" | "input_filename" | "input_line_number", 0) => {
                    DemandTransfer::Subtree
                }
                ("modulemeta", 0) => DemandTransfer::Subtree,
                // The jqf extension families: every law reads the whole input (or every byte of its filter arguments'
                // first outputs), so `Subtree` is the only honest declaration.
                (
                    "union"
                    | "intersect"
                    | "except"
                    | "uuid"
                    | "uuid_v4"
                    | "uuid_v7"
                    | "md5"
                    | "sha1"
                    | "sha256"
                    | "sha512"
                    | "xxhash"
                    | "hex_encode"
                    | "hex_decode"
                    | "base64_encode"
                    | "base64_decode"
                    | "base64url_encode"
                    | "base64url_decode"
                    | "percent_encode"
                    | "percent_decode"
                    | "base32_encode"
                    | "base32_decode"
                    | "quoted_printable_encode"
                    | "quoted_printable_decode"
                    | "hmac_sha1"
                    | "hmac_sha256"
                    | "hmac_sha512"
                    | "hmac_sha1_base64url"
                    | "hmac_sha256_base64url"
                    | "hmac_sha512_base64url"
                    | "blake3"
                    | "crc32"
                    | "gzip_compress"
                    | "gzip_decompress"
                    | "deflate_compress"
                    | "deflate_decompress"
                    | "zlib_compress"
                    | "zlib_decompress"
                    | "numfmt"
                    | "e"
                    | "pi"
                    | "tau"
                    | "degrees"
                    | "radians"
                    | "pow10"
                    | "recip"
                    | "round_even"
                    | "signum"
                    | "fract"
                    | "log"
                    | "round"
                    | "sum"
                    | "avg"
                    | "median"
                    | "quantile"
                    | "stddev"
                    | "variance"
                    | "count"
                    | "frequency"
                    | "parse_url"
                    | "parse_query_string"
                    | "parse_logfmt"
                    | "parse_syslog"
                    | "parse_user_agent"
                    | "parse_grok"
                    | "diff"
                    | "sample"
                    | "shuffle"
                    | "fill_forward"
                    | "hmac"
                    | "rand"
                    | "randint"
                    | "choice"
                    | "json_pointer"
                    | "jsonpath"
                    | "schema_infer"
                    | "schema_validate"
                    | "schema_errors"
                    // The drift verb reads the whole value AND the whole schema — same class as the rest of the
                    // schema family.
                    | "schema_diff"
                    | "ip_valid"
                    | "ip_version"
                    | "ip_class"
                    | "ip_canonical"
                    | "ip_in_cidr"
                    // The selector seam's two doors read the WHOLE document (a profile matches elements anywhere in the
                    // recovered tree), so their transfer is the subtree, never a shallower claim.
                    | "xpath"
                    | "css"
                    // The redact/fuzzy familiesevery law reads the whole piped value (or every byte of its
                    // pattern/marker/key/other/threshold arguments), so `Subtree` is the only honest declaration.
                    | "redact"
                    | "redact_keyed"
                    | "edit_distance"
                    | "similarity"
                    | "fuzzy_match"
                    // The user-declared reusable index:
                    // the declaration builds its keyed multimap over a container reached by a static path from the
                    // input's ROOT, which the classifier cannot see from the filter arguments — Subtree is the only
                    // honest declaration.
                    | "declare_index"
                    // The facts projection reads the whole input value and every attached fact — Subtree,
                    // conservatively.
                    | "json_facts",
                    0..=2,
                ) => DemandTransfer::Subtree,
                ("melt" | "pivot", 4) => DemandTransfer::Subtree,
                (name, arity) => panic!("overload {name}/{arity} has no seeded transfer"),
            };
            assert_eq!(
                declared, expected,
                "overload {}/{} declares the wrong transfer",
                overload.canonical_name, overload.arity
            );
            // The cross-field rule the const validation asserts, re-read here as a runtime fact over the ACTUAL
            // inventory.
            assert_eq!(
                overload.execution == BuiltinExecution::Lowering,
                declared == DemandTransfer::ViaLowering
            );
        }
    }

    #[test]
    fn an_unregistered_id_has_no_transfer() {
        assert!(demand_transfer(u16::MAX).is_none());
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one exhaustive table-alignment walk over every registry family; splitting the assertions apart would hide the pairwise law they pin together"
    )]
    fn migrated_payload_slices_match_their_overload_records() {
        // The const coverage walk proves pairwise alignment; this pins the slice LENGTHS as a readable runtime fact
        // beside it.
        assert_eq!(
            super::builtins::core::PAYLOADS.len(),
            super::builtins::core::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::math::PAYLOADS.len(),
            super::builtins::math::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::kinds::PAYLOADS.len(),
            super::builtins::kinds::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::pointer::PAYLOADS.len(),
            super::builtins::pointer::OVERLOADS.len()
        );
        #[cfg(feature = "ext-jsonpath")]
        assert_eq!(
            super::builtins::jsonpath::PAYLOADS.len(),
            super::builtins::jsonpath::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::top_k::PAYLOADS.len(),
            super::builtins::top_k::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::strings::PAYLOADS.len(),
            super::builtins::strings::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::regex::PAYLOADS.len(),
            super::builtins::regex::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::streams::PAYLOADS.len(),
            super::builtins::streams::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::parse::PAYLOADS.len(),
            super::builtins::parse::OVERLOADS.len()
        );
        #[cfg(feature = "ext-net")]
        assert_eq!(
            super::builtins::net::PAYLOADS.len(),
            super::builtins::net::OVERLOADS.len()
        );
        #[cfg(feature = "ext-schema")]
        assert_eq!(
            super::builtins::schema::PAYLOADS.len(),
            super::builtins::schema::OVERLOADS.len()
        );
        #[cfg(feature = "ext-hash")]
        assert_eq!(
            super::builtins::extension::PAYLOADS.len(),
            super::builtins::extension::OVERLOADS.len()
        );
        #[cfg(feature = "ext-redact")]
        assert_eq!(
            super::builtins::redact::PAYLOADS.len(),
            super::builtins::redact::OVERLOADS.len()
        );
        #[cfg(feature = "ext-fuzzy")]
        assert_eq!(
            super::builtins::fuzzy::PAYLOADS.len(),
            super::builtins::fuzzy::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::time::PAYLOADS.len(),
            super::builtins::time::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::process::PAYLOADS.len(),
            super::builtins::process::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::rider::PAYLOADS.len(),
            super::builtins::rider::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::control::PAYLOADS.len(),
            super::builtins::control::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::collection::PAYLOADS.len(),
            super::builtins::collection::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::paths::PAYLOADS.len(),
            super::builtins::paths::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::order::PAYLOADS.len(),
            super::builtins::order::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::text::PAYLOADS.len(),
            super::builtins::text::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::format::PAYLOADS.len(),
            super::builtins::format::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::search::PAYLOADS.len(),
            super::builtins::search::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::index::PAYLOADS.len(),
            super::builtins::index::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::facts::PAYLOADS.len(),
            super::builtins::facts::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::diff::PAYLOADS.len(),
            super::builtins::diff::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::entries::PAYLOADS.len(),
            super::builtins::entries::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::generate::PAYLOADS.len(),
            super::builtins::generate::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::reshape::PAYLOADS.len(),
            super::builtins::reshape::OVERLOADS.len()
        );
        assert_eq!(
            super::builtins::selector::PAYLOADS.len(),
            super::builtins::selector::OVERLOADS.len()
        );
    }

    #[test]
    fn every_overload_dispatches_to_its_execution_kind() {
        for overload in builtin_overloads() {
            match (overload.execution, dispatch(overload.id)) {
                (BuiltinExecution::Evaluator, Some(super::BuiltinDispatch::Evaluator(_)))
                | (
                    BuiltinExecution::Lowering,
                    Some(super::BuiltinDispatch::Lowering(
                        Lowering::Map
                        | Lowering::First
                        | Lowering::Limit
                        | Lowering::PathsFiltered
                        | Lowering::Del
                        | Lowering::WithEntries
                        | Lowering::NthIndex
                        | Lowering::Nth
                        | Lowering::Skip
                        | Lowering::Add
                        | Lowering::In
                        | Lowering::Pick
                        | Lowering::InStream
                        | Lowering::Index
                        | Lowering::JoinIndexed
                        | Lowering::Inside,
                    )),
                ) => {}
                other => panic!(
                    "overload {} has no matching dispatch: {other:?}",
                    overload.canonical_name
                ),
            }
        }
    }
}
