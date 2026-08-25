//! The `--stream` event drive family: the input served as `[path, leaf]` events.

use super::{
    Array, Box, Cell, CodecCatalog, CodecInputOutcome, CompiledProgram, DialectId, EncodeRequest, EngineResult,
    EngineRunError, EventParser, FacadeFraming, FormatId, ItemSink, PipelineError, PipelineFailure, PipelinePolicy,
    Publication, PublicationStatus, RaisedError, Rc, RefCell, ResourceContext, ReusableEncoderSession, SequenceError,
    SequenceValueError, StreamEvent, String, Value, ValueOutcome, Vec, note_single_document_output, overflow,
    run_one_owned_value, validate_credits, vec,
};

/// The parser's one failure channel: the machine allocation class.
pub(crate) fn parser_machine_error(error: EngineRunError) -> jqf_codec_core::CodecError {
    match error {
        EngineRunError::Codec(codec) => codec,
        _ => jqf_codec_core::CodecError::new(jqf_codec_core::CodecFailureKind::AllocationFailure),
    }
}

/// the reference's shared input cursor over `--stream` events : `input`/
/// `inputs` pull the next `[path, leaf]` event from the parser, exactly as
/// the reference's single `jq_util_input_next_input` serves both the driver's per-event
/// runs and the program's own pulls from ONE parser. A parse refusal is a
/// catch-eligible raise at the pull site (the reference's law: `try input catch .`
/// over a malformed stream answers the catch); the line number is captured
/// at pull time, which is the line the event was scanned on — the reference's
/// `input_line_number` value.
///
/// The cursor OWNS its bytes: the host-extension seam stores `Box<dyn Any>`
/// (a `'static` box), so a borrowing cursor cannot live there. Only an
/// input-family program pays the copy — a program that never pulls never
/// attaches a cursor, and the peak-RSS `--stream` lane (identity) never
/// reaches here.
pub(crate) struct EventInputCursor {
    parser: RefCell<EventParser<'static>>,
    /// The source label `input_filename` reports.
    filename: Option<String>,
    /// The current event's scan line, captured at pull time.
    current_line: Cell<u64>,
    /// `-n -s --stream` (the reference's slurp-on-pull law): the FIRST pull drains the
    /// parser into one array and hands it over; later pulls are empty.
    slurp_once: Cell<bool>,
}

impl EventInputCursor {
    fn new(input: &[u8], stream_errors: bool, filename: &str, slurp_once: bool) -> Self {
        // set_buf copies the bytes into the parser's owned buffer, which is
        // what makes the cursor `'static` (see the struct doc).
        let mut parser = EventParser::incremental(stream_errors);
        parser.set_buf(input, true);
        Self {
            parser: RefCell::new(parser),
            filename: Some(String::from(filename)),
            current_line: Cell::new(0),
            slurp_once: Cell::new(slurp_once),
        }
    }

    /// The current event's line, for the drive's per-event error reports.
    fn event_line(&self) -> u64 {
        self.current_line.get()
    }

    fn pull(
        &self,
        resources: &jqf_resource::ResourceContext<'_>,
    ) -> Result<Option<Value>, jqf_engine::InputSourceError> {
        if self.slurp_once.get() {
            // the reference's `-n -s --stream`: the first pull drains the parser into
            // ONE array (error events included under `--stream-errors`) and
            // hands it over; the cursor is parked afterwards.
            self.slurp_once.set(false);
            let mut collected = Array::try_new().map_err(|_| jqf_engine::InputSourceError::Allocation)?;
            loop {
                match self.parser.borrow_mut().next_event(resources) {
                    Ok(StreamEvent::Event(value)) => collected
                        .try_push(value)
                        .map_err(|_| jqf_engine::InputSourceError::Allocation)?,
                    Ok(StreamEvent::Done | StreamEvent::NeedMore) => break,
                    Ok(StreamEvent::Refused(message)) => {
                        return Err(jqf_engine::InputSourceError::Refused(message));
                    }
                    Err(_) => return Err(jqf_engine::InputSourceError::Allocation),
                }
            }
            self.current_line.set(0);
            return Ok(Some(Value::Array(collected)));
        }
        let event = self.parser.borrow_mut().next_event(resources);
        match event {
            Ok(StreamEvent::Event(value)) => {
                self.current_line.set(u64::from(self.parser.borrow().line()));
                Ok(Some(value))
            }
            Ok(StreamEvent::Done) => Ok(None),
            // The cursor owns the WHOLE input, so a need for more bytes is a
            // broken assumption, never a clean end: refusing keeps a parser
            // drift from silently truncating the event stream.
            Ok(StreamEvent::NeedMore) => Err(jqf_engine::InputSourceError::Refused(String::from(
                "the stream-event parser ran out of input mid-event",
            ))),
            Ok(StreamEvent::Refused(message)) => Err(jqf_engine::InputSourceError::Refused(message)),
            Err(_) => Err(jqf_engine::InputSourceError::Allocation),
        }
    }
}

