//! Strict RFC 8259 decoder registration and source-backed access provider.
//!
//! `no_std` + `alloc`. Depends on `jqf-codec-core`, `jqf-source`, `jqf-resource`, and `jqf-data`. JSONC, JSON5, NDJSON,
//! and json-seq live in sibling modules. The encode side renders under [`JsonEncodeOptions`]; [`json_escape_byte`] /
//! [`push_json_escaped`] are public. Compact-framed and prefixed encoder factories stay crate-private.

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use closed structured codec errors"
)]

extern crate alloc;

mod byte_scan;
mod decode;
mod edit;
mod encode;
mod encode_cursor;
mod error;
mod json_escape;
mod lazy;
mod lex;
mod options;
mod parse;
mod provider;
mod record_diag;
mod record_route;
mod registration;
mod routes;
mod scoped;
mod storage;
mod tag;

pub mod json5;
pub mod jsonc;
pub mod ndjson;
pub mod seq;

#[cfg(test)]
/// Shared helpers for unit and integration tests. Integration tests reach this file through `include!`, never through a
/// crate-root import (a cfg(test) item does not exist in another compilation unit), so plain module visibility is all
/// any consumer can ever use.
mod test_support;

pub(crate) mod edit_support {
    //! The key-walk primitives shared by the strict-JSON and commented-JSON edit seams: one rewind-to-colon law, one
    //! escape-parity quote walk.

    use crate::byte_scan::is_json_ws;

    /// Walks back from `value_start` over whitespace, one `:`, and whitespace again, never passing below `floor`.
    /// Returns the cursor ON the key's last byte (its closing quote or identifier tail), or `None` when no colon
    /// precedes within `floor`.
    pub(crate) fn rewind_to_key(source: &[u8], value_start: usize, floor: usize) -> Option<usize> {
        let mut p = value_start;
        while p > floor && is_json_ws(source[p - 1]) {
            p -= 1;
        }
        if p == floor || source[p - 1] != b':' {
            return None;
        }
        p -= 1;
        while p > floor && is_json_ws(source[p - 1]) {
            p -= 1;
        }
        Some(p)
    }

    /// Walks backward from just below `close` — the rewind cursor naming the key's closing-quote position exclusively
    /// — to the matching opening `quote`. A quote byte preceded by an EVEN run of backslashes is structural; an odd
    /// run is an escaped literal inside the string. `None` when the walk reaches `floor` first.
    pub(crate) fn opening_quote_left(source: &[u8], close: usize, quote: u8, floor: usize) -> Option<usize> {
        let mut p = close.saturating_sub(1);
        while p > floor {
            if source[p - 1] != quote {
                p -= 1;
                continue;
            }
            let mut run = 0;
            while p >= run + 2 && source[p - 2 - run] == b'\\' {
                run += 1;
            }
            if run % 2 == 0 {
                return Some(p - 1);
            }
            p -= 1 + run;
        }
        None
    }
}

use jqf_codec_core::{CodecError, CodecFailureKind, EncodeRequest};

pub use json_escape::{json_escape_byte, push_json_escaped};
pub use options::{JsonEncodeOptions, JsonIndent, VALUE_SEPARATORS};

pub use registration::{FORMAT_ID, RFC8259_DIALECT_ID, registration};
// The stable physical route identities are re-exported at the root so receipts and consumers keep the historic
// `crate::FULL_PHYSICAL_ROUTE_ID` spelling; the definitions live in [`routes`]. Enumerated explicitly (the module holds
// exactly these three public consts) so a new route identity joins this surface deliberately, never by glob.
pub use routes::{ENCODE_PHYSICAL_ROUTE_ID, FULL_PHYSICAL_ROUTE_ID, SCOPED_PHYSICAL_ROUTE_ID};

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

