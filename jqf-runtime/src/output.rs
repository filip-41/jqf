//! The output encoding a drive spec names, plus the request glue both drives derive from the same `Copy` fields.
//!
//! Both drives — the adjacent-value one and the record one — decide the same four things from the same three `Copy`
//! fields: which format and dialect to encode into, which codec options carry the caller's choices, and who owns the
//! byte between items. Deriving that twice is how the JSON indent reached the serial route and not the parallel one; it
//! is derived here once instead. The rest of the request glue (codec registrations, catalog, decode policy, retained
//! source) is derived here once too .

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_codec_delimited::CsvEncodeOptions;
use jqf_codec_json::{
    JsonEncodeOptions,
    ndjson::{NdjsonEncodeOptions, NdjsonTerminator},
    seq::{JsonSeqEncodeOptions, JsonSeqSuffix},
};
use jqf_data::{DialectId, FormatId};
use jqf_sdk::{CodecCatalog, FacadeFraming, PipelinePolicy};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

pub(crate) fn built_in_format(id: &'static str) -> FormatId {
    FormatId::try_new(id).unwrap_or_else(|error| unreachable!("a built-in format identity is valid: {error:?}"))
}

pub(crate) fn built_in_dialect(id: &'static str) -> DialectId {
    DialectId::try_new(id).unwrap_or_else(|error| unreachable!("a built-in dialect identity is valid: {error:?}"))
}

/// Which family of bytes a drive publishes.
///
/// One enum rather than a pair of booleans because the profiles are exclusive and the CSV one is qualified:
/// `csv_header` without `csv` was a state the type could hold and no reader could interpret.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputTarget {
    /// RFC 8259 JSON, one document per item.
    Json,
    /// NDJSON, whose codec owns the terminator between items.
    Ndjson,
    /// JSON Text Sequences (RFC 7464), whose codec owns the RS prefix and LF suffix of every item.
    JsonSeq,
    /// CSV. `header` selects the HEADERED profile, which derives one header row from the first item's keys and checks
    /// every later item against it. `utf8` selects the Unicode-capable output family (`csv.jqf-utf8@1`) — the same
    /// quoting/CRLF encoder under a name that does not advertise an RFC; `false` names the RFC-named
    /// `csv.jqf-rfc4180@1` profiles.
    Csv {
        /// Whether the headered profile is selected.
        header: bool,
        /// Whether the UTF-8-named output family is selected.
        utf8: bool,
    },
    /// Tab-separated TSV: TAB joins, LF terminates, no quoting.
    Tsv {
        /// Whether the headered profile is selected.
        header: bool,
    },
    /// One presentation renderer (`render.*@1`). Each record renders to its own complete frame; the facade appends the
    /// frame's final LF. The dialect names the exact renderer and the options carry the composition law
    /// (width/header/shape/separator), so a worker rebuilding its drive from the spec publishes byte-identical frames.
    Render {
        /// The registered render dialect identity (`render.gfm-table@1`, …).
        dialect: &'static str,
        /// The render composition options.
        options: jqf_codec_render::RenderEncodeOptions,
    },
    /// YAML (`yaml.block@1` and the other output profiles). The dialect names the registered output profile; the target
    /// schema is always the default core (the whole-document ladder derives a non-core schema from a YAML INPUT
    /// dialect, and a record input is never YAML). The codec owns each item's bytes INCLUDING the block profile's
    /// inter-document `---`; the facade owns the item LF (the registration's `ItemByteOwner` law). Stateful: the
    /// separator lives in the encoder factory, so a record drive publishing YAML plans SERIAL — see
    /// [`Self::is_stateful`].
    Yaml {
        /// The registered YAML output-profile identity (`yaml.block@1`, …).
        dialect: &'static str,
    },
    /// One markup document renderer (`xml`, `html`). Each record renders to its own complete synthesized `<root>`
    /// document; the facade appends each item's LF (the registrations' `ItemByteOwner` law). The markup encoders are
    /// stateless per item — unlike YAML's between-documents separator — so this target stays parallel-eligible.
    Markup {
        /// The registered format identity (`xml`, `html`).
        format: &'static str,
        /// The registered output-profile identity.
        dialect: &'static str,
    },
}

impl OutputTarget {
    /// Whether this target carries state ACROSS items.
    ///
    /// The headered delimited profiles do: one header row is derived from the FIRST item's keys. Split across morsels
    /// that becomes one header per morsel, so such an output is fenced off the parallel lane.
    #[must_use]
    pub const fn is_stateful(self) -> bool {
        matches!(
            self,
            Self::Csv { header: true, .. } | Self::Tsv { header: true } | Self::Yaml { .. }
        )
    }
}