impl jqf_engine::InputSource for EventInputCursor {
    fn next(
        &mut self,
        resources: &mut jqf_resource::ResourceContext<'_>,
    ) -> Result<Option<Value>, jqf_engine::InputSourceError> {
        self.pull(resources)
    }

    fn current_filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    fn current_line(&self) -> u64 {
        self.current_line.get()
    }

    fn mark_current(&self) {
        // The line was captured at pull time; nothing to mark.
    }
}

/// The `'static` wrapper the host extension stores: the extension is a
/// `Box<dyn Any>`, and the engine recovers it through the trait object, so
/// the drive holds its own `Rc<EventInputCursor>` while the engine reaches
/// the same cursor through this wrapper.
pub(crate) struct SharedEventCursor(Rc<EventInputCursor>);

impl jqf_engine::InputSource for SharedEventCursor {
    fn next(
        &mut self,
        resources: &mut jqf_resource::ResourceContext<'_>,
    ) -> Result<Option<Value>, jqf_engine::InputSourceError> {
        self.0.pull(resources)
    }

    fn current_filename(&self) -> Option<&str> {
        self.0.current_filename()
    }

    fn current_line(&self) -> u64 {
        self.0.current_line()
    }

    fn mark_current(&self) {
        self.0.mark_current();
    }
}

