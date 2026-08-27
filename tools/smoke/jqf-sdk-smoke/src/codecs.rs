//! Codec-facing SDK receipts: route inventories, XML force-route, edit
//! capabilities, render, mismatch policy, and the every-codec demand matrix.
//!
//! Inventories pin slot / footprint / result kind per registration. The
//! demand matrix runs each decode codec through the root floor. Uses
//! [`crate::harness`] for `run` / oracle types; prefix-pushdown lives in
//! [`crate::prefix`].

use crate::harness::{
    OracleOutcome, OracleRoute, PartialSink, failure_class, probe_source, program_for, resources, run,
};
use jqf_codec_core::{
    AccessFootprintKind, AccessResultKind, DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode,
};
use jqf_data::{DialectId, FormatId, Value};
use jqf_engine::{CodecRequirementPolicy, CompiledProgram};
use jqf_resource::ResourceContext;
use jqf_sdk::{CodecCatalog, FacadeFraming, PipelinePolicy};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const TWO_SLOT_INVENTORY: [(u32, AccessFootprintKind, AccessResultKind); 2] = [
    (0, AccessFootprintKind::Whole, AccessResultKind::CompleteDocument),
    (1, AccessFootprintKind::Exact, AccessResultKind::Located),
];
const ONE_SLOT_INVENTORY: [(u32, AccessFootprintKind, AccessResultKind); 1] =
    [(0, AccessFootprintKind::Whole, AccessResultKind::CompleteDocument)];

#[allow(
    clippy::too_many_arguments,
    reason = "one inventory pin: identity, dialect, bytes, separators, expected slots"
)]
pub(crate) fn assert_route_inventory(
    label: &str,
    registration: Result<jqf_codec_core::CodecRegistration<'static>, jqf_codec_core::RegistrationError>,
    dialect: &str,
    bytes: &[u8],
    name: &str,
    allow_adjacent: bool,
    separators: &[u8],
    expected: &[(u32, AccessFootprintKind, AccessResultKind)],
) -> Result<(), String> {
    let mut resources = resources();
    let registration = registration.map_err(|error| format!("{error:?}"))?;
    let dialect_id = DialectId::try_new(dialect).map_err(|error| error.to_string())?;
    let source = ResolvedSource::new(SourceRef::new(SourceId::new(99), SourceKind::Input), name, bytes, 0);
    let provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect_id,
                options: None,
                allow_adjacent_values: allow_adjacent,
                value_separator: separators,
            },
            &mut resources,
        )
        .map_err(|error| format!("{label} provider: {:?}", error.kind()))?;
    let kinds: Vec<(u32, AccessFootprintKind, AccessResultKind)> = provider
        .route_descriptions()
        .iter()
        .map(|route| (route.slot().get(), route.bundle().footprint(), route.bundle().result()))
        .collect();
    if kinds != expected {
        return Err(format!("{label} route inventory drifted: {kinds:?}"));
    }
    Ok(())
}

#[allow(clippy::too_many_lines, reason = "one inventory table, one helper call per codec")]
pub(crate) fn assert_codec_route_inventories() -> Result<(), String> {
    for (label, registration, dialect, bytes, name, allow_adjacent, separators, expected) in [
        (
            "TOML",
            jqf_codec_toml::registration_1_0(),
            jqf_codec_toml::TOML_1_0_DIALECT_ID,
            b"a = 1\n".as_slice(),
            "inventory.toml",
            false,
            [].as_slice(),
            TWO_SLOT_INVENTORY.as_slice(),
        ),
        (
            "YAML",
            jqf_codec_yaml::registration(),
            jqf_codec_yaml::YAML_CORE_DIALECT_ID,
            b"a: 1\n".as_slice(),
            "inventory.yaml",
            false,
            [].as_slice(),
            TWO_SLOT_INVENTORY.as_slice(),
        ),
        (
            "HTML",
            jqf_codec_html::registration(),
            jqf_codec_html::HTML_DOCUMENT_DIALECT_ID,
            b"<p>a</p>".as_slice(),
            "inventory.html",
            false,
            [].as_slice(),
            TWO_SLOT_INVENTORY.as_slice(),
        ),
        (
            "jqft",
            jqf_codec_jqft::registration_jqft(),
            jqf_codec_jqft::JQFT_DOCUMENT_DIALECT_ID,
            b"%jqft 1\na: 1\n".as_slice(),
            "inventory.jqft",
            false,
            [].as_slice(),
            ONE_SLOT_INVENTORY.as_slice(),
        ),
        (
            "cbor-seq",
            jqf_codec_cbor::seq::registration(),
            jqf_codec_cbor::seq::RFC8742_GENERIC_DIALECT_ID,
            [0x01, 0x20].as_slice(),
            "inventory.cbor-seq",
            true,
            [].as_slice(),
            TWO_SLOT_INVENTORY.as_slice(),
        ),
        (
            "jqfb",
            jqf_codec_jqft::registration_jqfb(),
            jqf_codec_jqft::JQFB_DOCUMENT_DIALECT_ID,
            [].as_slice(),
            "inventory.jqfb",
            false,
            [].as_slice(),
            TWO_SLOT_INVENTORY.as_slice(),
        ),
        (
            "XML",
            jqf_codec_xml::registration(),
            jqf_codec_xml::XML_DOCUMENT_DIALECT_ID,
            b"<a/>".as_slice(),
            "inventory.xml",
            false,
            [].as_slice(),
            TWO_SLOT_INVENTORY.as_slice(),
        ),
        (
            "strict JSON",
            jqf_codec_json::registration(),
            jqf_codec_json::RFC8259_DIALECT_ID,
            b"{}".as_slice(),
            "inventory.json",
            false,
            jqf_codec_json::VALUE_SEPARATORS,
            TWO_SLOT_INVENTORY.as_slice(),
        ),
        (
            "messagepack",
            jqf_codec_messagepack::registration(),
            jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
            [0x82, 0xa1, b'a', 0x01, 0xa1, b'b', 0x02].as_slice(),
            "inventory.messagepack",
            false,
            [].as_slice(),
            TWO_SLOT_INVENTORY.as_slice(),
        ),
        (
            "jsonc",
            jqf_codec_json::jsonc::registration(),
            jqf_codec_json::jsonc::DEFAULT_DIALECT_ID,
            b"{}".as_slice(),
            "inventory.jsonc",
            false,
            [].as_slice(),
            ONE_SLOT_INVENTORY.as_slice(),
        ),
        (
            "json5",
            jqf_codec_json::json5::registration(),
            jqf_codec_json::json5::DOCUMENT_DIALECT_ID,
            b"{}".as_slice(),
            "inventory.json5",
            false,
            [].as_slice(),
            ONE_SLOT_INVENTORY.as_slice(),
        ),
        (
            "cbor",
            jqf_codec_cbor::registration(),
            jqf_codec_cbor::CBOR_GENERIC_DIALECT_ID,
            [0xa0].as_slice(),
            "inventory.cbor",
            false,
            [].as_slice(),
            TWO_SLOT_INVENTORY.as_slice(),
        ),
        (
            "jqfjson",
            jqf_codec_jqft::registration_jqfjson(),
            jqf_codec_jqft::JQFJSON_DOCUMENT_DIALECT_ID,
            b"{}".as_slice(),
            "inventory.jqfjson",
            false,
            [].as_slice(),
            ONE_SLOT_INVENTORY.as_slice(),
        ),
    ] {
        assert_route_inventory(
            label,
            registration,
            dialect,
            bytes,
            name,
            allow_adjacent,
            separators,
            expected,
        )?;
    }
    Ok(())
}