/// Checks that a request names THIS codec's target.
///
/// Options are deliberately not inspected here. This check once also rejected any request carrying options, which was
/// how the codec said "I have none"; now that encoding takes an indent, the option channel is owned by
/// [`encode::JsonEncodeOptions`], which rejects a value of the wrong type under this codec's schema identity.
fn validate_target(request: EncodeRequest<'_, '_>) -> Result<(), CodecError> {
    if request.format.as_str() != FORMAT_ID || request.dialect.as_str() != RFC8259_DIALECT_ID {
        Err(CodecError::new(CodecFailureKind::RequirementMismatch))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FORMAT_ID, FULL_PHYSICAL_ROUTE_ID, RFC8259_DIALECT_ID, registration};
    use alloc::string::String;
    use alloc::vec::Vec;
    use jqf_codec_core::{
        AccessAdapter, AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, AccessResult, CodecDemand,
        CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, ErasedAccessSession, ExactPath,
        ExactSelectionRecord, ReusableAccessSession, ValidationMode,
    };
    use jqf_data::{
        DiagnosticCoverage, DialectId, DocumentCapability, DocumentCapabilityFamily, NodeHandle, NumberView,
        ScalarView, ValueKind,
    };
    use jqf_resource::{RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    use crate::test_support;

    #[derive(Clone, Copy)]
    enum Step<'a> {
        Member(&'a str),
        Index(i64),
    }

    /// An unlimited ledger for a materialization used only as a CORRECTNESS witness, in a test whose own account is
    /// deliberately capped or measured. Materializing charges the owned value's payload now, so a witness billed to the
    /// account under test would change what that test measures.
    fn witness_ledger() -> ResourceContext<'static> {
        test_support::resources()
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(91), SourceKind::Input),
            "test.json",
            bytes,
            0,
        )
    }

    fn demand(resources: &ResourceContext<'_>) -> CodecDemand {
        semantic_demand(resources)
    }

    fn semantic_demand(resources: &ResourceContext<'_>) -> CodecDemand {
        let mut demand = CodecDemand::try_new(resources);
        demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
        demand.try_insert(&DemandClause::ValueShape).expect("value shape");
        demand
    }

    fn requirement(
        path: Option<&[Step<'_>]>,
        diagnostics: DiagnosticPolicy,
        resources: &ResourceContext<'_>,
    ) -> AccessRequirement {
        let guarantees = AccessGuarantees::strict(diagnostics);
        if let Some(path) = path {
            let mut exact = ExactPath::try_new(resources);
            for step in path {
                match step {
                    Step::Member(member) => exact.try_push_semantic_member(member, resources).expect("member"),
                    // Infallible by signature (unlike the member arm's fallible push): an index step allocates nothing.
                    Step::Index(index) => exact.try_push_semantic_index(*index, resources),
                }
            }
            let footprint = AccessFootprint::try_exact(exact, resources);
            AccessRequirement::try_exact(footprint, demand(resources), guarantees, resources)
                .expect("exact requirement")
        } else {
            AccessRequirement::try_whole(demand(resources), guarantees, resources).expect("whole requirement")
        }
    }

    fn run<'source>(
        bytes: &'source [u8],
        path: Option<&[Step<'_>]>,
        diagnostics: DiagnosticPolicy,
    ) -> Result<AccessResult<'source>, jqf_codec_core::CodecError> {
        let mut resources = test_support::resources();
        let registration = registration().expect("registration");
        let mut provider = registration.decoder().expect("decoder").create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics,
                dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: crate::VALUE_SEPARATORS,
            },
            &mut resources,
        )?;
        let requirement = requirement(path, diagnostics, &resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources)?;
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)
    }

    /// The canonicality verdict belongs to ONE document. It only ever falls, so a recycled state that kept it would
    /// answer "not canonical" for every later document and silently retire the byte-identity echo for the rest of a
    /// stream.
    #[test]
    fn a_recycled_state_re_earns_the_canonicality_verdict_per_document() {
        use jqf_codec_core::{AccessInput, AccessSession as _};

        let mut resources = test_support::resources();
        let mut state = crate::parse::JsonParseState::new(
            DiagnosticCoverage::NotRequested,
            jqf_data::BuilderCoverage::minimal_semantic(),
            crate::storage::ParseMode::Document,
        );
        let canonical_of =
            |state: &mut crate::parse::JsonParseState, resources: &mut ResourceContext<'_>, bytes: &'static [u8]| {
                let mut run = CodecRunContext::new(resources);
                run.set_cooperative_credits(4_096);
                let result = state
                    .decode(AccessInput::Source(source(bytes)), &mut run)
                    .expect("parse");
                let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
                    panic!("expected a full document")
                };
                product.document().source_canonical()
            };

        // `\/` is a non-minimal escape: the render is `/`, so the source is not its own answer.
        assert!(
            !canonical_of(&mut state, &mut resources, b"[\"a\\/b\"]"),
            "a non-minimal escape must clear the verdict"
        );
        state.reset(
            DiagnosticCoverage::NotRequested,
            jqf_data::BuilderCoverage::minimal_semantic(),
            crate::storage::ParseMode::Document,
        );
        assert!(
            canonical_of(&mut state, &mut resources, b"[1]"),
            "the next document must earn its own verdict"
        );
    }

    /// Parses `bytes` as one complete document, optionally with the on-demand container-span frontier forced on at
    /// `frontier` frames deep, returning the materialized root and how many spans the decode deferred.
    ///
    /// This drives the parse state DIRECTLY rather than through the process switch: the switch is a dark-launch
    /// mechanism for the CLI differential, and a test that flipped it would race every other test in this binary.
    fn parse_with_frontier(bytes: &[u8], frontier: Option<usize>) -> (alloc::string::String, u32) {
        use jqf_codec_core::{AccessInput, AccessSession as _};

        let mut resources = test_support::resources();
        let mut state = crate::parse::JsonParseState::new(
            DiagnosticCoverage::NotRequested,
            jqf_data::BuilderCoverage::minimal_semantic(),
            crate::storage::ParseMode::Document,
        );
        if let Some(depth) = frontier {
            state.enable_lazy_frontier(depth);
        }
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = state
            .decode(AccessInput::Source(source(bytes)), &mut run)
            .expect("parse");
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("expected a full document")
        };
        let spans = product.document().container_span_count();
        let value = product
            .document()
            .materialize_root(&mut resources)
            .expect("materialize");
        (alloc::format!("{value:?}"), spans)
    }

    /// Parses `bytes` as one complete document with the prune hint armed, returning the materialized root's rendering.
    fn parse_with_prune(bytes: &[u8], tree: &jqf_codec_core::PruneTree) -> alloc::string::String {
        use jqf_codec_core::{AccessInput, AccessSession as _};

        let mut resources = test_support::resources();
        let mut state = crate::parse::JsonParseState::new(
            DiagnosticCoverage::NotRequested,
            jqf_data::BuilderCoverage::minimal_semantic(),
            crate::storage::ParseMode::Document,
        );
        state.enable_prune(tree);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = state
            .decode(AccessInput::Source(source(bytes)), &mut run)
            .expect("parse");
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("expected a full document")
        };
        let value = product
            .document()
            .materialize_root(&mut resources)
            .expect("materialize");
        alloc::format!("{value:?}")
    }

    /// The prune hint's core claim: a document decoded with the hint materializes to exactly what an eager decode of
    /// the HAND-PRUNED document produces — unobservable members gone, kept members verbatim, array element counts
    /// intact (spine-only containers survive empty).
    #[test]
    fn a_pruned_parse_omits_unobservable_members_and_keeps_the_rest() {
        let resources = test_support::resources();
        let mut tree = jqf_codec_core::PruneTree::try_new(&resources).expect("tree");
        // Keep `.users[].id` whole and `.orders` as an iterated spine, the shape the engine emits for a
        // navigated-but-unread array: {orders: {[]: {}}, users: {[]: {id: *}}}.
        let keep_all = tree.try_push_node(true).expect("id subtree");
        let per_element = tree.try_push_node(false).expect("element");
        tree.try_push_key(per_element, "id", keep_all).expect("id key");
        let users = tree.try_push_node(false).expect("users");
        tree.try_set_element(users, per_element).expect("element");
        let spine = tree.try_push_node(false).expect("spine");
        let orders = tree.try_push_node(false).expect("orders");
        tree.try_set_element(orders, spine).expect("orders element");
        tree.try_push_key(jqf_codec_core::PruneTree::ROOT, "orders", orders)
            .expect("orders key");
        tree.try_push_key(jqf_codec_core::PruneTree::ROOT, "users", users)
            .expect("users key");

        let document = br#"{"meta":{"version":3,"deep":[[1],[2]]},
            "orders":[{"sku":"a","qty":2},{"sku":"b","qty":1}],
            "users":[{"id":1,"name":"ann","tags":["x","y"]},{"id":2,"name":"bo"}]}"#;
        let pruned = parse_with_prune(document, &tree);
        let (hand_pruned, _) = parse_with_frontier(br#"{"orders":[{},{}],"users":[{"id":1},{"id":2}]}"#, None);
        assert_eq!(pruned, hand_pruned);
    }

    #[test]
    fn a_forced_container_span_frontier_materializes_the_eager_value() {
        for (bytes, expected_spans) in [
            (br#"{"a":[1,2,{"b":"x"}],"c":{"d":[true,null]}}"#.as_slice(), 2),
            (br#"[[],{},[[[]]],{"k":{"j":[]}}]"#.as_slice(), 4),
            (
                b"{\"e\":\"\xc3\xa9\\n\",\"n\":[1e3,-0.5,12345678901234567890]}".as_slice(),
                1,
            ),
            (br#"[{"dup":1,"dup":2}]"#.as_slice(), 1),
            // Nothing nested: the frontier is on and defers NOTHING, which must still publish the identical document.
            (br#"{"only":"scalar"}"#.as_slice(), 0),
            (b"[]".as_slice(), 0),
            (b"7".as_slice(), 0),
        ] {
            let (eager, eager_spans) = parse_with_frontier(bytes, None);
            let (lazy, lazy_spans) = parse_with_frontier(bytes, Some(1));
            assert_eq!(eager_spans, 0, "an eager parse never defers");
            assert_eq!(
                lazy_spans,
                expected_spans,
                "frontier engagement over {}",
                core::str::from_utf8(bytes).expect("utf8"),
            );
            assert_eq!(
                eager,
                lazy,
                "frontier changed the value of {}",
                core::str::from_utf8(bytes).expect("utf8"),
            );
        }
    }

    /// Parses `bytes` as ONE COPY-MODE value, asking for the frontier at `frontier` frames deep and optionally arming
    /// it in a span-retaining mode FIRST so the recycle-mode reset has something to disarm.
    ///
    /// Returns the materialized root and how many spans the published document holds, which for a copy-mode run must
    /// always be zero.
    fn parse_owned_run_with_frontier(
        bytes: &[u8],
        frontier: usize,
        arm_before_reset: bool,
    ) -> (alloc::string::String, u32) {
        let mut resources = test_support::resources();
        let mut state = if arm_before_reset {
            let mut armed = crate::parse::JsonParseState::new(
                DiagnosticCoverage::NotRequested,
                jqf_data::BuilderCoverage::minimal_semantic(),
                crate::storage::ParseMode::Document,
            );
            armed.enable_lazy_frontier(frontier);
            armed.reset(
                DiagnosticCoverage::NotRequested,
                jqf_data::BuilderCoverage::minimal_semantic(),
                crate::storage::ParseMode::OwnedRun,
            );
            armed
        } else {
            crate::parse::JsonParseState::new(
                DiagnosticCoverage::NotRequested,
                jqf_data::BuilderCoverage::minimal_semantic(),
                crate::storage::ParseMode::OwnedRun,
            )
        };
        state.enable_lazy_frontier(frontier);
        for _ in 0..16_384 {
            let mut run = CodecRunContext::new(&mut resources);
            match state.poll_owned(source(bytes), &mut run).expect("parse") {
                crate::parse::OwnedRunPoll::Ready(product) => {
                    let spans = product.document().container_span_count();
                    let value = product
                        .document()
                        .materialize_root(&mut resources)
                        .expect("materialize");
                    return (alloc::format!("{value:?}"), spans);
                }
                crate::parse::OwnedRunPoll::Pending => {
                    resources.try_begin_next_cooperative_entry(4_096).expect("resume");
                }
            }
        }
        panic!("owned-run frontier parse exceeded bounded poll count")
    }

    /// The soundness constraint, in code: a lazy span document may never be published out of a copy-mode session.
    ///
    /// `ParseMode::OwnedRun` exists because the published document must OUTLIVE the buffer it was decoded from — the
    /// member-scoped route materializes out of a buffer its session recycles, and record-parallel NDJSON and
    /// adjacent-value shards rely on that contract. A container span into such a buffer could never be read back, so
    /// asking for the frontier there must be REFUSED rather than honored, and refusal has to mean an eager document
    /// rather than an error: the caller asked for a document, and it gets the one it would have got without the switch.
    ///
    /// The positive control is the third assertion. Without it the test would pass just as well on bytes that hold
    /// nothing deferrable, which would prove nothing at all.
    #[test]
    fn a_copy_mode_session_refuses_the_frontier_and_publishes_no_span() {
        for bytes in [
            br#"{"a":[1,2,{"b":"x"}],"c":{"d":[true,null]}}"#.as_slice(),
            br#"[[],{},[[[]]],{"k":{"j":[]}}]"#.as_slice(),
            br#"[{"dup":1,"dup":2}]"#.as_slice(),
        ] {
            let label = core::str::from_utf8(bytes).expect("utf8");
            let (eager, eager_spans) = parse_with_frontier(bytes, None);
            assert_eq!(eager_spans, 0, "an eager parse never defers");

            let (fresh, fresh_spans) = parse_owned_run_with_frontier(bytes, 1, false);
            assert_eq!(fresh_spans, 0, "copy mode committed a span over {label}");
            assert_eq!(fresh, eager, "copy mode changed the value of {label}");

            // A session that was armed in a span-retaining mode and RECYCLED into copy mode must not inherit permission
            // to name spans of the buffer it is about to reuse.
            let (recycled, recycled_spans) = parse_owned_run_with_frontier(bytes, 1, true);
            assert_eq!(
                recycled_spans, 0,
                "a recycled session kept its frontier into copy mode over {label}"
            );
            assert_eq!(
                recycled, eager,
                "a recycled copy-mode session changed the value of {label}"
            );

            // The positive control: these exact bytes DO defer when the mode can retain spans, so the two zeroes above
            // are a refusal and not an absence of anything to refuse.
            let (_, span_retaining) = parse_with_frontier(bytes, Some(1));
            assert!(
                span_retaining > 0,
                "the control case deferred nothing over {label}, so the refusal is vacuous"
            );
        }
    }

    #[test]
    fn low_credit_scan_runs_to_completion_under_the_replaced_meter() {
        let bytes = alloc::format!("\"{}\"", "x".repeat(298));
        let mut resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64)).expect("account"),
            &test_support::CONTROL,
            WorkMeter::try_new_v1(2).expect("work"),
        )
        .expect("resources");
        let registration = registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes.as_bytes()),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = requirement(None, DiagnosticPolicy::ErrorsOnly, &resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");

        // The cooperative-yield protocol is gone: the same decode runs to completion with a two-credit budget,
        // replenishing at its loop heads.
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(2);
        let _ = session.decode(&mut run).expect("decode");
    }

    fn located<'result, 'source>(
        result: &'result AccessResult<'source>,
    ) -> &'result jqf_codec_core::LocatedOutcome<'source> {
        let AccessOutcome::Located(located) = result.outcome() else {
            panic!("expected located result")
        };
        located
    }

    fn selected_integer<'result, 'source>(result: &'result AccessResult<'source>) -> &'result str
    where
        'source: 'result,
    {
        let selected = located(result);
        let ExactSelectionRecord::Node { node, .. } = selected.result() else {
            panic!("expected selected node")
        };
        let scalar = selected
            .product()
            .document()
            .value_view(*node)
            .expect("view")
            .scalar()
            .expect("scalar read")
            .expect("scalar");
        match scalar {
            ScalarView::Number(NumberView::Integer(integer)) => integer,
            _ => panic!("expected integer"),
        }
    }

    #[test]
    fn unescaped_strings_and_keys_use_the_sealed_source_route() {
        let result = run(
            br#"{"alpha":"beta","esc\u0061ped":"value","number":123}"#,
            None,
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect("JSON decodes");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let stats = product.document().text_storage_stats().expect("text stats");
        assert!(stats.trusted_session_source_attachment);
        assert_eq!(stats.source_keys, 2);
        assert_eq!(stats.stored_keys, 1);
        assert_eq!(stats.source_string_values, 2);
        assert_eq!(stats.stored_string_values, 0);
        assert_eq!(stats.stored_integer_refs, 1);
        // `123` is spelled canonically in the input, so it takes the same source-span route the unescaped strings do
        // and leaves the arena holding only the one text no span can name: the escaped key.
        assert_eq!(stats.source_integer_values, 1);
        assert_eq!(stats.stored_decimal_coefficient_refs, 0);
        assert_eq!(stats.decoded_arena_len, "escaped".len());
    }

    /// The source-span route belongs to numbers whose canonical text IS the input's, and to no others. Each decimal
    /// here canonicalizes to something the input does not literally contain — a coefficient with the `.` removed
    /// (`1.5` -> `15`), a coefficient with the exponent folded into a scale (`1e3` -> `1`), a synthesized zero (`0e5`
    /// -> `0`) — so each must still be built in the arena. The integers, sign and negative zero included, must not
    /// be.
    #[test]
    fn a_long_non_ascii_string_decodes_across_work_grants() {
        // A work grant is 256 bytes. A string of 300+ non-ASCII content bytes used to Suspend without advancing and
        // livelock.
        let mut bytes = Vec::new();
        bytes.push(b'"');
        for _ in 0..300 {
            bytes.extend_from_slice("é".as_bytes());
        }
        bytes.push(b'"');
        run(&bytes, None, DiagnosticPolicy::ErrorsOnly).expect("long UTF-8 string decodes");
    }

    #[test]
    fn only_verbatim_integers_take_the_source_number_route() {
        let result = run(
            br"[7,-7,0,-0,1.5,1e3,0e5,-1.5e-3,1.50]",
            None,
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect("JSON decodes");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let stats = product.document().text_storage_stats().expect("text stats");
        assert_eq!(stats.stored_integer_refs, 4);
        assert_eq!(stats.source_integer_values, 4);
        assert_eq!(stats.stored_decimal_coefficient_refs, 5);
        // `15`, `1`, `0`, `-15`, `150` — the five coefficients, and nothing else.
        assert_eq!(stats.decoded_arena_len, 10);
    }

    #[test]
    fn fully_escaped_text_skips_source_sealing_and_attachment() {
        let result = run(br#"{"\u0061":"\u0062"}"#, None, DiagnosticPolicy::ErrorsOnly).expect("JSON decodes");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let stats = product.document().text_storage_stats().expect("text stats");
        // An ESCAPED string records an out-of-band AUTHORED span for its token, so a fully-escaped document commits
        // spans after all, and the session seals and attaches the source to make them addressable. What survives is the
        // STORED half: no text is bound as a zero-copy source ref (the escaped content is built in the arena), only the
        // authored-span record seals.
        assert!(stats.trusted_session_source_attachment);
        assert_eq!(stats.source_keys, 0);
        assert_eq!(stats.source_string_values, 0);
        assert_eq!(stats.stored_keys, 1);
        assert_eq!(stats.stored_string_values, 1);
    }

    /// Extracts the primary diagnostic's `(code name, primary label start offset)` from a rejected decode, asserting
    /// the `json` namespace and `InvalidInput` failure kind common to every strict-JSON reject.
    fn reject_diagnostic(error: &jqf_codec_core::CodecError) -> (&str, u32) {
        assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::InvalidInput);
        let diagnostic = error.diagnostic().expect("structured diagnostic");
        assert_eq!(diagnostic.code().namespace().name(), "json");
        let span = diagnostic.labels().first().expect("primary label").span();
        (diagnostic.code().name(), span.start())
    }

    // The single-pass parser validates UTF-8 inline as it builds, so the pre-scan's old "any invalid UTF-8 anywhere is
    // `invalid-utf8`, decided before grammar" ordering is gone. The tests below pin the single-pass contract: invalid
    // UTF-8 *inside string content* is `invalid-utf8` at the offending sequence's lead byte, while invalid bytes
    // *outside* a string are ordinary grammar rejects at the grammar position. Accept/reject verdicts are unchanged
    // (the differential and 200k fuzz corpora confirm no new divergence class); only the reject taxonomy for
    // out-of-string bytes differs from the two-pass.

    #[test]
    fn out_of_string_invalid_utf8_at_value_position_is_a_grammar_reject() {
        // `[` then a bare 0xFF where a value is expected. Single-pass rejects it as `expected-value` at the byte, not
        // `invalid-utf8`.
        let error = run(&[b'[', 0xFF, b']'], None, DiagnosticPolicy::ErrorsOnly)
            .expect_err("bare 0xFF at a value position is rejected");
        assert_eq!(reject_diagnostic(&error), ("expected-value", 1));
    }

    #[test]
    fn out_of_string_invalid_utf8_after_a_complete_value_is_trailing_content() {
        // A complete `[]`, then a bare 0xFF: the value is done, so the byte is trailing content at its offset, not
        // `invalid-utf8`.
        let error = run(&[b'[', b']', 0xFF], None, DiagnosticPolicy::ErrorsOnly)
            .expect_err("trailing 0xFF after a complete value is rejected");
        assert_eq!(reject_diagnostic(&error), ("trailing-content", 2));
    }

    #[test]
    fn in_string_mid_buffer_invalid_utf8_is_reported_at_the_lead_byte() {
        // `"X` then a 0xE2 three-byte lead whose continuation bytes (`Y`, `Z`) are invalid: reported `invalid-utf8` at
        // the lead byte (offset 2), not at the byte where continuation validation trips.
        let error = run(
            &[b'"', b'X', 0xE2, b'Y', b'Z', b'"'],
            None,
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect_err("invalid multi-byte sequence in string content is rejected");
        assert_eq!(reject_diagnostic(&error), ("invalid-utf8", 2));
    }

    #[test]
    fn in_string_truncated_multibyte_is_invalid_utf8_where_the_bytes_stopped() {
        // `"a` then a lone 0xF0 four-byte lead with no continuation bytes before the last byte the decoder was given:
        // truncated UTF-8, reported `invalid-utf8` at the CUT (offset 3), not at the lead — the decoder cannot tell
        // end of input from end of a growing window, and the cut is the position a streaming caller reads as "would
        // complete with more input".
        let error = run(&[b'"', b'a', 0xF0], None, DiagnosticPolicy::ErrorsOnly)
            .expect_err("truncated multi-byte sequence at end of input is rejected");
        assert_eq!(reject_diagnostic(&error), ("invalid-utf8", 3));
    }

    // The plain-string scan (`byte_scan::plain_string_prefix_len`) stops the run at the first `"`, `\`, control byte,
    // DEL, or byte >= 0x80, so a completed plain run is copy-safe ASCII by construction. The parser therefore
    // constructs the run `&str` without re-validating it with `from_utf8`; the multi-byte arm keeps the authoritative
    // `from_utf8` over each high-byte scalar. The tests below pin decoded content across that fusion — pure-ASCII
    // runs, multi-byte scalars around an escape, unchanged rejection, and a scalar straddling a cooperative poll
    // boundary.

    /// Decodes `bytes` as a whole document and returns the materialized root string, asserting decoded content rather
    /// than only accept/reject.
    fn decoded_string(bytes: &[u8]) -> alloc::string::String {
        let result = run(bytes, None, DiagnosticPolicy::ErrorsOnly).expect("string decodes");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let Ok(jqf_data::Value::String(value)) = product.document().materialize_root(&mut witness_ledger()) else {
            panic!("expected string root")
        };
        alloc::string::String::from(value.as_str())
    }

    /// Decodes `bytes` under a deliberately tiny work budget so the cooperative byte grants are small enough to split
    /// multi-byte UTF-8 scalars across replenishes, then returns the materialized root string.
    fn decoded_string_tiny_budget(bytes: &[u8]) -> alloc::string::String {
        let mut resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64)).expect("account"),
            &test_support::CONTROL,
            WorkMeter::try_new_v1(3).expect("work"),
        )
        .expect("resources");
        let registration = registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = requirement(None, DiagnosticPolicy::ErrorsOnly, &resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(3);
        let result = session.decode(&mut run).expect("decode");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let Ok(jqf_data::Value::String(value)) = product.document().materialize_root(&mut resources) else {
            panic!("expected string root")
        };
        alloc::string::String::from(value.as_str())
    }

    #[test]
    fn fused_ascii_run_decodes_pure_ascii_string_content() {
        // A leading escape forces decoded-text mode, so the trailing 40-byte ASCII run is copied through the fused
        // (unchecked) plain-run path — the run also crosses the 16-byte SIMD scan word. Decodes byte for byte.
        let body = alloc::format!("\t{}", "x".repeat(40));
        let bytes = alloc::format!("\"\\t{}\"", "x".repeat(40));
        assert_eq!(decoded_string(bytes.as_bytes()), body);
    }

    #[test]
    fn fused_prefix_copy_preserves_multibyte_utf8() {
        // `"café\nx"`: the plain-ASCII run "caf", then a multi-byte scalar (é), then an escape whose prefix copy
        // `[start, cursor)` spans "café" (ASCII plus a validated multi-byte scalar). The fused unchecked prefix copy
        // must reproduce the multi-byte bytes exactly, and the trailing "x" exercises the fused plain-run copy in
        // decoded mode.
        assert_eq!(decoded_string("\"café\\nx\"".as_bytes()), "café\nx");
    }

    #[test]
    fn fused_scan_preserves_escape_adjacent_high_bytes() {
        // `"é\né"`: a multi-byte scalar, an escape (its prefix copy carries the first é), then another multi-byte
        // scalar via the retained validation arm. Both high-byte scalars round-trip around the escape.
        assert_eq!(decoded_string("\"é\\né\"".as_bytes()), "é\né");
    }

    #[test]
    fn decoded_mode_invalid_utf8_still_rejects_at_lead_byte() {
        // With an escape forcing decoded-text mode, an invalid three-byte sequence (0xE2 lead, non-continuation bytes)
        // is still rejected as `invalid-utf8` at the lead byte by the retained validation arm — the ASCII-path fusion
        // did not weaken rejection.
        let error = run(
            &[b'"', b'a', b'\\', b'n', 0xE2, b'Y', b'Z', b'"'],
            None,
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect_err("invalid multi-byte sequence in decoded string content is rejected");
        assert_eq!(reject_diagnostic(&error), ("invalid-utf8", 4));
    }

    #[test]
    fn multibyte_scalar_straddling_a_poll_boundary_decodes_intact() {
        // Under a tiny work budget the cooperative byte grants split multi-byte scalars across `Pending` resumes. A
        // decoded-mode string of ten `é` scalars (each two bytes) after a leading escape must still reassemble
        // exactly, proving the scan carries no lost high-bit state across polls.
        let body = alloc::format!("\t{}", "é".repeat(10));
        let bytes = alloc::format!("\"\\t{}\"", "é".repeat(10));
        assert_eq!(decoded_string_tiny_budget(bytes.as_bytes()), body);
    }

    #[test]
    fn many_short_strings_decode_identically_without_the_closer_rescan() {
        // Each empty or one-byte string ends with the closer as the next remaining byte, so the string walk must not
        // launch a 16-byte scan there. The decoded document is the same as before the skip.
        let result = run(br#"["","a","bc","xyz",""]"#, None, DiagnosticPolicy::ErrorsOnly).expect("decodes");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let Ok(jqf_data::Value::Array(array)) = product.document().materialize_root(&mut witness_ledger()) else {
            panic!("expected array root")
        };
        let texts: alloc::vec::Vec<&str> = array
            .iter()
            .map(|value| match value {
                jqf_data::Value::String(text) => text.as_str(),
                _ => panic!("expected string element"),
            })
            .collect();
        assert_eq!(texts, ["", "a", "bc", "xyz", ""]);
    }

    #[test]
    fn a_long_number_still_parses_with_u32_offsets() {
        // A 200-digit integer, then a long decimal. Offsets stay well under u32; the overflow path is the same
        // CodecFailureKind::Overflow the source span type uses.
        let integer = "1".repeat(200);
        let input = alloc::format!("[{integer},1.{}e10]", "5".repeat(80));
        let result = run(input.as_bytes(), None, DiagnosticPolicy::ErrorsOnly).expect("long number decodes");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let Ok(jqf_data::Value::Array(array)) = product.document().materialize_root(&mut witness_ledger()) else {
            panic!("expected array")
        };
        assert_eq!(array.len(), 2);
        let jqf_data::Value::Number(first) = array.iter().next().expect("first") else {
            panic!("expected number")
        };
        assert_eq!(first.to_integer().expect("integer").as_str(), integer);
    }

    #[test]
    fn fused_digit_run_still_decodes_integer_decimal_and_negative_zero() {
        let result = run(br"[123456,1.5e10,-0]", None, DiagnosticPolicy::ErrorsOnly).expect("numbers decode");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let Ok(jqf_data::Value::Array(array)) = product.document().materialize_root(&mut witness_ledger()) else {
            panic!("expected array root")
        };
        assert_eq!(array.len(), 3);
        let mut values = array.iter();
        let jqf_data::Value::Number(integer) = values.next().expect("123456") else {
            panic!("expected integer")
        };
        assert_eq!(integer.to_i64(), Some(123_456));
        let jqf_data::Value::Number(decimal) = values.next().expect("1.5e10") else {
            panic!("expected decimal")
        };
        let decimal = decimal.as_decimal().expect("decimal");
        assert_eq!(decimal.coefficient().as_str(), "15");
        assert_eq!(decimal.scale(), -9);
        let jqf_data::Value::Number(neg_zero) = values.next().expect("-0") else {
            panic!("expected -0")
        };
        assert_eq!(neg_zero.to_integer().expect("-0 integer").as_str(), "-0");
    }

    #[test]
    fn unicode_escape_one_step_keeps_bmp_pair_and_lone_low() {
        assert_eq!(decoded_string(br#""\u0041""#), "A");
        assert_eq!(decoded_string("\"\\uD800\\uDC00\"".as_bytes()), "\u{10000}");
        assert_eq!(decoded_string(br#""\uDC00""#), "\u{fffd}");
    }

    #[test]
    fn in_window_strings_and_integers_stay_in_the_token_batch() {
        let result = run(
            br#"{"a":1,"b":-0,"c":"xyz","d":[2,3]}"#,
            None,
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect("decodes");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let value = product
            .document()
            .materialize_root(&mut witness_ledger())
            .expect("materialize");
        let jqf_data::Value::Object(object) = value else {
            panic!("expected object")
        };
        assert_eq!(object.len(), 4);
    }

    #[test]
    fn a_string_containing_del_decodes_and_encodes_as_today() {
        // Grammar admits raw 0x7f; the encoder escapes it as `\u007f`.
        let input = [b'"', 0x7f, b'"'];
        assert_eq!(decoded_string(&input), "\u{7f}");
        let result = run(&input, None, DiagnosticPolicy::ErrorsOnly).expect("del decodes");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let Ok(jqf_data::Value::String(value)) = product.document().materialize_root(&mut witness_ledger()) else {
            panic!("expected string")
        };
        assert_eq!(value.as_str(), "\u{7f}");
    }

    #[test]
    fn oversized_capacity_estimate_never_rejects_a_document_that_fits_exactly() {
        // The source-length capacity estimate is a hint, never an admission requirement. Here a long single-string
        // document needs only one node but the `source_len / 40` estimate asks for many more; under a memory limit
        // sized so the actual document fits but the estimate does not, the decode must degrade to amortized growth and
        // still succeed.
        let body = "x".repeat(80_000);
        let bytes = alloc::format!("\"{body}\"");
        // The finished document is one source-span string node with zero decoded text (the 80 KB body stays in the
        // borrowed source, uncharged), so it fits in a few KB. The estimate, however, asks for ~2000 (80002 / 40)
        // up-front node + owner-position slots — tens of KB. A 64 KB limit clears the real document but not the
        // estimate, forcing the degrade path.
        let limit = 64 * 1024;
        let mut resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, limit, u64::MAX, 64)).expect("account"),
            &test_support::CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources");
        let registration = registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes.as_bytes()),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = requirement(None, DiagnosticPolicy::ErrorsOnly, &resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).expect("degraded estimate must not reject");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        // The witness materializes against its OWN ledger. Materialization now charges the owned value's payload (the
        // 80 KB body leaves the borrowed source and becomes a shared allocation), and this test's account is
        // deliberately capped BELOW that to force the decoder's degrade path — charging the witness to it would test
        // the cap, not the degrade.
        assert!(matches!(
            product.document().materialize_root(&mut witness_ledger()),
            Ok(jqf_data::Value::String(value)) if value.as_str() == body
        ));
    }

    #[test]
    fn large_uniform_document_reprojects_capacity_and_decodes_correctly() {
        // A uniform many-node document past the 1 MiB reprojection threshold: the parser samples node/occurrence
        // density from the consumed prefix and reserves the projected final table size in one step instead of doubling
        // repeatedly. The decode must still produce the exact array.
        const COUNT: usize = 200_000;
        let mut bytes = alloc::vec::Vec::with_capacity(1_400_000);
        bytes.push(b'[');
        for index in 0..COUNT {
            if index > 0 {
                bytes.push(b',');
            }
            bytes.extend_from_slice(alloc::format!("{index}").as_bytes());
        }
        bytes.push(b']');
        assert!(bytes.len() > (1 << 20), "input must exceed the reprojection threshold");
        let result = run(&bytes, None, DiagnosticPolicy::ErrorsOnly).expect("large array decodes");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let Ok(jqf_data::Value::Array(array)) = product.document().materialize_root(&mut witness_ledger()) else {
            panic!("expected array root")
        };
        assert_eq!(array.len(), COUNT);
    }

    #[test]
    fn capacity_reprojection_never_rejects_a_document_that_fits_exactly() {
        // Reprojection is a hint, never an admission requirement — the same contract the initial estimate honors. A
        // dense prefix of many small numbers makes the sampled density project a huge final node count, while the bulk
        // of the source is one giant borrowed-span string that adds a single node: the actual document is small. Under
        // a limit that clears the real document but not the inflated projection, the denied reprojection must roll back
        // and the decode must still succeed.
        const NUMBERS: usize = 100_000;
        let mut bytes = alloc::vec::Vec::with_capacity(2_400_000);
        bytes.push(b'[');
        for _ in 0..NUMBERS {
            bytes.extend_from_slice(b"1,");
        }
        bytes.push(b'"');
        bytes.extend_from_slice(&alloc::vec![b'x'; 2_000_000]);
        bytes.extend_from_slice(b"\"]");
        assert!(bytes.len() > (1 << 20), "input must exceed the reprojection threshold");
        // The giant string stays a borrowed source span (uncharged); the real charged document is ~100k tiny number
        // nodes. 48 MiB clears that but not the density projection (~800k nodes across three tables).
        let limit = 48 * 1024 * 1024;
        let mut resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, limit, u64::MAX, 64)).expect("account"),
            &test_support::CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources");
        let registration = registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(&bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = requirement(None, DiagnosticPolicy::ErrorsOnly, &resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).expect("denied reprojection must not reject");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let Ok(jqf_data::Value::Array(array)) = product.document().materialize_root(&mut resources) else {
            panic!("expected array root")
        };
        assert_eq!(array.len(), NUMBERS + 1);
    }

    /// The reprojection clamp. The same adversarial shape as above (a dense prefix of one-byte numbers whose sampled
    /// density projects a far larger final node count than the giant-string tail ever sustains), but now under a
    /// generous limit so the reprojection is *granted* rather than rolled back. The clamp caps every projection at
    /// `source_len / MIN_BYTES_PER_NODE` nodes, so the peak working memory the decode reserves stays bounded by that
    /// physical ceiling — an unclamped projection that trusted the sampled density plus its margin would reserve
    /// strictly more. The document must still decode exactly.
    #[test]
    fn reprojection_reservation_stays_within_the_physical_node_ceiling() {
        use jqf_resource::MemoryCategory;

        const NUMBERS: usize = 100_000;
        let mut bytes = alloc::vec::Vec::with_capacity(2_400_000);
        bytes.push(b'[');
        for _ in 0..NUMBERS {
            bytes.extend_from_slice(b"1,");
        }
        bytes.push(b'"');
        bytes.extend_from_slice(&alloc::vec![b'x'; 2_000_000]);
        bytes.extend_from_slice(b"\"]");
        let source_len = bytes.len();
        assert!(source_len > (1 << 20), "input exceeds the reprojection threshold");

        let mut resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64)).expect("account"),
            &test_support::CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources");
        let registration = registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(&bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = requirement(None, DiagnosticPolicy::ErrorsOnly, &resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).expect("decode");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        // The correctness witness materializes against its OWN ledger: the peak asserted below is the DECODER's
        // projection, and an owned value charged to the same account would fold the materialization's bytes into it.
        let Ok(jqf_data::Value::Array(array)) = product.document().materialize_root(&mut witness_ledger()) else {
            panic!("expected array root")
        };
        assert_eq!(array.len(), NUMBERS + 1);

        // The clamp caps every node/occurrence projection at `source_len / MIN_BYTES_PER_NODE`, so the reservation can
        // never scale beyond what the source could physically hold. The working peak here stays well under the source
        // length itself (the actual document is one giant borrowed-span string plus ~100k tiny number nodes), whereas
        // an unclamped projection that trusted the dense sample's density would reserve node tables several times
        // larger. Guard against that slide: the peak must not grow to a multiple of the input.
        let working_peak = resources.snapshot().memory(MemoryCategory::Working).peak();
        let ceiling = source_len as u64;
        assert!(
            working_peak <= ceiling,
            "working peak {working_peak} exceeded clamp ceiling {ceiling} (source_len {source_len})"
        );
    }

    #[test]
    fn semantic_root_demand_decodes_minimal_coverage_but_materializes_identically() {
        let bytes = br#"{"a":[1,2],"a":{"x":null}}"#;
        let mut resources = test_support::resources();
        let registration = registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let guarantees = AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly);
        let requirement =
            AccessRequirement::try_whole(semantic_demand(&resources), guarantees, &resources).expect("requirement");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let result = {
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4_096);
            session.decode(&mut run).expect("decode")
        };
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let coverage = product.document().coverage();
        assert!(!coverage.contains(DocumentCapability::Topology));
        assert!(!coverage.contains(DocumentCapability::AttachedFacts));
        assert!(coverage.contains(DocumentCapability::SemanticNodes));
        // The absent-topology reader still refuses precisely.
        assert!(product.document().occurrence_count().is_err());
        // Last-value-wins object semantics still materialize from winner entries.
        let jqf_data::Value::Object(object) = product
            .document()
            .materialize_root(&mut resources)
            .expect("materialize")
        else {
            panic!("expected object root")
        };
        assert_eq!(object.len(), 1);
    }

    #[test]
    fn provider_has_one_real_route_and_truthful_attribute_absence_support() {
        let mut resources = test_support::resources();
        let registration = registration().expect("registration");
        let provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(br"null"),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        // Whole (slot 0) and scoped exact (slot 1); the element-stream slot was deleted with the element-stream result
        // kind.
        assert_eq!(provider.route_descriptions().len(), 2);
        assert!(provider.supports_attribute_absence());
    }

    #[test]
    fn whole_binds_full_route_and_exact_binds_scoped_route() {
        let whole = run(br#"{"a":1}"#, None, DiagnosticPolicy::ErrorsOnly).expect("whole");
        let AccessOutcome::FullDocument(product) = whole.outcome() else {
            panic!("whole document")
        };
        assert_eq!(whole.report().adapter(), AccessAdapter::None);
        assert_eq!(whole.report().route().expect("receipt").route(), FULL_PHYSICAL_ROUTE_ID);
        assert!(
            product
                .document()
                .coverage()
                .authoritative_empty_families()
                .contains(DocumentCapabilityFamily::Attributes)
        );

        // The exact-root requirement Direct-binds the scoped route (adapter None, scoped physical identity, slot 1),
        // not the whole route + generic exact adapter it used before Stage 6.
        let exact = run(br#"{"a":1}"#, Some(&[]), DiagnosticPolicy::ErrorsOnly).expect("exact root");
        let selected = located(&exact);
        let ExactSelectionRecord::Node { node, .. } = selected.result() else {
            panic!("node")
        };
        assert_eq!(*node, selected.product().document().root_handle());
        assert_eq!(exact.report().adapter(), AccessAdapter::None);
        assert_eq!(
            exact.report().route().expect("receipt").route(),
            super::SCOPED_PHYSICAL_ROUTE_ID
        );
        assert_eq!(exact.report().route().expect("receipt").slot().get(), 1);

        // Empty path over a nested document must locate the ROOT, not the first inner container to close (the array
        // under `a`).
        let nested_root = run(br#"{"a":[1,2]}"#, Some(&[]), DiagnosticPolicy::ErrorsOnly).expect("nested empty path");
        let selected = located(&nested_root);
        let ExactSelectionRecord::Node { node, .. } = selected.result() else {
            panic!("node")
        };
        let kind = selected
            .product()
            .document()
            .value_view(*node)
            .expect("view")
            .kind()
            .expect("kind");
        assert_eq!(kind, ValueKind::Object, "empty path locates the root object");
    }

    #[test]
    fn generic_exact_fallback_selects_members_and_signed_indices() {
        let member = run(
            br#"{"a":{"items":[10,20]}}"#,
            Some(&[Step::Member("a"), Step::Member("items"), Step::Index(0)]),
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect("positive");
        assert_eq!(selected_integer(&member), "10");

        let negative = run(
            br#"{"a":{"items":[10,20]}}"#,
            Some(&[Step::Member("a"), Step::Member("items"), Step::Index(-1)]),
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect("negative");
        assert_eq!(selected_integer(&negative), "20");

        // Last-value-wins: the fused validate walk must keep the LAST matching member, including when the winner is
        // nested.
        let last_wins = run(
            br#"{"a":1,"a":2}"#,
            Some(&[Step::Member("a")]),
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect("last-wins");
        assert_eq!(selected_integer(&last_wins), "2");
        let nested_last = run(
            br#"{"a":{"b":1},"a":{"b":2}}"#,
            Some(&[Step::Member("a"), Step::Member("b")]),
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect("nested last-wins");
        assert_eq!(selected_integer(&nested_last), "2");

        // Nested members of skipped array elements must not count as elements.
        let indexed = run(
            br#"{"catalog":[{"name":1,"skip":{"x":0}},{"name":2,"skip":{"x":0}}]}"#,
            Some(&[Step::Member("catalog"), Step::Index(1), Step::Member("name")]),
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect("index through sibling objects");
        assert_eq!(selected_integer(&indexed), "2");
    }

    #[test]
    fn scoped_negative_observations_report_the_same_record() {
        let missing = run(
            br#"{"a":1}"#,
            Some(&[Step::Member("missing")]),
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect("missing");
        assert!(matches!(
            located(&missing).result(),
            ExactSelectionRecord::Missing { step_index: 0, .. }
        ));
        // The scoped route retains only a placeholder for a negative observation rather than the whole document; the
        // SDK renders `null` from the record.
        assert_eq!(located(&missing).product().document().node_count(), 1);

        let mismatch = run(
            br#"{"a":1}"#,
            Some(&[Step::Member("a"), Step::Member("b")]),
            DiagnosticPolicy::ErrorsOnly,
        )
        .expect("mismatch");
        assert!(matches!(
            located(&mismatch).result(),
            ExactSelectionRecord::TypeMismatch {
                step_index: 1,
                actual_type: ValueKind::Number,
                ..
            }
        ));
    }

    #[test]
    fn exact_fallback_never_bypasses_strict_trailing_validation() {
        let result = run(
            br#"{"a":1} false"#,
            Some(&[Step::Member("a")]),
            DiagnosticPolicy::ErrorsOnly,
        );
        assert!(result.is_err());
    }

    #[test]
    fn successful_diagnostic_policy_and_report_match_document_coverage() {
        let result = run(br"null", None, DiagnosticPolicy::All).expect("all diagnostics");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("whole")
        };
        assert_eq!(result.report().diagnostics(), DiagnosticCoverage::AuthoritativeEmpty);
        assert_eq!(
            product.document().coverage().diagnostic_coverage(),
            DiagnosticCoverage::AuthoritativeEmpty
        );
    }

    /// Opens the provider's already-charged source at `start_offset` with the Stage 7 adjacent-values opt-in, mirroring
    /// how `jqf-sdk`'s `execute_sequence` decodes one value in a stream without re-charging input bytes.
    fn run_adjacent<'source>(
        bytes: &'source [u8],
        start_offset: u64,
        path: Option<&[Step<'_>]>,
    ) -> Result<AccessResult<'source>, jqf_codec_core::CodecError> {
        let mut resources = test_support::resources();
        let registration = registration().expect("registration");
        let mut provider = registration.decoder().expect("decoder").create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: true,
                value_separator: crate::VALUE_SEPARATORS,
            },
            &mut resources,
        )?;
        let requirement = requirement(path, DiagnosticPolicy::ErrorsOnly, &resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open_at(&handle, start_offset, &mut resources)?;
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run)
    }

    fn decode_ready<'source>(
        session: &mut ErasedAccessSession<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> AccessResult<'source> {
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run).expect("adjacent decode succeeds")
    }

    fn assert_adjacent_reuse_stays_stable_across_outlier_values(bytes: &[u8], path: Option<&[Step<'_>]>) {
        let mut resources = test_support::resources();
        let registration = registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: true,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = requirement(path, DiagnosticPolicy::ErrorsOnly, &resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut reuse = ReusableAccessSession::new();

        let first = decode_ready(
            provider
                .open_at_reusing(&handle, 0, &mut reuse, &mut resources)
                .expect("outlier session opens"),
            &mut resources,
        );
        let mut offset = first
            .report()
            .consumed_offset()
            .expect("outlier reports consumed bytes")
            + 1;
        drop(first);
        let outlier_current = resources.snapshot().memory_current_bytes();

        let second = decode_ready(
            provider
                .open_at_reusing(&handle, offset, &mut reuse, &mut resources)
                .expect("first tiny session reopens"),
            &mut resources,
        );
        offset += second
            .report()
            .consumed_offset()
            .expect("tiny value reports consumed bytes")
            + 1;
        drop(second);
        let first_tail_current = resources.snapshot().memory_current_bytes();

        let third = decode_ready(
            provider
                .open_at_reusing(&handle, offset, &mut reuse, &mut resources)
                .expect("second tiny session reopens"),
            &mut resources,
        );
        drop(third);
        let second_tail_current = resources.snapshot().memory_current_bytes();

        // The parser's outlier buffer is now allocator-managed (a plain `Vec` charged by the counting allocator), so
        // its release is not observable through the threaded ledger's Working residency. What the threaded ledger must
        // show is stability: the reuse session must neither grow nor leak across the tiny-record tail.
        assert!(
            first_tail_current <= outlier_current.saturating_add(1_024),
            "the reused session must not grow threaded residency: outlier={outlier_current}, tail={first_tail_current}"
        );
        assert!(
            second_tail_current <= first_tail_current.saturating_add(1_024),
            "the bounded tiny-record tail must remain stable: first={first_tail_current}, second={second_tail_current}"
        );

        drop(reuse);
        drop(provider);
    }

    /// Renders one element value to a canonical structural string, deep enough to distinguish any two distinct JSON
    /// values in the fixtures.
    fn render(product: &jqf_codec_core::DocumentProduct<'_>, handle: NodeHandle) -> String {
        let view = product.document().value_view(handle).expect("view");
        if let Some(scalar) = view.scalar().expect("scalar read") {
            return match scalar {
                ScalarView::Null => String::from("null"),
                ScalarView::Bool(true) => String::from("true"),
                ScalarView::Bool(false) => String::from("false"),
                ScalarView::Number(NumberView::Integer(text)) => String::from(text),
                ScalarView::Number(NumberView::Decimal { coefficient, scale }) => {
                    alloc::format!("{coefficient}e{scale}")
                }
                ScalarView::Number(_) => String::from("<num>"),
                ScalarView::String(text) => alloc::format!("{text:?}"),
                _ => String::from("<scalar>"),
            };
        }
        match view.kind().expect("kind") {
            ValueKind::Array => {
                let array = view.array().expect("array").expect("array view");
                let mut out = String::from("[");
                for (index, item) in array.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    let node = product.document().node_handle(item.node()).expect("handle");
                    out.push_str(&render(product, node));
                }
                out.push(']');
                out
            }
            ValueKind::Object => {
                let object = view.object().expect("object").expect("object view");
                let mut out = String::from("{");
                for (index, entry) in object.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    let entry = entry.expect("entry");
                    let node = product.document().node_handle(entry.value().node()).expect("handle");
                    let _ = core::fmt::Write::write_fmt(
                        &mut out,
                        format_args!("{:?}:{}", entry.key(), render(product, node)),
                    );
                }
                out.push('}');
                out
            }
            _ => String::from("<value>"),
        }
    }

    fn deep_array(depth: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(depth.saturating_mul(2).saturating_add(1));
        bytes.resize(depth, b'[');
        bytes.push(b'0');
        bytes.resize(depth.saturating_mul(2).saturating_add(1), b']');
        bytes
    }

    #[test]
    fn whole_adjacent_reuse_releases_outlier_parser_capacity() {
        let mut bytes = deep_array(512);
        bytes.extend_from_slice(b"\n0\n0");
        assert_adjacent_reuse_stays_stable_across_outlier_values(&bytes, None);
    }

    #[test]
    fn scoped_adjacent_reuse_releases_outlier_validator_and_materializer_capacity() {
        let nested = deep_array(512);
        let mut bytes = Vec::with_capacity(nested.len().saturating_add(32));
        bytes.extend_from_slice(br#"{"v":"#);
        bytes.extend_from_slice(&nested);
        bytes.extend_from_slice(
            br#"}
{"v":0}
{"v":0}"#,
        );
        let path = [Step::Member("v")];
        assert_adjacent_reuse_stays_stable_across_outlier_values(&bytes, Some(&path));
    }

    #[test]
    fn one_provider_shares_schema_but_not_document_identity_across_adjacent_values() {
        let bytes: &[u8] = br#"{"v":1}
{"v":1}"#;
        let mut resources = test_support::resources();
        let registration = registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: true,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = requirement(None, DiagnosticPolicy::ErrorsOnly, &resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut reuse = ReusableAccessSession::new();

        let first = decode_ready(
            provider
                .open_at_reusing(&handle, 0, &mut reuse, &mut resources)
                .expect("first session opens"),
            &mut resources,
        );
        let (AccessOutcome::FullDocument(first), first_report) = first.into_parts() else {
            panic!("first value is a complete document")
        };
        let second_offset = first_report
            .consumed_offset()
            .expect("first value reports its consumed bytes")
            + 1;
        let second = decode_ready(
            provider
                .open_at_reusing(&handle, second_offset, &mut reuse, &mut resources)
                .expect("second session reopens"),
            &mut resources,
        );
        let (AccessOutcome::FullDocument(second), _) = second.into_parts() else {
            panic!("second value is a complete document")
        };

        assert_ne!(first.document().key(), second.document().key());
        assert_ne!(first.document().root_handle(), second.document().root_handle());
        assert!(
            first.document().benchmark_shares_schema_storage_with(second.document()),
            "one provider must retain one immutable schema allocation"
        );

        drop(reuse);
        drop(provider);
        for document in [first.document(), second.document()] {
            let jqf_data::Value::Object(object) = document
                .materialize_root(&mut resources)
                .expect("document remains readable")
            else {
                panic!("expected object root")
            };
            assert_eq!(object.len(), 1);
        }
    }

    #[test]
    fn adjacent_values_opt_in_reports_consumed_offset_on_whole_route() {
        let bytes: &[u8] = br#"{"a":1} {"a":2}"#;
        let first = run_adjacent(bytes, 0, None).expect("first value");
        assert_eq!(first.report().consumed_offset(), Some(7));
        assert!(matches!(first.outcome(), AccessOutcome::FullDocument(_)));

        // The second value's consumed offset is relative to the slice this session was opened on (`open_at` narrows to
        // start at byte 8), not to the whole buffer, so it is also 7 (its own length), and no input bytes were
        // re-charged to open it (`create_provider` charged the whole 15-byte buffer exactly once, up front).
        let second = run_adjacent(bytes, 8, None).expect("second value");
        assert_eq!(second.report().consumed_offset(), Some(7));
        assert!(matches!(second.outcome(), AccessOutcome::FullDocument(_)));
    }

    #[test]
    fn adjacent_path_consumes_a_source_start_byte_order_mark() {
        // The reference strips exactly one UTF-8 BOM before the FIRST value (RFC 8259 §8.1); the adjacent-value entry
        // point owns the law. The BOM is part of the value's consumed extent, exactly as this encode law treats it (the
        // stream starts after the mark).
        let result = run_adjacent(b"\xef\xbb\xbf\"bom\"", 0, None).expect("BOM-prefixed value decodes");
        assert_eq!(result.report().consumed_offset(), Some(8));
        assert!(matches!(result.outcome(), AccessOutcome::FullDocument(_)));
    }

    #[test]
    fn adjacent_path_accepts_a_bom_only_at_absolute_offset_zero() {
        // BOM before whitespace and a value is still a source-start BOM.
        let result = run_adjacent(b"\xef\xbb\xbf \"x\"", 0, None).expect("BOM before whitespace decodes");
        assert_eq!(result.report().consumed_offset(), Some(7));
        // BOM after whitespace is NOT at source offset zero: the reference rejects it.
        assert!(run_adjacent(b" \xef\xbb\xbf\"x\"", 0, None).is_err());
        // A BOM before a LATER adjacent value is not a source-start BOM either: the reference consumes only the first.
        assert!(run_adjacent(b"\"a\"\xef\xbb\xbf\"b\"", 3, None).is_err());
        // A bare BOM is still "expected one complete JSON value".
        assert!(run_adjacent(b"\xef\xbb\xbf", 0, None).is_err());
    }

    #[test]
    fn scoped_route_consumes_the_same_source_start_byte_order_mark() {
        // Route consistency law (the BOM fix): a source-start BOM must decode identically whatever program asked —
        // the whole-document route consumes it, and the member-scoped exact-path route must consume it too: its
        // validator reproduces the whole-input strictness exactly before materializing a subtree, or `.a` on a
        // BOM-prefixed file fails where `.` succeeds. The consumed extent includes the mark exactly as the whole
        // route's does (`3 + len({"a":1})`).
        let result = run_adjacent(b"\xef\xbb\xbf{\"a\":1}", 0, Some(&[Step::Member("a")]))
            .expect("BOM-prefixed scoped value decodes");
        assert_eq!(result.report().consumed_offset(), Some(10));
        // The same negative laws the whole route keeps: a BOM after whitespace, or before a later value, is not a
        // source-start BOM. The later-value offset is the first value's consumed extent (`7` for `{"a":1}`), exactly
        // where the sequence drive reopens the source.
        assert!(run_adjacent(b" \xef\xbb\xbf{\"a\":1}", 0, Some(&[Step::Member("a")])).is_err());
        assert!(run_adjacent(b"{\"a\":1}\xef\xbb\xbf{\"b\":2}", 7, Some(&[Step::Member("b")])).is_err());
    }

    /// A byte-order mark SPLIT by a read boundary is an incomplete mark, not a wrong byte. Both routes label the cut,
    /// which is what makes the streaming drive refill: the first byte of the mark is also an incomplete UTF-8 lead, so
    /// the hold should always have fired here and did not — a BOM-prefixed file piped through a slow writer was a
    /// parse error.
    #[test]
    fn a_byte_order_mark_cut_by_the_window_is_labeled_at_the_cut() {
        for cut in [&b"\xef"[..], b"\xef\xbb"] {
            let at = u32::try_from(cut.len()).expect("test input fits");
            assert_eq!(
                reject_diagnostic(&run_adjacent(cut, 0, None).expect_err("incomplete mark")).1,
                at,
                "whole route must label {cut:02x?} at the cut"
            );
            assert_eq!(
                reject_diagnostic(&run_adjacent(cut, 0, Some(&[Step::Member("a")])).expect_err("incomplete mark")).1,
                at,
                "scoped route must label {cut:02x?} at the cut"
            );
        }
        // A lead byte that is NOT the start of a mark keeps its own position: the hold is for a split mark, not for
        // every 0xEF.
        assert_eq!(
            reject_diagnostic(&run_adjacent(b"\xef\x28", 0, None).expect_err("bad lead")).1,
            0
        );
    }

    /// Adjacent-value decode takes the same zero-copy source-span routes that whole-document decode does, over the
    /// extent its own value occupies.
    ///
    /// The second value is the load-bearing one: its session is opened over everything from its own first byte to end
    /// of input, so every span it commits, and the seal that authenticates them, are in its own coordinates. A seal or
    /// an attachment naming any other segment would resolve neighbouring bytes rather than fail, which is what the
    /// rendering pins.
    #[test]
    fn adjacent_values_take_the_source_span_route_over_their_own_extent() {
        const BYTES: &[u8] = br#"{"alpha":"first","n":123} {"alpha":"second","n":456}"#;
        for (start, rendered) in [
            (0, r#"{"alpha":"first","n":123}"#),
            (26, r#"{"alpha":"second","n":456}"#),
        ] {
            let result = run_adjacent(BYTES, start, None).expect("adjacent value decodes");
            let AccessOutcome::FullDocument(product) = result.outcome() else {
                panic!("expected full document")
            };
            assert_eq!(render(product, product.document().root_handle()), rendered);
            let stats = product.document().text_storage_stats().expect("text stats");
            assert!(stats.trusted_session_source_attachment);
            assert_eq!(stats.source_keys, 2);
            assert_eq!(stats.source_string_values, 1);
            assert_eq!(stats.source_integer_values, 1);
            // Every text this value carries is a span of it, so the arena that would otherwise hold copies of them is
            // empty.
            assert_eq!(stats.decoded_arena_len, 0);
        }
    }

    /// Whichever route a value's text takes, the value it decodes to is the same. Every shape below is decoded twice
    /// — once as one value of an adjacent stream, once as a whole document over exactly its own bytes — and the two
    /// must render identically, across the escape-copy, verbatim span, and canonicalized-number paths alike.
    #[test]
    fn adjacent_and_whole_document_decode_agree_on_every_text_route() {
        const SHAPES: [&[u8]; 8] = [
            br#""plain""#,
            b"\"esc\xc3\xa9\\n\"",
            b"123",
            b"-0.5e3",
            br#"{"k":"v","esc\u0061ped":1}"#,
            br#"[1,"two",{"three":3},[],{}]"#,
            b"null",
            b"[]",
        ];
        for shape in SHAPES {
            let adjacent = run_adjacent(shape, 0, None).expect("adjacent value decodes");
            assert_eq!(adjacent.report().consumed_offset(), Some(shape.len() as u64));
            let whole = run(shape, None, DiagnosticPolicy::ErrorsOnly).expect("document decodes");
            let (AccessOutcome::FullDocument(adjacent), AccessOutcome::FullDocument(whole)) =
                (adjacent.outcome(), whole.outcome())
            else {
                panic!("expected full documents")
            };
            assert_eq!(
                render(adjacent, adjacent.document().root_handle()),
                render(whole, whole.document().root_handle())
            );
        }
    }

    /// A value that fails after committing source spans still reports its fault at the position it read, in the
    /// coordinates of the session it was opened on — the spans it abandoned change nothing about where the parse was.
    #[test]
    fn adjacent_value_rejects_report_offsets_after_committed_source_spans() {
        const BYTES: &[u8] = br#"1 {"a":"ok","b":}"#;
        let first = run_adjacent(BYTES, 2, None);
        assert_eq!(
            reject_diagnostic(&first.expect_err("missing member value rejected")),
            ("expected-value", 14)
        );
    }

    #[test]
    fn adjacent_values_opt_in_reports_consumed_offset_on_scoped_route() {
        let bytes: &[u8] = br#"{"a":1} {"a":2}"#;
        let path = [Step::Member("a")];
        let first = run_adjacent(bytes, 0, Some(&path)).expect("first value");
        assert_eq!(first.report().consumed_offset(), Some(7));
        assert_eq!(selected_integer(&first), "1");

        let second = run_adjacent(bytes, 8, Some(&path)).expect("second value");
        assert_eq!(second.report().consumed_offset(), Some(7));
        assert_eq!(selected_integer(&second), "2");
    }

    #[test]
    fn adjacent_values_opt_in_tolerates_non_whitespace_trailing_bytes() {
        // The opt-in stops validating at the end of the root value; whatever follows (more JSON, or garbage) is the
        // caller's concern.
        let result = run_adjacent(br#"{"a":1}garbage"#, 0, None).expect("value despite trailing garbage");
        assert_eq!(result.report().consumed_offset(), Some(7));
    }

    #[test]
    fn adjacent_values_default_still_rejects_trailing_content() {
        // `open()` (offset 0, no opt-in) keeps today's strict single-document contract exactly: the differential corpus
        // pins these rejects.
        let result = run(br#"{"a":1} {"a":2}"#, None, DiagnosticPolicy::ErrorsOnly);
        assert!(result.is_err());
        assert_eq!(
            reject_diagnostic(&result.expect_err("trailing content rejected")),
            ("trailing-content", 8)
        );
    }

    /// In strict single-document mode a bare word that ends at a value-boundary is a complete value, so what follows is
    /// trailing content (`1"a"` -> the number ends at byte 1, then the `"` is trailing), while a bare word butted
    /// against another bare word is one malformed token rejected in place (`truex`, `1x`). None of these change the
    /// reject class the differential's strict corpus pins.
    #[test]
    fn strict_single_document_bare_word_boundary_rejects() {
        let number_then_string = run(br#"1"a""#, None, DiagnosticPolicy::ErrorsOnly);
        assert_eq!(
            reject_diagnostic(&number_then_string.expect_err("trailing string rejected")),
            ("trailing-content", 1)
        );
        let literal_then_string = run(br#"null"a""#, None, DiagnosticPolicy::ErrorsOnly);
        assert_eq!(
            reject_diagnostic(&literal_then_string.expect_err("trailing string rejected")),
            ("trailing-content", 4)
        );
        let literal_suffix = run(b"truex", None, DiagnosticPolicy::ErrorsOnly);
        assert_eq!(
            reject_diagnostic(&literal_suffix.expect_err("bad literal suffix rejected")),
            ("invalid-literal", 4)
        );
        let number_suffix = run(b"1x", None, DiagnosticPolicy::ErrorsOnly);
        assert_eq!(
            reject_diagnostic(&number_suffix.expect_err("bad number suffix rejected")),
            ("invalid-number", 1)
        );
    }

    /// In adjacent mode the same boundary rule splits `1"a"` into two values — the first consumes exactly its own
    /// byte — but an invalid bare-word suffix (`truex`) is still rejected wholesale, before the value is emitted,
    /// matching the reference.
    #[test]
    fn adjacent_bare_word_boundary_splits_values_but_rejects_bad_suffix() {
        let first = run_adjacent(br#"1"a""#, 0, None).expect("number is one complete value");
        assert_eq!(first.report().consumed_offset(), Some(1));
        // The second value opens at the boundary byte and decodes the string.
        let second = run_adjacent(br#"1"a""#, 1, None).expect("adjacent string value");
        assert_eq!(second.report().consumed_offset(), Some(3));

        let bad_literal = run_adjacent(b"truex", 0, None);
        assert_eq!(
            reject_diagnostic(&bad_literal.expect_err("bad literal suffix rejected wholesale")),
            ("invalid-literal", 4)
        );
    }

    /// The streaming hold law is a property of the BYTES, not of the route the program bound: a token that runs to the
    /// last byte given could still be extended by the next read, so both graders must say so. Without it the exact-path
    /// route publishes `inf` where the whole route holds for `infinity`, and the output depends on where `read(2)` cut
    /// the stream.
    #[test]
    fn both_routes_report_the_same_open_ended_verdict() {
        // Every token family whose spelling can still grow: a bare number, the non-finite spellings (`inf` is a proper
        // prefix of `infinity`), and the bare literals.
        for open in [
            &b"1234"[..],
            b"-0.5e1",
            b"inf",
            b"Infinity",
            b"nan",
            b"snan",
            b"-inf",
            b"+nan",
            b"null",
            b"true",
            b"false",
        ] {
            let whole = run_adjacent(open, 0, None).expect("whole route accepts");
            let scoped = run_adjacent(open, 0, Some(&[])).expect("scoped route accepts");
            assert!(
                whole.report().open_ended(),
                "whole route must report {open:?} open-ended"
            );
            assert!(
                scoped.report().open_ended(),
                "scoped route must report {open:?} open-ended"
            );
        }
        // A delimiter closed the token, so neither route holds.
        for closed in [&b"1234 "[..], b"inf ", b"null ", b"[inf]", b"\"s\""] {
            let whole = run_adjacent(closed, 0, None).expect("whole route accepts");
            let scoped = run_adjacent(closed, 0, Some(&[])).expect("scoped route accepts");
            assert!(!whole.report().open_ended(), "whole route must not hold {closed:?}");
            assert!(!scoped.report().open_ended(), "scoped route must not hold {closed:?}");
        }
    }

    /// A window that runs out in the MIDDLE of a non-finite spelling is an incomplete token, not a wrong byte: both
    /// routes label the failure at the cut, which is what makes the streaming drive refill instead of failing.
    /// `inf`/`infinity` were the one token family that split into a hard error.
    #[test]
    fn a_nonfinite_spelling_cut_by_the_window_is_labeled_at_the_cut() {
        for cut in [
            &b"i"[..],
            b"in",
            b"infi",
            b"infinit",
            b"na",
            b"sna",
            b"-in",
            b"+i",
            b"[in",
            b"{\"a\":in",
        ] {
            let whole = run_adjacent(cut, 0, None).expect_err("incomplete token rejects");
            let scoped = run_adjacent(cut, 0, Some(&[])).expect_err("incomplete token rejects");
            let at = u32::try_from(cut.len()).expect("test input fits");
            assert_eq!(
                reject_diagnostic(&whole).1,
                at,
                "whole route must label {cut:?} at the cut"
            );
            assert_eq!(
                reject_diagnostic(&scoped).1,
                at,
                "scoped route must label {cut:?} at the cut"
            );
        }
        // The boundary violation is unchanged: a COMPLETE spelling butted against a non-boundary byte is one malformed
        // token at the offender.
        for (bad, at) in [(&b"nanx"[..], 3), (b"inf1", 3), (b"snanx", 4)] {
            assert_eq!(
                reject_diagnostic(&run_adjacent(bad, 0, None).expect_err("rejects")),
                ("invalid-literal", at)
            );
        }
    }

    /// A decode outcome reduced to everything two graders must agree on: the accept/reject decision, the structured
    /// reject (`json` code + primary-label byte position), and — on an ACCEPT — the report shape the streaming
    /// drive reads. An acceptance is not "the same acceptance" if one grader says the last token ran to the bytes' end
    /// and the other does not: the drive holds on one and publishes on the other, so the output depends on which route
    /// the program bound.
    #[derive(Debug, PartialEq, Eq)]
    struct Verdict {
        accepted: bool,
        code: alloc::string::String,
        position: u32,
        open_ended: bool,
        consumed_offset: Option<u64>,
    }

    /// Classifies a decode outcome into a comparable [`Verdict`]. Whole-parser and scoped-validate outcomes must map to
    /// equal verdicts on every input.
    fn verdict(result: Result<AccessResult<'_>, jqf_codec_core::CodecError>) -> Verdict {
        use alloc::string::String;
        match result {
            Ok(result) => Verdict {
                accepted: true,
                code: String::new(),
                position: 0,
                open_ended: result.report().open_ended(),
                consumed_offset: result.report().consumed_offset(),
            },
            Err(error) => {
                let (code, position) = if error.kind() == jqf_codec_core::CodecFailureKind::InvalidInput
                    && let Some(diagnostic) = error.diagnostic()
                {
                    (
                        String::from(diagnostic.code().name()),
                        diagnostic.labels().first().map_or(0, |label| label.span().start()),
                    )
                } else {
                    (String::from("<non-diagnostic>"), 0)
                };
                Verdict {
                    accepted: false,
                    code,
                    position,
                    open_ended: false,
                    consumed_offset: None,
                }
            }
        }
    }

    /// The standing grammar-drift fence for the ~450 duplicated lines between the whole parser
    /// ([`crate::parse::JsonParseState`]) and the scoped validate phase ([`crate::scoped::ScopedSession`]). It takes a
    /// set of valid seed documents that exercise every production, applies deterministic byte mutations to each, and
    /// asserts the two parsers return an identical verdict — the same accept/reject decision and, on a reject, the
    /// same `json` error code at the same byte position. A divergence means the duplicated grammars have drifted.
    /// Deterministic (fixed seed) and bounded, so it runs as an ordinary `cargo test`.
    #[test]
    fn scoped_validate_and_whole_parser_never_disagree_under_mutation() {
        const VARIANTS_PER_SEED: usize = 256;
        const SEEDS: &[&[u8]] = &[
            br#"{"a":1}"#,
            b"[1,2,3]",
            br#""hello""#,
            b"42",
            b"true",
            b"false",
            b"null",
            b"-0.5e10",
            br#"{"a":[1,{"b":"x"}],"c":null}"#,
            br#""esc\n\t\"\\\/ end""#,
            b"\"\xc3\xa9 raw utf8\"",
            b"[]",
            b"{}",
            b"\"\\ud83d\\ude00\"",
            b"1.5E-3",
            br#"{"k":{"k2":{"k3":[true,false,null]}}}"#,
            br#"[ 1 , 2 , { "x" : [ ] } ]"#,
        ];
        /// Bytes chosen to stress structural transitions, string/escape/number
        // lexing, and the UTF-8 and control-byte rejects.
        const PALETTE: &[u8] = &[
            b'"', b'\\', b',', b':', b'[', b']', b'{', b'}', b'0', b'9', b'-', b'.', b'e', b'E', b'+', b't', b'f',
            b'n', b'u', b'x', b' ', b'\n', b'\t', b'/', 0x00, 0x1f, 0x7f, 0x80, 0xc0, 0xff,
        ];

        // xorshift64* — a deterministic, dependency-free PRNG. `pick(n)` returns a value in `0..n` without a lossy
        // `u64 as usize` cast.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let mut pick = |n: usize| -> usize { usize::try_from(next() % n as u64).expect("modulus fits usize") };

        for seed in SEEDS {
            for _ in 0..VARIANTS_PER_SEED {
                let mut bytes = alloc::vec::Vec::from(*seed);
                if bytes.is_empty() {
                    continue;
                }
                let edits = 1 + pick(4);
                for _ in 0..edits {
                    let position = pick(bytes.len());
                    match pick(8) {
                        // Truncate here (a common real reject: an unterminated value or string).
                        0 => bytes.truncate(position.max(1)),
                        // Insert a palette byte.
                        1 => {
                            let insert = PALETTE[pick(PALETTE.len())];
                            bytes.insert(position, insert);
                        }
                        // Replace with a palette byte (the common case).
                        _ => bytes[position] = PALETTE[pick(PALETTE.len())],
                    }
                    if bytes.is_empty() {
                        bytes.push(b' ');
                    }
                }

                let hex = || {
                    bytes
                        .iter()
                        .map(|b| alloc::format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                let whole = verdict(run(&bytes, None, DiagnosticPolicy::ErrorsOnly));
                let scoped = verdict(run(&bytes, Some(&[]), DiagnosticPolicy::ErrorsOnly));
                assert_eq!(
                    whole,
                    scoped,
                    "grammar drift on {:?} (hex {}) : whole={whole:?} scoped={scoped:?}",
                    alloc::string::String::from_utf8_lossy(&bytes),
                    hex(),
                );
                // The same fence in adjacent-value mode, which is the ONLY mode that publishes a consumed offset and an
                // open-ended report — whole-document mode collapses both to nothing, so the single-document pass
                // above cannot see report-shape drift.
                let whole = verdict(run_adjacent(&bytes, 0, None));
                let scoped = verdict(run_adjacent(&bytes, 0, Some(&[])));
                assert_eq!(
                    whole,
                    scoped,
                    "adjacent grammar drift on {:?} (hex {}) : whole={whole:?} scoped={scoped:?}",
                    alloc::string::String::from_utf8_lossy(&bytes),
                    hex(),
                );
            }
        }
    }

    /// The UTF-8 pre-scan must reject a lead whose continuation is missing in the same position the whole parser does
    /// (the mutation test's hex 22 c3 2b... variant).
    #[test]
    fn scoped_utf8_pre_scan_rejects_a_bad_lead_like_the_whole_parser() {
        for bytes in [
            b"\"\xC3+ ra, u".as_slice(),
            b"\"\xE2\x82+ ra, u",
            b"\"\x80+ ra, u",
            b"\"\xC0\xAF\"",
            b"\"\xED\xA0\x80\"",
            b"\"\xF4\x90\x80\x80\"",
            b"\"\xF0\x8F\x80\x80\"",
            b"\"\xC3\xA9 ok\"",
            "\"héllo\"".as_bytes(),
            "日本語".as_bytes(),
        ] {
            let whole = verdict(run(bytes, None, DiagnosticPolicy::ErrorsOnly));
            let scoped = verdict(run(bytes, Some(&[]), DiagnosticPolicy::ErrorsOnly));
            assert_eq!(
                whole, scoped,
                "grammar drift on {bytes:02x?}: whole={whole:?} scoped={scoped:?}"
            );
        }
    }

    #[test]
    fn proved_utf8_skip_still_decodes_unicode_and_rejects_invalid() {
        assert_eq!(decoded_string("\"héllo 中文\"".as_bytes()), "héllo 中文");
        let scoped = run("\"héllo 中文\"".as_bytes(), Some(&[]), DiagnosticPolicy::ErrorsOnly).expect("unicode scoped");
        let Ok(jqf_data::Value::String(value)) = located(&scoped)
            .product()
            .document()
            .materialize_root(&mut witness_ledger())
        else {
            panic!("expected string")
        };
        assert_eq!(value.as_str(), "héllo 中文");
        run(b"\"\xC3+\"", None, DiagnosticPolicy::ErrorsOnly).expect_err("invalid utf-8 still rejects");
        run(b"\"\xC3+\"", Some(&[]), DiagnosticPolicy::ErrorsOnly).expect_err("invalid utf-8 still rejects on scoped");
    }

    /// The reader laws: the reference's non-finite number spellings are accepted with the same value semantics —
    /// `nan` (case-insensitive, optional sign) is a NUMBER whose NaN renders the null literal, `inf`/`infinity`
    /// (case-insensitive, optional sign) the clamped widest binary64 — and the value-boundary law rejects
    /// `nanx`/`inf1` wholesale.
    #[test]
    fn the_nonfinite_literals_are_accepted_with_reference_value_semantics() {
        // NaN renders `null` through the canonical number text (the same `format_binary64` law the encoder uses), so
        // the materialized value is a NUMBER, not null.
        for spelling in [&b"nan"[..], b"NaN", b"NAN", b"Nan", b"+nan", b"-nan"] {
            let result = run(spelling, None, DiagnosticPolicy::ErrorsOnly).expect("decodes");
            let AccessOutcome::FullDocument(product) = result.outcome() else {
                panic!("expected full document")
            };
            let Ok(jqf_data::Value::Number(number)) = product.document().materialize_root(&mut witness_ledger()) else {
                panic!("nan must decode as a number")
            };
            let float = number.as_float().expect("nan is a binary64");
            assert!(float.get().is_nan(), "{spelling:?} is NaN");
            assert_eq!(
                float.bits(),
                0x7ff8_0000_0000_0000,
                "{spelling:?} uses the fixed positive quiet NaN bits"
            );
        }
        for (spelling, expected) in [
            (&b"Infinity"[..], f64::INFINITY),
            (b"inf", f64::INFINITY),
            (b"infinity", f64::INFINITY),
            (b"INFINITY", f64::INFINITY),
            (b"iNf", f64::INFINITY),
            (b"+Infinity", f64::INFINITY),
            (b"-Infinity", f64::NEG_INFINITY),
            (b"-inf", f64::NEG_INFINITY),
        ] {
            let result = run(spelling, None, DiagnosticPolicy::ErrorsOnly).expect("decodes");
            let AccessOutcome::FullDocument(product) = result.outcome() else {
                panic!("expected full document")
            };
            let Ok(jqf_data::Value::Number(number)) = product.document().materialize_root(&mut witness_ledger()) else {
                panic!("infinity must decode as a number")
            };
            assert_eq!(
                number.as_float().map(jqf_data::Float::get),
                Some(expected),
                "{spelling:?} is the signed binary64 infinity"
            );
        }
        // The value-boundary law: a bare-word suffix is one malformed token.
        for spelling in [&b"nanx"[..], b"inf1", b"infinite"] {
            let result = run(spelling, None, DiagnosticPolicy::ErrorsOnly);
            assert!(result.is_err(), "{spelling:?} must be rejected (no value boundary)");
        }
        // Inside a container, exactly like the reference.
        let result = run(b"{\"a\":nan,\"b\":-Infinity}", None, DiagnosticPolicy::ErrorsOnly).expect("decodes");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected full document")
        };
        let Ok(jqf_data::Value::Object(object)) = product.document().materialize_root(&mut witness_ledger()) else {
            panic!("expected object root")
        };
        assert!(matches!(object.get("a"), Some(jqf_data::Value::Number(_))));
        assert!(matches!(object.get("b"), Some(jqf_data::Value::Number(_))));
    }

    /// RFC 8259 forbids a leading zero, and the refusal's message names the divergence from the reference's lenient
    /// reader.
    #[test]
    fn the_leading_zero_refusal_names_the_divergence() {
        for spelling in [&b"00"[..], b"01", b"007", b"00.5", b"-01"] {
            let error = run(spelling, None, DiagnosticPolicy::ErrorsOnly).expect_err("refused");
            assert_eq!(
                error.kind(),
                jqf_codec_core::CodecFailureKind::InvalidInput,
                "{spelling:?} is an invalid number"
            );
            let message = error.diagnostic().map(|d| alloc::string::String::from(d.message()));
            let message = message.unwrap_or_default();
            assert!(
                message.contains("leading zeros"),
                "{spelling:?} message names the divergence: {message}"
            );
        }
    }

    /// Encodes a decoded document's root through the JSON encoder's ordinary compact session, returning the complete
    /// bytes. `resources` must be the context the document was decoded under — the product's account anchor refuses a
    /// foreign ledger.
    fn encode_compact(
        product: &jqf_codec_core::DocumentProduct<'_>,
        handle: jqf_data::NodeHandle,
        resources: &mut ResourceContext<'static>,
    ) -> alloc::vec::Vec<u8> {
        use jqf_codec_core::{EncodeItem, EncodeRequest, PreservationRequest};

        let factory = registration()
            .expect("registration")
            .encoder()
            .expect("encoder")
            .create_factory(
                EncodeRequest {
                    format: &jqf_data::FormatId::try_new(FORMAT_ID).expect("format"),
                    dialect: &jqf_data::DialectId::try_new(RFC8259_DIALECT_ID).expect("dialect"),
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    preservation: PreservationRequest::Report,
                    options: None,
                },
                resources,
            )
            .expect("factory");
        let item = EncodeItem::try_located(product, handle).expect("located");
        let mut session = factory
            .start(item, PreservationRequest::Report, resources)
            .expect("start");
        let mut out = alloc::vec::Vec::new();
        {
            let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
            let mut run = jqf_codec_core::CodecRunContext::new(resources);
            run.set_cooperative_credits(4_096);
            session.encode(&mut sink, &mut run).expect("encode");
        }
        out
    }

    /// The encoder cursor's object-member cache must key on the OBJECT, not on the stack depth: `{"x":1}` (inside `a`)
    /// and `{"y":2}` (inside `f`) sit at the SAME cursor depth with the SAME member index, so a depth-only key would
    /// serve `x`'s entry for `y` and emit the second object as `{"x":1}`. The fix pins the current item in the cache
    /// key.
    #[test]
    fn encoder_member_cache_distinguishes_objects_at_the_same_depth() {
        let bytes = br#"{"a":[{"x":1}],"f":[{"y":2}]}"#;
        let mut resources = test_support::resources();
        let mut provider = registration()
            .expect("registration")
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::RFC8259_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: crate::VALUE_SEPARATORS,
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = requirement(None, DiagnosticPolicy::ErrorsOnly, &resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let outcome = {
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4_096);
            session.decode(&mut run).expect("decode")
        };
        let jqf_codec_core::AccessOutcome::FullDocument(product) = outcome.outcome() else {
            panic!("expected full document")
        };
        let encoded = encode_compact(product, product.document().root_handle(), &mut resources);
        assert_eq!(
            encoded, br#"{"a":[{"x":1}],"f":[{"y":2}]}"#,
            "the second object's member must be its own, not the first's"
        );
    }
}
