//! The CLI's input surface: reading stdin and input files, the `-R` raw transform, and the binding/diff file readers
//! that run BEFORE stdin is touched (the adopted precedence). Everything here produces raw bytes or values; no route
//! decision happens in this module.

use std::fs;
use std::io::{self, Read as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::args::{CliFormat, CliInputSelection, RecordInputKind};
use crate::errors::{CliFailure, ExitClass, render_pipeline_failure, resource_note};

/// A positional input file could not be opened (item 2). jq reports each unreadable file to stderr and CONTINUES with
/// the readable ones, then forces exit 2 at the end — the failure class overrides the run's own exit (a compile error's
/// 3 still wins, because jq rejects the program before any file is opened; a runtime error's 5 does not). One request
/// runs per process, so the flag is process state; the CLI's exit decision in `main_run` reads it after the request
/// thread joins.
static MISSING_POSITIONAL_FILE: AtomicBool = AtomicBool::new(false);

/// Records that a positional input file failed to open. The diagnostic was printed at the read site (jq prints one line
/// per bad file, in file order, and keeps going); this flag carries only the exit-class consequence.
pub(crate) fn record_missing_file() {
    MISSING_POSITIONAL_FILE.store(true, Ordering::Relaxed);
}

/// Whether any positional input file failed to open. The final exit decision forces the adopted exit 2 when it is set.
pub(crate) fn missing_positional_file() -> bool {
    MISSING_POSITIONAL_FILE.load(Ordering::Relaxed)
}

/// Reads the whole of stdin, sizing the buffer from the source when the source can say how long it is.
///
/// A regular file redirected onto stdin (`jqf. < catalog.json`, the shape every benchmark and most invocations use)
/// knows its own length, so the buffer is allocated once at exactly that length instead of doubling into it — four
/// reallocations and 10 MB of copying saved on the fixture, and no headroom to trim afterwards. A PIPE knows nothing,
/// so its reads land in EXACT-SIZED chunks that are consolidated ONCE into an exactly-sized retained buffer (§0b) —
/// never one geometrically growing `Vec`.
///
/// The `take` bound is unchanged and stays the authority: the hint only sizes the buffer, and a file that grows between
/// the stat and the read is still cut off at the same limit.
///
/// # Why this is a MEMORY fix and not a convenience
///
/// The retained input is live for the WHOLE request — every route borrows it — so the read buffer's growth headroom is
/// not transient, it is a floor under peak RSS. On the 10 MB catalog fixture the geometric buffer settled at 16,777,216
/// bytes for 9,932,426 of input: 6.8 MB held from the first read to process exit. Against the streaming lanes, whose
/// whole peak is about 30 MB, that headroom was more than half of it — `.catalog[] |.name` falls 30,441,472 →
/// 13,631,488 bytes of peak RSS on this change alone.
///
/// A POST-HOC `shrink_to_fit` cannot do this and was measured not to: peak RSS counts pages TOUCHED, and the read has
/// already touched every page of the oversized buffer, so releasing it afterwards moves the peak by kilobytes. The only
/// fix is never to allocate the headroom, which needs a length nobody but the source can supply.
///
/// The pipe arm has the same disease in a worse form (§0b, measured on a 200 MB fold through a pipe): geometric growth
/// to the 8 MiB cap and 4 MiB steps thereafter means ~125 reallocations for a 200 MB input, and every reallocation
/// strands a freed block in the allocator — peak RSS measured 3.28× the input while only ONE copy was ever live (the
/// ledger's ambient stayed ≈input). Chunked accumulation touches each byte twice at worst — once into its exact-sized
/// chunk, once in the single consolidation copy — so the peak is bounded near 2× live instead of live-plus-stranded-
/// growth, and after the chunks drop the process settles at exactly one copy, the file route's shape.
pub(crate) fn read_stdin() -> Result<Vec<u8>, CliFailure> {
    let stdin = io::stdin();
    // `Stdin` is not a `File`, so the length hint comes from a borrowed descriptor: `File:from` would close stdin on
    // drop, and `ManuallyDrop` around it is the standard way to stat a descriptor you do not own. Unix-only: the
    // descriptor API is POSIX, and on other platforms the hint is simply absent, which falls back to the ordinary
    // geometric read.
    let hint = {
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd as _, FromRawFd as _};
            // SAFETY: the file is wrapped in `ManuallyDrop`, so the borrowed stdin descriptor is never closed by this
            // scope; nothing reads through it.
            let file = core::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(stdin.as_raw_fd()) });
            file.metadata()
                .ok()
                .filter(std::fs::Metadata::is_file)
                .map(|metadata| metadata.len())
                .and_then(|length| usize::try_from(length).ok())
        }
        #[cfg(not(unix))]
        {
            None
        }
    };
    let mut reader = stdin.lock();
    read_stdin_from(&mut reader, hint)
}

