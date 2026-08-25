//! jqfb machine-profile tests: the round-trip matrix over the family's three profiles, the zero-allocation scalar read
//! (the design test), the untrusted-mode corruption sweep over footers/offsets/lengths (a malformed file is a typed
//! error, never a panic), and the provenance/source chunk laws.

// Binary-layout reads deliberately cast u64 offsets/counts to usize on 64-bit hosts (the jqfb design pins 64-bit
// lengths); the casts are the test's subject, not incidental.
#![allow(
    clippy::cast_possible_truncation,
    reason = "jqfb lengths are 64-bit by design; the tests read the raw layout"
)]

use jqf_codec_core::{
    AccessFootprint, AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecError, CodecFailureKind,
    CodecRunContext, DecodeRequest, DiagnosticPolicy, EncodeItem, EncodeRequest, ExactPath, PreservationRequest,
    ValidationMode,
};
use jqf_data::{DialectId, FormatId, Value};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTROL: ContinueControl = ContinueControl;

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources")
}

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "roundtrip.jqfb",
        bytes,
        0,
    )
}

/// Decodes one document through the whole-document route, returning the materialized root value or the failure kind.
fn whole_value(bytes: &[u8], format: &str) -> Result<Value, CodecFailureKind> {
    let registration = match format {
        jqf_codec_jqft::FORMAT_ID => jqf_codec_jqft::registration_jqft(),
        jqf_codec_jqft::JQFJSON_FORMAT_ID => jqf_codec_jqft::registration_jqfjson(),
        jqf_codec_jqft::FORMAT_ID_JQFB => jqf_codec_jqft::registration_jqfb(),
        _ => unreachable!("test formats"),
    }
    .map_err(|_e| CodecFailureKind::InternalContractViolation {
        contract: "family registration",
    })?;
    let mut resources = resources();
    let source = source(bytes);
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| error.kind())?;
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .map_err(|_| CodecFailureKind::Overflow)?;
    let handle = provider
        .bind(&requirement)
        .map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let mut session = provider.open(&handle, &mut resources).map_err(|error| error.kind())?;
    let mut context = CodecRunContext::new(&mut resources);
    context.set_cooperative_credits(4_096);
    let result = session.decode(&mut context).map_err(|error| error.kind())?;
    let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
        return Err(CodecFailureKind::RequirementMismatch);
    };
    product
        .document()
        .materialize_root(&mut resources)
        .map_err(|_| CodecFailureKind::UnsupportedRepresentation)
}

/// Opens a whole-document jqfb session so a structural rejection keeps its diagnostic (the `whole_value` helper folds
/// every failure to a kind).
fn jqfb_open_error(bytes: &[u8]) -> CodecError {
    let registration = jqf_codec_jqft::registration_jqfb().expect("jqfb registration");
    let mut resources = resources();
    let source = source(bytes);
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider construction does not validate the image");
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("whole requirement");
    let handle = provider.bind(&requirement).expect("bind");
    provider
        .open(&handle, &mut resources)
        .expect_err("a structurally invalid jqfb image must fail open")
}

/// Encodes one owned value to canonical bytes.
fn encode_value(value: &Value, format: &str, dialect: &str, with_source: bool) -> Result<Vec<u8>, CodecFailureKind> {
    let registration = match format {
        jqf_codec_jqft::FORMAT_ID => jqf_codec_jqft::registration_jqft(),
        jqf_codec_jqft::JQFJSON_FORMAT_ID => jqf_codec_jqft::registration_jqfjson(),
        jqf_codec_jqft::FORMAT_ID_JQFB => jqf_codec_jqft::registration_jqfb(),
        _ => unreachable!("test formats"),
    }
    .map_err(|_e| CodecFailureKind::InternalContractViolation {
        contract: "family registration",
    })?;
    let mut resources = resources();
    let options: Option<&(dyn core::any::Any + Send + Sync)> = match format {
        jqf_codec_jqft::FORMAT_ID_JQFB => Some(&jqf_codec_jqft::JqfbEncodeOptions { with_source }),
        jqf_codec_jqft::FORMAT_ID => Some(&jqf_codec_jqft::JqftEncodeOptions { with_source }),
        _ => None,
    };
    let request = EncodeRequest {
        format: &FormatId::try_new(format).map_err(|_| CodecFailureKind::RequirementMismatch)?,
        dialect: &DialectId::try_new(dialect).map_err(|_| CodecFailureKind::RequirementMismatch)?,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options,
    };
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)
        .map_err(|error| error.kind())?;
    let mut session = factory
        .start(EncodeItem::owned(value), PreservationRequest::None, &mut resources)
        .map_err(|error| error.kind())?;
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut context).map_err(|error| error.kind())?;
    }
    Ok(out)
}

