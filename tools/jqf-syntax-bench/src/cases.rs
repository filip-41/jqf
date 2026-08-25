use std::{
    hash::{Hash, Hasher},
    hint::black_box,
};

use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef, Span};
use jqf_syntax::{
    ExprKind, Lexer, ParsedSyntax, SourceItem, SourceUnit, SyntaxNodeKind, SyntaxNodeRef, SyntaxWalk, TemplateSegment,
    TokenKind, WalkEvent, decode_literal_into, parse_program, parse_query,
};

use crate::fixtures::{
    ESCAPED_LITERAL_BYTES, EscapedLiteral, GENERATED_PROGRAM_BYTES, GENERATED_PROGRAM_DEFINITIONS, GeneratedProgram,
    escaped_literal_256k, feature_rich_query, generated_program_1m, interpolation_heavy_query, large_program,
    mixed_postfix_query, string_heavy_query,
};

const SOURCE: SourceRef = SourceRef::new(SourceId::new(0), SourceKind::Query);
const GENERATED_VISITOR_EVENTS: u64 = 454_112;
const GENERATED_VISITOR_ENTERS: u64 = 227_056;
const GENERATED_VISITOR_EXITS: u64 = 227_056;
const GENERATED_VISITOR_MAXIMUM_DEPTH: usize = 7;
const GENERATED_VISITOR_FINAL_DEPTH: usize = 0;
const GENERATED_VISITOR_NODE_ACCESSORS: u64 = 10_811;
const GENERATED_VISITOR_ATTRIBUTES: u64 = 10_811;
const GENERATED_VISITOR_CHECKSUM: u64 = 0xb5dc_8ca9_d137_95e1;

pub(crate) fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    let feature_rich = feature_rich_query();
    let generated = generated_program_1m();
    let GeneratedProgram {
        source: generated_source,
        definition_count,
    } = generated;
    vec![
        Box::new(ParseCase::new(
            "lexer/feature-rich-query",
            ParseMode::Lexer,
            feature_rich.clone(),
            true,
            None,
        )),
        Box::new(ParseCase::new(
            "parser/short-path",
            ParseMode::Query,
            ".users[9999].email".into(),
            false,
            None,
        )),
        Box::new(ParseCase::new(
            "parser/feature-rich-query",
            ParseMode::Query,
            feature_rich.clone(),
            true,
            None,
        )),
        Box::new(ParseCase::new(
            "parser/string-heavy-query",
            ParseMode::Query,
            string_heavy_query(),
            false,
            None,
        )),
        Box::new(ParseCase::new(
            "parser/interpolation-heavy-query",
            ParseMode::Query,
            interpolation_heavy_query(),
            false,
            None,
        )),
        Box::new(ParseCase::new(
            "parser/mixed-postfix-query",
            ParseMode::Query,
            mixed_postfix_query(),
            true,
            None,
        )),
        Box::new(ParseCase::new(
            "parser/large-program",
            ParseMode::Program,
            large_program(),
            true,
            Some(128),
        )),
        Box::new(ParseCase::new(
            "parser/generated-program-1m",
            ParseMode::Program,
            generated_source.clone(),
            true,
            Some(definition_count),
        )),
        Box::new(VisitorCase::new(generated_source, definition_count)),
        Box::new(StringDecodeCase::new(escaped_literal_256k())),
    ]
}

#[derive(Clone, Copy)]
enum ParseMode {
    Lexer,
    Query,
    Program,
}

struct ParseCase {
    name: &'static str,
    mode: ParseMode,
    source: String,
    requires_accessors: bool,
    expected_definitions: Option<usize>,
}

impl ParseCase {
    const fn new(
        name: &'static str,
        mode: ParseMode,
        source: String,
        requires_accessors: bool,
        expected_definitions: Option<usize>,
    ) -> Self {
        Self {
            name,
            mode,
            source,
            requires_accessors,
            expected_definitions,
        }
    }