/// The read loop behind [`read_stdin`], split from the descriptor probing so the growth law is unit-testable over an
/// in-memory reader (a test cannot spawn a real pipe into this process's stdin).
///
/// Known length (`hint`): one exact allocation, straight into the retained buffer. Unknown length (a pipe, FIFO, or
/// socket): exact-sized chunks consolidated once — the shape §0b prescribes.
fn read_stdin_from(reader: &mut impl io::Read, hint: Option<usize>) -> Result<Vec<u8>, CliFailure> {
    const CHUNK: usize = 64 * 1024;
    if let Some(length) = hint {
        // Known length: read straight into the retained buffer. A 64 KiB bounce would memcpy every byte twice on the
        // path every benchmark uses (`jqf. < file`). The hint is the take bound: a file that grows between stat and
        // read is still cut off at that length.
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(length)
            .map_err(|_| CliFailure::from("cannot grow the stdin buffer"))?;
        let limit = u64::try_from(length).unwrap_or(u64::MAX);
        reader
            .take(limit)
            .read_to_end(&mut buffer)
            .map_err(|error| CliFailure::from(format!("cannot read stdin: {error}")))?;
        return Ok(buffer);
    }
    // No length: reads land in EXACT-SIZED chunks — never one geometrically growing buffer, whose every reallocation
    // strands a freed block in the allocator for the rest of the request (measured 3.28× input peak on a 200 MB piped
    // fold with only one copy live; §0b). The chunk scratch is one fallible 64 KiB allocation, so a ceiling below the
    // chunk size still refuses at read instead of aborting.
    let mut chunk = Vec::new();
    chunk
        .try_reserve_exact(CHUNK)
        .map_err(|_| CliFailure::from("cannot allocate the stdin read buffer"))?;
    chunk.resize(CHUNK, 0);
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut total: usize = 0;
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(CliFailure::from(format!("cannot read stdin: {error}"))),
        };
        // Each piece is sized exactly to its read: no headroom anywhere in the accumulation, so the live footprint
        // during the read IS the input. Fallible reserve keeps the ceiling refusal graceful.
        let mut piece = Vec::new();
        piece
            .try_reserve_exact(read)
            .map_err(|_| CliFailure::from("cannot grow the stdin buffer"))?;
        piece.extend_from_slice(&chunk[..read]);
        total = total.saturating_add(read);
        chunks.push(piece);
    }
    // One consolidation: the retained buffer is reserved at exactly the total and filled by draining the chunks, so
    // residency falls monotonically from the two-copy consolidation moment back to the single live copy the request
    // actually keeps. `capacity == len` afterwards — the caller's buffer carries no growth headroom.
    let mut input = Vec::new();
    input
        .try_reserve_exact(total)
        .map_err(|_| CliFailure::from("cannot grow the stdin buffer"))?;
    for piece in chunks.drain(..) {
        input.extend_from_slice(&piece);
    }
    Ok(input)
}