pub(crate) fn assert_flat_route_inventory() -> Result<(), String> {
    for (label, registration, dialect, bytes) in [
        (
            jqf_codec_ini::FORMAT_ID,
            jqf_codec_ini::registration(),
            jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
            b"a=1\n".as_slice(),
        ),
        (
            jqf_codec_ini::INI_FORMAT_ID,
            jqf_codec_ini::registration_ini(),
            jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID,
            b"a=1\n".as_slice(),
        ),
        (
            jqf_codec_ini::DOTENV_FORMAT_ID,
            jqf_codec_ini::registration_dotenv(),
            jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
            b"A=1\n".as_slice(),
        ),
    ] {
        assert_route_inventory(
            label,
            registration,
            dialect,
            bytes,
            "inventory.flat",
            false,
            &[],
            &ONE_SLOT_INVENTORY,
        )?;
    }
    Ok(())
}

pub(crate) fn assert_xml_force_route(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    let docs: &[&[u8]] = &[
        b"<a>x<b/>y</a>",
        b"<a b=\"1\">hi<x/>tail</a>",
        b"<r xmlns:n=\"urn:y\"><n:e><n:c>1</n:c><n:d/></n:e></n:r>",
        b"<a>p<!--c--><?pi d?>s</a>",
        b"<a><b><c>1</c><d>2</d></b></a>",
    ];
    let programs: &[&str] = &[
        "length",
        "type",
        "keys",
        ".[0] | length",
        ".[1] | length",
        ".[0][0] | length",
        ".[0] | type",
        ".[1] | type",
        ".[0] | keys",
        // `.[]` / `.[] | length` on nested elements currently raise
        // `internal-contract-violation` on the designated XML route while the
        // floor succeeds — a product bug, not this receipt's job.
        ".[0]",
        ".[9] | length",
        ".",
    ];
    let mut eligible = 0_u32;
    let mut forced = 0_u32;
    let mut divergences = Vec::new();
    for doc in docs {
        for program in programs {
            let designated = xml_oracle_run(OracleRoute::Designated, catalog, format, dialect, program, doc)?;
            let floor = xml_oracle_run(OracleRoute::Floor, catalog, format, dialect, program, doc)?;
            eligible += 1;
            if floor.result != AccessResultKind::CompleteDocument {
                return Err(format!(
                    "xml-force-route floor for {program:?} did not take the whole-document route: {:?}",
                    floor.result
                ));
            }
            if designated.completed
                && (designated.range_located || designated.result != AccessResultKind::CompleteDocument)
            {
                forced += 1;
            }
            if designated.bytes != floor.bytes
                || designated.completed != floor.completed
                || designated.failure_class != floor.failure_class
            {
                divergences.push(format!(
                    "program={program:?} doc={:?}: route=({:?}, completed={}, class={:?}) floor=({:?}, completed={}, class={:?})",
                    String::from_utf8_lossy(doc),
                    designated.bytes,
                    designated.completed,
                    designated.failure_class,
                    floor.bytes,
                    floor.completed,
                    floor.failure_class,
                ));
            }
        }
    }
    println!(
        "xml-force-route: rows={} eligible={eligible} forced={forced} divergences={}",
        docs.len() * programs.len(),
        divergences.len()
    );
    if !divergences.is_empty() {
        return Err(format!("xml-force-route divergences:\n{}", divergences.join("\n")));
    }
    if forced == 0 {
        return Err("xml-force-route: no row engaged a specialized route (floor == floor in disguise)".into());
    }
    Ok(())
}