    fn check_accessors(&self, node_accessors: u64, attributes: u64) -> Result<(), String> {
        if self.requires_accessors
            && (!(self.source.contains(".@") && self.source.contains(".&")) || node_accessors == 0 || attributes == 0)
        {
            return Err("representative parse lost required typed .@ or .& syntax".into());
        }
        Ok(())
    }

    fn lexer_preflight(&mut self) -> Result<PreflightReceipt, String> {
        let operation_checksum = self.run();
        let mut token_count = 0_u64;
        let mut eof_count = 0_u64;
        let mut error_count = 0_u64;
        let mut node_accessors = 0_u64;
        let mut attributes = 0_u64;
        let mut token_checksum = checksum_seed();
        for token in Lexer::new(&self.source).map_err(|error| error.to_string())? {
            token_checksum = mix(
                token_checksum,
                u64::try_from(token_kind_id(token.kind)).map_err(|_| "token kind overflow")?,
            );
            token_checksum = mix(token_checksum, u64::from(token.span.start()));
            token_checksum = mix(token_checksum, u64::from(token.span.end()));
            if token.kind == TokenKind::Eof {
                eof_count += 1;
            } else {
                token_count += 1;
            }
            error_count += u64::from(token.kind == TokenKind::Error);
            node_accessors += u64::from(token.kind == TokenKind::DotAt);
            attributes += u64::from(token.kind == TokenKind::DotAmp);
        }
        self.check_accessors(node_accessors, attributes)?;
        if operation_checksum != token_count + eof_count || token_count == 0 || eof_count != 1 || error_count != 0 {
            return Err("lexer token count, EOF count, error count, or operation receipt differed".into());
        }
        Ok(PreflightReceipt::new(
            token_checksum,
            format!(
                "root=tokens tokens={token_count} eof={eof_count} errors={error_count} token_checksum=0x{token_checksum:016x} node_accessors={node_accessors} attributes={attributes}"
            ),
        ))
    }

    fn query_preflight(&mut self) -> Result<PreflightReceipt, String> {
        let operation_checksum = self.run();
        let parsed = parse_query(SOURCE, &self.source).map_err(|error| error.to_string())?;
        if !parsed.diagnostics().is_empty() {
            return Err(format!(
                "query produced {} diagnostics: {:?}",
                parsed.diagnostics().len(),
                parsed.diagnostics()
            ));
        }
        let syntax = parsed.syntax().ok_or("query produced no syntax root")?;
        let node = SyntaxNodeRef::query(syntax.root());
        let expected_checksum = node_receipt(node, 0);
        let (node_accessors, attributes) = accessor_counts(SyntaxWalk::query(syntax.root()));
        self.check_accessors(node_accessors, attributes)?;
        if operation_checksum != expected_checksum {
            return Err("query root receipt differed from measured parse".into());
        }
        Ok(PreflightReceipt::new(
            operation_checksum,
            format!(
                "root={:?} diagnostics=0 span={}..{} source_bytes={} node_accessors={node_accessors} attributes={attributes} checksum=0x{operation_checksum:016x}",
                node.kind(),
                node.span().start(),
                node.span().end(),
                self.source.len(),
            ),
        ))
    }

    fn program_preflight(&mut self) -> Result<PreflightReceipt, String> {
        let operation_checksum = self.run();
        let parsed = parse_program(SOURCE, &self.source).map_err(|error| error.to_string())?;
        if !parsed.diagnostics().is_empty() {
            return Err(format!(
                "program produced {} diagnostics: {:?}",
                parsed.diagnostics().len(),
                parsed.diagnostics()
            ));
        }
        let syntax = parsed.syntax().ok_or("program produced no syntax root")?;
        let unit = syntax.root();
        let definitions = definition_count(unit);
        let expected_checksum = program_receipt(unit);
        let (node_accessors, attributes) = accessor_counts(SyntaxWalk::source_unit(unit));
        self.check_accessors(node_accessors, attributes)?;
        if operation_checksum != expected_checksum
            || unit.expression.is_none()
            || self
                .expected_definitions
                .is_some_and(|expected| definitions != expected)
        {
            return Err("program root, final expression, definition count, or receipt differed".into());
        }
        Ok(PreflightReceipt::new(
            operation_checksum,
            format!(
                "root=SourceUnit diagnostics=0 span={}..{} source_bytes={} items={} definitions={definitions} final_expression=true node_accessors={node_accessors} attributes={attributes} checksum=0x{operation_checksum:016x}",
                unit.span.start(),
                unit.span.end(),
                self.source.len(),
                unit.items.len(),
            ),
        ))
    }
}