/// Whether stdin is a NON-SEEKABLE streaming source — a pipe, FIFO, or socket — rather than a seekable regular file or
/// an interactive terminal.
///
/// The 058 W4 seekability rule reads THIS fact, never "is the input stdin": a regular file redirected onto stdin is
/// seekable and keeps every whole-read fast route (the shape every benchmark uses), while a pipe or FIFO streams per
/// value. A character device (a terminal, `/dev/null`) is neither: it reads whole, exactly as it does today — a
/// terminal is interactive, where per-line framing would break a multi-line document a user is typing, and there is no
/// writer pushing bytes, so no hang to fix.
///
/// The fact is determined from the descriptor itself (fstat), never from whether a filename was given: the same stdin
/// may be a regular file, a FIFO, or a pipe depending on how the shell invoked the process.
pub(crate) fn stdin_is_streaming_source() -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::fs::FileTypeExt as _;
        let stdin = io::stdin();
        // SAFETY: the descriptor is wrapped in `ManuallyDrop`, so the borrowed stdin descriptor is never closed by this
        // scope; nothing reads through it. The same shape `read_stdin` uses for its length hint.
        let file = core::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(stdin.as_raw_fd()) });
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        metadata.file_type().is_fifo() || metadata.file_type().is_socket()
    }
    // Tier 2 (Windows and other platforms): the conservative default is the whole-read path, which is today's behavior
    // everywhere. Streaming stdin is a Unix pipe/FIFO capability and stays one until a platform arm names its own
    // streaming kinds.
    #[cfg(not(unix))]
    {
        false
    }
}

/// Renders an `io:Error` the way jq does: Rust's Display appends a `(os error N)` parenthetical that jq never prints.
pub(crate) fn io_error_text(error: &io::Error) -> String {
    let text = error.to_string();
    match text.strip_suffix(&format!(" (os error {})", error.raw_os_error().unwrap_or(0))) {
        Some(stripped) => stripped.to_owned(),
        None => text,
    }
}

/// Reads the request input into ONE retained byte stream and its per-file provenance: stdin (labelled `<stdin>`, no
/// ranges) when no file is named, or every positional file's bytes appended in argument order with NO separator (the
/// adopted multi-file law — a file ending `2` followed by one starting `3` makes the single value `23`,). Each file's
/// contiguous byte range lets `input_filename` and the per-file `input_line_number` reset attribute a value to the file
/// holding its LAST byte, exactly as jq does. The file bytes are appended DIRECTLY into the combined buffer, so a
/// multi-file request holds one retained copy of the input, not one per file.
pub(crate) fn read_combined_input<'a>(
    files: &[impl AsRef<Path>],
    labels: &'a [String],
) -> Result<(Vec<u8>, Vec<jqf_source::SourceFileRange<'a>>), CliFailure> {
    if files.is_empty() {
        return Ok((read_stdin()?, Vec::new()));
    }
    let mut bytes = Vec::new();
    let mut ranges = Vec::with_capacity(files.len());
    for (path, label) in files.iter().zip(labels.iter()) {
        let start = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if let Err(failure) = append_input_file(&mut bytes, path.as_ref(), label) {
            // the adopted multi-file law (item 2): an unreadable positional file is REPORTED and skipped, never a
            // request abort — the readable files still run, and the request exits 2 at the end (the exit-class override
            // lives in `main_run`). The diagnostic prints here, in file order, exactly as jq interleaves it with its
            // per-file opens.
            record_missing_file();
            crate::eprint_line_buffered(&format!("jqf: {failure}"));
        }
        ranges.push(jqf_source::SourceFileRange::new(
            label,
            start,
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        ));
    }
    Ok((bytes, ranges))
}