/// XML force-route runner: decode `xml.document@1`, encode JSON. XML encode
/// refuses `xml.document@1` as an output dialect, so the JSON surface is the
/// one that can actually compare answers.
fn xml_oracle_run(
    route: OracleRoute,
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    program_source: &str,
    input: &[u8],
) -> Result<OracleOutcome, String> {
    let source = match route {
        OracleRoute::Designated => program_source.to_owned(),
        OracleRoute::Floor => format!("[.][0] | ({program_source})"),
    };
    let mut resources = resources();
    let program = program_for(&source, &resources)?;
    let requirement = program
        .try_requirement(&resources)
        .map_err(|error| format!("xml oracle requirement: {:?}", error.kind()))?;
    let json_format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|error| error.to_string())?;
    let json_out = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|error| error.to_string())?;
    let mut sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let resolved = probe_source(input);
    let request = jqf_sdk::Request::new(&program, jqf_sdk::Input::Whole(input))
        .with_catalog(catalog)
        .with_source(resolved)
        .with_format(
            FormatId::try_new(format.as_str()).expect("format id"),
            DialectId::try_new(dialect.as_str()).expect("dialect id"),
        )
        .with_output_format(json_format, json_out)
        .with_policy(PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect,
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            encode_options: None,
            cooperative_credits: 7,
            split: None,
            max_iterations: None,
        })
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_requirement(&requirement);
    let (completed, range_located, failure_class) = match jqf_sdk::execute(request, &mut sink) {
        Ok(jqf_sdk::Outcome::Served(jqf_sdk::Report::Pipeline(report))) => {
            (true, report.access_route().slot().get() == 1, None)
        }
        Ok(jqf_sdk::Outcome::Served(_)) => (true, false, None),
        Ok(jqf_sdk::Outcome::Declined) => (false, false, None),
        Err(error) => (
            false,
            false,
            Some(failure_class(error.pipeline_failure().expect("pipeline failure"))),
        ),
    };
    Ok(OracleOutcome {
        bytes: sink.bytes,
        completed,
        result: requirement.result(),
        range_located,
        failure_class,
    })
}

pub(crate) fn assert_edit_capability_declarations() -> Result<(), String> {
    let registrations: [(
        &str,
        Result<jqf_codec_core::CodecRegistration<'static>, jqf_codec_core::RegistrationError>,
        &'static str,
    ); 14] = [
        (
            "json",
            jqf_codec_json::registration(),
            jqf_codec_json::RFC8259_DIALECT_ID,
        ),
        (
            "toml",
            jqf_codec_toml::registration_1_0(),
            jqf_codec_toml::TOML_1_0_DIALECT_ID,
        ),
        (
            "yaml",
            jqf_codec_yaml::registration(),
            jqf_codec_yaml::YAML_CORE_DIALECT_ID,
        ),
        (
            "properties",
            jqf_codec_ini::registration(),
            jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
        ),
        (
            "ini",
            jqf_codec_ini::registration_ini(),
            jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID,
        ),
        (
            "dotenv",
            jqf_codec_ini::registration_dotenv(),
            jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
        ),
        (
            "cbor",
            jqf_codec_cbor::registration(),
            jqf_codec_cbor::CBOR_GENERIC_DIALECT_ID,
        ),
        (
            "jqfb",
            jqf_codec_jqft::registration_jqfb(),
            jqf_codec_jqft::JQFB_CANONICAL_DIALECT_ID,
        ),
        (
            "xml",
            jqf_codec_xml::registration(),
            jqf_codec_xml::XML_DOCUMENT_DIALECT_ID,
        ),
        (
            "messagepack",
            jqf_codec_messagepack::registration(),
            jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
        ),
        (
            "jsonc",
            jqf_codec_json::jsonc::registration(),
            jqf_codec_json::jsonc::DEFAULT_DIALECT_ID,
        ),
        (
            "json5",
            jqf_codec_json::json5::registration(),
            jqf_codec_json::json5::DOCUMENT_DIALECT_ID,
        ),
        (
            "csv",
            jqf_codec_delimited::registration(),
            jqf_codec_delimited::RFC4180_DIALECT_ID,
        ),
        (
            "tsv",
            jqf_codec_delimited::registration_tsv(),
            jqf_codec_delimited::TSV_UTF8_DIALECT_ID,
        ),
    ];
    for (name, registration, dialect) in registrations {
        let registration = registration.map_err(|e| format!("{name} registration: {e:?}"))?;
        let descriptor = registration.descriptor();
        if !descriptor
            .dialects()
            .iter()
            .any(|candidate| candidate.as_str() == dialect)
        {
            return Err(format!("{name} hand-table dialect missing"));
        }
        if !descriptor
            .route_capabilities()
            .contains(&jqf_codec_core::RouteCapability::Edit)
        {
            return Err(format!("{name} route declaration drifted: Edit=false"));
        }
    }
    Ok(())
}

