//! Deterministic `jqf-source` benchmark cases and their correctness receipts.

use jqf_bench_core::{BenchmarkCase, CaseMetadata, PreflightReceipt};
use jqf_sdk::{BytePatch, PatchSet};
use jqf_source::{
    Diagnostic, DiagnosticSource, Label, LabelStyle, Namespace, Severity, SourceId, SourceKind, SourceRef, Span,
};

const MEBIBYTE: usize = 1_048_576;
const PATCH_BYTES: usize = 4 * MEBIBYTE;
const PATCH_COUNT: usize = 1_024;
const BASE_OFFSET: u64 = 4_096;
const DIAGNOSTIC_MESSAGE: &str = "invalid catalog value";
const DIAGNOSTIC_SOURCE_LABEL: &str = "catalog.json";
const DIAGNOSTIC_PRIMARY_LABEL: &str = "value starts here";
const DIAGNOSTIC_SECONDARY_LABEL: &str = "insert a closing quote";
/// Every owned byte one `build()` copies: the message, the source label, and
/// both label messages.
const DIAGNOSTIC_COPIED_BYTES: usize = DIAGNOSTIC_MESSAGE.len()
    + DIAGNOSTIC_SOURCE_LABEL.len()
    + DIAGNOSTIC_PRIMARY_LABEL.len()
    + DIAGNOSTIC_SECONDARY_LABEL.len();
const DIAGNOSTIC_NAMESPACE: Namespace = Namespace::new("bench");
const INPUT_SOURCE: SourceRef = SourceRef::new(SourceId::new(7), SourceKind::Input);

/// Construct every deterministic source benchmark case in stable display order.
#[must_use]
pub fn cases() -> Vec<Box<dyn BenchmarkCase>> {
    vec![
        Box::new(PatchApplyCase {
            fixture: PatchFixture::new(),
        }),
        Box::new(DiagnosticBuildCase::new()),
    ]
}

struct PatchApplyCase {
    fixture: PatchFixture,
}