/// Appends one input file's bytes to `bytes`, growing it in place so the retained buffer is the ONLY copy of the file's
/// content (a file read whole then copied would hold two at peak). No ceiling, exactly as the eager read has none.
fn append_input_file(bytes: &mut Vec<u8>, path: &Path, label: &str) -> Result<(), CliFailure> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).map_err(|error| CliFailure::Message {
        // the adopted wording (items 2 and 6): `Could not open file <path>: <reason>`, with `io_error_text` stripping
        // Rust's `(os error N)` suffix that jq never prints. Scripts that grep stderr must not break on the wording or
        // the suffix.
        class: ExitClass::Usage,
        message: format!("error: Could not open file {label}: {}", io_error_text(&error)),
    })?;
    if let Some(length) = file
        .metadata()
        .ok()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
    {
        bytes
            .try_reserve_exact(length)
            .map_err(|_| CliFailure::from(format!("cannot grow the input buffer for {label}")))?;
    }
    let mut chunk = Vec::new();
    chunk
        .try_reserve_exact(64 * 1024)
        .map_err(|_| CliFailure::from(format!("cannot allocate the read buffer for {label}")))?;
    chunk.resize(64 * 1024, 0);
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                // Distinct from the open arm's `Could not open file`: the file OPENED and the failure came mid-read
                // (EISDIR on a directory, EIO on a device), and a script grepping stderr must be able to tell the two
                // apart.
                return Err(CliFailure::Message {
                    class: ExitClass::Usage,
                    message: format!("error: Could not read file {label}: {}", io_error_text(&error)),
                });
            }
        }
    }
    Ok(())
}

/// Parses exactly one strict JSON value from `text`, returning `None` for any malformed text or trailing content. Backs
/// the adopted `--argjson`, whose every failure — malformed or trailing — is the adopted `invalid JSON text passed to
/// --argjson`. The value is read by the ENGINE's jq-faithful JSON reader, the same reader the `fromjson` builtin uses,
/// so `--argjson` accepts exactly what the adopted parser accepts.
pub(crate) fn parse_single_json_value(
    text: &str,
    resources: &jqf_resource::ResourceContext<'_>,
) -> Option<jqf_data::Value> {
    jqf_engine::decode_json(text, resources).ok()
}

/// Reads one file-backed binding's file, with the adopted failure frame.
///
/// The read happens during argument processing, BEFORE stdin is touched (the adopted precedence), and the message is
/// the adopted exit-2 `Bad JSON in <flag> NAME FILE: Could not open FILE: <reason>`. The flag name differs between the
/// two kinds, so it is spelled by the caller.
pub(crate) fn read_binding_file(flag: &str, name: &str, file: impl AsRef<Path>) -> Result<Vec<u8>, CliFailure> {
    let file = file.as_ref();
    let label = file.display();
    std::fs::read(file).map_err(|error| CliFailure::Message {
        class: ExitClass::Usage,
        message: format!(
            "Bad JSON in {flag} {name} {label}: Could not open {label}: {}",
            io_error_text(&error)
        ),
    })
}

/// Builds the `$NAME` value for the adopted `--slurpfile NAME FILE`: the array of every adjacent JSON value read from
/// `FILE`, in file order.
///
/// The file was read (as bytes) before stdin; every failure here — unparsable content — is the adopted exit-2 `Bad JSON
/// in --slurpfile NAME FILE: …` with the parse message in the adopted spelling and the position absolute across the
/// whole file. Invalid UTF-8 is converted lossily before the parse: each invalid byte becomes one U+FFFD, which matches
/// jq exactly inside strings (the value is accepted) and in the fault CLASS outside them; only the column of a fault
/// message can drift (one invalid byte counts as its three UTF-8 bytes instead of one), which is a recorded narrowing.
pub(crate) fn slurpfile_value(
    name: &str,
    file: &str,
    bytes: &[u8],
    resources: &jqf_resource::ResourceContext<'_>,
) -> Result<jqf_data::Value, CliFailure> {
    let text = String::from_utf8_lossy(bytes);
    let values = jqf_engine::decode_json_sequence(&text, resources).map_err(|error| {
        let message = match &error {
            jqf_engine::EngineRunError::Raised(jqf_data::Value::String(message)) => message.as_str().to_owned(),
            _ => {
                return CliFailure::Message {
                    class: ExitClass::Usage,
                    message: format!("Bad JSON in --slurpfile {name} {file}: cannot allocate"),
                };
            }
        };
        CliFailure::Message {
            class: ExitClass::Usage,
            message: format!("Bad JSON in --slurpfile {name} {file}: {message}"),
        }
    })?;
    let array = jqf_data::Array::try_from_vec(values).map_err(|_| CliFailure::Message {
        class: ExitClass::Usage,
        message: format!("Bad JSON in --slurpfile {name} {file}: cannot allocate"),
    })?;
    Ok(jqf_data::Value::Array(array))
}