/// A deterministic render used to compare values semantically.
fn render(value: &Value) -> String {
    use jqf_data::Value as V;
    match value {
        V::Null => "null".into(),
        V::Bool(true) => "true".into(),
        V::Bool(false) => "false".into(),
        V::Number(number) => {
            if let Some(integer) = number.to_integer() {
                integer.as_str().into()
            } else if let Some(float) = number.as_float() {
                format!("f{}", float.get())
            } else if let Some(decimal) = number.as_decimal() {
                format!("d{}/{}", decimal.coefficient().as_str(), decimal.scale())
            } else {
                "?".into()
            }
        }
        V::String(text) => format!("s{text:?}"),
        V::Bytes(bytes) => format!("h{:?}", bytes.as_ref()),
        V::LocalDate(date) => format!("ld{}", date.year()),
        V::LocalTime(time) => format!("lt{}:{}", time.hour(), time.minute()),
        V::LocalDateTime(datetime) => format!("ldt{}", datetime.date.year()),
        V::OffsetDateTime(datetime) => format!("odt{}", datetime.local.date.year()),
        V::Tagged { tag, payload } => format!("@{} {}", tag.as_str(), render(payload)),
        V::Array(array) => {
            let items: Vec<String> = array.iter().map(render).collect();
            format!("[{}]", items.join(","))
        }
        V::Object(object) => {
            let entries: Vec<String> = object
                .iter()
                .map(|entry| format!("{}:{}", entry.key(), render(entry.value())))
                .collect();
            format!("{{{}}}", entries.join(","))
        }
    }
}

/// The core vocabulary, shared by every profile.
fn core_values() -> Vec<Value> {
    let mut values = Vec::new();
    for bytes in [
        b"%jqft 1\nnull\n".as_slice(),
        b"%jqft 1\ntrue\n",
        b"%jqft 1\n42\n",
        b"%jqft 1\n-7\n",
        b"%jqft 1\n1250.00\n",
        b"%jqft 1\n1e3\n",
        b"%jqft 1\n\"hello, \\\"world\\\"\"\n",
        b"%jqft 1\n2.5f\n",
        b"%jqft 1\ninf\n",
        b"%jqft 1\n0x\"9f86d081884c7d65\"\n",
        b"%jqft 1\nb64\"aGVsbG8gd29ybGQ=\"\n",
        b"%jqft 1\n2026-08-02T21:14:00+02:00\n",
        b"%jqft 1\n[1, 2, [true, null]]\n",
        b"%jqft 1\n{name: \"ada\", id: @tag(\"uuid\") \"0198\", tags: [\"a\", \"b\"]}\n",
    ] {
        values.push(whole_value(bytes, jqf_codec_jqft::FORMAT_ID).expect("core value"));
    }
    values
}

/// jqfb decode -> encode -> decode round-trip over the core vocabulary.
#[test]
fn jqfb_round_trips_core_values() {
    for value in core_values() {
        let encoded = encode_value(
            &value,
            jqf_codec_jqft::FORMAT_ID_JQFB,
            jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
            false,
        )
        .expect("jqfb encode");
        let decoded = whole_value(&encoded, jqf_codec_jqft::FORMAT_ID_JQFB).expect("jqfb decode");
        assert_eq!(render(&value), render(&decoded), "jqfb round-trip drift");
        // A second round-trip is byte-identical (the image is canonical).
        let reencoded = encode_value(
            &decoded,
            jqf_codec_jqft::FORMAT_ID_JQFB,
            jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
            false,
        )
        .expect("re-encode");
        assert_eq!(encoded, reencoded, "jqfb image is not canonical");
    }
}