impl BenchmarkCase for PatchApplyCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("patch/apply-4m-sparse", PATCH_COUNT as u64, PATCH_BYTES as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let operation_checksum = self.run();
        let output = self.apply()?;
        let expected = self.fixture.reference_output();
        let full_output_checksum = hash_bytes(&expected);
        let expected_operation_checksum = compact_output_checksum(&expected, patch_sample_offsets(&self.fixture.specs));
        let first = self.fixture.specs.first().ok_or("patch fixture has no patches")?;
        let last = self.fixture.specs.last().ok_or("patch fixture has no patches")?;
        let insertions = self.fixture.specs.iter().filter(|spec| spec.start == spec.end).count();
        let replacements = self.fixture.specs.len() - insertions;
        if output != expected
            || operation_checksum != expected_operation_checksum
            || self.fixture.specs.len() != PATCH_COUNT
            || insertions != PATCH_COUNT / 2
            || replacements != PATCH_COUNT / 2
            || output.len() != self.fixture.output_bytes
            || first.start != first.end
            || last.start >= last.end
            || last.end > self.fixture.original.len()
        {
            return Err("patched bytes, checksum, count, or patch positions differed from the fixture".into());
        }
        Ok(PreflightReceipt::new(
            operation_checksum,
            format!(
                "input_bytes={} patches={} insertions={insertions} replacements={replacements} output_bytes={} first={}..{} last={}..{} operation_checksum=0x{operation_checksum:016x} full_output_checksum=0x{full_output_checksum:016x}",
                self.fixture.original.len(),
                self.fixture.specs.len(),
                output.len(),
                first.start,
                first.end,
                last.start,
                last.end,
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let output = self.apply().expect("deterministic patch fixture is valid");
        let checksum = compact_output_checksum(&output, patch_sample_offsets(&self.fixture.specs));
        drop(output);
        checksum
    }
}

impl PatchApplyCase {
    fn apply(&self) -> Result<Vec<u8>, String> {
        self.fixture
            .patches
            .apply(Some(INPUT_SOURCE), &self.fixture.original)
            .map_err(|error| error.to_string())
    }
}

struct DiagnosticBuildCase;

impl DiagnosticBuildCase {
    const fn new() -> Self {
        Self
    }

    fn build() -> Diagnostic {
        Diagnostic::new(
            DIAGNOSTIC_NAMESPACE.code("invalid.value"),
            Severity::Error,
            DIAGNOSTIC_MESSAGE,
        )
        .with_source(DiagnosticSource::new(
            INPUT_SOURCE,
            DIAGNOSTIC_SOURCE_LABEL,
            BASE_OFFSET,
        ))
        .with_label(Label::primary(
            INPUT_SOURCE,
            Span::from_usize(24, 36),
            DIAGNOSTIC_PRIMARY_LABEL,
        ))
        .with_label(Label::secondary(
            INPUT_SOURCE,
            Span::from_usize(64, 64),
            DIAGNOSTIC_SECONDARY_LABEL,
        ))
    }
}

impl BenchmarkCase for DiagnosticBuildCase {
    fn metadata(&self) -> CaseMetadata {
        CaseMetadata::new("diagnostic/build-typical", 1, DIAGNOSTIC_COPIED_BYTES as u64)
    }

    fn preflight(&mut self) -> Result<PreflightReceipt, String> {
        let checksum = self.run();
        let diagnostic = Self::build();
        let expected_checksum = expected_diagnostic_checksum();
        let copied_bytes = diagnostic.message().len()
            + diagnostic
                .sources()
                .iter()
                .map(|source| source.label().len())
                .sum::<usize>()
            + diagnostic
                .labels()
                .iter()
                .map(|label| label.message().len())
                .sum::<usize>();
        if checksum != expected_checksum
            || diagnostic.code() != DIAGNOSTIC_NAMESPACE.code("invalid.value")
            || diagnostic.severity() != Severity::Error
            || diagnostic.message() != DIAGNOSTIC_MESSAGE
            || diagnostic.sources().len() != 1
            || diagnostic.labels().len() != 2
            || copied_bytes != DIAGNOSTIC_COPIED_BYTES
            || diagnostic.sources()[0].source() != INPUT_SOURCE
            || diagnostic.sources()[0].label() != DIAGNOSTIC_SOURCE_LABEL
            || diagnostic.sources()[0].base_offset() != BASE_OFFSET
            || !label_matches(
                &diagnostic.labels()[0],
                LabelStyle::Primary,
                INPUT_SOURCE,
                Span::from_usize(24, 36),
                DIAGNOSTIC_PRIMARY_LABEL,
            )
            || !label_matches(
                &diagnostic.labels()[1],
                LabelStyle::Secondary,
                INPUT_SOURCE,
                Span::from_usize(64, 64),
                DIAGNOSTIC_SECONDARY_LABEL,
            )
        {
            return Err("complete ordered diagnostic projection or checksum differed".into());
        }
        Ok(PreflightReceipt::new(
            checksum,
            format!(
                "code=bench.invalid.value severity=error message=\"invalid catalog value\" sources=[input#7:catalog.json@4096] labels=[primary:input#7:24..36,secondary:input#7:64..64] copied_bytes={copied_bytes} checksum=0x{checksum:016x}",
            ),
        ))
    }

    fn run(&mut self) -> u64 {
        let diagnostic = Self::build();
        let checksum = diagnostic_checksum(&diagnostic);
        drop(diagnostic);
        checksum
    }
}

struct PatchFixture {
    original: Vec<u8>,
    specs: Vec<PatchSpec>,
    patches: PatchSet,
    output_bytes: usize,
}

#[derive(Clone)]
struct PatchSpec {
    start: usize,
    end: usize,
    replacement: Vec<u8>,
    output_start: usize,
}

impl PatchFixture {
    fn new() -> Self {
        let mut original = Vec::with_capacity(PATCH_BYTES);
        for index in 0..PATCH_BYTES {
            original.push(u8::try_from((index.wrapping_mul(17).wrapping_add(31)) % 251).expect("modulo 251 fits u8"));
        }
        let mut specs = Vec::with_capacity(PATCH_COUNT);
        let mut output_growth = 0;
        for index in 0..PATCH_COUNT {
            let start = ((index + 1) * PATCH_BYTES / (PATCH_COUNT + 1)) & !15;
            let end = if index % 2 == 0 { start } else { start + 8 };
            let replacement = patch_replacement(index, start == end);
            let output_start = start + output_growth;
            output_growth += replacement.len() - (end - start);
            specs.push(PatchSpec {
                start,
                end,
                replacement,
                output_start,
            });
        }
        let patches = PatchSet::try_new(
            Some(INPUT_SOURCE),
            original.len(),
            specs
                .iter()
                .map(|spec| BytePatch::try_from_usize(spec.start, spec.end, spec.replacement.clone()))
                .collect::<Result<Vec<_>, _>>()
                .expect("fixed sparse patch spans are valid"),
        )
        .expect("fixed sparse patches are ordered and non-overlapping");
        let output_bytes = original.len() + output_growth;
        Self {
            original,
            specs,
            patches,
            output_bytes,
        }
    }

    fn reference_output(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.output_bytes);
        let mut cursor = 0;
        for spec in &self.specs {
            output.extend_from_slice(&self.original[cursor..spec.start]);
            output.extend_from_slice(&spec.replacement);
            cursor = spec.end;
        }
        output.extend_from_slice(&self.original[cursor..]);
        assert_eq!(output.len(), self.output_bytes);
        output
    }
}

fn patch_replacement(index: usize, insertion: bool) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut replacement = Vec::with_capacity(if insertion { 6 } else { 10 });
    replacement.push(if insertion { b'I' } else { b'R' });
    for shift in [12, 8, 4, 0] {
        replacement.push(HEX[(index >> shift) & 0x0f]);
    }
    if insertion {
        replacement.push(b'!');
    } else {
        replacement.extend_from_slice(b":jqf!");
    }
    replacement
}