/// Reads one `--diff` file as ONE document through the codec catalog (seam 1): the per-side format/dialect selection
/// resolves like any input, the decode is the same whole-document drive the edit lane runs, and the file must contain
/// exactly one document — zero or several is a defined usage error naming the count. THE YAML EXCEPTION (syntax165 T6):
/// a YAML file is a `---`-separated document STREAM, never a single document, so a multi-document YAML side compares
/// its FIRST document instead of refusing — one item per unit, the same law every other YAML route serves. Formats that
/// are genuinely one document (TOML, CBOR) keep the exactly-one law. Decode REFUSALS keep their codec diagnostics: a
/// TOML parse error inside `--diff` reads exactly like the same error outside it (gate 1). `decode_policy` carries the
/// per-side adjacency decision; the caller derives it from the side's own format, never the request's.
#[allow(
    clippy::too_many_lines,
    reason = "the per-side diff decode is one sequential law: read, resolve, decode (access or \
              record), count; splitting it would scatter the exactly-one law"
)]
pub(crate) fn parse_diff_document(
    path: impl AsRef<Path>,
    selection: CliInputSelection,
    catalog: jqf_sdk::CodecCatalog<'_, '_>,
    decode_policy: jqf_sdk::PipelinePolicy<'_>,
    resources: &mut jqf_resource::ResourceContext<'_>,
) -> Result<jqf_data::Value, CliFailure> {
    let path = path.as_ref();
    let label = path.to_string_lossy();
    let bytes = fs::read(path).map_err(|error| CliFailure::Message {
        class: ExitClass::Usage,
        message: format!("Could not open {label}: {}", io_error_text(&error)),
    })?;
    // The diff lane reads its two sides itself (the request took no eager read), so each side joins the request's input
    // where it is read.
    resources
        .charge_input(bytes.len() as u64)
        .map_err(|error| format!("error: cannot account the input: {}", resource_note(error)))?;
    let format = jqf_data::FormatId::try_new(selection.format.id()).map_err(|_| CliFailure::Message {
        class: ExitClass::Usage,
        message: format!("invalid built-in format identity: {}", selection.format.id()),
    })?;
    let dialect = jqf_data::DialectId::try_new(selection.dialect.id()).map_err(|_| CliFailure::Message {
        class: ExitClass::Usage,
        message: format!("invalid built-in dialect identity: {}", selection.dialect.id()),
    })?;
    let source = jqf_source::ResolvedSource::new(
        jqf_source::SourceRef::new(jqf_source::SourceId::new(0), jqf_source::SourceKind::Input),
        &label,
        &bytes,
        0,
    );
    let values = match selection.dialect.record_kind() {
        // A record-only side (NDJSON/json-seq/CSV) registers no access-ladder decoder: the provider FRAMES payload
        // ranges and each payload is decoded through the payload codec's ordinary ladder (the record-stream law —
        // framing is not a document route).
        Some(kind) => {
            let payload_format: &str;
            let payload_dialect: &str;
            // The header shape reaches the PAYLOAD decode through the decode request's options — the same seam the
            // record drive uses (the framer consumed the header; the payload provider must learn it to key the row).
            // Only CSV carries payload options; the holder lives beside the options so the borrow survives the match.
            let mut decode_options: Option<&(dyn core::any::Any + Send + Sync)> = None;
            let csv_options_holder;
            let provider = match kind {
                RecordInputKind::Ndjson => {
                    let options = jqf_codec_json::ndjson::NdjsonDecodeOptions::try_new(None, bytes.len() as u64)
                        .map_err(|error| render_record_options_error(&error))?;
                    payload_format = jqf_codec_json::FORMAT_ID;
                    payload_dialect = jqf_codec_json::RFC8259_DIALECT_ID;
                    jqf_codec_json::ndjson::create_record_provider(
                        source,
                        selection
                            .dialect
                            .ndjson_profile()
                            .unwrap_or(jqf_codec_json::ndjson::NdjsonProfile::Strict),
                        options,
                        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
                        decode_policy.decode.validation,
                        resources,
                    )
                    .map_err(|error| render_record_options_error(&error))?
                }
                RecordInputKind::JsonSeq => {
                    let options = jqf_codec_json::seq::JsonSeqDecodeOptions::try_new(None, bytes.len() as u64)
                        .map_err(|error| render_record_options_error(&error))?;
                    payload_format = jqf_codec_json::FORMAT_ID;
                    payload_dialect = jqf_codec_json::RFC8259_DIALECT_ID;
                    jqf_codec_json::seq::create_record_provider(
                        source,
                        selection
                            .dialect
                            .json_seq_profile()
                            .unwrap_or(jqf_codec_json::seq::JsonSeqProfile::Strict),
                        options,
                        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
                        decode_policy.decode.validation,
                        resources,
                    )
                    .map_err(|error| render_record_options_error(&error))?
                }
                RecordInputKind::Csv { header, tsv } => {
                    let options = if tsv {
                        jqf_codec_delimited::CsvDecodeOptions::try_new_tsv(None, bytes.len() as u64, header)
                    } else {
                        jqf_codec_delimited::CsvDecodeOptions::try_new(
                            selection.csv_delimiter,
                            None,
                            bytes.len() as u64,
                            header,
                        )
                    }
                    .map_err(|error| render_record_options_error(&error))?;
                    csv_options_holder = options;
                    // The payload format is the GRAMMAR's own (134): the TSV grammar decodes records as `tsv`, never a
                    // hard-coded csv id.
                    payload_format = options.format_id();
                    payload_dialect = options.dialect_id();
                    decode_options = Some(&csv_options_holder as &(dyn core::any::Any + Send + Sync));
                    jqf_codec_delimited::create_record_provider(
                        source,
                        csv_options_holder,
                        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
                        decode_policy.decode.validation,
                        resources,
                    )
                    .map_err(|error| render_record_options_error(&error))?
                }
            };
            let payload_format = jqf_data::FormatId::try_new(payload_format).map_err(|_| CliFailure::Message {
                class: ExitClass::Usage,
                message: "invalid built-in payload format identity".into(),
            })?;
            let payload_dialect = jqf_data::DialectId::try_new(payload_dialect).map_err(|_| CliFailure::Message {
                class: ExitClass::Usage,
                message: "invalid built-in payload dialect identity".into(),
            })?;
            let record_policy = jqf_sdk::PipelinePolicy {
                decode: jqf_codec_core::DecodeRequest {
                    // A record payload is exactly one complete text.
                    allow_adjacent_values: false,
                    options: decode_options,
                    ..decode_policy.decode
                },
                ..decode_policy
            };
            jqf_sdk::decode_record_values::<io::Error>(
                catalog,
                provider,
                record_slot(kind),
                source,
                &payload_format,
                &payload_dialect,
                record_policy,
                resources,
            )
            .map_err(|error| render_pipeline_failure(&error))?
        }
        None => {
            jqf_sdk::decode_source_values::<io::Error>(catalog, source, &format, &dialect, decode_policy, resources)
                .map_err(|error| render_pipeline_failure(&error))?
        }
    };
    match values.len() {
        1 => Ok(values.into_iter().next().expect("the exactly-one arm sees one value")),
        // A YAML side is a document stream (syntax165 T6): compare the first unit, never refuse the lane for the stream
        // shape every other YAML route serves.
        count if count > 1 && selection.format == CliFormat::Yaml => {
            Ok(values.into_iter().next().expect("the count arm sees one"))
        }
        count => Err(CliFailure::Message {
            // The exactly-one-document law (scope): a multi-document file in a genuinely one-document format is a
            // defined usage error naming the count, never a silent first-document read.
            class: ExitClass::Usage,
            message: format!("{label}: expected exactly one document, found {count}"),
        }),
    }
}

