//! Force-route corpus: designated route vs whole-document floor over the
//! frozen compat dump rows.
//!
//! Every dump row whose projection predicates admit is run both ways;
//! bytes, completion, and `failure_class` must agree. Loader:
//! `corpus/compat-rows.tsv`.

use crate::harness::{OracleRoute, oracle_run_over, program_for, resources};
use jqf_codec_core::{AccessRequirement, AccessResultKind};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CompiledProgram, HostIo, ProjectionClass};
use jqf_sdk::CodecCatalog;

const FORCE_ROUTE_CORPUS_ROWS: usize = 5696;

/// Whether a compiled corpus program takes a route the floor cannot, and so belongs
/// in the differential.
///
/// Admits the range rungs (boundary-less count and bare-slice publish), the
/// recognized count demand, and the recognized element-iteration demand. A
/// container count `PATH | length` belongs because a non-array container
/// declines to this very floor. Count/element presence is read off the
/// instantiated packed plan, not `CompiledProgram` demand getters.
fn sweep_admits(program: &CompiledProgram, requirement: &AccessRequirement) -> bool {
    program.host_io() == HostIo::SpanCut || requirement.count().is_some() || requirement.element().is_some()
}

/// A row whose program does not COMPILE has no class and no route. That is only
/// ever a row the corpus already expects jqf to fail on (`reject`: deliberately out
/// of the static-path subset; `typeerror`: an undefined builtin, an unbound `$x`, a
/// `.[:]`). Any other kind reaching here would be an unclassified row hiding inside
/// the sweep, so it is a hard failure rather than a skip.
fn assert_row_may_not_compile(row: &CorpusRow) -> Result<(), String> {
    if row.kind != "reject" && row.kind != "typeerror" {
        return Err(format!(
            "force-route row {} (kind={}) does not compile: {:?}",
            row.index, row.kind, row.program
        ));
    }
    Ok(())
}

/// Rows whose program jqf deliberately rejects (the `reject` kind) do not
/// compile and are counted as unparsed rather than skipped silently.
#[allow(
    clippy::too_many_lines,
    reason = "the standing force-route corpus loop: one row at a time, floor-vs-designated, byte + class + completion; a split would obscure the comparison it pins"
)]
pub(crate) fn assert_force_route_corpus(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    let rows = corpus_rows()?;
    if rows.len() != FORCE_ROUTE_CORPUS_ROWS {
        return Err(format!(
            "force-route corpus dump returned {} rows, expected {FORCE_ROUTE_CORPUS_ROWS}",
            rows.len()
        ));
    }

    let mut unparsed = 0_u32;
    let mut structure = 0_u32;
    let mut fields = 0_u32;
    let mut subtree = 0_u32;
    let mut eligible = 0_u32;
    let mut forced = 0_u32;
    let mut divergences = Vec::new();

    for row in &rows {
        let resources = resources();
        let Ok(program) = program_for(&row.program, &resources) else {
            assert_row_may_not_compile(row)?;
            unparsed += 1;
            continue;
        };
        match program.projection_class() {
            ProjectionClass::Structure => structure += 1,
            ProjectionClass::Fields(_) => fields += 1,
            ProjectionClass::Subtree => subtree += 1,
        }
        let requirement = program.try_requirement(&resources).map_err(|error| {
            format!(
                "force-route row {} cannot instantiate requirement: {:?}",
                row.index,
                error.kind()
            )
        })?;
        if !sweep_admits(&program, &requirement) {
            continue;
        }
        eligible += 1;
        let count_or_element = requirement.count().is_some() || requirement.element().is_some();
        drop(program);

        let designated = oracle_run_over(
            OracleRoute::Designated,
            catalog,
            format,
            dialect,
            &row.program,
            &row.input,
        )?;
        let floor = oracle_run_over(OracleRoute::Floor, catalog, format, dialect, &row.program, &row.input)?;
        if floor.result != AccessResultKind::CompleteDocument {
            return Err(format!(
                "force-route floor for {:?} did not take the whole-document route: {:?}",
                row.program, floor.result
            ));
        }
        // A failed fast-rung run is not a forced lane: the route fell through
        // and published nothing, so counting it would inflate the proof that the
        // comparison is not floor ≡ floor in disguise. Count/element rows that
        // completed are served (the demand hint rode the whole-document
        // requirement); a failed run is not.
        if designated.completed && (designated.range_located || count_or_element) {
            forced += 1;
        }
        if designated.bytes != floor.bytes
            || designated.completed != floor.completed
            || designated.failure_class != floor.failure_class
        {
            divergences.push(format!(
                "{} kind={} program={:?}: route=({:?}, completed={}, class={:?}) floor=({:?}, completed={}, class={:?})",
                row.index,
                row.kind,
                row.program,
                String::from_utf8_lossy(&designated.bytes),
                designated.completed,
                designated.failure_class,
                String::from_utf8_lossy(&floor.bytes),
                floor.completed,
                floor.failure_class,
            ));
        }
    }

    println!(
        "force-route: rows={} unparsed={unparsed} class_structure={structure} class_fields={fields} class_subtree={subtree} eligible={eligible} forced={forced} divergences={}",
        rows.len(),
        divergences.len()
    );

    if !divergences.is_empty() {
        return Err(format!("force-route divergences:\n{}", divergences.join("\n")));
    }
    // A lane that forces nothing proves nothing: `route ≡ floor` must be a real
    // comparison, never floor ≡ floor in disguise.
    if forced == 0 {
        return Err(
            "force-route swept the corpus without taking a designated count, element, or range-locate route".to_owned(),
        );
    }
    Ok(())
}