/// Executes the bounded `--stream` event form : the engine's
/// [`EventParser`] walks the retained JSON text one jq `[path, leaf]` event at
/// a time and the program runs once per event (or once over the collected
/// array under `--slurp`), publishing through the ordinary encoder.
///
/// This is the BOUNDED realization of the `tostream | P` rewrite: the parsed
/// document never exists, so a document larger than the peak-RSS lane's
/// ceiling streams with only the path stack resident. Parse refusals are
/// terminal ([`EventStreamError::ParseRefused`], prior events stand) under
/// `--stream`; under `--stream-errors` the parser turns them into
/// `[message, path]` events like jq's.
///
/// A program that reads the input family (`input`/`inputs`/`input_filename`/
/// `input_line_number`) is served from an `EventInputCursor` sharing the
/// parser with the drive — the reference's single-puller law, where the events ARE the
/// shared input sequence . The `-s` form parks no cursor (the reference's
/// post-slurp `break` law), and a program that never pulls never drives the
/// parser, so `-n --stream '.'` answers `null` without parsing.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the same boundary inventory the other drives thread; the event drive is one linear \
              orchestration whose per-event tail mirrors the sequence drive's"
)]
pub(crate) fn execute_stream_events<Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    input: &[u8],
    stream_errors: bool,
    slurp: bool,
    program: &CompiledProgram,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    label: &str,
) -> Result<EventStreamReport, EventStreamError<Sink::Error>> {
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication).map_err(EventStreamError::Pipeline)?;
    note_single_document_output(catalog, output_format, output_dialect, &mut publication, false)
        .map_err(EventStreamError::Pipeline)?;

    let encoder = catalog
        .encoder(output_format, output_dialect)
        .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Registry(error))))?;
    let encoding_policy = policy.encoding();
    let factory = encoder
        .create_factory(
            EncodeRequest {
                format: output_format,
                dialect: output_dialect,
                diagnostics: encoding_policy.diagnostics,
                preservation: encoding_policy.preservation,
                options: encoding_policy.options,
            },
            resources,
        )
        .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(error))))?;
    let mut reused_encoder = ReusableEncoderSession::new();
    let mut parser = EventParser::new(input, stream_errors);
    // The parser's only failure channel is the machine allocation class.
    let parser_error = |publication: &mut Publication, error: EngineRunError| {
        EventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(parser_machine_error(error))))
    };
    if slurp {
        // `-s --stream`: every event (error events included under
        // `--stream-errors`) is collected into one array, then the program
        // runs once over it — the reference's slurped stream, byte-identical to the
        // rewrite `[.[] | tostream] | P` for the shared shapes.
        let mut collected = Array::try_new().map_err(|_| {
            EventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(jqf_codec_core::CodecError::new(
                jqf_codec_core::CodecFailureKind::AllocationFailure,
            ))))
        })?;
        loop {
            match parser
                .next_event(resources)
                .map_err(|e| parser_error(&mut publication, e))?
            {
                StreamEvent::Event(value) => {
                    collected.try_push(value).map_err(|_| {
                        EventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(
                            jqf_codec_core::CodecError::new(jqf_codec_core::CodecFailureKind::AllocationFailure),
                        )))
                    })?;
                }
                StreamEvent::Done => break,
                StreamEvent::Refused(message) => {
                    return Err(EventStreamError::ParseRefused(message));
                }
                // The whole-input shape feeds one final buffer; NeedMore is
                // the incremental drive's signal and never occurs here.
                StreamEvent::NeedMore => {
                    unreachable!("the whole-input event parser never needs more input")
                }
            }
        }
        let (outcome, items) = run_one_owned_value(
            program,
            CodecInputOutcome::Result(EngineResult::owned(Value::Array(collected))),
            &factory,
            &mut reused_encoder,
            0,
            policy.max_iterations,
            encoding_policy,
            framing,
            resources,
            sink,
            &mut publication,
        )
        .map_err(EventStreamError::Pipeline)?;
        match outcome {
            // The slurped drive runs once: a trailing failure ends the request
            // exactly as the -s drive's does (no next event exists).
            Some(ValueOutcome::Mismatch(error)) => {
                let mismatch = error.mismatch;
                sink.report_value_error(error.into_sequence_error(0, 0, None))
                    .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error))))?;
                return Err(EventStreamError::Pipeline(publication.fail(mismatch.into_failure())));
            }
            Some(ValueOutcome::Codec(error)) => {
                sink.report_value_error(SequenceValueError::try_for_codec(0, 0, None, &error))
                    .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error))))?;
                return Err(EventStreamError::Pipeline(
                    publication.fail(PipelineFailure::Codec(error)),
                ));
            }
            Some(ValueOutcome::Raised(value)) => {
                let reported = value.clone();
                let report = SequenceValueError::try_for_raised(0, 0, None, reported)
                    .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(error))))?;
                sink.report_value_error(report)
                    .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error))))?;
                return Err(EventStreamError::Pipeline(
                    publication.fail(PipelineFailure::Raised(RaisedError { value })),
                ));
            }
            Some(ValueOutcome::SplitName { index, detail }) => {
                return Err(EventStreamError::Pipeline(
                    publication.fail(PipelineFailure::SplitName { index, detail }),
                ));
            }
            None => {}
        }
        return Ok(EventStreamReport {
            publication: publication.status(),
            items,
        });
    }
    // The per-event drive: each event runs the program once, and a per-event
    // runtime error is reported and the loop continues, exactly as the
    // adjacent-value sequence treats a per-value error (the reference's law; the exit
    // class is the LAST event's class). A parse refusal is terminal with
    // every earlier event's bytes already published.
    //
    // An input-family program shares the parser with the drive through the
    // event cursor (the reference's single-puller law); without one, the drive pulls
    // directly. Attaching the cursor makes `input`/`inputs` pull FURTHER
    // events from the same parser, so `jq --stream 'input'` over `1 2`
    // answers `[[],2]` (the driver consumed the first event).
    let mut items = 0u64;
    let mut event_index = 0u64;
    let mut last_error: Option<SequenceError> = None;
    let cursor = if program.uses_input_family() {
        // The cursor copies the retained bytes (the `'static` extension
        // seam); an input-family program materializes the input either way.
        let shared = Rc::new(EventInputCursor::new(input, stream_errors, label, false));
        resources.set_host_extension(Box::new(jqf_engine::InputSourceHandle::new(Box::new(
            SharedEventCursor(Rc::clone(&shared)),
        ))));
        Some(shared)
    } else {
        None
    };
    loop {
        let event = match &cursor {
            Some(cursor) => match cursor.pull(resources) {
                Ok(Some(value)) => StreamEvent::Event(value),
                Ok(None) => StreamEvent::Done,
                Err(jqf_engine::InputSourceError::Refused(message)) => {
                    return Err(EventStreamError::ParseRefused(message));
                }
                Err(jqf_engine::InputSourceError::Allocation) => {
                    return Err(EventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(
                        jqf_codec_core::CodecError::new(jqf_codec_core::CodecFailureKind::AllocationFailure),
                    ))));
                }
            },
            None => parser
                .next_event(resources)
                .map_err(|e| parser_error(&mut publication, e))?,
        };
        let input_line = match &cursor {
            Some(cursor) => cursor.event_line(),
            None => u64::from(parser.line()),
        };
        match event {
            StreamEvent::Event(value) => {
                let (outcome, advanced) = run_one_owned_value(
                    program,
                    CodecInputOutcome::Result(EngineResult::owned(value)),
                    &factory,
                    &mut reused_encoder,
                    items,
                    policy.max_iterations,
                    encoding_policy,
                    framing,
                    resources,
                    sink,
                    &mut publication,
                )
                .map_err(EventStreamError::Pipeline)?;
                items = advanced;
                match outcome {
                    Some(ValueOutcome::Mismatch(error)) => {
                        let mismatch = error.mismatch;
                        sink.report_value_error(error.into_sequence_error(event_index, input_line, None))
                            .map_err(|error| {
                                EventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error)))
                            })?;
                        last_error = Some(SequenceError::Mismatch(mismatch));
                    }
                    Some(ValueOutcome::Codec(error)) => {
                        sink.report_value_error(SequenceValueError::try_for_codec(
                            event_index,
                            input_line,
                            None,
                            &error,
                        ))
                        .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error))))?;
                        last_error = Some(SequenceError::Codec(error));
                    }
                    Some(ValueOutcome::Raised(value)) => {
                        let reported = value.clone();
                        let report = SequenceValueError::try_for_raised(event_index, input_line, None, reported)
                            .map_err(|error| {
                                EventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(error)))
                            })?;
                        sink.report_value_error(report).map_err(|error| {
                            EventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error)))
                        })?;
                        last_error = Some(SequenceError::Raised(value));
                    }
                    Some(ValueOutcome::SplitName { index, detail }) => {
                        last_error = Some(SequenceError::SplitName { index, detail });
                    }
                    None => last_error = None,
                }
                event_index = event_index
                    .checked_add(1)
                    .ok_or_else(|| EventStreamError::Pipeline(overflow::<Sink::Error>(&publication)))?;
            }
            StreamEvent::Done => break,
            StreamEvent::Refused(message) => {
                return Err(EventStreamError::ParseRefused(message));
            }
            // The whole-input shape feeds one final buffer; NeedMore is the
            // incremental drive's signal and never occurs here.
            StreamEvent::NeedMore => {
                unreachable!("the whole-input event parser never needs more input")
            }
        }
    }
    match last_error {
        Some(SequenceError::Mismatch(mismatch)) => {
            Err(EventStreamError::Pipeline(publication.fail(mismatch.into_failure())))
        }
        Some(SequenceError::Raised(value)) => Err(EventStreamError::Pipeline(
            publication.fail(PipelineFailure::Raised(RaisedError { value })),
        )),
        Some(SequenceError::Codec(error)) => Err(EventStreamError::Pipeline(
            publication.fail(PipelineFailure::Codec(error)),
        )),
        Some(SequenceError::SplitName { index, detail }) => Err(EventStreamError::Pipeline(
            publication.fail(PipelineFailure::SplitName { index, detail }),
        )),
        None => Ok(EventStreamReport {
            publication: publication.status(),
            items,
        }),
    }
}

