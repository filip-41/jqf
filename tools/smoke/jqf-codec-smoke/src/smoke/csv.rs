//! CSV codec receipt battery (the first record-stream vertical after NDJSON).
//!
//! Pins the CSV codec's surface: the registration's operation declaration
//! (decode + encode + validate-tags over one format with two dialects), the
//! record provider's single `RecordStream` route at slot 0, the per-record
//! payload decode shape (one array of field strings per record, header row
//! included), the deterministic RFC 4180 encoder (quoting, delimiter), and the
//! authoritative tag absence.

use crate::drive::{resources, resume, source, whole_requirement};
use jqf_codec_core::{
    AccessFootprintKind, AccessOutcome, AccessResultKind, CodecFailureKind, CodecRunContext, DecodeRequest,
    DiagnosticPolicy, RecordPoll, ValidationMode,
};
use jqf_codec_delimited::{
    CsvDecodeOptions, CsvEncodeOptions, FORMAT_ID, JQF_RFC4180_DIALECT_ID, JQF_RFC4180_HEADER_DIALECT_ID,
    JQF_UTF8_DIALECT_ID, JQF_UTF8_HEADER_DIALECT_ID, RECORD_ROUTE_SLOT, RFC4180_DIALECT_ID, RFC4180_HEADER_DIALECT_ID,
    UTF8_DIALECT_ID, UTF8_HEADER_DIALECT_ID,
};
use jqf_data::{DialectId, FormatId, Value};
use jqf_resource::ResourceContext;

/// Drives the CSV payload provider over one record range and materializes the
/// published document root.
fn decode_record(bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<Value, String> {
    let registration = jqf_codec_delimited::registration().map_err(|error| format!("{error:?}"))?;
    let options =
        CsvDecodeOptions::try_new_rfc4180(None, None, u64::MAX, false).map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("csv decoder factory")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_delimited::JQF_RFC4180_DIALECT_ID).expect("dialect"),
                options: Some(&options as &(dyn core::any::Any + Send + Sync)),
                allow_adjacent_values: false,
                value_separator: &[],
            },
            resources,
        )
        .map_err(|error| format!("csv provider: {error:?}"))?;
    let requirement = whole_requirement(resources);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, resources)
        .map_err(|error| format!("open: {error:?}"))?;
    {
        let mut run = CodecRunContext::new(&mut *resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).map_err(|error| format!("decode: {error:?}"))?;
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            return Err("expected full document".into());
        };
        product
            .document()
            .materialize_root(&mut *resources)
            .map_err(|error| format!("materialize: {error:?}"))
    }
}