/// One drive's output target, with all codecs' option values built and owned so [`OutputEncoding::options`] can borrow
/// whichever one the target selects.
pub(crate) struct OutputEncoding {
    target: OutputTarget,
    /// See [`JsonItemSuffix`].
    no_newline: bool,
    ndjson_options: NdjsonEncodeOptions,
    json_options: JsonEncodeOptions,
    json_seq_options: JsonSeqEncodeOptions,
    csv_options: CsvEncodeOptions,
    yaml_schema: jqf_codec_yaml::YamlTargetSchema,
    /// The never-read options payload for targets whose codecs take no options channel (markup): `Some` keeps the
    /// "always Some" law of [`Self::options`], and those factories ignore the payload entirely.
    no_options: (),
}

impl OutputEncoding {
    /// Builds all codecs' options inside the CALLER's ledger, which is what lets a worker reconstruct this from its own
    /// numeric grant.
    ///
    /// `no_newline` is `-j` / `--join-output`: the facade suffix between items is dropped. The flag also implies raw
    /// root strings, which the caller carries in `json.raw_strings`.
    pub(crate) fn new(
        target: OutputTarget,
        terminator: NdjsonTerminator,
        json: JsonEncodeOptions,
        no_newline: bool,
        csv_delimiter: Option<u8>,
    ) -> Self {
        Self {
            target,
            no_newline,
            ndjson_options: NdjsonEncodeOptions::new(terminator),
            json_options: json,
            json_seq_options: JsonSeqEncodeOptions::new(
                json,
                if json.raw_output_nul {
                    // `--raw-output0` implies `-j`'s no-LF law and replaces the LF with NUL.
                    JsonSeqSuffix::Nul
                } else if no_newline {
                    JsonSeqSuffix::NoSuffix
                } else {
                    JsonSeqSuffix::Lf
                },
            ),
            // The delimiter dial rides the CSV output too, so a tab-delimited request round-trips through CSV output
            // with the same delimiter it read. The TSV target binds its own tab/no-quote options, whatever the dial
            // says. The CLI's `parse_csv_delimiter` validates against the codec's own closed rule before a byte is
            // read, so a refusal here means a host that bypassed that parser: loud in debug, and the canonical default
            // stands because the worker reconstruction of this struct must stay infallible.
            csv_options: {
                if let Some(byte) = csv_delimiter {
                    debug_assert!(
                        jqf_codec_delimited::is_valid_delimiter(byte),
                        "an invalid CSV delimiter reached the encode options and silently defaults"
                    );
                }
                match target {
                    OutputTarget::Tsv { .. } => {
                        CsvEncodeOptions::try_new_tsv().unwrap_or_else(|_| CsvEncodeOptions::default())
                    }
                    _ => CsvEncodeOptions::try_new(csv_delimiter).unwrap_or_else(|_| CsvEncodeOptions::default()),
                }
            },
            // A record input is never YAML, so the target schema is always the default core: the whole-document
            // ladder's non-core schemas are derived from a YAML INPUT dialect (main.rs), which cannot occur here.
            yaml_schema: jqf_codec_yaml::YamlTargetSchema::default(),
            no_options: (),
        }
    }

    pub(crate) fn format(&self) -> FormatId {
        built_in_format(match self.target {
            OutputTarget::Ndjson => jqf_codec_core::record_options::NDJSON_FORMAT_ID,
            OutputTarget::JsonSeq => jqf_codec_core::record_options::JSON_SEQ_FORMAT_ID,
            OutputTarget::Csv { .. } => jqf_codec_core::record_options::CSV_FORMAT_ID,
            OutputTarget::Tsv { .. } => jqf_codec_core::record_options::TSV_FORMAT_ID,
            OutputTarget::Json => jqf_codec_core::record_options::JSON_FORMAT_ID,
            OutputTarget::Render { .. } => jqf_codec_render::FORMAT_ID,
            OutputTarget::Yaml { .. } => jqf_codec_yaml::FORMAT_ID,
            OutputTarget::Markup { format, .. } => format,
        })
    }

    pub(crate) fn dialect(&self) -> DialectId {
        built_in_dialect(match self.target {
            OutputTarget::Ndjson => jqf_codec_core::record_options::NDJSON_STRICT_DIALECT_ID,
            OutputTarget::JsonSeq => jqf_codec_core::record_options::JSON_SEQ_JQF_DIALECT_ID,
            OutputTarget::Csv {
                header: false,
                utf8: false,
            } => jqf_codec_core::record_options::CSV_JQF_RFC4180_DIALECT_ID,
            OutputTarget::Csv {
                header: true,
                utf8: false,
            } => jqf_codec_core::record_options::CSV_JQF_RFC4180_HEADER_DIALECT_ID,
            OutputTarget::Csv {
                header: false,
                utf8: true,
            } => jqf_codec_core::record_options::CSV_JQF_UTF8_DIALECT_ID,
            OutputTarget::Csv {
                header: true,
                utf8: true,
            } => jqf_codec_core::record_options::CSV_JQF_UTF8_HEADER_DIALECT_ID,
            OutputTarget::Tsv { header: false } => jqf_codec_core::record_options::TSV_JQF_LF_DIALECT_ID,
            OutputTarget::Tsv { header: true } => jqf_codec_core::record_options::TSV_JQF_LF_HEADER_DIALECT_ID,
            OutputTarget::Json => jqf_codec_core::record_options::RFC8259_DIALECT_ID,
            OutputTarget::Render { dialect, .. }
            | OutputTarget::Yaml { dialect }
            | OutputTarget::Markup { dialect, .. } => dialect,
        })
    }