/// Successful publication receipt for the bounded `--stream` event drive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventStreamReport {
    publication: PublicationStatus,
    items: u64,
}

impl EventStreamReport {
    /// Final publication state across every item and its facade framing.
    #[must_use]
    pub const fn publication(self) -> PublicationStatus {
        self.publication
    }
    /// Number of ordered items published across every event, including zero.
    #[must_use]
    pub const fn items(self) -> u64 {
        self.items
    }
}

/// The bounded `--stream` drive's failure, at the boundary that raised it.
#[derive(Debug)]
pub(crate) enum EventStreamError<SinkError> {
    /// The pipeline's own failure (runtime class, raised value, halt, sink).
    Pipeline(PipelineError<SinkError>),
    /// A `--stream` parse refusal: the reference's message text. Every earlier event's
    /// bytes stand; the request fails with the parse class (exit 5).
    ParseRefused(String),
}

/// Executes the `-n --stream` event form : the reference's canonical streaming
/// idiom `fromstream(inputs)` — the program runs ONCE over `null`, and the
/// input family pulls `[path, leaf]` events from the shared [`EventParser`]
/// (through an `EventInputCursor`), exactly as the reference's `-n` runs the filter once
/// while `jq_util_input_next_input` streams the parser on demand.
///
/// the reference's precedence laws: a program that never pulls the
/// input leaves the parser entirely untouched (`-n --stream '.'` answers
/// `null` over any bytes, valid or not); a pull-time parse refusal is a
/// CATCH-ELIGIBLE raise (`try input catch .` answers the catch); and under
/// `-n -s --stream` the FIRST pull drains the parser into ONE array and hands
/// it over (`[inputs]` answers `[ [events] ]`).
#[allow(
    clippy::too_many_arguments,
    reason = "the same boundary inventory execute_stream_events threads; the null-first event               drive is one run over null read as a single obligation"
)]
pub(crate) fn execute_stream_events_null_first<Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    input: &[u8],
    stream_errors: bool,
    slurp: bool,
    program: &CompiledProgram,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    label: &str,
) -> Result<EventStreamReport, EventStreamError<Sink::Error>> {
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication).map_err(EventStreamError::Pipeline)?;
    note_single_document_output(catalog, output_format, output_dialect, &mut publication, false)
        .map_err(EventStreamError::Pipeline)?;

    let encoder = catalog
        .encoder(output_format, output_dialect)
        .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Registry(error))))?;
    let encoding_policy = policy.encoding();
    let factory = encoder
        .create_factory(
            EncodeRequest {
                format: output_format,
                dialect: output_dialect,
                diagnostics: encoding_policy.diagnostics,
                preservation: encoding_policy.preservation,
                options: encoding_policy.options,
            },
            resources,
        )
        .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(error))))?;
    let mut reused_encoder = ReusableEncoderSession::new();
    // The cursor owns the parser; a program that never pulls never drives it,
    // so `-n --stream '.'` answers `null` without parsing — the reference's law.
    let shared = Rc::new(EventInputCursor::new(input, stream_errors, label, slurp));
    resources.set_host_extension(Box::new(jqf_engine::InputSourceHandle::new(Box::new(
        SharedEventCursor(Rc::clone(&shared)),
    ))));
    let (outcome, items) = run_one_owned_value(
        program,
        CodecInputOutcome::Result(EngineResult::owned(Value::Null)),
        &factory,
        &mut reused_encoder,
        0,
        policy.max_iterations,
        encoding_policy,
        framing,
        resources,
        sink,
        &mut publication,
    )
    .map_err(EventStreamError::Pipeline)?;
    match outcome {
        // The null-first drive runs once: a trailing failure ends the request
        // exactly as the -n drive's does (no next event exists).
        Some(ValueOutcome::Mismatch(error)) => {
            let mismatch = error.mismatch;
            sink.report_value_error(error.into_sequence_error(0, 0, None))
                .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error))))?;
            return Err(EventStreamError::Pipeline(publication.fail(mismatch.into_failure())));
        }
        Some(ValueOutcome::Codec(error)) => {
            sink.report_value_error(SequenceValueError::try_for_codec(0, 0, None, &error))
                .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error))))?;
            return Err(EventStreamError::Pipeline(
                publication.fail(PipelineFailure::Codec(error)),
            ));
        }
        Some(ValueOutcome::Raised(value)) => {
            let reported = value.clone();
            let report = SequenceValueError::try_for_raised(0, 0, None, reported)
                .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(error))))?;
            sink.report_value_error(report)
                .map_err(|error| EventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error))))?;
            return Err(EventStreamError::Pipeline(
                publication.fail(PipelineFailure::Raised(RaisedError { value })),
            ));
        }
        Some(ValueOutcome::SplitName { index, detail }) => {
            return Err(EventStreamError::Pipeline(
                publication.fail(PipelineFailure::SplitName { index, detail }),
            ));
        }
        None => {}
    }
    Ok(EventStreamReport {
        publication: publication.status(),
        items,
    })
}