/// Frames one CSV stream and returns the record payloads it hands out.
fn frame_records(bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<Vec<Vec<u8>>, String> {
    let options = CsvDecodeOptions::try_new(None, None, u64::MAX, false).map_err(|error| format!("{error:?}"))?;
    let mut provider = jqf_codec_delimited::create_record_provider(
        source(bytes),
        options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Strict,
        resources,
    )
    .map_err(|error| format!("record provider: {error:?}"))?;
    let mut stream = provider
        .open_record_route(RECORD_ROUTE_SLOT, resources)
        .map_err(|error| format!("open route: {error:?}"))?;
    let limit = jqf_codec_core::RecordBatchLimit::new(256, 256 * 1024).ok_or_else(|| "batch limit".to_string())?;
    let mut batch = jqf_codec_core::RecordBatch::new();
    let mut payloads = Vec::new();
    for _ in 0..16_384 {
        let mut run = CodecRunContext::new(resources);
        match stream
            .poll(limit, &mut batch, &mut run)
            .map_err(|error| format!("poll: {error:?}"))?
        {
            RecordPoll::Filled => {
                for entry in batch.entries() {
                    if let jqf_codec_core::RecordEntry::Record(item) = entry {
                        payloads.push(item.lease().payload().to_vec());
                    }
                }
                batch.clear();
            }
            RecordPoll::Pending => {
                resume(resources);
            }
            RecordPoll::End(_) => return Ok(payloads),
        }
    }
    Err("framer never ended".into())
}

#[expect(
    clippy::too_many_lines,
    reason = "a smoke main is one sequential assertion battery; splitting it would hide the \
              inventory ordering the receipt pins"
)]
pub fn run() -> Result<(), String> {
    // 1. Registration surface.
    let registration = jqf_codec_delimited::registration().map_err(|error| format!("{error:?}"))?;
    let descriptor = registration.descriptor();
    if descriptor.format().as_str() != FORMAT_ID {
        return Err("csv format id mismatch".into());
    }
    let dialects: std::vec::Vec<&str> = descriptor.dialects().iter().map(|d| d.as_str()).collect();
    if dialects
        != [
            RFC4180_DIALECT_ID,
            RFC4180_HEADER_DIALECT_ID,
            UTF8_DIALECT_ID,
            UTF8_HEADER_DIALECT_ID,
            JQF_RFC4180_DIALECT_ID,
            JQF_RFC4180_HEADER_DIALECT_ID,
            JQF_UTF8_DIALECT_ID,
            JQF_UTF8_HEADER_DIALECT_ID,
        ]
    {
        return Err(format!("csv dialects mismatch: {dialects:?}"));
    }
    let ops = descriptor.operations();
    if !ops.decode() || !ops.encode() || !ops.validate_tags() {
        return Err("csv must declare decode+encode+validate-tags".into());
    }

    // 2. Record provider inventory: one route, slot 0, RecordStream.
    {
        let mut resources = resources();
        let options = CsvDecodeOptions::try_new(None, None, 1 << 20, false).map_err(|error| format!("{error:?}"))?;
        let provider = jqf_codec_delimited::create_record_provider(
            source(b"a,b\n1,2\n"),
            options,
            DiagnosticPolicy::ErrorsOnly,
            ValidationMode::Strict,
            &mut resources,
        )
        .map_err(|error| format!("record provider: {error:?}"))?;
        let routes = provider.record_route_descriptions();

        if routes.len() != 1
            || routes[0].slot() != RECORD_ROUTE_SLOT
            || routes[0].bundle().footprint() != AccessFootprintKind::Whole
            || routes[0].bundle().result() != AccessResultKind::RecordStream
        {
            return Err("csv did not advertise one record-stream route at slot 0".into());
        }
    }

    // 3. Framing: quote-aware boundaries (embedded newline stays one record).
    {
        let mut resources = resources();
        let payloads = frame_records(b"x\n\"a\nb\"\nc,d\n", &mut resources)?;
        if payloads.len() != 3 || payloads[0] != b"x" || payloads[1] != b"\"a\nb\"" || payloads[2] != b"c,d" {
            return Err(format!("csv framing diverged: {payloads:?}"));
        }
    }

    // 4. Payload decode: every record (header included) is an array of strings.
    {
        let mut resources = resources();
        let value = decode_record(b"name,age", &mut resources)?;
        let Value::Array(array) = &value else {
            return Err(format!("expected array root, got {value:?}"));
        };
        let fields: std::vec::Vec<&str> = array
            .iter()
            .map(|v| match v {
                Value::String(s) => s.as_str(),
                _ => "?",
            })
            .collect();
        if fields != ["name", "age"] {
            return Err(format!("csv record fields diverged: {fields:?}"));
        }
        match decode_record(b"a,\tb", &mut resources) {
            Err(error) if error.contains("InvalidInput") => {}
            other => {
                return Err(format!("RFC 4180 unquoted TAB must be InvalidInput, got {other:?}"));
            }
        }
    }

    // 5. Encoder: deterministic RFC 4180 output with quoting.
    {
        let mut resources = resources();
        let format = FormatId::try_new(FORMAT_ID).map_err(|e| e.to_string())?;
        let dialect = DialectId::try_new(JQF_RFC4180_DIALECT_ID).map_err(|e| e.to_string())?;
        let options = CsvEncodeOptions::try_new(None).map_err(|error| format!("{error:?}"))?;
        let opaque = &options as &(dyn core::any::Any + Send + Sync);
        let factory = registration
            .encoder()
            .expect("csv encoder factory")
            .create_factory(
                jqf_codec_core::EncodeRequest {
                    format: &format,
                    dialect: &dialect,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    preservation: jqf_codec_core::PreservationRequest::None,
                    options: Some(opaque),
                },
                &mut resources,
            )
            .map_err(|error| format!("encoder factory: {error:?}"))?;
        let mut array = jqf_data::Array::try_new().map_err(|e| format!("array: {e:?}"))?;
        let text = jqf_data::Shared::<str>::try_from_str("a,b").map_err(|e| format!("string: {e:?}"))?;
        array
            .try_push(Value::String(text))
            .map_err(|e| format!("push: {e:?}"))?;
        let item = Value::Array(array);
        let item = jqf_codec_core::EncodeItem::Owned(&item);
        let mut session = factory
            .start(item, jqf_codec_core::PreservationRequest::None, &mut resources)
            .map_err(|error| format!("encode start: {error:?}"))?;
        let mut out = Vec::new();
        {
            let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4_096);
            session
                .encode(&mut sink, &mut run)
                .map_err(|error| format!("encode: {error:?}"))?;
        }
        // `"a,b"` must be quoted; the terminator is codec-owned, and the
        // RFC-named CSV output dialect appends CRLF (139 DEF2).
        if out != b"\"a,b\"\r\n" {
            return Err(format!("csv encoder diverged: {:?}", String::from_utf8_lossy(&out)));
        }
    }

    // 6. Tag rejection: CSV has no native tag layer.
    {
        let mut resources = resources();
        let validator = registration
            .tag_validator()
            .expect("csv tag validator")
            .create_validator(
                jqf_codec_core::EncodeRequest {
                    format: &FormatId::try_new(FORMAT_ID).map_err(|e| e.to_string())?,
                    dialect: &DialectId::try_new(JQF_RFC4180_DIALECT_ID).map_err(|e| e.to_string())?,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    preservation: jqf_codec_core::PreservationRequest::None,
                    options: None,
                },
                &mut resources,
            )
            .map_err(|error| format!("validator: {error:?}"))?;
        let tag = jqf_data::TagId::try_new_unaccounted("!custom").map_err(|error| format!("tag: {error:?}"))?;
        match validator.validate(&[&tag], &resources) {
            Err(error) if error.kind() == CodecFailureKind::InvalidTag => {}
            other => return Err(format!("csv tag validator accepted a tag: {other:?}")),
        }
    }

    println!(
        "codec-csv-smoke: registration=true record_inventory=true framing=true payload_decode=true encode=true tags=true"
    );
    Ok(())
}