impl BenchmarkCase for ParseCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(
            self.name,
            1,
            u64::try_from(self.source.len()).expect("syntax source length fits u64"),
        )
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        match self.mode {
            ParseMode::Lexer => self.lexer_preflight(),
            ParseMode::Query => self.query_preflight(),
            ParseMode::Program => self.program_preflight(),
        }
    }

    fn run(&mut self) -> u64 {
        match self.mode {
            ParseMode::Lexer => Lexer::new(black_box(self.source.as_str()))
                .expect("preflight proved lexer source is representable")
                .count() as u64,
            ParseMode::Query => {
                let parsed = parse_query(SOURCE, black_box(self.source.as_str()))
                    .expect("preflight proved query source is representable");
                let syntax = parsed.syntax().expect("preflight proved query has a syntax root");
                let checksum = node_receipt(SyntaxNodeRef::query(syntax.root()), 0);
                drop(parsed);
                checksum
            }
            ParseMode::Program => {
                let parsed = parse_program(SOURCE, black_box(self.source.as_str()))
                    .expect("preflight proved program source is representable");
                let unit = parsed
                    .syntax()
                    .expect("preflight proved program has a syntax root")
                    .root();
                let checksum = program_receipt(unit);
                drop(parsed);
                checksum
            }
        }
    }
}

struct VisitorCase {
    source: String,
    syntax: ParsedSyntax<SourceUnit>,
    definition_count: usize,
}

impl VisitorCase {
    fn new(source: String, definition_count: usize) -> Self {
        let syntax = parse_program(SOURCE, &source)
            .expect("generated traversal source is representable")
            .into_valid_syntax()
            .expect("generated traversal source is valid");
        Self {
            source,
            syntax,
            definition_count,
        }
    }
}