/// The streaming-stdin `--stream` drive : the incremental
/// [`EventParser`] is fed the reference's fgets-shaped chunks as they arrive from the
/// read callback, so a JSON input larger than memory streams with only the
/// path stack and the current chunk resident — the promise `--help` makes,
/// which the whole-read route cannot keep on a pipe. The chunk shape is the reference's
/// own `jq_util_input_read_more` (`fgets` at most 4091 bytes ending at a
/// newline, extended to complete a trailing multi-byte character), so error
/// recovery and line/column accounting are byte-identical to the whole-input
/// run over the same bytes.
///
/// Parse refusals are terminal ([`StreamingEventStreamError::ParseRefused`],
/// prior events stand). The input family is NOT served here: an input-family
/// program's shared cursor is whole-input by construction, so the CLI routes
/// those requests to the whole-read event drive (the narrowing,
/// applied to the event route).
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the streaming event drive carries the same explicit ownership boundaries as \
              the whole-read sibling it parallels, and its chunk-feed loop is one linear \
              sequential law"
)]
pub(crate) fn execute_stream_events_streaming<Sink, ReadError, ReadFn>(
    catalog: CodecCatalog<'_, '_>,
    stream_errors: bool,
    program: &CompiledProgram,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    mut read: ReadFn,
) -> Result<EventStreamReport, StreamingEventStreamError<Sink::Error, ReadError>>
where
    Sink: ItemSink,
    ReadFn: FnMut(&mut [u8]) -> Result<usize, ReadError>,
{
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication).map_err(StreamingEventStreamError::Pipeline)?;
    note_single_document_output(catalog, output_format, output_dialect, &mut publication, false)
        .map_err(StreamingEventStreamError::Pipeline)?;

    let encoder = catalog
        .encoder(output_format, output_dialect)
        .map_err(|error| StreamingEventStreamError::Pipeline(publication.fail(PipelineFailure::Registry(error))))?;
    let encoding_policy = policy.encoding();
    let factory = encoder
        .create_factory(
            EncodeRequest {
                format: output_format,
                dialect: output_dialect,
                diagnostics: encoding_policy.diagnostics,
                preservation: encoding_policy.preservation,
                options: encoding_policy.options,
            },
            resources,
        )
        .map_err(|error| StreamingEventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(error))))?;
    let mut reused_encoder = ReusableEncoderSession::new();
    let mut parser = EventParser::incremental(stream_errors);
    let parser_error = |publication: &mut Publication, error: EngineRunError| {
        StreamingEventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(parser_machine_error(error))))
    };
    let mut items = 0u64;
    let mut event_index = 0u64;
    let mut last_error: Option<SequenceError> = None;
    let mut carry: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; CHUNK_MAX];
    let mut fed_final = false;
    loop {
        let event = match parser.next_event(resources) {
            Ok(event) => event,
            Err(error) => return Err(parser_error(&mut publication, error)),
        };
        match event {
            StreamEvent::Event(value) => {
                let input_line = u64::from(parser.line());
                let (outcome, advanced) = run_one_owned_value(
                    program,
                    CodecInputOutcome::Result(EngineResult::owned(value)),
                    &factory,
                    &mut reused_encoder,
                    items,
                    policy.max_iterations,
                    encoding_policy,
                    framing,
                    resources,
                    sink,
                    &mut publication,
                )
                .map_err(StreamingEventStreamError::Pipeline)?;
                items = advanced;
                match outcome {
                    Some(ValueOutcome::Mismatch(error)) => {
                        let mismatch = error.mismatch;
                        sink.report_value_error(error.into_sequence_error(event_index, input_line, None))
                            .map_err(|error| {
                                StreamingEventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error)))
                            })?;
                        last_error = Some(SequenceError::Mismatch(mismatch));
                    }
                    Some(ValueOutcome::Codec(error)) => {
                        sink.report_value_error(SequenceValueError::try_for_codec(
                            event_index,
                            input_line,
                            None,
                            &error,
                        ))
                        .map_err(|error| {
                            StreamingEventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error)))
                        })?;
                        last_error = Some(SequenceError::Codec(error));
                    }
                    Some(ValueOutcome::Raised(value)) => {
                        let reported = value.clone();
                        let report = SequenceValueError::try_for_raised(event_index, input_line, None, reported)
                            .map_err(|error| {
                                StreamingEventStreamError::Pipeline(publication.fail(PipelineFailure::Codec(error)))
                            })?;
                        sink.report_value_error(report).map_err(|error| {
                            StreamingEventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error)))
                        })?;
                        last_error = Some(SequenceError::Raised(value));
                    }
                    Some(ValueOutcome::SplitName { index, detail }) => {
                        last_error = Some(SequenceError::SplitName { index, detail });
                    }
                    None => last_error = None,
                }
                event_index = event_index
                    .checked_add(1)
                    .ok_or_else(|| StreamingEventStreamError::Pipeline(overflow::<Sink::Error>(&publication)))?;
            }
            StreamEvent::Done => break,
            StreamEvent::Refused(message) => {
                return Err(StreamingEventStreamError::ParseRefused(message));
            }
            StreamEvent::NeedMore => {
                if fed_final {
                    // The final buffer always runs the reference's EOF handling; a
                    // NeedMore after it is an internal contract violation.
                    return Err(StreamingEventStreamError::Pipeline(publication.fail(
                        PipelineFailure::Codec(jqf_codec_core::CodecError::new(
                            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                                contract: "incremental event parser needs more after the final buffer",
                            },
                        )),
                    )));
                }
                // The next read blocks: make the already-published events
                // visible before the drive waits for bytes that may never
                // come (the sink's `flush` default is a no-op; the CLI
                // overrides it) — the flush-cadence law, on the event
                // drive.
                sink.flush().map_err(|error| {
                    StreamingEventStreamError::Pipeline(publication.fail(PipelineFailure::Sink(error)))
                })?;
                let chunk =
                    read_fgets_chunk(&mut read, &mut carry, &mut scratch).map_err(StreamingEventStreamError::Read)?;
                if let Some(chunk) = chunk {
                    parser.set_buf(&chunk, false);
                } else {
                    // EOF: the final (empty) buffer runs the reference's EOF handling.
                    fed_final = true;
                    parser.set_buf(&[], true);
                }
            }
        }
    }
    match last_error {
        Some(SequenceError::Mismatch(mismatch)) => Err(StreamingEventStreamError::Pipeline(
            publication.fail(mismatch.into_failure()),
        )),
        Some(SequenceError::Raised(value)) => Err(StreamingEventStreamError::Pipeline(
            publication.fail(PipelineFailure::Raised(RaisedError { value })),
        )),
        Some(SequenceError::Codec(error)) => Err(StreamingEventStreamError::Pipeline(
            publication.fail(PipelineFailure::Codec(error)),
        )),
        Some(SequenceError::SplitName { index, detail }) => Err(StreamingEventStreamError::Pipeline(
            publication.fail(PipelineFailure::SplitName { index, detail }),
        )),
        None => Ok(EventStreamReport {
            publication: publication.status(),
            items,
        }),
    }
}

