//! Opaque record-format identity strings and the shared record-route slot.
//!
//! Option structs, indent/fill, and terminator spellings live with the grammar owner (`jqf-codec-json`,
//! `jqf-codec-delimited`). This module names the formats so the catalog and CLI can match them without importing a
//! parser.

use crate::RouteSlot;

/// Stable strict-JSON format identity text.
pub const JSON_FORMAT_ID: &str = "json";
/// Stable RFC 8259 (strict JSON) input dialect identity text.
pub const RFC8259_DIALECT_ID: &str = "rfc8259";

/// Stable NDJSON format identity text.
pub const NDJSON_FORMAT_ID: &str = "ndjson";
/// Stable strict NDJSON dialect identity text.
pub const NDJSON_STRICT_DIALECT_ID: &str = "ndjson.strict@1";
/// Stable recovering NDJSON dialect identity text.
pub const NDJSON_RECOVERING_DIALECT_ID: &str = "ndjson.recovering@1";

/// Stable JSON Text Sequence format identity text.
pub const JSON_SEQ_FORMAT_ID: &str = "json-seq";
/// Stable strict JSON Text Sequence dialect identity text.
pub const JSON_SEQ_STRICT_DIALECT_ID: &str = "json-seq.strict@1";
/// Stable JSON Text Sequence output dialect identity text.
pub const JSON_SEQ_JQF_DIALECT_ID: &str = "json-seq.jqf@1";

/// Stable CSV format identity text.
pub const CSV_FORMAT_ID: &str = "csv";
/// Stable RFC 4180 input dialect identity text.
pub const CSV_RFC4180_DIALECT_ID: &str = "csv.rfc4180@1";
/// Stable HEADERED RFC 4180 input dialect identity text.
pub const CSV_RFC4180_HEADER_DIALECT_ID: &str = "csv.rfc4180-header@1";
/// Stable deterministic RFC 4180 output-profile identity text.
pub const CSV_JQF_RFC4180_DIALECT_ID: &str = "csv.jqf-rfc4180@1";
/// Stable deterministic HEADERED RFC 4180 output-profile identity text.
pub const CSV_JQF_RFC4180_HEADER_DIALECT_ID: &str = "csv.jqf-rfc4180-header@1";
/// Stable Unicode-capable CSV input dialect identity text: the RFC 4180 quoting grammar with UTF-8 admission instead of
/// the frozen ASCII TEXTDATA alphabet.
pub const CSV_UTF8_DIALECT_ID: &str = "csv.utf8@1";
/// Stable HEADERED Unicode-capable CSV input dialect identity text.
pub const CSV_UTF8_HEADER_DIALECT_ID: &str = "csv.utf8-header@1";
/// Stable deterministic Unicode-capable CSV output-profile identity text: the same quoting/CRLF encode family as the
/// RFC-named profile.
pub const CSV_JQF_UTF8_DIALECT_ID: &str = "csv.jqf-utf8@1";
/// Stable deterministic HEADERED Unicode-capable CSV output-profile identity.
pub const CSV_JQF_UTF8_HEADER_DIALECT_ID: &str = "csv.jqf-utf8-header@1";

/// Stable TSV format identity text (the second delimited grammar).
pub const TSV_FORMAT_ID: &str = "tsv";
/// Stable TSV input dialect identity text: tab-delimited, no quote.
pub const TSV_UTF8_DIALECT_ID: &str = "tsv.utf8@1";
/// Stable HEADERED TSV input dialect identity text.
pub const TSV_UTF8_HEADER_DIALECT_ID: &str = "tsv.utf8-header@1";
/// Stable deterministic TSV output-profile identity text: TAB joins, LF terminates.
pub const TSV_JQF_LF_DIALECT_ID: &str = "tsv.jqf-lf@1";
/// Stable deterministic HEADERED TSV output-profile identity text.
pub const TSV_JQF_LF_HEADER_DIALECT_ID: &str = "tsv.jqf-lf-header@1";

/// The record-provider slot every record format advertises.
///
/// All three record formats (NDJSON, json-seq, CSV/TSV) advertise exactly one record route at slot zero; a single
/// shared constant replaces three per-codec copies that could drift.
pub const RECORD_ROUTE_SLOT: RouteSlot = RouteSlot::new(0);