/// The one record-route slot every record provider in this module opens.
fn record_slot(kind: RecordInputKind) -> jqf_codec_core::RouteSlot {
    match kind {
        RecordInputKind::Ndjson => jqf_codec_json::ndjson::RECORD_ROUTE_SLOT,
        RecordInputKind::JsonSeq => jqf_codec_json::seq::RECORD_ROUTE_SLOT,
        RecordInputKind::Csv { .. } => jqf_codec_delimited::RECORD_ROUTE_SLOT,
    }
}

/// Renders one record-provider construction failure (options or opening) with the file's path as the frame — the
/// decode-refusal law: a bad option or a malformed stream reads exactly as it reads on the record route.
fn render_record_options_error(error: &jqf_codec_core::CodecError) -> CliFailure {
    CliFailure::Codec {
        kind: error.kind(),
        diagnostic: crate::errors::render_codec_diagnostic(error),
    }
}

/// Builds the `$NAME` value for the adopted `--rawfile NAME FILE`: `FILE`'s raw bytes as one string. Invalid UTF-8
/// becomes one U+FFFD per invalid byte (the adopted lossy law byte for byte); a NUL stays a NUL.
pub(crate) fn rawfile_value(
    name: &str,
    file: &str,
    bytes: &[u8],
    _resources: &jqf_resource::ResourceContext<'_>,
) -> Result<jqf_data::Value, CliFailure> {
    let text = String::from_utf8_lossy(bytes);
    jqf_data::Value::try_string(&text).map_err(|_| CliFailure::Message {
        class: ExitClass::Usage,
        message: format!("Bad JSON in --rawfile {name} {file}: cannot allocate"),
    })
}