/// the reference's fgets chunk ceiling (the engine parser's `CHUNK_MAX`): a 4096-byte
/// buffer minus a 4-byte UTF-8 tail reservation, so a chunk is at most 4091
/// bytes.
pub(crate) const CHUNK_MAX: usize = 4091;

/// The streaming `--stream` drive's failure, at the boundary that raised it.
#[derive(Debug)]
pub enum StreamingEventStreamError<SinkError, ReadError> {
    /// The pipeline's own failure (runtime class, raised value, halt, sink).
    Pipeline(PipelineError<SinkError>),
    /// The read callback failed.
    Read(ReadError),
    /// A `--stream` parse refusal: the reference's message text. Every earlier event's
    /// bytes stand; the request fails with the parse class (exit 5).
    ParseRefused(String),
}

/// the reference's `jq_util_input_read_more` chunk shape: at most 4091 bytes ending at
/// the first newline; a newline-free 4091-byte run is extended to complete a
/// trailing multi-byte UTF-8 character (the reference's `jvp_utf8_backtrack` + `fread`).
/// Bytes read past a newline stay in `carry` for the next chunk, exactly as
/// they remain in the pipe for the reference's next `fgets`. `None` means EOF with
/// nothing held.
pub(crate) fn read_fgets_chunk<ReadError, ReadFn>(
    read: &mut ReadFn,
    carry: &mut Vec<u8>,
    scratch: &mut [u8],
) -> Result<Option<Vec<u8>>, ReadError>
where
    ReadFn: FnMut(&mut [u8]) -> Result<usize, ReadError>,
{
    // A complete line already held: serve it before any further read.
    if let Some(pos) = carry.iter().position(|byte| *byte == b'\n') {
        let chunk = carry[..=pos].to_vec();
        carry.drain(..=pos);
        return Ok(Some(chunk));
    }
    // Top the carry up to 4091 bytes, stopping at a newline or EOF.
    while carry.len() < CHUNK_MAX {
        let want = CHUNK_MAX - carry.len();
        let n = read(&mut scratch[..want])?;
        if n == 0 {
            break;
        }
        carry.extend_from_slice(&scratch[..n]);
        if carry.contains(&b'\n') {
            break;
        }
    }
    if carry.is_empty() {
        return Ok(None);
    }
    let newline = carry.iter().position(|byte| *byte == b'\n');
    let mut end = match newline {
        Some(pos) => pos + 1,
        None => carry.len(),
    };
    // A newline-free run cut at 4091 bytes must not split a trailing
    // multi-byte character. `from_utf8` names the incomplete tail (if any);
    // an invalid lead such as a lone 0x80 is not a partial character and
    // must not backtrack — subtracting a coding length smaller than the
    // walked span would wrap and panic on the next slice.
    if newline.is_none()
        && end >= CHUNK_MAX
        && let Err(err) = core::str::from_utf8(&carry[..end])
        && err.error_len().is_none()
    {
        let lead = err.valid_up_to();
        if let Some(length) = utf8_coding_length(carry[lead]) {
            let seen = end - lead;
            if seen < usize::from(length) {
                let missing = usize::from(length) - seen;
                let mut tail = [0u8; 3];
                let n = read(&mut tail[..missing])?;
                carry.extend_from_slice(&tail[..n]);
                end = carry.len();
            }
        }
    }
    let chunk = carry[..end].to_vec();
    carry.drain(..end);
    Ok(Some(chunk))
}