/// One row of the frozen CLI corpus TSV.
struct CorpusRow {
    index: usize,
    kind: String,
    input: Vec<u8>,
    program: String,
}

/// Reads the frozen corpus row set from `corpus/compat-rows.tsv`.
///
/// The path is resolved from `CARGO_MANIFEST_DIR` so the sweep is independent of
/// the working directory.
fn corpus_rows() -> Result<Vec<CorpusRow>, String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/compat-rows.tsv");
    let text = std::fs::read_to_string(&path).map_err(|error| format!("corpus {}: {error}", path.display()))?;
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let (Some(kind), Some(input), Some(program), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(format!("corpus dump row {index} is malformed: {line:?}"));
        };
        let program = decode_base64(program)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .ok_or_else(|| format!("corpus dump row {index} program is not UTF-8 base64"))?;
        rows.push(CorpusRow {
            index,
            kind: kind.to_owned(),
            input: decode_base64(input).ok_or_else(|| format!("corpus dump row {index} input is not base64"))?,
            program,
        });
    }
    Ok(rows)
}

/// Standard base64 with padding. The corpus dump is the only consumer; a
/// dependency for sixty lines of table lookup would be worse than the table.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let chunks = bytes.chunks_exact(4);
    let last = chunks.len().saturating_sub(1);
    for (index, chunk) in chunks.enumerate() {
        if index != last && chunk.contains(&b'=') {
            return None;
        }
        let pad = usize::from(chunk[2] == b'=') + usize::from(chunk[3] == b'=');
        if pad == 1 && chunk[2] == b'=' {
            return None;
        }
        let mut packed = 0_u32;
        for (offset, &byte) in chunk.iter().enumerate() {
            let value = if byte == b'=' && offset >= 4 - pad {
                0
            } else {
                sextet(byte)?
            };
            packed = (packed << 6) | value;
        }
        out.push(u8::try_from((packed >> 16) & 0xff).ok()?);
        if pad < 2 {
            out.push(u8::try_from((packed >> 8) & 0xff).ok()?);
        }
        if pad < 1 {
            out.push(u8::try_from(packed & 0xff).ok()?);
        }
    }
    Some(out)
}