impl BenchmarkCase for VisitorCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(
            "visitor/generated-program-1m",
            GENERATED_VISITOR_EVENTS,
            GENERATED_PROGRAM_BYTES as u64,
        )
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let operation_checksum = self.run();
        let receipt = traversal_receipt(self.syntax.root());
        if operation_checksum != GENERATED_VISITOR_CHECKSUM
            || receipt.checksum != GENERATED_VISITOR_CHECKSUM
            || receipt.enters + receipt.exits != GENERATED_VISITOR_EVENTS
            || receipt.enters != GENERATED_VISITOR_ENTERS
            || receipt.exits != GENERATED_VISITOR_EXITS
            || receipt.maximum_depth != GENERATED_VISITOR_MAXIMUM_DEPTH
            || receipt.final_depth != GENERATED_VISITOR_FINAL_DEPTH
            || receipt.node_accessors != GENERATED_VISITOR_NODE_ACCESSORS
            || receipt.attributes != GENERATED_VISITOR_ATTRIBUTES
            || self.source.len() != GENERATED_PROGRAM_BYTES
            || self.definition_count != GENERATED_PROGRAM_DEFINITIONS
            || definition_count(self.syntax.root()) != self.definition_count
        {
            return Err("typed visitor checksum, event contract, fixture, or accessor receipt differed".into());
        }
        Ok(PreflightReceipt::new(
            operation_checksum,
            format!(
                "root=SourceUnit source_bytes={} definitions={} events={} enters={} exits={} maximum_depth={} final_depth=0 node_accessors={} attributes={} traversal_checksum=0x{operation_checksum:016x}",
                self.source.len(),
                self.definition_count,
                receipt.enters + receipt.exits,
                receipt.enters,
                receipt.exits,
                receipt.maximum_depth,
                receipt.node_accessors,
                receipt.attributes,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        traversal_receipt(self.syntax.root()).checksum
    }
}

struct StringDecodeCase {
    source: String,
    syntax: ParsedSyntax<jqf_syntax::Expr>,
    literal_span: Span,
    expected: String,
    escape_count: usize,
}

impl StringDecodeCase {
    fn new(fixture: EscapedLiteral) -> Self {
        let syntax = parse_query(SOURCE, &fixture.source)
            .expect("escaped literal source is representable")
            .into_valid_syntax()
            .expect("escaped literal source is valid");
        let literal_span = {
            let ExprKind::String(template) = syntax.root().kind() else {
                panic!("escaped literal fixture root is a string");
            };
            let [TemplateSegment::Literal { span }] = template.segments() else {
                panic!("escaped literal fixture has exactly one literal segment");
            };
            *span
        };
        Self {
            source: fixture.source,
            syntax,
            literal_span,
            expected: fixture.expected,
            escape_count: fixture.escape_count,
        }
    }

    fn decode(&self) -> Result<String, String> {
        let bound = self
            .syntax
            .bind(ResolvedSource::new(
                SOURCE,
                "escaped-256k.jq",
                self.source.as_bytes(),
                0,
            ))
            .map_err(|error| error.to_string())?;
        let mut output = String::new();
        decode_literal_into(bound.source(), SOURCE, self.literal_span, &mut output)
            .map_err(|error| error.to_string())?;
        Ok(output)
    }
}

impl BenchmarkCase for StringDecodeCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new(
            "string-decode/escaped-256k",
            ESCAPED_LITERAL_BYTES as u64,
            ESCAPED_LITERAL_BYTES as u64,
        )
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let operation_checksum = self.run();
        let output = self.decode()?;
        let expected_operation_checksum = compact_output_checksum(self.expected.as_bytes());
        let full_output_checksum = hash_bytes(self.expected.as_bytes());
        let bound = self
            .syntax
            .bind(ResolvedSource::new(
                SOURCE,
                "escaped-256k.jq",
                self.source.as_bytes(),
                0,
            ))
            .map_err(|error| error.to_string())?;
        if output != self.expected
            || operation_checksum != expected_operation_checksum
            || self.literal_span.len() as usize != ESCAPED_LITERAL_BYTES
            || self.source.len() != ESCAPED_LITERAL_BYTES + 2
            || bound.source().source_ref() != SOURCE
            || bound.source().label() != "escaped-256k.jq"
        {
            return Err("bound string decode output, span, source metadata, or checksum differed".into());
        }
        Ok(PreflightReceipt::new(
            operation_checksum,
            format!(
                "root=String diagnostics=0 source_bytes={} encoded_bytes={} decoded_bytes={} escapes={} source=query#0 label=escaped-256k.jq span={}..{} operation_checksum=0x{operation_checksum:016x} full_output_checksum=0x{full_output_checksum:016x}",
                self.source.len(),
                ESCAPED_LITERAL_BYTES,
                output.len(),
                self.escape_count,
                self.literal_span.start(),
                self.literal_span.end(),
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let output = self.decode().expect("preflight proved bound literal decoding succeeds");
        let checksum = compact_output_checksum(output.as_bytes());
        drop(output);
        checksum
    }
}

#[derive(Clone, Copy)]
struct TraversalReceipt {
    checksum: u64,
    enters: u64,
    exits: u64,
    maximum_depth: usize,
    final_depth: usize,
    node_accessors: u64,
    attributes: u64,
}

fn traversal_receipt(unit: &SourceUnit) -> TraversalReceipt {
    let mut checksum = ReceiptHasher::new();
    let mut enters = 0_u64;
    let mut exits = 0_u64;
    let mut depth = 0_usize;
    let mut maximum_depth = 0_usize;
    let mut node_accessors = 0_u64;
    let mut attributes = 0_u64;
    for event in SyntaxWalk::source_unit(unit) {
        match event {
            WalkEvent::Enter(node) => {
                enters += 1;
                depth += 1;
                maximum_depth = maximum_depth.max(depth);
                node_accessors += u64::from(node.kind() == SyntaxNodeKind::NodeAccessor);
                attributes += u64::from(node.kind() == SyntaxNodeKind::Attribute);
                checksum.write_u8(1);
                node.kind().hash(&mut checksum);
                checksum.write_u32(node.span().start());
                checksum.write_u32(node.span().end());
            }
            WalkEvent::Exit(node) => {
                exits += 1;
                checksum.write_u8(2);
                node.kind().hash(&mut checksum);
                checksum.write_u32(node.span().start());
                checksum.write_u32(node.span().end());
                depth = depth.saturating_sub(1);
            }
        }
    }
    TraversalReceipt {
        checksum: checksum.finish(),
        enters,
        exits,
        maximum_depth,
        final_depth: depth,
        node_accessors,
        attributes,
    }
}

fn accessor_counts(walk: SyntaxWalk<'_>) -> (u64, u64) {
    let mut node_accessors = 0_u64;
    let mut attributes = 0_u64;
    for event in walk {
        let WalkEvent::Enter(node) = event else {
            continue;
        };
        node_accessors += u64::from(node.kind() == SyntaxNodeKind::NodeAccessor);
        attributes += u64::from(node.kind() == SyntaxNodeKind::Attribute);
    }
    (node_accessors, attributes)
}

fn definition_count(unit: &SourceUnit) -> usize {
    unit.items
        .iter()
        .filter(|item| matches!(item, SourceItem::Def(_)))
        .count()
}

fn program_receipt(unit: &SourceUnit) -> u64 {
    node_receipt(
        SyntaxNodeRef::source_unit(unit),
        u64::try_from(unit.items.len()).expect("source item count fits u64"),
    )
}

fn node_receipt(node: SyntaxNodeRef<'_>, extra: u64) -> u64 {
    let mut checksum = ReceiptHasher::new();
    node.kind().hash(&mut checksum);
    checksum.write_u32(node.span().start());
    checksum.write_u32(node.span().end());
    checksum.write_u64(extra);
    checksum.finish()
}

fn token_kind_id(kind: TokenKind) -> usize {
    TokenKind::ALL
        .iter()
        .position(|candidate| *candidate == kind)
        .expect("lexer token belongs to the public inventory")
}

fn compact_output_checksum(bytes: &[u8]) -> u64 {
    let mut checksum = mix(
        checksum_seed(),
        u64::try_from(bytes.len()).expect("decoded output length fits u64"),
    );
    for offset in output_sample_offsets(bytes.len()) {
        checksum = mix(checksum, u64::try_from(offset).expect("decoded output offset fits u64"));
        checksum = mix(checksum, u64::from(bytes[offset]));
    }
    checksum
}

fn output_sample_offsets(length: usize) -> [usize; 5] {
    assert!(length > 0, "decoded benchmark output is nonempty");
    [0, length / 4, length / 2, (length / 4) * 3, length - 1]
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    bytes.iter().fold(checksum_seed(), |mut checksum, byte| {
        checksum ^= u64::from(*byte);
        checksum.wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn mix(checksum: u64, value: u64) -> u64 {
    value.to_le_bytes().iter().fold(checksum, |mut hash, byte| {
        hash ^= u64::from(*byte);
        hash.wrapping_mul(0x0000_0100_0000_01b3)
    })
}

const fn checksum_seed() -> u64 {
    0xcbf2_9ce4_8422_2325
}

struct ReceiptHasher(u64);

impl ReceiptHasher {
    const fn new() -> Self {
        Self(checksum_seed())
    }
}

impl Hasher for ReceiptHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}