/// Builds the `$__schema` value for `--schema FILE` : the file's bytes, already read with the binding files, parsed as
/// exactly ONE strict JSON document — the value-schema document itself. The file is always JSON, whatever
/// `--input-format` the DATA uses, so the parse is the `--diff` precedent's: one value, any parse failure reported with
/// the file's path as the frame, exit 2 (usage).
pub(crate) fn parse_schema_document(
    path: &str,
    bytes: &[u8],
    resources: &jqf_resource::ResourceContext<'_>,
) -> Result<jqf_data::Value, CliFailure> {
    let text = String::from_utf8_lossy(bytes);
    jqf_engine::decode_json(&text, resources).map_err(|error| {
        let message = match &error {
            jqf_engine::EngineRunError::Raised(jqf_data::Value::String(message)) => message.as_str().to_owned(),
            _ => {
                return CliFailure::Message {
                    class: ExitClass::Usage,
                    message: format!("{path}: cannot allocate"),
                };
            }
        };
        CliFailure::Message {
            class: ExitClass::Usage,
            message: format!("{path}: {message}"),
        }
    })
}

/// Renders the adopted `-R` input: each LINE of `bytes` becomes one JSON string, or the whole input becomes one string
/// when `slurp`. Lines split on `\n` only (a `\r` stays in the line, as jq keeps it); the final line needs no newline;
/// a trailing newline does not produce a final empty line; an empty input produces no values at all. Invalid UTF-8 is
/// converted lossily (one U+FFFD per invalid byte), exactly as jq renders `-R` output.
pub(crate) fn raw_input_bytes(bytes: &[u8], slurp: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if slurp {
        let text = String::from_utf8_lossy(bytes);
        out.push(b'"');
        jqf_codec_json::push_json_escaped(&mut out, text.as_bytes());
        out.push(b'"');
        return out;
    }
    if bytes.is_empty() {
        return out;
    }
    // the adopted `-R` line law byte for byte: the input splits on `\n`; when it ENDS with a newline, exactly ONE
    // trailing empty segment is dropped (`"a\n"` is one line, `"\n"` is one empty line, `"a\n\n"` is two lines, `""` is
    // no lines at all); interior empty lines stay empty strings; a `\r` before the newline stays in the line.
    let mut segments: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    if bytes.last() == Some(&b'\n') {
        segments.pop();
    }
    for segment in segments {
        push_json_string(&mut out, segment);
    }
    out
}