fn diagnostic_checksum(diagnostic: &Diagnostic) -> u64 {
    let code = diagnostic.code();
    let mut checksum = hash_bytes_with_seed(checksum_seed(), code.namespace().name().as_bytes());
    checksum = hash_bytes_with_seed(checksum, code.name().as_bytes());
    checksum = hash_bytes_with_seed(checksum, severity_name(diagnostic.severity()));
    checksum = hash_bytes_with_seed(checksum, diagnostic.message().as_bytes());
    checksum = mix(checksum, diagnostic.sources().len() as u64);
    for source in diagnostic.sources() {
        checksum = mix_source(checksum, source.source());
        checksum = hash_bytes_with_seed(checksum, source.label().as_bytes());
        checksum = mix(checksum, source.base_offset());
    }
    checksum = mix(checksum, diagnostic.labels().len() as u64);
    for label in diagnostic.labels() {
        checksum = hash_bytes_with_seed(checksum, label_style_name(label.style()));
        checksum = mix_source(checksum, label.source());
        checksum = mix_span(checksum, label.span());
        checksum = hash_bytes_with_seed(checksum, label.message().as_bytes());
    }
    checksum
}

fn expected_diagnostic_checksum() -> u64 {
    let mut checksum = hash_bytes_with_seed(checksum_seed(), b"bench");
    checksum = hash_bytes_with_seed(checksum, b"invalid.value");
    checksum = hash_bytes_with_seed(checksum, b"error");
    checksum = hash_bytes_with_seed(checksum, DIAGNOSTIC_MESSAGE.as_bytes());
    checksum = mix(checksum, 1);
    checksum = mix_source(checksum, INPUT_SOURCE);
    checksum = hash_bytes_with_seed(checksum, DIAGNOSTIC_SOURCE_LABEL.as_bytes());
    checksum = mix(checksum, BASE_OFFSET);
    checksum = mix(checksum, 2);
    for (style, source, span, message) in [
        (
            LabelStyle::Primary,
            INPUT_SOURCE,
            Span::from_usize(24, 36),
            DIAGNOSTIC_PRIMARY_LABEL,
        ),
        (
            LabelStyle::Secondary,
            INPUT_SOURCE,
            Span::from_usize(64, 64),
            DIAGNOSTIC_SECONDARY_LABEL,
        ),
    ] {
        checksum = hash_bytes_with_seed(checksum, label_style_name(style));
        checksum = mix_source(checksum, source);
        checksum = mix_span(checksum, span);
        checksum = hash_bytes_with_seed(checksum, message.as_bytes());
    }
    checksum
}

fn label_matches(label: &Label, style: LabelStyle, source: SourceRef, span: Span, message: &str) -> bool {
    label.style() == style && label.source() == source && label.span() == span && label.message() == message
}

fn severity_name(severity: Severity) -> &'static [u8] {
    if severity == Severity::Error {
        b"error"
    } else if severity == Severity::Warning {
        b"warning"
    } else if severity == Severity::Info {
        b"info"
    } else if severity == Severity::Trace {
        b"trace"
    } else {
        b"unknown"
    }
}

fn label_style_name(style: LabelStyle) -> &'static [u8] {
    if style == LabelStyle::Primary {
        b"primary"
    } else if style == LabelStyle::Secondary {
        b"secondary"
    } else {
        b"unknown"
    }
}

fn mix_source(checksum: u64, source: SourceRef) -> u64 {
    let checksum = hash_bytes_with_seed(checksum, source.kind().as_str().as_bytes());
    mix(checksum, u64::from(source.id().get()))
}

fn mix_span(checksum: u64, span: Span) -> u64 {
    mix(mix(checksum, u64::from(span.start())), u64::from(span.end()))
}