    /// The encode options for the selected target.
    ///
    /// Always `Some`: a target whose options are omitted takes the CODEC's own defaults, which for JSON is compact — so
    /// omitting them here is how a route silently stops honoring the caller's indent.
    pub(crate) fn options(&self) -> &(dyn core::any::Any + Send + Sync) {
        match &self.target {
            OutputTarget::Ndjson => &self.ndjson_options,
            OutputTarget::JsonSeq => &self.json_seq_options,
            OutputTarget::Csv { .. } | OutputTarget::Tsv { .. } => &self.csv_options,
            OutputTarget::Json => &self.json_options,
            OutputTarget::Render { options, .. } => options,
            OutputTarget::Yaml { .. } => &self.yaml_schema,
            OutputTarget::Markup { .. } => &self.no_options,
        }
    }

    /// Who writes the byte between items.
    ///
    /// For JSON output the facade owns the item suffix; for render output the facade owns each frame's final LF (the
    /// codec publishes only the frame's interior LFs); for NDJSON, json-seq, and CSV output the CODEC appends its own
    /// framing inside its staging buffer (json-seq owns both the RS prefix and the LF suffix), so the facade suffix is
    /// empty and an item can never receive two newlines.
    ///
    /// JSON suffix order: `--raw-output0` replaces the suffix with NUL and wins over `-j`. That is the same order the
    /// CLI already claims, derived here so the parallel value lane cannot emit LF for NUL.
    pub(crate) const fn framing(&self) -> FacadeFraming<'static> {
        if matches!(self.target, OutputTarget::Json) {
            FacadeFraming::item_suffix(
                JsonItemSuffix::from_dials(self.json_options.raw_output_nul, self.no_newline).as_bytes(),
            )
        } else if matches!(
            self.target,
            OutputTarget::Render { .. } | OutputTarget::Yaml { .. } | OutputTarget::Markup { .. }
        ) {
            FacadeFraming::item_suffix(b"\n")
        } else {
            FacadeFraming::item_suffix(b"")
        }
    }
}

/// The JSON facade item-suffix law: one derivation for every consumer.
///
/// `--raw-output0` replaces the suffix with NUL and wins over `-j`. Non-JSON targets never use this type. NDJSON /
/// json-seq / CSV / TSV: the codec owns framing. Render / YAML / markup: the facade writes LF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonItemSuffix {
    raw_output_nul: bool,
    no_newline: bool,
}

impl JsonItemSuffix {
    /// Builds the law from the two style dials.
    #[must_use]
    pub const fn from_dials(raw_output_nul: bool, no_newline: bool) -> Self {
        Self {
            raw_output_nul,
            no_newline,
        }
    }

    /// The exact facade suffix those dials produce.
    #[must_use]
    pub const fn as_bytes(self) -> &'static [u8] {
        if self.raw_output_nul {
            b"\0"
        } else if self.no_newline {
            b""
        } else {
            b"\n"
        }
    }
}

/// The decode-side request glue both drives share: codec registrations, catalog, output encoding, decode policy, and
/// the retained source. The two drives differ in exactly two laws — whether CSV joins the catalog and whether adjacent
/// values are allowed — so both are parameters, never copied glue . The `'r` lifetime is the borrow of the local
/// registrations array inside [`build_request`]; the closure receives the pieces before that array drops.
pub(crate) struct DriveRequest<'a, 'r> {
    /// The catalog over this request's codec inventory.
    pub(crate) catalog: CodecCatalog<'r, 'static>,
    /// The payload input format (always the built-in JSON family).
    pub(crate) format: FormatId,
    /// The payload input dialect.
    pub(crate) dialect: DialectId,
    /// The output format the encoder selects.
    pub(crate) output_format: FormatId,
    /// The output dialect the encoder selects.
    pub(crate) output_dialect: DialectId,
    /// Construction and cooperative-resume policy. Carries the borrow of the local encode options, so it uses the short
    /// `'r` lifetime.
    pub(crate) policy: PipelinePolicy<'r>,
    /// The facade framing between items.
    pub(crate) framing: FacadeFraming<'a>,
    /// The retained source the drives decode.
    pub(crate) source: ResolvedSource<'a>,
}