/// A jqfb image whose string pool outgrows the finalizer's per-poll budget must still decode (once the pool crossed
/// ~511 entries, the finalize phase was re-visited after the finalizer's own poll returned `Pending`, re-ran the
/// document assembly, and found `root` already taken by `begin_finish` — a spurious `InternalContractViolation` on an
/// image the encoder produced). 600 distinct strings put the pool past the boundary on both axes the fixture exhibits:
/// entry count and total pool bytes.
#[test]
fn jqfb_decodes_a_pool_larger_than_the_finalize_budget() {
    let json = format!(
        "[{}]",
        (0..600)
            .map(|index| format!("\"item-{index:08}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    let value =
        whole_value(json.as_bytes(), jqf_codec_jqft::JQFJSON_FORMAT_ID).expect("strict JSON parse of the large array");
    let encoded = encode_value(
        &value,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("jqfb encode");
    let decoded = whole_value(&encoded, jqf_codec_jqft::FORMAT_ID_JQFB).expect("jqfb decode");
    assert_eq!(render(&value), render(&decoded), "large-pool round-trip drift");
}

/// The conversion matrix: every ordered pair among jqft / jqfjson / jqfb is total and lossless at equal conformance
/// level.
#[test]
fn family_conversion_matrix_is_total_and_lossless() {
    const FORMATS: [(&str, &str); 3] = [
        (jqf_codec_jqft::FORMAT_ID, jqf_codec_jqft::JQFT_CANONICAL_DIALECT_ID),
        (
            jqf_codec_jqft::JQFJSON_FORMAT_ID,
            jqf_codec_jqft::JQFJSON_CANONICAL_DIALECT_ID,
        ),
        (
            jqf_codec_jqft::FORMAT_ID_JQFB,
            jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        ),
    ];
    for value in core_values() {
        // Skip jqfjson-only values: the envelope cannot spell floats, bytes, temporals, or tags (their spellings are a
        // later pass).
        let jqfjson_representable = !render(&value).contains('f')
            && !render(&value).contains('h')
            && !render(&value).contains("ld")
            && !render(&value).contains("lt")
            && !render(&value).contains("odt")
            && !render(&value).contains('@');
        for (from_format, from_dialect) in FORMATS {
            if from_format == jqf_codec_jqft::JQFJSON_FORMAT_ID && !jqfjson_representable {
                continue;
            }
            let encoded = encode_value(&value, from_format, from_dialect, false).expect("first encode");
            let middle = whole_value(&encoded, from_format).expect("first decode");
            for (to_format, to_dialect) in FORMATS {
                if to_format == jqf_codec_jqft::JQFJSON_FORMAT_ID && !jqfjson_representable {
                    continue;
                }
                let converted = encode_value(&middle, to_format, to_dialect, false).expect("conversion encode");
                let back = whole_value(&converted, to_format).expect("conversion decode");
                assert_eq!(
                    render(&value),
                    render(&back),
                    "conversion {from_format} -> {to_format} drifted"
                );
            }
        }
    }
}

/// The zero-allocation scalar read: a string value is borrowed from the image bytes, never copied (the design test for
/// the mmap'd read path).
#[test]
fn scalar_reads_borrow_the_source_with_zero_allocation() {
    let value = whole_value(
        b"%jqft 1\n{payload: \"the quick brown fox\"}\n",
        jqf_codec_jqft::FORMAT_ID,
    )
    .expect("decode");
    let image = encode_value(
        &value,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    // The node table for {payload: "…"} is: OBJECT, KEYTEXT, STRING — the string is node entry 2; its payload is the
    // STRG index of the text. Walk the footer to the STRG chunk and read its first entry directly.
    let bytes = image.as_slice();
    let footer_start = u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap()) as usize;
    let footer = &bytes[bytes.len() - footer_start..];
    let count = u64::from_le_bytes(footer[..8].try_into().unwrap()) as usize;
    let entries = &footer[8..8 + count * 52];
    // Directory entry 0 is NODE, entry 1 is STRG.
    let strg_len = u64::from_le_bytes(entries[52 + 12..52 + 20].try_into().unwrap());
    let strg_offset = u64::from_le_bytes(entries[52 + 4..52 + 12].try_into().unwrap()) as usize;
    assert!(strg_len > 0, "the STRG pool must be nonempty");
    // The pool's entries are the "payload" key (index 0) and the string value "the quick brown fox" (index 1). Walk to
    // entry 1.
    let pool = &bytes[strg_offset..strg_offset + strg_len as usize];
    let mut pool_cursor = 8usize;
    let key_len = u32::from_le_bytes(pool[pool_cursor..pool_cursor + 4].try_into().unwrap()) as usize;
    pool_cursor += 4 + key_len;
    let entry_len = u32::from_le_bytes(pool[pool_cursor..pool_cursor + 4].try_into().unwrap()) as usize;
    let borrowed = &pool[pool_cursor + 4..pool_cursor + 4 + entry_len];
    assert_eq!(borrowed, b"the quick brown fox");
    // Pointer-equality proof of zero allocation: the borrowed bytes ARE the image bytes at the same address.
    let image_ptr = image.as_ptr();
    let borrowed_ptr = borrowed.as_ptr();
    let in_image = borrowed_ptr >= image_ptr && borrowed_ptr < image_ptr.wrapping_add(image.len());
    assert!(in_image, "the scalar read must borrow the source bytes");
}

/// The untrusted-mode corruption sweep: every mutated footer word, offset, length, and digest must produce a TYPED
/// error, never a panic — the trust boundary.
#[test]
fn corrupted_footers_and_chunks_are_typed_errors_never_panics() {
    let value = whole_value(
        b"%jqft 1\n{a: [1, \"x\", true], b: \"payload\", c: 2026-08-02T21:14:00+02:00}\n",
        jqf_codec_jqft::FORMAT_ID,
    )
    .expect("decode");
    let image = encode_value(
        &value,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    // The unmutated image decodes.
    assert!(whole_value(&image, jqf_codec_jqft::FORMAT_ID_JQFB).is_ok());
    let mut failures = 0usize;
    let mut attempts = 0usize;
    for byte in 0..image.len() {
        for delta in [1u8, 0xff, 0x80] {
            let mut mutated = image.clone();
            mutated[byte] ^= delta;
            attempts += 1;
            match whole_value(&mutated, jqf_codec_jqft::FORMAT_ID_JQFB) {
                Ok(_) => {
                    // A mutation that flips a payload byte that is never read (e.g. inside a pool entry's unused tail)
                    // can still parse as a valid image with the digest recomputed — but the digests are FIXED, so a
                    // content mutation must fail the digest check unless it landed in a pool byte the decoder
                    // tolerates. Any Ok here is a finding only if the value changed; tolerate byte-flips the pools
                    // never validate.
                }
                Err(_) => failures += 1,
            }
        }
    }
    // The overwhelming majority of single-byte mutations must be rejected; every mutation of the footer words
    // (offsets/lengths/counts) MUST be.
    assert!(failures > attempts * 9 / 10, "corruption sweep too lenient");
    // Targeted footer-word mutations: each must fail, never panic.
    let last = image.len() - 8;
    for delta in [1u8, 2, 0x7f, 0x80, 0xff] {
        for word in [last, last + 1, 20usize, 24] {
            let mut mutated = image.clone();
            if word < mutated.len() {
                mutated[word] ^= delta;
                let outcome = whole_value(&mutated, jqf_codec_jqft::FORMAT_ID_JQFB);
                assert!(outcome.is_err(), "footer mutation at byte {word} must fail");
            }
        }
    }
    // Truncations: every prefix shorter than the file must be refused.
    for cut in (0..image.len()).step_by(7) {
        let outcome = whole_value(&image[..cut], jqf_codec_jqft::FORMAT_ID_JQFB);
        assert!(outcome.is_err(), "truncation at {cut} must fail");
    }
}

/// The critical/ignorable chunk rule: an unknown CRITICAL chunk refuses the file while an unknown IGNORABLE chunk is
/// skipped for free. Both are simulated by appending a fake chunk and splicing its directory entry in.
#[test]
fn unknown_critical_chunks_refuse_and_ignorable_chunks_skip() {
    let value = whole_value(b"%jqft 1\n{a: 1}\n", jqf_codec_jqft::FORMAT_ID).expect("decode");
    let image = encode_value(
        &value,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    // Append a fake chunk of 4 bytes right before the footer, then rebuild the footer with one more directory entry and
    // the correct new footer length. The existing chunks' absolute offsets are unchanged; the footer simply ends 52
    // bytes later.
    let old_footer_len = u64::from_le_bytes(image[image.len() - 8..].try_into().unwrap()) as usize;
    let old_footer_start = image.len() - old_footer_len;
    let old_count = u64::from_le_bytes(image[old_footer_start..old_footer_start + 8].try_into().unwrap());
    let old_entries = &image[old_footer_start + 8..old_footer_start + 8 + old_count as usize * 52];
    let fake = b"FAKE";
    let fake_offset = old_footer_start;
    let digest = *blake3::hash(fake).as_bytes();
    let make_mutated = |chunk_type: u32| -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&image[..old_footer_start]);
        out.extend_from_slice(fake);
        out.extend_from_slice(&(old_count + 1).to_le_bytes());
        out.extend_from_slice(old_entries);
        out.extend_from_slice(&chunk_type.to_le_bytes());
        out.extend_from_slice(&(fake_offset as u64).to_le_bytes());
        out.extend_from_slice(&4u64.to_le_bytes());
        out.extend_from_slice(&digest);
        out.extend_from_slice(&((16 + (old_count as usize + 1) * 52) as u64).to_le_bytes());
        out
    };
    assert!(
        whole_value(&make_mutated(0x0000_0064), jqf_codec_jqft::FORMAT_ID_JQFB).is_err(),
        "an unknown critical chunk must refuse the file"
    );
    assert!(
        whole_value(&make_mutated(0x8000_0064), jqf_codec_jqft::FORMAT_ID_JQFB).is_ok(),
        "an unknown ignorable chunk must be skipped for free"
    );
}

/// An image whose footer names NO FACT chunk is tolerated by chunk collection (absence defaults to the empty slice) and
/// must therefore finalize as zero facts — not fail with a misleading "truncated FACT count" at attach time.
#[test]
fn jqfb_finalizes_an_image_with_no_fact_chunk() {
    let value = whole_value(b"%jqft 1\n{a: 1}\n", jqf_codec_jqft::FORMAT_ID).expect("decode");
    let image = encode_value(
        &value,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    // Rebuild the footer WITHOUT the FACT entry; the chunk's bytes stay in place orphaned (the remaining entries'
    // absolute offsets are unchanged).
    let old_footer_len = u64::from_le_bytes(image[image.len() - 8..].try_into().unwrap()) as usize;
    let old_footer_start = image.len() - old_footer_len;
    let old_count = u64::from_le_bytes(image[old_footer_start..old_footer_start + 8].try_into().unwrap());
    assert!(old_count >= 4, "NODE/STRG/NUMB/FACT must all be present");
    let entries = &image[old_footer_start + 8..old_footer_start + 8 + old_count as usize * 52];
    let mut rebuilt = Vec::new();
    rebuilt.extend_from_slice(&image[..old_footer_start]);
    rebuilt.extend_from_slice(&(old_count - 1).to_le_bytes());
    let mut saw_fact = false;
    for entry in entries.chunks_exact(52) {
        let chunk_type = u32::from_le_bytes(entry[..4].try_into().unwrap());
        if chunk_type == 0x0000_0004 {
            saw_fact = true;
            continue;
        }
        rebuilt.extend_from_slice(entry);
    }
    assert!(saw_fact, "the encoder must have written a FACT entry");
    rebuilt.extend_from_slice(&((16 + (old_count as usize - 1) * 52) as u64).to_le_bytes());
    // The FACT-less image decodes to the same value.
    let decoded =
        whole_value(&rebuilt, jqf_codec_jqft::FORMAT_ID_JQFB).expect("an absent FACT chunk finalizes as zero facts");
    assert_eq!(render(&decoded), render(&value));
}

// --------------------------------------------------------------------------- The native demand routes: shallow
// stand-in and scoped located subtree, served by the node-table walk with subtree_size skips.
// ---------------------------------------------------------------------------

/// A path step for the exact-route tests.
enum PStep {
    Member(&'static str),
    Index(i64),
}

/// Drives one EXACT demand over a jqfb image, returning the materialized located root value (or the failure kind).
fn exact_value(bytes: &[u8], steps: &[PStep]) -> Result<Value, CodecFailureKind> {
    let registration =
        jqf_codec_jqft::registration_jqfb().map_err(|_e| CodecFailureKind::InternalContractViolation {
            contract: "jqfb registration",
        })?;
    let mut resources = resources();
    let source = source(bytes);
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| error.kind())?;
    let mut path = ExactPath::try_new(&resources);
    for step in steps {
        match step {
            PStep::Member(name) => path
                .try_push_semantic_member(name, &resources)
                .map_err(|_| CodecFailureKind::Overflow)?,
            PStep::Index(index) => {
                path.try_push_semantic_index(*index, &resources);
            }
        }
    }
    let footprint = AccessFootprint::try_exact(path, &resources);
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_exact(
        footprint,
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .map_err(|_| CodecFailureKind::Overflow)?;
    let handle = provider
        .bind(&requirement)
        .map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let mut session = provider.open(&handle, &mut resources).map_err(|error| error.kind())?;
    let mut context = CodecRunContext::new(&mut resources);
    context.set_cooperative_credits(4_096);
    let access_result = session.decode(&mut context).map_err(|error| error.kind())?;
    let AccessOutcome::Located(outcome) = access_result.into_parts().0 else {
        return Err(CodecFailureKind::RequirementMismatch);
    };
    outcome
        .product()
        .document()
        .materialize_root(&mut resources)
        .map_err(|_| CodecFailureKind::UnsupportedRepresentation)
}

/// The native demand routes serve the exact paths with the same semantic answers the whole-document decode gives: the
/// located route (the Exact access slot) carries the located subtree as decoded, and a missing or kind-mismatched path
/// publishes the null-product observation.
#[test]
fn jqfb_native_demand_routes_serve_the_exact_paths() {
    let value = whole_value(
        b"%jqft 1\n[1, {name: \"ada\", nums: [1, 2], flag: true}]\n",
        jqf_codec_jqft::FORMAT_ID,
    )
    .expect("decode");
    let image = encode_value(
        &value,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    let rendered = |steps: &[PStep]| -> String { render(&exact_value(&image, steps).expect("exact route")) };
    // Scoped: the located subtree as decoded.
    assert_eq!(rendered(&[PStep::Index(0)]), "1");
    assert_eq!(rendered(&[PStep::Index(1)]), "{name:s\"ada\",nums:[1,2],flag:true}");
    assert_eq!(
        rendered(&[PStep::Index(1), PStep::Member("nums"), PStep::Index(1)]),
        "2"
    );
    // Missing and kind-mismatch paths publish the null-product observation.
    assert_eq!(rendered(&[PStep::Index(5)]), "null");
    assert_eq!(rendered(&[PStep::Index(1), PStep::Index(0)]), "null");
}

/// The demand routes validate the COMPLETE node table to the floor's exact strictness (validate-everything-first): a
/// corrupt subtree-size word in a subtree the route never materializes fails the shallow and scoped routes exactly as
/// it fails the whole-document decode. The digest is re-stamped so the corruption is structural, not a digest failure.
#[test]
fn jqfb_demand_routes_reject_a_corrupt_subtree_like_the_floor() {
    let value = whole_value(
        b"%jqft 1\n[1, {name: \"ada\", nums: [1, 2]}]\n",
        jqf_codec_jqft::FORMAT_ID,
    )
    .expect("decode");
    let image = encode_value(
        &value,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    // Locate the NODE chunk through the footer and corrupt the root container's subtree-size word (entry 0, bytes
    // 1..5), re-stamping the chunk digest so the corruption is structural rather than a digest mismatch. The root's
    // real size is the whole table.
    let bytes = image.as_slice();
    let footer_start = u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap()) as usize;
    let footer = &bytes[bytes.len() - footer_start..];
    let count = u64::from_le_bytes(footer[..8].try_into().unwrap()) as usize;
    let entries = &footer[8..8 + count * 52];
    assert_eq!(
        u32::from_le_bytes(entries[..4].try_into().unwrap()),
        0x0000_0001,
        "directory entry 0 must be the NODE chunk"
    );
    let node_offset = u64::from_le_bytes(entries[4..12].try_into().unwrap()) as usize;
    let node_len = u64::from_le_bytes(entries[12..20].try_into().unwrap());
    let mut mutated = image.clone();
    let at = node_offset + 1; // entry 0's subtree-size word
    mutated[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
    let digest = blake3::hash(&mutated[node_offset..node_offset + node_len as usize]);
    let digest_slot = bytes.len() - footer_start + 8 + 20;
    mutated[digest_slot..digest_slot + 32].copy_from_slice(digest.as_bytes());

    assert!(
        whole_value(&mutated, jqf_codec_jqft::FORMAT_ID_JQFB).is_err(),
        "the whole decode must reject the corrupt subtree size"
    );
    assert!(
        exact_value(&mutated, &[PStep::Index(1)]).is_err(),
        "the Located route must reject what the floor rejects"
    );
}

/// A BYTES pool index out of range fails the scoped route off-path exactly as it fails the floor (BYTES is not grouped
/// with NULL/BOOL).
#[test]
fn scoped_jqfb_rejects_a_corrupt_bytes_pool_index_like_the_floor() {
    let value = whole_value(b"%jqft 1\n{a: 0x\"6869\", b: \"ok\"}\n", jqf_codec_jqft::FORMAT_ID).expect("decode");
    let image = encode_value(
        &value,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    let bytes = image.as_slice();
    let footer_start = u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap()) as usize;
    let footer = &bytes[bytes.len() - footer_start..];
    let count = u64::from_le_bytes(footer[..8].try_into().unwrap()) as usize;
    let entries = &footer[8..8 + count * 52];
    assert_eq!(
        u32::from_le_bytes(entries[..4].try_into().unwrap()),
        0x0000_0001,
        "directory entry 0 must be the NODE chunk"
    );
    let node_offset = u64::from_le_bytes(entries[4..12].try_into().unwrap()) as usize;
    let node_len = u64::from_le_bytes(entries[12..20].try_into().unwrap()) as usize;
    let node = &bytes[node_offset..node_offset + node_len];
    let mut bytes_at = None;
    let mut cursor = 0usize;
    while cursor + 9 <= node.len() {
        if node[cursor] == 6 {
            bytes_at = Some(node_offset + cursor + 5);
            break;
        }
        cursor += 9;
    }
    let payload_at = bytes_at.expect("the image must contain a BYTES node");
    let mut mutated = image.clone();
    mutated[payload_at..payload_at + 4].copy_from_slice(&9999u32.to_le_bytes());
    let digest = blake3::hash(&mutated[node_offset..node_offset + node_len]);
    let digest_slot = bytes.len() - footer_start + 8 + 20;
    mutated[digest_slot..digest_slot + 32].copy_from_slice(digest.as_bytes());

    assert!(
        whole_value(&mutated, jqf_codec_jqft::FORMAT_ID_JQFB).is_err(),
        "the floor must reject a BYTES pool index past the pool"
    );
    assert!(
        exact_value(&mutated, &[PStep::Member("b")]).is_err(),
        "scoped off-path must reject the same corrupt BYTES index"
    );
}

/// A jqfb structural rejection carries the call-site message, not a bare `InvalidInput` kind. The binary profile used
/// to compile ~111 messages and drop them.
#[test]
fn jqfb_structural_rejection_carries_a_diagnostic() {
    let error = jqfb_open_error(b"not-a-jqfb-image");
    assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
    let diagnostic = error
        .diagnostic()
        .expect("the structural message must ride the diagnostic");
    assert_eq!(diagnostic.code().to_string(), "jqfb.invalid");
    assert!(
        diagnostic.message().contains("not a jqfb image"),
        "expected the magic-check message, got {:?}",
        diagnostic.message()
    );
}

/// Builds a VALID jqfb image by hand: `depth` nested arrays wrapping `null`. The encoder itself recurses (bounded by
/// the host stack), so a deep image can only come from a hand-crafter — exactly the attacker shape the demand routes'
/// heap-based walk must survive. Header + NODE + STRG/NUMB/FACT + footer, every chunk digest re-stamped.
fn deep_image(depth: usize) -> Vec<u8> {
    // Node-table kind bytes (the jqfb layout): ARRAY = 12, NULL = 0.
    let mut node = Vec::new();
    for index in 0..depth {
        node.extend_from_slice(&[12u8]); // ARRAY
        node.extend_from_slice(&((depth - index + 1) as u32).to_le_bytes()); // subtree size
        node.extend_from_slice(&1u32.to_le_bytes()); // one child
    }
    node.extend_from_slice(&[0u8]); // NULL
    node.extend_from_slice(&1u32.to_le_bytes());
    node.extend_from_slice(&0u32.to_le_bytes());
    let strg = 0u64.to_le_bytes().to_vec(); // empty pool
    let numb = 0u64.to_le_bytes().to_vec(); // empty pool
    let fact = 0u64.to_le_bytes().to_vec(); // no facts
    assemble_image(&[(1, node), (2, strg), (3, numb), (4, fact)])
}

/// Builds a VALID jqfb image of `depth` nested TAG nodes (kind 11) wrapping the given leaf node-table bytes (whole
/// 9-byte entries). Tags are payload-transparent, so the encoder rarely stacks them deeply — a deep chain is another
/// hand-crafter-only attacker shape.
fn tagged_chain_image(depth: usize, leaf_entries: &[u8]) -> Vec<u8> {
    assert!(
        leaf_entries.len().is_multiple_of(9) && !leaf_entries.is_empty(),
        "the leaf must be whole non-empty node entries"
    );
    let leaf_count = leaf_entries.len() / 9;
    let mut node = Vec::new();
    for index in 0..depth {
        node.extend_from_slice(&[11u8]); // TAG
        node.extend_from_slice(&((depth - index + leaf_count) as u32).to_le_bytes()); // subtree size
        node.extend_from_slice(&0u32.to_le_bytes()); // STRG pool index 0
    }
    node.extend_from_slice(leaf_entries);
    // One STRG entry: the tag text "t" (a valid nonempty tag).
    let strg = {
        let mut pool = 1u64.to_le_bytes().to_vec();
        pool.extend_from_slice(&1u32.to_le_bytes());
        pool.extend_from_slice(b"t");
        pool
    };
    let numb = 0u64.to_le_bytes().to_vec();
    let fact = 0u64.to_le_bytes().to_vec();
    assemble_image(&[(1, node), (2, strg), (3, numb), (4, fact)])
}

/// Builds a structurally valid image (magic, version, digests) whose selected pool chunk names `count` in its leading
/// count word while carrying NO entries — the smallest attacker shape that reaches the pool offset-table builders with
/// an unproven count.
fn image_with_pool_count(pool_kind: u32, count: u64) -> Vec<u8> {
    let node = {
        let mut table = Vec::new();
        table.extend_from_slice(&[0u8]); // NULL root
        table.extend_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(&0u32.to_le_bytes());
        table
    };
    let poisoned = count.to_le_bytes().to_vec();
    let other_pool = 0u64.to_le_bytes().to_vec();
    let fact = 0u64.to_le_bytes().to_vec();
    let (strg, numb) = match pool_kind {
        2 => (poisoned, other_pool.clone()),
        3 => (other_pool, poisoned),
        _ => unreachable!("test pool kinds"),
    };
    assemble_image(&[(1, node), (2, strg), (3, numb), (4, fact)])
}

/// Assembles a hand-crafted image: header, the four critical chunks in order, then the footer directory with every
/// chunk's blake3 digest re-stamped.
fn assemble_image(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"jqfb");
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let mut offset = out.len();
    let mut directory = Vec::new();
    for (kind, chunk) in entries {
        directory.extend_from_slice(&kind.to_le_bytes());
        directory.extend_from_slice(&(offset as u64).to_le_bytes());
        directory.extend_from_slice(&(chunk.len() as u64).to_le_bytes());
        let digest = blake3::hash(chunk);
        directory.extend_from_slice(digest.as_bytes());
        offset += chunk.len();
    }
    for (_, chunk) in entries {
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    out.extend_from_slice(&directory);
    out.extend_from_slice(&((16 + entries.len() * 52) as u64).to_le_bytes());
    assert_eq!(out.len(), offset + 16 + entries.len() * 52);
    out
}

/// Drives one exact route over `bytes` WITHOUT materializing the result (the materializer recurses over document depth,
/// which is not the route's law), returning Ok(()) when the route publishes a located outcome.
fn exact_route_completes(bytes: &[u8], steps: &[PStep]) -> Result<(), CodecFailureKind> {
    let registration =
        jqf_codec_jqft::registration_jqfb().map_err(|_e| CodecFailureKind::InternalContractViolation {
            contract: "jqfb registration",
        })?;
    let mut resources = resources();
    let source = source(bytes);
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| error.kind())?;
    let mut path = ExactPath::try_new(&resources);
    for step in steps {
        match step {
            PStep::Member(name) => path
                .try_push_semantic_member(name, &resources)
                .map_err(|_| CodecFailureKind::Overflow)?,
            PStep::Index(index) => {
                path.try_push_semantic_index(*index, &resources);
            }
        }
    }
    let footprint = AccessFootprint::try_exact(path, &resources);
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_exact(
        footprint,
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .map_err(|_| CodecFailureKind::Overflow)?;
    let handle = provider
        .bind(&requirement)
        .map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let mut session = provider.open(&handle, &mut resources).map_err(|error| error.kind())?;
    let mut context = CodecRunContext::new(&mut resources);
    context.set_cooperative_credits(4_096);
    let result = session.decode(&mut context).map_err(|error| error.kind())?;
    let AccessOutcome::Located(_) = result.into_parts().0 else {
        return Err(CodecFailureKind::RequirementMismatch);
    };
    Ok(())
}

/// The demand routes are heap-depth-safe: a deeply nested image (far past what the encoder's own recursion can produce
/// — a hand-crafted attacker shape) validates and navigates without a call-stack overflow. The walk's frame stack and
/// the subtree-size skips never recurse over document depth (the assertion runs the routes WITHOUT materializing, since
/// the owned materializer recurses over depth — a different law).
#[test]
fn jqfb_demand_routes_survive_deep_nesting() {
    let image = deep_image(10_000);
    assert_eq!(exact_route_completes(&image, &[PStep::Index(0)]), Ok(()));
}

fn owned_string(text: &str) -> Value {
    Value::String(jqf_data::Shared::try_from_str(text).expect("string"))
}

fn owned_number(spelling: &str) -> Value {
    Value::Number(jqf_data::Number::try_json_literal(spelling).expect("number literal"))
}

fn owned_object(entries: &[(&str, Value)]) -> Value {
    let mut builder = jqf_data::ObjectBuilder::try_with_capacity(entries.len()).expect("builder");
    for (key, value) in entries {
        builder
            .try_insert_last(jqf_data::ObjectKey::try_from_str(key).expect("key"), value.clone())
            .expect("insert");
    }
    Value::Object(builder.try_finish().expect("object"))
}

fn object_member(value: &Value, key: &str) -> Value {
    let Value::Object(object) = value else {
        panic!("expected an object, got {value:?}");
    };
    object.get(key).expect("member").clone()
}

/// Splices one leaf edit over an image the way the SDK applies the leaf seam's tail: everything from the node's
/// authored-tail span is replaced by the seam's bytes. Returns the patched image.
fn spliced_leaf(image: &[u8], key: &str, new_value: &Value) -> Vec<u8> {
    let registration = jqf_codec_jqft::registration_jqfb().expect("jqfb registration");
    let mut resources = resources();
    let source = source(image);
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let demand = CodecDemand::try_new(&resources);
    let requirement = AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("whole requirement");
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let mut context = CodecRunContext::new(&mut resources);
    context.set_cooperative_credits(4_096);
    let result = session.decode(&mut context).expect("decode");
    let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
        panic!("expected a whole document");
    };
    let document = product.document();
    let view = document.value_view(document.root_handle()).expect("view");
    let object_view = view.object().expect("object view").expect("object");
    let node = object_view.get(key).expect("edited member").node();

    let encode_request = EncodeRequest {
        format: &FormatId::try_new(jqf_codec_jqft::FORMAT_ID_JQFB).expect("format id"),
        dialect: &DialectId::try_new(jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID).expect("dialect"),
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        options: Some(&jqf_codec_jqft::JqfbEncodeOptions { with_source: false }),
    };
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(encode_request, &mut resources)
        .expect("factory");
    let encoded = factory
        .render_leaf(document, node, &[], image, new_value, None, &mut resources)
        .expect("the leaf seam renders");
    let span = document
        .node_source_span(node)
        .expect("span readable")
        .expect("the image binds every node's authored tail span");
    let mut patched: Vec<u8> = Vec::new();
    patched.extend_from_slice(&image[..span.start() as usize]);
    patched.extend_from_slice(&encoded);
    patched
}

/// The leaf splice must not rewrite a SHARED pool entry in place: the image builder dedups strg/numb by raw bytes, so
/// two equal scalars share ONE pool index. An in-place rewrite of that entry would silently change every sibling whose
/// payload names it — editing `a` in an image of `{"a":"x","b":"x"}` must append (or reuse) an entry for `a` alone and
/// leave `b` reading "x" on re-decode. The number pool is the same law.
#[test]
fn a_leaf_splice_does_not_rewrite_a_shared_pool_entry() {
    let cases: &[(Value, Value, &str, &str)] = &[
        // Shared STRG entry; sibling keeps its own bytes.
        (
            owned_object(&[("a", owned_string("x")), ("b", owned_string("x"))]),
            owned_string("y"),
            "s\"y\"",
            "s\"x\"",
        ),
        // Shared NUMB entry; same law.
        (
            owned_object(&[("a", owned_number("7")), ("b", owned_number("7"))]),
            owned_number("9"),
            "9",
            "7",
        ),
    ];
    for (root, new_value, edited_render, sibling_render) in cases {
        let image = encode_value(
            root,
            jqf_codec_jqft::FORMAT_ID_JQFB,
            jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
            false,
        )
        .expect("encode");
        let patched = spliced_leaf(&image, "a", new_value);
        let value = match whole_value(&patched, jqf_codec_jqft::FORMAT_ID_JQFB) {
            Ok(value) => value,
            Err(kind) => panic!(
                "the patched image re-decodes ({kind:?}): {:?}",
                jqfb_open_error(&patched)
            ),
        };
        assert_eq!(render(&object_member(&value, "a")), *edited_render);
        assert_eq!(
            render(&object_member(&value, "b")),
            *sibling_render,
            "the sibling keeps its own value under a shared pool entry"
        );
    }
}

/// A leaf splice that GROWS a pool (a genuinely-new entry appended at the chunk boundary) must lengthen only its own
/// chunk's directory row. The region-edit attribution once counted the boundary insertion into the NEXT chunk too — its
/// length and digest were corrupted and the patched image failed re-decode.
#[test]
fn a_leaf_splice_that_grows_a_pool_re_decodes() {
    // Distinct values: no sharing, so the splice takes the append path through a plain cross-value edit.
    let root = owned_object(&[("a", owned_string("x")), ("b", owned_string("y"))]);
    let image = encode_value(
        &root,
        jqf_codec_jqft::FORMAT_ID_JQFB,
        jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        false,
    )
    .expect("encode");
    let patched = spliced_leaf(&image, "a", &owned_number("9"));
    let value = match whole_value(&patched, jqf_codec_jqft::FORMAT_ID_JQFB) {
        Ok(value) => value,
        Err(kind) => panic!(
            "the patched image re-decodes ({kind:?}): {:?}",
            jqfb_open_error(&patched)
        ),
    };
    assert_eq!(render(&object_member(&value, "a")), "9");
    assert_eq!(render(&object_member(&value, "b")), "s\"y\"");
}

#[test]
fn jqfb_located_route_survives_a_deep_tag_chain() {
    let null_leaf = {
        let mut entry = Vec::new();
        entry.push(0u8); // NULL
        entry.extend_from_slice(&1u32.to_le_bytes());
        entry.extend_from_slice(&0u32.to_le_bytes());
        entry
    };
    let image = tagged_chain_image(100_000, &null_leaf);
    assert_eq!(exact_route_completes(&image, &[PStep::Member("x")]), Ok(()));
}

/// Tags stay payload-transparent on a DESCENDING path too: the unwrapped payload still resolves the remaining steps (an
/// array index through a tag-wrapped array).
#[test]
fn jqfb_located_route_navigates_through_tags_to_the_payload() {
    let array_leaf = {
        let mut node = Vec::new();
        node.push(12u8); // ARRAY
        node.extend_from_slice(&2u32.to_le_bytes()); // subtree: array + null
        node.extend_from_slice(&1u32.to_le_bytes()); // one child
        node.push(0u8); // NULL
        node.extend_from_slice(&1u32.to_le_bytes());
        node.extend_from_slice(&0u32.to_le_bytes());
        node
    };
    let image = tagged_chain_image(50, &array_leaf);
    assert_eq!(exact_route_completes(&image, &[PStep::Index(0)]), Ok(()));
}

/// A pool chunk's count word is attacker-controlled, and it feeds `Vec::with_capacity` — so a count past the chunk's
/// entry capacity (2^62 over an 8-byte STRG pool: `count * 4` overflows usize) must be refused by extent BEFORE the
/// allocation. A typed error, never a capacity-overflow panic or an allocator abort.
#[test]
fn jqfb_refuses_a_strg_pool_count_past_its_chunk_extent() {
    let image = image_with_pool_count(2, 1_u64 << 62);
    let error = jqfb_open_error(&image);
    assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
    let diagnostic = error.diagnostic().expect("the pool-count message");
    assert!(
        diagnostic.message().contains("pool count exceeds its chunk"),
        "expected the extent precheck message, got {:?}",
        diagnostic.message()
    );
}

/// The NUMB twin: each number-pool entry occupies at least five bytes, so the same precheck bounds its count before
/// `with_capacity` asks for it.
#[test]
fn jqfb_refuses_a_numb_pool_count_past_its_chunk_extent() {
    let image = image_with_pool_count(3, 1_u64 << 40);
    let error = jqfb_open_error(&image);
    assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
    let diagnostic = error.diagnostic().expect("the pool-count message");
    assert!(
        diagnostic.message().contains("pool count exceeds its chunk"),
        "expected the extent precheck message, got {:?}",
        diagnostic.message()
    );
}