fn patch_sample_offsets(specs: &[PatchSpec]) -> impl Iterator<Item = usize> + '_ {
    specs.iter().flat_map(|spec| {
        [
            spec.output_start,
            spec.output_start + spec.replacement.len() / 2,
            spec.output_start + spec.replacement.len() - 1,
        ]
    })
}

fn compact_output_checksum(bytes: &[u8], sample_offsets: impl IntoIterator<Item = usize>) -> u64 {
    let mut checksum = mix(
        checksum_seed(),
        u64::try_from(bytes.len()).expect("benchmark output length fits u64"),
    );
    for offset in sample_offsets {
        checksum = mix(checksum, u64::try_from(offset).expect("sample offset fits u64"));
        checksum = mix(checksum, u64::from(bytes[offset]));
    }
    checksum
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    hash_bytes_with_seed(checksum_seed(), bytes)
}

const fn checksum_seed() -> u64 {
    0xcbf2_9ce4_8422_2325
}

fn hash_bytes_with_seed(mut checksum: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        checksum ^= u64::from(byte);
        checksum = checksum.wrapping_mul(0x0000_0100_0000_01b3);
    }
    checksum
}

fn mix(checksum: u64, value: u64) -> u64 {
    hash_bytes_with_seed(checksum, &value.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_case_preflight_retains_a_nonzero_checksum_and_detail() {
        let mut cases = cases();
        let metadata: Vec<_> = cases.iter().map(Box::as_ref).map(BenchmarkCase::metadata).collect();
        assert_eq!(
            metadata.iter().map(|metadata| metadata.name).collect::<Vec<_>>(),
            ["patch/apply-4m-sparse", "diagnostic/build-typical"]
        );
        assert_eq!(
            metadata
                .iter()
                .map(|metadata| (metadata.operations_per_invocation, metadata.bytes_per_invocation))
                .collect::<Vec<_>>(),
            [
                (PATCH_COUNT as u64, PATCH_BYTES as u64),
                (1, DIAGNOSTIC_COPIED_BYTES as u64),
            ]
        );
        for case in &mut cases {
            let name = case.metadata().name;
            let receipt = case.preflight().expect("deterministic case preflight");
            assert_ne!(receipt.checksum, 0, "{name}");
            assert!(receipt.detail.contains("checksum=0x"), "{name}");
            let witnesses: &[&str] = match name {
                "patch/apply-4m-sparse" => &[
                    "input_bytes=4194304",
                    "patches=1024",
                    "insertions=512",
                    "replacements=512",
                    "output_bytes=4198400",
                ],
                "diagnostic/build-typical" => &[
                    "sources=[input#7:catalog.json@4096]",
                    "labels=[primary:input#7:24..36,secondary:input#7:64..64]",
                    "copied_bytes=72",
                ],
                _ => &[],
            };
            for witness in witnesses {
                assert!(receipt.detail.contains(witness), "{name}: {witness}");
            }
        }
    }

    #[test]
    fn fixtures_cover_the_declared_size_and_boundary_contracts() {
        let patches = PatchFixture::new();
        assert_eq!(patches.original.len(), PATCH_BYTES);
        assert_eq!(patches.specs.len(), PATCH_COUNT);
        assert!(patches.specs.windows(2).all(|pair| pair[0].end <= pair[1].start));
        assert_eq!(
            patches.specs.iter().filter(|spec| spec.start == spec.end).count(),
            PATCH_COUNT / 2
        );
        assert_eq!(
            patches.specs.iter().filter(|spec| spec.start < spec.end).count(),
            PATCH_COUNT / 2
        );
        assert_eq!(patches.reference_output().len(), patches.output_bytes);
        assert_eq!(patches.output_bytes, PATCH_BYTES + 4_096);
    }

    #[test]
    fn diagnostic_receipt_witnesses_the_complete_ordered_projection() {
        let mut case = DiagnosticBuildCase::new();
        let receipt = case.preflight().expect("diagnostic preflight");
        for witness in [
            "code=bench.invalid.value severity=error message=\"invalid catalog value\"",
            "sources=[input#7:catalog.json@4096]",
            "labels=[primary:input#7:24..36,secondary:input#7:64..64]",
            "copied_bytes=72",
        ] {
            assert!(receipt.detail.contains(witness), "{witness}");
        }

        let diagnostic = DiagnosticBuildCase::build();
        assert_eq!(diagnostic.sources().len(), 1);
        assert_eq!(diagnostic.labels().len(), 2);
        assert_eq!(diagnostic_checksum(&diagnostic), expected_diagnostic_checksum());
    }
}