/// Builds the request glue both drives share and hands it to `with`, which runs the drive's own tail (requirement
/// lowering and the drive call stay in the caller — they are the parts that differ).
///
/// The catalog arrives from the caller: the drives no longer construct the codec registrations themselves, so the
/// `runtime → codec-json/delimited` dep edges are gone. The value lane passes the same five-registration record catalog
/// the record lane does; its own targets are JSON-family only, so the csv/tsv entries are never selected.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_request<'a, R>(
    target: OutputTarget,
    terminator: NdjsonTerminator,
    json_style: JsonEncodeOptions,
    no_newline: bool,
    csv_delimiter: Option<u8>,
    catalog: CodecCatalog<'static, 'static>,
    source_name: &'a str,
    bytes: &'a [u8],
    allow_adjacent_values: bool,
    cooperative_credits: u32,
    // The frame-transition ceiling (`--max-iterations`); `None` is uncapped.
    max_iterations: Option<u64>,
    split: Option<&'a jqf_engine::CompiledProgram>,
    with: impl for<'r> FnOnce(DriveRequest<'a, 'r>) -> R,
) -> R {
    let encoding = OutputEncoding::new(target, terminator, json_style, no_newline, csv_delimiter);
    let output_format = encoding.format();
    let output_dialect = encoding.dialect();
    let encode_options = Some(encoding.options());
    let framing = encoding.framing();
    // The decode side of these drives always reads the built-in JSON family (the payload input); the request's dialect
    // names it.
    let input_dialect = built_in_dialect(jqf_codec_core::record_options::RFC8259_DIALECT_ID);
    let policy = PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: &input_dialect,
            options: None,
            allow_adjacent_values,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options,
        cooperative_credits,
        split,

        max_iterations,
    };
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(0), SourceKind::Input),
        source_name,
        bytes,
        0,
    );
    with(DriveRequest {
        catalog,
        format: built_in_format(jqf_codec_core::record_options::JSON_FORMAT_ID),
        dialect: built_in_dialect(jqf_codec_core::record_options::RFC8259_DIALECT_ID),
        output_format,
        output_dialect,
        policy,
        framing,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::{JsonItemSuffix, OutputEncoding, OutputTarget};
    use jqf_codec_json::{JsonEncodeOptions, ndjson::NdjsonTerminator};

    fn json_options(raw_output_nul: bool) -> JsonEncodeOptions {
        JsonEncodeOptions {
            raw_output_nul,
            ..JsonEncodeOptions::default()
        }
    }

    fn encoding(raw_output_nul: bool, no_newline: bool) -> OutputEncoding {
        OutputEncoding::new(
            OutputTarget::Json,
            NdjsonTerminator::Lf,
            json_options(raw_output_nul),
            no_newline,
            None,
        )
    }

    #[test]
    fn json_item_suffix_emits_nul_when_raw_output_nul_is_set() {
        assert_eq!(JsonItemSuffix::from_dials(true, false).as_bytes(), b"\0");
        assert_eq!(
            JsonItemSuffix::from_dials(true, true).as_bytes(),
            b"\0",
            "raw-output0 wins over join-output"
        );
        assert_eq!(JsonItemSuffix::from_dials(false, true).as_bytes(), b"");
        assert_eq!(JsonItemSuffix::from_dials(false, false).as_bytes(), b"\n");
    }

    fn assert_suffix(encoding: &OutputEncoding, expected: &'static [u8]) {
        assert_eq!(
            format!("{:?}", encoding.framing()),
            format!("{:?}", jqf_sdk::FacadeFraming::item_suffix(expected))
        );
    }

    #[test]
    fn json_framing_honors_raw_output_nul() {
        assert_suffix(&encoding(true, false), b"\0");
        assert_suffix(&encoding(true, true), b"\0");
        assert_suffix(&encoding(false, true), b"");
        assert_suffix(&encoding(false, false), b"\n");
    }

    #[test]
    fn ndjson_keeps_an_empty_facade_suffix() {
        let ndjson = OutputEncoding::new(
            OutputTarget::Ndjson,
            NdjsonTerminator::Lf,
            json_options(true),
            false,
            None,
        );
        assert_suffix(&ndjson, b"");
    }

    #[test]
    fn render_yaml_markup_get_facade_lf() {
        let yaml = OutputEncoding::new(
            OutputTarget::Yaml {
                dialect: "yaml.block@1",
            },
            NdjsonTerminator::Lf,
            json_options(false),
            false,
            None,
        );
        assert_suffix(&yaml, b"\n");
        let markup = OutputEncoding::new(
            OutputTarget::Markup {
                format: "xml",
                dialect: "xml.document@1",
            },
            NdjsonTerminator::Lf,
            json_options(false),
            false,
            None,
        );
        assert_suffix(&markup, b"\n");
    }
}