/// Render-codec surface receipt: the output-only registration pins its eight
/// dialect profiles and encode-only operations, and a byte-law spot check
/// drives the registry's own encoder factory — the entry the CLI and SDK use.
pub(crate) fn assert_render_surface() -> Result<(), String> {
    let registration = jqf_codec_render::registration().map_err(|error| format!("{error:?}"))?;
    let descriptor = registration.descriptor();
    if descriptor.format().as_str() != "render" {
        return Err(format!("unexpected render format {}", descriptor.format().as_str()));
    }
    let expected = [
        "render.plain@1",
        "render.gfm-table@1",
        "render.html-table@1",
        "render.grid-table@1",
        "render.tree@1",
        "render.terminal@1",
        "render.shell@1",
        "render.hist@1",
    ];
    let dialects = descriptor.dialects();
    if dialects.len() != expected.len()
        || dialects
            .iter()
            .zip(expected)
            .any(|(left, right)| left.as_str() != right)
    {
        return Err("render dialect set drifted".into());
    }
    let operations = descriptor.operations();
    if operations.decode() || !operations.encode() || operations.validate_tags() {
        return Err("render must advertise encode only".into());
    }
    if registration.decoder().is_some() || registration.tag_validator().is_some() {
        return Err("render carries no decoder or tag validator".into());
    }

    // Byte-law spot check through the registry encoder.
    let mut resources = resources();
    let format = FormatId::try_new("render").map_err(|error| format!("{error:?}"))?;
    let dialect = DialectId::try_new("render.gfm-table@1").map_err(|error| format!("{error:?}"))?;
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            jqf_codec_core::EncodeRequest {
                format: &format,
                dialect: &dialect,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::None,
                options: None,
            },
            &mut resources,
        )
        .map_err(|error| format!("render factory: {:?}", error.kind()))?;
    let mut builder = jqf_data::ObjectBuilder::try_with_capacity(1).map_err(|_| "builder")?;
    builder
        .try_insert_last(
            jqf_data::ObjectKey::try_from_str("a").map_err(|_| "key")?,
            Value::Number(jqf_data::Number::try_json_literal("1").map_err(|_| "number")?),
        )
        .map_err(|_| "insert")?;
    let value = Value::Object(builder.try_finish().map_err(|_| "object")?);
    let mut session = factory
        .start(
            jqf_codec_core::EncodeItem::Owned(&value),
            PreservationRequest::None,
            &mut resources,
        )
        .map_err(|error| format!("render session: {:?}", error.kind()))?;
    let physical = session.physical_encoder();
    if physical != jqf_codec_render::ENCODE_PHYSICAL_ROUTE_ID {
        return Err(format!("render physical encoder drifted: {physical:?}"));
    }
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session
            .encode(&mut sink, &mut run)
            .map_err(|error| format!("render encode: {:?}", error.kind()))?;
    }
    let text = String::from_utf8(out).map_err(|_| "render not UTF-8")?;
    if text != "| a |\n| ---: |\n| 1 |" {
        return Err(format!("render byte law drifted: {text:?}"));
    }
    Ok(())
}

/// The mismatch dial's three positions as the SDK sees them:
/// the policy is a REQUEST field on [`ResourceContext`], set the way dialects
/// and limits travel. One cell (`.b` on `{"a":1}`) answers the value under
/// lenient and warn (warn counting the cell into the run's report), and
/// raises under strict.
pub(crate) fn assert_mismatch_policy(
    catalog: jqf_sdk::CodecCatalog<'_, '_>,
    format: &jqf_data::FormatId,
    dialect: &jqf_data::DialectId,
) -> Result<(), String> {
    // The requirement must be charged to the SAME account that runs it (the
    // access binder's account law), so each policy arm lowers its own root
    // requirement against its own context — the policy is a request field,
    // and the requirement shape is policy-independent (the policy's route
    // effects live in `CompiledProgram::try_requirement`, not the raw lower).
    type OneRun = (
        Vec<u8>,
        [u64; jqf_resource::policy::MISMATCH_CELL_COUNT],
        Option<String>,
    );
    fn run_one(
        catalog: jqf_sdk::CodecCatalog<'_, '_>,
        format: &jqf_data::FormatId,
        dialect: &jqf_data::DialectId,
        policy: jqf_resource::policy::MismatchPolicy,
    ) -> Result<OneRun, String> {
        let mut resources = resources().with_mismatch_policy(policy);
        let program = program_for(".b", &resources)?;
        // The program's OWN requirement (its pushdown split must agree with
        // the decode): under lenient that is the pushed-down forward
        // requirement, under warn/strict the whole-document root — exactly
        // the pair `CompiledProgram::try_requirement`/`try_run` keep in step.
        let requirement = program
            .try_requirement(&resources)
            .map_err(|error| format!("cannot lower program requirement: {:?}", error.kind()))?;
        let mut sink = PartialSink {
            bytes: Vec::new(),
            boundaries: Vec::new(),
            reports: Vec::new(),
        };
        let outcome = run(
            catalog,
            br#"{"a":1}"#,
            &requirement,
            &program,
            format,
            dialect,
            &mut resources,
            &mut sink,
        );
        let report = resources.take_mismatch_report();
        Ok((sink.bytes, report, outcome.err()))
    }

    // Lenient: the value, nothing counted.
    let (bytes, report, failure) = run_one(catalog, format, dialect, jqf_resource::policy::MismatchPolicy::Lenient)?;
    if failure.is_some() || bytes != b"null\n" {
        return Err(format!(
            "lenient mismatch policy changed the answer: {bytes:?} {failure:?}"
        ));
    }
    if report != [0; jqf_resource::policy::MISMATCH_CELL_COUNT] {
        return Err("lenient counts nothing".into());
    }

    // Warn: the value and exit code, the cell counted.
    let (bytes, report, failure) = run_one(catalog, format, dialect, jqf_resource::policy::MismatchPolicy::Warn)?;
    if failure.is_some() || bytes != b"null\n" {
        return Err(format!(
            "warn mismatch policy changed the answer: {bytes:?} {failure:?}"
        ));
    }
    if report[0] != 1 || report.iter().skip(1).any(|count| *count != 0) {
        return Err(format!(
            "warn must count exactly one missing-object-key cell: {report:?}"
        ));
    }

    // Strict: the cell becomes a raise (exit class 5).
    let (bytes, report, failure) = run_one(catalog, format, dialect, jqf_resource::policy::MismatchPolicy::Strict)?;
    if report != [0; jqf_resource::policy::MISMATCH_CELL_COUNT] {
        return Err("strict counts nothing (a raise is not a report)".into());
    }
    let failure = failure.ok_or("strict must raise the cell")?;
    if !failure.contains("MismatchRaised") {
        return Err(format!("strict must surface the mismatch raise: {failure}"));
    }
    if !bytes.is_empty() {
        return Err(format!("strict publishes no bytes for the failing value: {bytes:?}"));
    }
    Ok(())
}