/// Appends one JSON string literal (quotes and escapes) for `bytes`, which may be invalid UTF-8: the lossy conversion
/// replaces each invalid byte with U+FFFD before the escaped bytes are written. The escape law is the JSON JSON codec's
/// `jqf_codec_json:push_json_escaped`.
pub(crate) fn push_json_string(out: &mut Vec<u8>, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    out.push(b'"');
    jqf_codec_json::push_json_escaped(out, text.as_bytes());
    out.push(b'"');
    out.push(b'\n');
}

/// Reads one input file whole. No ceiling: the retained input is what every route borrows, and `--max-memory-bytes` is
/// the operator's opt-in bound.
pub(crate) fn read_input_file(path: impl AsRef<Path>) -> Result<Vec<u8>, CliFailure> {
    let path = path.as_ref();
    fs::read(path).map_err(|error| CliFailure::Message {
        // the adopted wording and no `(os error N)` suffix, for the same reason the multi-file path spells it this way
        // (item 6).
        class: ExitClass::Usage,
        message: format!(
            "error: Could not open file {}: {}",
            path.display(),
            io_error_text(&error)
        ),
    })
}

/// The source bytes after the last document: the facade's trailing whitespace.
///
/// JSON values never end in whitespace, so the last non-separator byte is the last byte of the final document and
/// everything after it is the original tail an in-place edit must preserve.
pub(crate) fn trailing_bytes(input: &[u8]) -> &[u8] {
    match input
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
    {
        Some(position) => &input[position + 1..],
        None => input,
    }
}

#[cfg(test)]
mod tests {
    use super::read_stdin_from;
    use std::io::Cursor;

    #[test]
    fn piped_stdin_returns_an_exactly_sized_buffer() {
        // §0b: the no-hint (pipe/FIFO) arm must not leave growth headroom on the retained buffer — a growing Vec
        // strands freed blocks in the allocator for the whole request. The payload crosses several 64 KiB chunk
        // boundaries and ends on a partial chunk, so both the multi-chunk accumulation and the consolidation are
        // exercised.
        let payload: Vec<u8> = (0..=255_u8).cycle().take(3 * 64 * 1024 + 17).collect();
        let read = read_stdin_from(&mut Cursor::new(payload.clone()), None)
            .unwrap_or_else(|failure| panic!("pipe read succeeds: {failure}"));
        assert_eq!(read.len(), payload.len());
        assert_eq!(
            read.capacity(),
            read.len(),
            "the pipe read must return an exactly sized buffer"
        );
        assert_eq!(read, payload);
    }

    #[test]
    fn empty_piped_stdin_is_an_empty_exactly_sized_buffer() {
        let read = read_stdin_from(&mut Cursor::new(Vec::new()), None)
            .unwrap_or_else(|failure| panic!("empty pipe read succeeds: {failure}"));
        assert!(read.is_empty());
        assert_eq!(read.capacity(), 0);
    }

    #[test]
    fn single_chunk_piped_stdin_is_exact_and_faithful() {
        let payload = b"hello pipe".to_vec();
        let read = read_stdin_from(&mut Cursor::new(payload.clone()), None)
            .unwrap_or_else(|failure| panic!("pipe read succeeds: {failure}"));
        assert_eq!(read, payload);
        assert_eq!(read.capacity(), read.len());
    }

    #[test]
    fn the_known_length_hint_reads_at_most_the_hint() {
        // The hint arm is unchanged by the pipe fix; this pins its take bound (a file that grows between stat and read
        // is cut off at the hinted length).
        let payload = b"hello world".to_vec();
        let read = read_stdin_from(&mut Cursor::new(payload), Some(5))
            .unwrap_or_else(|failure| panic!("hinted read succeeds: {failure}"));
        assert_eq!(read, b"hello");
    }
}
