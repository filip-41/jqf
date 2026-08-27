//! Record-stream receipts: NDJSON, CSV, and json-seq as the SDK drives them.
//!
//! Each assert pins the record-slot inventory and byte identity with the
//! adjacent-value path (or the json-seq RS-stripped oracle). The worker-grant
//! measurement lives in [`crate::grants`]. Uses [`crate::harness`] for the
//! pipeline drive.

use crate::harness::{CONTROL, PartialSink, json_dialect, program_for, resources, resources_with};
use jqf_codec_core::{AccessResultKind, DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_sdk::{CodecCatalog, FacadeFraming, PipelinePolicy};

/// Record-route receipt: the `jqf.record-stream@1` slot as the SDK drives it,
/// and its byte identity with the adjacent-value path it must never diverge from.
///
/// Three things are pinned. The record inventory — exactly one route, slot 0,
/// result kind `RecordStream` — so the route-slot protocol's "inventories in
/// both smokes" duty is discharged here as well as in the codec-receipts crate.
/// The DRIVE: `execute_record_sequence` over a conforming NDJSON stream
/// publishes exactly what `execute_sequence` publishes over the same bytes,
/// which is the gate the whole vertical exists to satisfy. And the guard that
/// makes the two verticals non-interchangeable: a record payload decoded with
/// `allow_adjacent_values` is an internal contract violation, because it would
/// silently accept a second value on one physical line and report only the
/// first.
#[allow(
    clippy::too_many_lines,
    reason = "three complete pipeline invocations kept side by side so the byte-identity \
              comparison and the adjacent-value guard read as one receipt"
)]
pub(crate) fn assert_record_route(format: &FormatId, dialect: &DialectId) -> Result<(), String> {
    const INPUT: &[u8] = b"{\"v\":1}\n{\"v\":[2,3]}\n{\"v\":null}\n";

    let json = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let streams = jqf_codec_json::ndjson::registration().map_err(|error| format!("{error:?}"))?;
    let registrations = [&json, &streams];
    let catalog = CodecCatalog::new(&registrations);

    let mut adjacent_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    {
        let mut resources = resources();
        let program = program_for(".v", &resources)?;
        let requirement = program
            .try_requirement(&resources)
            .map_err(|error| format!("adjacent requirement: {:?}", error.kind()))?;
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "records.ndjson",
            INPUT,
            0,
        );
        jqf_sdk::execute(
            jqf_sdk::Request::new(&program, jqf_sdk::Input::Whole(source.bytes()))
                .with_catalog(catalog)
                .with_source(source)
                .with_format(
                    FormatId::try_new(format.as_str()).expect("format id"),
                    DialectId::try_new(dialect.as_str()).expect("dialect id"),
                )
                .with_output_format(
                    FormatId::try_new(format.as_str()).expect("format id"),
                    DialectId::try_new(dialect.as_str()).expect("dialect id"),
                )
                .with_policy(PipelinePolicy {
                    decode: DecodeRequest {
                        validation: ValidationMode::Strict,
                        diagnostics: DiagnosticPolicy::ErrorsOnly,
                        dialect: json_dialect(),
                        options: None,
                        allow_adjacent_values: true,
                        value_separator: jqf_codec_json::VALUE_SEPARATORS,
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
                .with_requirement(&requirement),
            &mut adjacent_sink,
        )
        .map_err(|error| format!("run: {:?}", error.pipeline_failure()))?;
    }

    let mut record_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let records;
    let inventory;
    {
        let mut resources = resources();
        let program = program_for(".v", &resources)?;
        let requirement = program
            .try_requirement(&resources)
            .map_err(|error| format!("record requirement: {:?}", error.kind()))?;
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "records.ndjson",
            INPUT,
            0,
        );
        let options = jqf_codec_json::ndjson::NdjsonDecodeOptions::try_new(None, 1 << 20)
            .map_err(|error| format!("record ceiling: {:?}", error.kind()))?;
        let provider = jqf_codec_json::ndjson::create_record_provider(
            source,
            jqf_codec_json::ndjson::NdjsonProfile::Strict,
            options,
            DiagnosticPolicy::ErrorsOnly,
            ValidationMode::Strict,
            &mut resources,
        )
        .map_err(|error| format!("record provider: {:?}", error.kind()))?;
        let routes = provider.record_route_descriptions();

        if routes.len() != 1
            || routes[0].slot() != jqf_codec_json::ndjson::RECORD_ROUTE_SLOT
            || routes[0].bundle().result() != AccessResultKind::RecordStream
        {
            return Err("NDJSON did not advertise one record-stream route at slot 0".into());
        }
        inventory = routes.len();
        records = match jqf_sdk::execute(
            jqf_sdk::Request::new(
                &program,
                jqf_sdk::Input::Records {
                    source: source.bytes(),
                    records: provider,
                    slot: jqf_codec_json::ndjson::RECORD_ROUTE_SLOT,
                },
            )
            .with_catalog(catalog)
            .with_source(source)
            .with_format(
                FormatId::try_new(format.as_str()).expect("format id"),
                DialectId::try_new(dialect.as_str()).expect("dialect id"),
            )
            .with_output_format(
                FormatId::try_new(format.as_str()).expect("format id"),
                DialectId::try_new(dialect.as_str()).expect("dialect id"),
            )
            .with_policy(PipelinePolicy {
                decode: DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: json_dialect(),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: jqf_codec_json::VALUE_SEPARATORS,
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
            .with_requirement(&requirement),
            &mut record_sink,
        )
        .map_err(|error| format!("run: {:?}", error.pipeline_failure()))?
        {
            jqf_sdk::Outcome::Served(jqf_sdk::Report::Record(report)) => report,
            other => return Err(format!("record outcome unexpected: {other:?}")),
        };
    }

    if record_sink.bytes != adjacent_sink.bytes || record_sink.boundaries != adjacent_sink.boundaries {
        return Err(format!(
            "record route diverged from the adjacent path: record={:?} adjacent={:?}",
            String::from_utf8_lossy(&record_sink.bytes),
            String::from_utf8_lossy(&adjacent_sink.bytes)
        ));
    }
    if records.records() != 3 || records.issues() != 0 || records.error_issues() != 0 {
        return Err(format!(
            "record report unexpected: records={} issues={} error_issues={}",
            records.records(),
            records.issues(),
            records.error_issues()
        ));
    }

    // The adjacent-value opt-in is refused for record payloads.
    {
        let mut resources = resources();
        let program = program_for(".v", &resources)?;
        let requirement = program
            .try_requirement(&resources)
            .map_err(|error| format!("record requirement: {:?}", error.kind()))?;
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "records.ndjson",
            INPUT,
            0,
        );
        let options = jqf_codec_json::ndjson::NdjsonDecodeOptions::try_new(None, 1 << 20)
            .map_err(|error| format!("record ceiling: {:?}", error.kind()))?;
        let provider = jqf_codec_json::ndjson::create_record_provider(
            source,
            jqf_codec_json::ndjson::NdjsonProfile::Strict,
            options,
            DiagnosticPolicy::ErrorsOnly,
            ValidationMode::Strict,
            &mut resources,
        )
        .map_err(|error| format!("record provider: {:?}", error.kind()))?;
        let mut sink = PartialSink {
            bytes: Vec::new(),
            boundaries: Vec::new(),
            reports: Vec::new(),
        };
        let refused = jqf_sdk::execute(
            jqf_sdk::Request::new(
                &program,
                jqf_sdk::Input::Records {
                    source: source.bytes(),
                    records: provider,
                    slot: jqf_codec_json::ndjson::RECORD_ROUTE_SLOT,
                },
            )
            .with_catalog(catalog)
            .with_source(source)
            .with_format(
                FormatId::try_new(format.as_str()).expect("format id"),
                DialectId::try_new(dialect.as_str()).expect("dialect id"),
            )
            .with_output_format(
                FormatId::try_new(format.as_str()).expect("format id"),
                DialectId::try_new(dialect.as_str()).expect("dialect id"),
            )
            .with_policy(PipelinePolicy {
                decode: DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: json_dialect(),
                    options: None,
                    allow_adjacent_values: true,
                    value_separator: jqf_codec_json::VALUE_SEPARATORS,
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
            .with_requirement(&requirement),
            &mut sink,
        )
        .map_err(|error| format!("run: {:?}", error.pipeline_failure()));
        match refused {
            Ok(_) => return Err("record payloads accepted the adjacent-value opt-in".into()),
            Err(text) if text.contains("record payload decode must not allow adjacent values") => {}
            Err(text) => {
                return Err(format!("adjacent-value opt-in failed for the wrong reason: {text}"));
            }
        }
    }

    println!(
        "record-route: inventory={inventory} records={} items={} byte_identity=true",
        records.records(),
        records.items()
    );
    Ok(())
}

/// CSV record-route receipt: the `jqf.record-stream@1` slot as the SDK drives
/// it for the RFC 4180 vertical.
///
/// Pins the record inventory (one route, slot 0, result kind `RecordStream`)
/// for the CSV provider, and the DRIVE: `execute_record_sequence` over a
/// conforming CSV stream publishes one array-of-fields document per record —
/// including the header row, which is just the first array — and the report
/// counts every record with zero issues.
#[allow(
    clippy::too_many_lines,
    reason = "one linear harness invocation mirroring the CLI's own record branch"
)]
pub(crate) fn assert_csv_route() -> Result<(), String> {
    const INPUT: &[u8] = b"name,age\nada,37\nbob,42\n";

    let csv = jqf_codec_delimited::registration().map_err(|error| format!("{error:?}"))?;
    let json = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let registrations = [&csv, &json];
    let catalog = CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_delimited::FORMAT_ID).map_err(|error| error.to_string())?;
    let dialect = DialectId::try_new(jqf_codec_delimited::RFC4180_DIALECT_ID).map_err(|error| error.to_string())?;
    let output_format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|error| error.to_string())?;
    let output_dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|error| error.to_string())?;

    let mut resources = resources();
    let program = program_for(".", &resources)?;
    let requirement = program
        .try_requirement(&resources)
        .map_err(|error| format!("csv requirement: {:?}", error.kind()))?;
    let source = jqf_source::ResolvedSource::new(
        jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
        "records.csv",
        INPUT,
        0,
    );
    let options = jqf_codec_delimited::CsvDecodeOptions::try_new_rfc4180(None, None, 1 << 20, false)
        .map_err(|error| format!("csv ceiling: {:?}", error.kind()))?;
    let provider = jqf_codec_delimited::create_record_provider(
        source,
        options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Strict,
        &mut resources,
    )
    .map_err(|error| format!("csv provider: {:?}", error.kind()))?;
    let routes = provider.record_route_descriptions();

    if routes.len() != 1
        || routes[0].slot() != jqf_codec_delimited::RECORD_ROUTE_SLOT
        || routes[0].bundle().result() != AccessResultKind::RecordStream
    {
        return Err("CSV did not advertise one record-stream route at slot 0".into());
    }
    let inventory = routes.len();
    let mut sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let report = match jqf_sdk::execute(
        jqf_sdk::Request::new(
            &program,
            jqf_sdk::Input::Records {
                source: source.bytes(),
                records: provider,
                slot: jqf_codec_delimited::RECORD_ROUTE_SLOT,
            },
        )
        .with_catalog(catalog)
        .with_source(source)
        .with_format(
            FormatId::try_new(format.as_str()).expect("format id"),
            DialectId::try_new(dialect.as_str()).expect("dialect id"),
        )
        .with_output_format(
            FormatId::try_new(output_format.as_str()).expect("format id"),
            DialectId::try_new(output_dialect.as_str()).expect("dialect id"),
        )
        .with_policy(PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &dialect,
                options: Some(&options),
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
        .with_requirement(&requirement),
        &mut sink,
    )
    .map_err(|error| format!("run: {:?}", error.pipeline_failure()))?
    {
        jqf_sdk::Outcome::Served(jqf_sdk::Report::Record(report)) => report,
        other => return Err(format!("record outcome unexpected: {other:?}")),
    };

    // Every record (header included) publishes as an array of its fields.
    let expected = "[\"name\",\"age\"]\n[\"ada\",\"37\"]\n[\"bob\",\"42\"]\n";
    if String::from_utf8_lossy(&sink.bytes) != expected {
        return Err(format!(
            "csv route bytes diverged: got {:?} expected {expected:?}",
            String::from_utf8_lossy(&sink.bytes)
        ));
    }
    if report.records() != 3 || report.issues() != 0 || report.error_issues() != 0 {
        return Err(format!(
            "csv report unexpected: records={} issues={} error_issues={}",
            report.records(),
            report.issues(),
            report.error_issues()
        ));
    }
    println!(
        "csv-record-route: inventory={inventory} records={} bytes=expected",
        report.records()
    );

    // TAB is not RFC 4180 TEXTDATA; an unquoted field carrying it must refuse.
    {
        const ILLEGAL: &[u8] = b"name,age\nada,\t37\n";
        let mut refuse_resources = resources_with(&CONTROL, u64::MAX, 7);
        let program = program_for(".", &refuse_resources)?;
        let requirement = program
            .try_requirement(&refuse_resources)
            .map_err(|error| format!("csv refuse requirement: {:?}", error.kind()))?;
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "illegal.csv",
            ILLEGAL,
            0,
        );
        let provider = jqf_codec_delimited::create_record_provider(
            source,
            options,
            DiagnosticPolicy::ErrorsOnly,
            ValidationMode::Strict,
            &mut refuse_resources,
        )
        .map_err(|error| format!("csv refuse provider: {:?}", error.kind()))?;
        let mut sink = PartialSink {
            bytes: Vec::new(),
            boundaries: Vec::new(),
            reports: Vec::new(),
        };
        let refused = jqf_sdk::execute(
            jqf_sdk::Request::new(
                &program,
                jqf_sdk::Input::Records {
                    source: source.bytes(),
                    records: provider,
                    slot: jqf_codec_delimited::RECORD_ROUTE_SLOT,
                },
            )
            .with_catalog(catalog)
            .with_source(source)
            .with_format(
                FormatId::try_new(format.as_str()).expect("format id"),
                DialectId::try_new(dialect.as_str()).expect("dialect id"),
            )
            .with_output_format(
                FormatId::try_new(output_format.as_str()).expect("format id"),
                DialectId::try_new(output_dialect.as_str()).expect("dialect id"),
            )
            .with_policy(PipelinePolicy {
                decode: DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &dialect,
                    options: Some(&options),
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
            .with_resources(&mut refuse_resources)
            .with_requirement(&requirement),
            &mut sink,
        );
        let Err(error) = refused else {
            return Err("RFC 4180 accepted a TAB in unquoted TEXTDATA".into());
        };
        let text = format!("{error:?}");
        if !text.contains("InvalidInput") {
            return Err(format!("TAB in unquoted TEXTDATA must be InvalidInput, got {text}"));
        }
    }
    Ok(())
}

/// json-seq record-route receipt: the `jqf.record-stream@1` slot as the
/// json-seq codec advertises it, and the byte identity of its records with
/// the adjacent-value path.
#[allow(
    clippy::too_many_lines,
    reason = "two pipeline invocations kept side by side so the byte-identity comparison reads as one"
)]
pub(crate) fn assert_json_seq_route() -> Result<(), String> {
    const SEQ_INPUT: &[u8] = b"\x1e{\"v\":1}\n\x1e{\"v\":[2,3]}\n\x1e{\"v\":null}\n";
    const ADJACENT_INPUT: &[u8] = b"{\"v\":1}\n{\"v\":[2,3]}\n{\"v\":null}\n";

    let json = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let json_seq = jqf_codec_json::seq::registration().map_err(|error| format!("{error:?}"))?;
    let registrations = [&json, &json_seq];
    let catalog = CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|e| e.to_string())?;
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|e| e.to_string())?;

    let mut adjacent_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    {
        let mut resources = resources();
        let program = program_for(".v", &resources)?;
        let requirement = program
            .try_requirement(&resources)
            .map_err(|error| format!("adjacent requirement: {:?}", error.kind()))?;
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "records.adjacent",
            ADJACENT_INPUT,
            0,
        );
        jqf_sdk::execute(
            jqf_sdk::Request::new(&program, jqf_sdk::Input::Whole(source.bytes()))
                .with_catalog(catalog)
                .with_source(source)
                .with_format(
                    FormatId::try_new(format.as_str()).expect("format id"),
                    DialectId::try_new(dialect.as_str()).expect("dialect id"),
                )
                .with_output_format(
                    FormatId::try_new(format.as_str()).expect("format id"),
                    DialectId::try_new(dialect.as_str()).expect("dialect id"),
                )
                .with_policy(PipelinePolicy {
                    decode: DecodeRequest {
                        validation: ValidationMode::Strict,
                        diagnostics: DiagnosticPolicy::ErrorsOnly,
                        dialect: json_dialect(),
                        options: None,
                        allow_adjacent_values: true,
                        value_separator: jqf_codec_json::VALUE_SEPARATORS,
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
                .with_requirement(&requirement),
            &mut adjacent_sink,
        )
        .map_err(|error| format!("run: {:?}", error.pipeline_failure()))?;
    }

    let mut record_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let report;
    {
        let mut resources = resources();
        let program = program_for(".v", &resources)?;
        let requirement = program
            .try_requirement(&resources)
            .map_err(|error| format!("json-seq requirement: {:?}", error.kind()))?;
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
            "records.json-seq",
            SEQ_INPUT,
            0,
        );
        let options = jqf_codec_json::seq::JsonSeqDecodeOptions::try_new(None, 1 << 20)
            .map_err(|error| format!("json-seq ceiling: {:?}", error.kind()))?;
        let provider = jqf_codec_json::seq::create_record_provider(
            source,
            jqf_codec_json::seq::JsonSeqProfile::Strict,
            options,
            DiagnosticPolicy::ErrorsOnly,
            ValidationMode::Strict,
            &mut resources,
        )
        .map_err(|error| format!("json-seq provider: {:?}", error.kind()))?;
        let routes = provider.record_route_descriptions();

        if routes.len() != 1
            || routes[0].slot() != jqf_codec_json::seq::RECORD_ROUTE_SLOT
            || routes[0].bundle().result() != AccessResultKind::RecordStream
        {
            return Err("json-seq did not advertise one record-stream route at slot 0".into());
        }
        report = match jqf_sdk::execute(
            jqf_sdk::Request::new(
                &program,
                jqf_sdk::Input::Records {
                    source: source.bytes(),
                    records: provider,
                    slot: jqf_codec_json::seq::RECORD_ROUTE_SLOT,
                },
            )
            .with_catalog(catalog)
            .with_source(source)
            .with_format(
                FormatId::try_new(format.as_str()).expect("format id"),
                DialectId::try_new(dialect.as_str()).expect("dialect id"),
            )
            .with_output_format(
                FormatId::try_new(format.as_str()).expect("format id"),
                DialectId::try_new(dialect.as_str()).expect("dialect id"),
            )
            .with_policy(PipelinePolicy {
                decode: DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: json_dialect(),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: jqf_codec_json::VALUE_SEPARATORS,
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
            .with_requirement(&requirement),
            &mut record_sink,
        )
        .map_err(|error| format!("run: {:?}", error.pipeline_failure()))?
        {
            jqf_sdk::Outcome::Served(jqf_sdk::Report::Record(report)) => report,
            other => return Err(format!("record outcome unexpected: {other:?}")),
        };
    }
    if record_sink.bytes != adjacent_sink.bytes || record_sink.boundaries != adjacent_sink.boundaries {
        return Err(format!(
            "json-seq records diverged from the adjacent path: record={:?} adjacent={:?}",
            String::from_utf8_lossy(&record_sink.bytes),
            String::from_utf8_lossy(&adjacent_sink.bytes)
        ));
    }
    if report.records() != 3 || report.issues() != 0 || report.error_issues() != 0 {
        return Err(format!(
            "json-seq report unexpected: records={} issues={} error_issues={}",
            report.records(),
            report.issues(),
            report.error_issues()
        ));
    }
    Ok(())
}