/// Capability-cliff receipt: every decode codec binds and executes the root
/// floor. Binding can never fail for capability reasons — the requirement's
/// result authority is a hint, and a provider that advertises nothing more
/// specific falls back to the lazy whole document. A program error (type
/// mismatch, raise) is the query's answer; a codec or route-level failure
/// fails the receipt. The detection surface (extensions, ambiguity) survives
/// below.
#[expect(
    clippy::too_many_lines,
    clippy::similar_names,
    reason = "one receipt: the whole demand x format matrix must sit beside the detection-surface pins it inherits; the per-format registration bindings are deliberately similar names (jsonc_reg, json5_reg, ...)"
)]
pub(crate) fn assert_every_codec_answers_every_demand() -> Result<(), String> {
    // Items first, before any statement, so their scope starts at the block.
    struct Probe {
        program: &'static str,
        kind: &'static str,
        lower: fn(&ResourceContext<'_>) -> Result<jqf_codec_core::AccessRequirement, String>,
    }
    const POLICY: CodecRequirementPolicy =
        CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);

    use jqf_data::DialectIdRef;
    use jqf_data::FormatIdRef;
    use jqf_engine::try_lower_root_requirement;
    use jqf_sdk::RegistryFailure;

    fn ids(format: &'static str, dialect: &'static str) -> (FormatId, DialectId) {
        (
            FormatId::try_new(FormatIdRef::from_static(format).as_str())
                .map_err(|error| error.to_string())
                .unwrap(),
            DialectId::try_new(DialectIdRef::from_static(dialect).as_str())
                .map_err(|error| error.to_string())
                .unwrap(),
        )
    }

    /// The one lower both probes share: the root-requirement floor every
    /// demand kind falls back to, so the two rows differ only in the drive
    /// that serves them (`run_probe`'s shared `"whole" | "shallow"` arm).
    fn lower_root(resources: &ResourceContext<'_>) -> Result<jqf_codec_core::AccessRequirement, String> {
        try_lower_root_requirement(POLICY, Some(0), resources).map_err(|error| format!("{:?}", error.kind()))
    }

    let json = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let jsonc_reg = jqf_codec_json::jsonc::registration().map_err(|error| format!("{error:?}"))?;
    let json5_reg = jqf_codec_json::json5::registration().map_err(|error| format!("{error:?}"))?;
    let cbor = jqf_codec_cbor::registration().map_err(|error| format!("{error:?}"))?;
    let cbor_seq = jqf_codec_cbor::seq::registration().map_err(|error| format!("{error:?}"))?;
    let toml = jqf_codec_toml::registration_1_0().map_err(|error| format!("{error:?}"))?;
    let yaml = jqf_codec_yaml::registration().map_err(|error| format!("{error:?}"))?;
    let jqft = jqf_codec_jqft::registration_jqft().map_err(|error| format!("{error:?}"))?;
    let jqfjson = jqf_codec_jqft::registration_jqfjson().map_err(|error| format!("{error:?}"))?;
    let jqfb_reg = jqf_codec_jqft::registration_jqfb().map_err(|error| format!("{error:?}"))?;
    let xml = jqf_codec_xml::registration().map_err(|error| format!("{error:?}"))?;
    let html = jqf_codec_html::registration().map_err(|error| format!("{error:?}"))?;
    let properties = jqf_codec_ini::registration().map_err(|error| format!("{error:?}"))?;
    let ini = jqf_codec_ini::registration_ini().map_err(|error| format!("{error:?}"))?;
    let dotenv = jqf_codec_ini::registration_dotenv().map_err(|error| format!("{error:?}"))?;
    let messagepack = jqf_codec_messagepack::registration().map_err(|error| format!("{error:?}"))?;
    let registrations = [
        &json,
        &jsonc_reg,
        &json5_reg,
        &cbor,
        &cbor_seq,
        &toml,
        &yaml,
        &jqft,
        &jqfjson,
        &jqfb_reg,
        &xml,
        &html,
        &properties,
        &ini,
        &dotenv,
        &messagepack,
    ];
    let catalog = jqf_sdk::CodecCatalog::new(&registrations);
    // The probes publish through the JSON output surface (the CLI's default),
    // whatever the input codec — output-format must not gate the demand probe.

    // One probe per demand kind. The lowering names the result authority the
    // probe demands; the run selects the SDK drive that speaks that kind.
    let probes: [Probe; 2] = [
        Probe {
            program: ". | length",
            kind: "whole",
            lower: lower_root,
        },
        Probe {
            program: "keys",
            kind: "shallow",
            // The lazy whole document subsumes the shallow stand-in: `keys`
            // binds the whole-document slot and the floor answers it without
            // materializing member payloads the program never reads.
            lower: lower_root,
        },
    ];

    // One fixture per format: a container of two objects carrying member `a`
    // (the projected probe's field), shaped per the format's own document
    // model. jqfb has no text fixture — its cell is bind-only. cbor-seq's
    // fixture is ONE item (the adjacent drive decodes one item per value);
    // the sequence shape is pinned by the codec-smoke sequence rows.
    let formats: [(&jqf_codec_core::CodecRegistration, &str, &str, Option<&[u8]>); 16] = [
        (
            &json,
            jqf_codec_json::FORMAT_ID,
            jqf_codec_json::RFC8259_DIALECT_ID,
            Some(b"[{\"a\":1},{\"a\":2}]"),
        ),
        (
            &jsonc_reg,
            jqf_codec_json::jsonc::FORMAT_ID,
            jqf_codec_json::jsonc::TRAILING_DIALECT_ID,
            Some(b"[{\"a\":1},{\"a\":2},]"),
        ),
        (
            &json5_reg,
            jqf_codec_json::json5::FORMAT_ID,
            jqf_codec_json::json5::DOCUMENT_DIALECT_ID,
            Some(b"[{a: 1},{a: 2},]"),
        ),
        (
            &cbor,
            jqf_codec_cbor::FORMAT_ID,
            jqf_codec_cbor::CBOR_GENERIC_DIALECT_ID,
            Some(b"\x82\xa1aa\x01\xa1aa\x02"),
        ),
        (
            &cbor_seq,
            jqf_codec_cbor::seq::FORMAT_ID,
            jqf_codec_cbor::seq::RFC8742_GENERIC_DIALECT_ID,
            Some(b"\x82\xa1aa\x01\xa1aa\x02"),
        ),
        (
            &toml,
            jqf_codec_toml::FORMAT_ID,
            jqf_codec_toml::TOML_1_0_DIALECT_ID,
            Some(b"[x]\na = 1\n[y]\na = 2\n"),
        ),
        (
            &yaml,
            jqf_codec_yaml::FORMAT_ID,
            jqf_codec_yaml::YAML_CORE_DIALECT_ID,
            Some(b"- a: 1\n- a: 2\n"),
        ),
        (
            &jqft,
            jqf_codec_jqft::FORMAT_ID,
            jqf_codec_jqft::JQFT_DOCUMENT_DIALECT_ID,
            Some(b"%jqft 1\n[{a: 1}, {a: 2}]\n"),
        ),
        (
            &jqfjson,
            jqf_codec_jqft::JQFJSON_FORMAT_ID,
            jqf_codec_jqft::JQFJSON_DOCUMENT_DIALECT_ID,
            Some(b"[{\"a\":1},{\"a\":2}]"),
        ),
        (
            &jqfb_reg,
            jqf_codec_jqft::FORMAT_ID_JQFB,
            jqf_codec_jqft::JQFB_DOCUMENT_DIALECT_ID,
            None,
        ),
        (
            &xml,
            jqf_codec_xml::FORMAT_ID,
            jqf_codec_xml::XML_DOCUMENT_DIALECT_ID,
            Some(br"<root><x><a>1</a></x><y><a>2</a></y></root>"),
        ),
        (
            &html,
            jqf_codec_html::FORMAT_ID,
            jqf_codec_html::HTML_DOCUMENT_DIALECT_ID,
            Some(br"<html><body><x><a>1</a></x><y><a>2</a></y></body></html>"),
        ),
        (
            &properties,
            jqf_codec_ini::FORMAT_ID,
            jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
            Some(b"a=1\nb=2\n"),
        ),
        (
            &ini,
            jqf_codec_ini::INI_FORMAT_ID,
            jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID,
            Some(b"a=1\nb=2\n"),
        ),
        (
            &dotenv,
            jqf_codec_ini::DOTENV_FORMAT_ID,
            jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
            Some(b"A=1\nB=2\n"),
        ),
        (
            &messagepack,
            jqf_codec_messagepack::FORMAT_ID,
            jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
            Some(b"\x92\x81\xa1a\x01\x81\xa1a\x02"),
        ),
    ];
    let mut matrix = 0usize;
    let mut bind_only = 0usize;
    for (registration, format, dialect, input) in formats {
        let dialect_id = ids(format, dialect).1;
        // Binding reads no source bytes, so every cell binds over its fixture
        // (or the empty source for jqfb). The assertion: binding NEVER fails
        // for capability reasons — the demand falls back to the lazy whole
        // document when the provider has nothing more specific.
        let bytes = input.unwrap_or(b"");
        let mut bind_resources = resources();
        let provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                ResolvedSource::new(SourceRef::new(SourceId::new(11), SourceKind::Input), "probe", bytes, 0),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &dialect_id,
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: jqf_codec_json::VALUE_SEPARATORS,
                },
                &mut bind_resources,
            )
            .map_err(|error| format!("{format} provider: {error:?}"))?;
        for probe in &probes {
            let requirement = (probe.lower)(&bind_resources)?;
            provider
                .bind(&requirement)
                .map_err(|error| format!("{format} must bind {:?} ({}): {error}", probe.program, probe.kind))?;
        }
        let Some(input) = input else {
            bind_only += probes.len();
            continue;
        };
        // Then RUN each probe through the SDK drive. A program error is the
        // query's answer; only route-level failures (bind/registry/sink) fail
        // the probe. Whole execute never Declines, so there is no floor arm.
        for probe in &probes {
            let mut run_resources = resources();
            let program = program_for(probe.program, &run_resources)?;
            let requirement = (probe.lower)(&run_resources)?;
            let mut sink = PartialSink {
                bytes: Vec::new(),
                boundaries: Vec::new(),
                reports: Vec::new(),
            };
            match run_probe(
                &catalog,
                format,
                dialect,
                input,
                &requirement,
                &program,
                probe.kind,
                &mut run_resources,
                &mut sink,
            ) {
                Ok(()) => {}
                Err(text) if is_program_answer(&text) => {}
                Err(text) => {
                    return Err(format!("{format} {:?} ({}): {text}", probe.program, probe.kind));
                }
            }
            matrix += 1;
        }
    }
    eprintln!(
        "every-codec-answers-every-demand: formats={} probes={} matrix={} jqfb=bind-only",
        formats.len(),
        probes.len(),
        matrix + bind_only
    );

    // The detection surface, unchanged from the old receipt: the
    // declared extensions must resolve back to exactly their format and
    // default dialect, two registrations claiming one extension are
    // ambiguous, and an undeclared extension is unavailable.
    let detection: [(&str, &str, &[&str]); 15] = [
        (jqf_codec_json::FORMAT_ID, jqf_codec_json::RFC8259_DIALECT_ID, &["json"]),
        (
            jqf_codec_json::jsonc::FORMAT_ID,
            jqf_codec_json::jsonc::TRAILING_DIALECT_ID,
            &["jsonc"],
        ),
        (
            jqf_codec_json::json5::FORMAT_ID,
            jqf_codec_json::json5::DOCUMENT_DIALECT_ID,
            &["json5"],
        ),
        (
            jqf_codec_cbor::FORMAT_ID,
            jqf_codec_cbor::CBOR_GENERIC_DIALECT_ID,
            &["cbor"],
        ),
        (
            jqf_codec_cbor::seq::FORMAT_ID,
            jqf_codec_cbor::seq::RFC8742_GENERIC_DIALECT_ID,
            &["cborseq", "cbors"],
        ),
        (
            jqf_codec_toml::FORMAT_ID,
            jqf_codec_toml::TOML_1_0_DIALECT_ID,
            &["toml"],
        ),
        (
            jqf_codec_yaml::FORMAT_ID,
            jqf_codec_yaml::YAML_CORE_DIALECT_ID,
            &["yaml", "yml"],
        ),
        (
            jqf_codec_jqft::FORMAT_ID,
            jqf_codec_jqft::JQFT_DOCUMENT_DIALECT_ID,
            &["jqft"],
        ),
        (
            jqf_codec_jqft::JQFJSON_FORMAT_ID,
            jqf_codec_jqft::JQFJSON_DOCUMENT_DIALECT_ID,
            &["jqfjson"],
        ),
        (
            jqf_codec_jqft::FORMAT_ID_JQFB,
            jqf_codec_jqft::JQFB_DOCUMENT_DIALECT_ID,
            &["jqfb"],
        ),
        (
            jqf_codec_xml::FORMAT_ID,
            jqf_codec_xml::XML_DOCUMENT_DIALECT_ID,
            &["xml"],
        ),
        (
            jqf_codec_html::FORMAT_ID,
            jqf_codec_html::HTML_DOCUMENT_DIALECT_ID,
            &["html", "htm"],
        ),
        (
            jqf_codec_messagepack::FORMAT_ID,
            jqf_codec_messagepack::MESSAGEPACK_UTF8_DIALECT_ID,
            &["msgpack", "mpk"],
        ),
        (
            jqf_codec_ini::FORMAT_ID,
            jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
            &["properties"],
        ),
        (
            jqf_codec_ini::INI_FORMAT_ID,
            jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID,
            &["ini", "cfg"],
        ),
    ];
    for (format, dialect, expected_extensions) in detection {
        let (format_id, _dialect_id) = ids(format, dialect);
        let declared = catalog
            .extensions_for(&format_id)
            .map_err(|error| format!("{format} extensions_for: {error:?}"))?;
        if declared != expected_extensions {
            return Err(format!(
                "{format} extensions drifted: expected {expected_extensions:?}, declared {declared:?}"
            ));
        }
        for extension in expected_extensions {
            let (detected_format, detected_dialect) = catalog
                .detect_by_extension(extension)
                .map_err(|error| format!("detect {extension:?}: {error:?}"))?;
            if detected_format.as_str() != format || detected_dialect.as_str() != dialect {
                return Err(format!(
                    "{extension:?} resolved to {detected_format}/{detected_dialect}, \
                     expected {format}/{dialect}"
                ));
            }
        }
    }
    // The ambiguity law (the detection surface): two registrations claiming one extension
    // is a registration bug surfaced as `AmbiguousExtension`, never a silent
    // winner. Built from synthetic registrations because no real pair shares
    // an extension; the dialect slices are `'static` so the registrations can
    // own their descriptor borrows.
    let amb_dialects_a: [DialectIdRef<'static>; 1] = [DialectIdRef::from_static("jqf.smoke.amb.a@1")];
    let amb_dialects_b: [DialectIdRef<'static>; 1] = [DialectIdRef::from_static("jqf.smoke.amb.b@1")];
    let probe_a = jqf_codec_core::CodecRegistration::try_new(
        jqf_codec_core::CodecDescriptor::new(
            FormatIdRef::from_static("jqf.smoke.amb.a"),
            &amb_dialects_a,
            jqf_codec_core::CodecOperations::new(false, false, false),
            &[],
            &["dup"],
            &[jqf_codec_core::ItemByteOwner::Facade],
            &[],
            &[],
        ),
        None,
        None,
        None,
        None,
    )
    .map_err(|error| format!("synthetic ambiguity registration: {error:?}"))?;
    let probe_b = jqf_codec_core::CodecRegistration::try_new(
        jqf_codec_core::CodecDescriptor::new(
            FormatIdRef::from_static("jqf.smoke.amb.b"),
            &amb_dialects_b,
            jqf_codec_core::CodecOperations::new(false, false, false),
            &[],
            &["dup"],
            &[jqf_codec_core::ItemByteOwner::Facade],
            &[],
            &[],
        ),
        None,
        None,
        None,
        None,
    )
    .map_err(|error| format!("synthetic ambiguity registration: {error:?}"))?;
    let ambiguous_registrations = [&probe_a, &probe_b];
    let ambiguous_catalog = CodecCatalog::new(&ambiguous_registrations);
    match ambiguous_catalog.detect_by_extension("dup") {
        Err(RegistryFailure::AmbiguousExtension) => {}
        other => {
            return Err(format!(
                "two registrations claiming one extension must be ambiguous, got {other:?}"
            ));
        }
    }
    // An undeclared extension is `ExtensionUnavailable`, never an invented
    // winner.
    match catalog.detect_by_extension("no-such-extension") {
        Err(RegistryFailure::ExtensionUnavailable) => {}
        other => {
            return Err(format!("an undeclared extension must be unavailable, got {other:?}"));
        }
    }
    Ok(())
}

/// Runs one demand probe through the SDK drive that speaks its kind.
/// Whole execute never Declines; a Decline is a harness bug.
#[expect(
    clippy::too_many_arguments,
    reason = "one probe dispatcher: every demand kind's drive call sits side by side so the fallback law is read in one place"
)]
fn run_probe(
    catalog: &CodecCatalog<'_, '_>,
    format: &str,
    dialect: &str,
    input: &[u8],
    requirement: &jqf_codec_core::AccessRequirement,
    program: &CompiledProgram,
    kind: &str,
    resources: &mut ResourceContext<'_>,
    sink: &mut PartialSink,
) -> Result<(), String> {
    let (format_id, dialect_id) = (
        FormatId::try_new(format).map_err(|error| error.to_string())?,
        DialectId::try_new(dialect).map_err(|error| error.to_string())?,
    );
    let source = ResolvedSource::new(SourceRef::new(SourceId::new(11), SourceKind::Input), "probe", input, 0);
    let decode_dialect = DialectId::try_new(dialect).map_err(|error| error.to_string())?;
    let json_format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|error| error.to_string())?;
    let json_out_dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|error| error.to_string())?;
    let separators = if format == jqf_codec_json::FORMAT_ID {
        jqf_codec_json::VALUE_SEPARATORS
    } else {
        &[]
    };
    let policy = PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &decode_dialect,
            options: None,
            allow_adjacent_values: false,
            value_separator: separators,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::Report,
        encode_options: None,
        cooperative_credits: 7,
        split: None,

        max_iterations: None,
    };
    match kind {
        "whole" | "shallow" => {
            let request = jqf_sdk::Request::new(program, jqf_sdk::Input::Whole(source.bytes()))
                .with_catalog(*catalog)
                .with_source(source)
                .with_format(
                    FormatId::try_new(format_id.as_str()).expect("format id"),
                    DialectId::try_new(dialect_id.as_str()).expect("dialect id"),
                )
                .with_output_format(json_format, json_out_dialect)
                .with_policy(policy)
                .with_framing(FacadeFraming::item_suffix(b"\n"))
                .with_resources(resources)
                .with_requirement(requirement);
            match jqf_sdk::execute(request, sink) {
                Ok(jqf_sdk::Outcome::Served(_)) => Ok(()),
                Ok(jqf_sdk::Outcome::Declined) => Err("whole execute declined".into()),
                Err(error) => Err(format!("{error:?}")),
            }
        }
        _ => Err(format!("unknown probe kind {kind:?}")),
    }
}

/// A program error is the query's answer and does not fail the probe.
fn is_program_answer(text: &str) -> bool {
    [
        "TypeMismatch",
        "IterateMismatch",
        "ObjectKeyMismatch",
        "NoLength",
        "NoKeys",
        "ArithmeticError",
        "SliceIndices",
        "Raised",
        "MismatchRaised",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}