/// The lead byte's UTF-8 sequence length: how many more bytes complete an
/// incomplete tail named by `from_utf8`.
pub(crate) fn utf8_coding_length(byte: u8) -> Option<u8> {
    match byte {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{CHUNK_MAX, read_fgets_chunk};

    fn feed(mut remaining: Vec<u8>) -> impl FnMut(&mut [u8]) -> Result<usize, &'static str> {
        move |buf| {
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            remaining.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn malformed_utf8_at_chunk_boundary_does_not_panic() {
        // 4090 ASCII + a lone continuation + the rest of the line. Pre-fix
        // this wrapped the coding-length subtraction and panicked on the
        // next slice.
        let mut input = vec![b'a'; 4090];
        input.push(0x80);
        input.extend_from_slice(b"bcd\n");
        let mut read = feed(input);
        let mut carry = Vec::new();
        let mut scratch = vec![0u8; CHUNK_MAX];
        let first = read_fgets_chunk(&mut read, &mut carry, &mut scratch)
            .expect("read")
            .expect("a chunk");
        assert_eq!(first.len(), CHUNK_MAX);
        assert_eq!(first[4090], 0x80);
        let second = read_fgets_chunk(&mut read, &mut carry, &mut scratch)
            .expect("read")
            .expect("the rest of the line");
        assert_eq!(&second, b"bcd\n");
    }

    #[test]
    fn incomplete_multibyte_at_chunk_boundary_is_completed() {
        let mut input = vec![b'a'; 4090];
        input.push(0xC2);
        input.push(0xA9);
        input.push(b'\n');
        let mut read = feed(input);
        let mut carry = Vec::new();
        let mut scratch = vec![0u8; CHUNK_MAX];
        let first = read_fgets_chunk(&mut read, &mut carry, &mut scratch)
            .expect("read")
            .expect("a chunk");
        assert_eq!(first.len(), 4092);
        assert_eq!(&first[4090..], &[0xC2, 0xA9]);
        let second = read_fgets_chunk(&mut read, &mut carry, &mut scratch)
            .expect("read")
            .expect("the held newline");
        assert_eq!(&second, b"\n");
    }
}
