//! The three table renderers: shared extraction, layout, and emission.
//!
//! `render.gfm-table@1`, `render.html-table@1`, and `render.grid-table@1` are deliberately atomic per table: one input
//! item is one table, drained completely before any of its frame is published. The item must be an OBJECT (a one-row
//! table whose column labels are its member identities) or an ARRAY OF OBJECTS (columns are the union of member
//! identities in first-appearance order, rows are the objects with missing members as `null`). Header labels therefore
//! come only from the extraction — never inferred from a first data row. A cell must be a core scalar or null (a
//! tagged cell publishes its payload); a NESTED container cell renders its compact JSON spelling, exactly the miller
//! convention — a cell is TEXT, so the structure travels as its JSON text rather than failing the table.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use hashbrown::HashMap;
use jqf_codec_core::{CodecError, TagLayer, project_tag, value_tag_layer};
use jqf_data::{Float, Value};
use jqf_resource::ResourceContext;

use super::atom::{AtomOrBreak, EscapeStyle, atomize};
use super::error::{allocation, contract, unsupported};
use super::options::{DEFAULT_SAMPLE_BYTES, HeaderPolicy, RenderEncodeOptions};
use super::scalar::{StringStyle, write_json_quoted, write_scalar};

/// Which table renderer emits a laid-out table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TableRenderer {
    /// `render.gfm-table@1`.
    Gfm,
    /// `render.html-table@1`.
    Html,
    /// `render.grid-table@1`.
    Grid,
}

impl TableRenderer {
    /// The escaping style this renderer's cells use.
    const fn style(self) -> EscapeStyle {
        match self {
            Self::Gfm => EscapeStyle::Gfm,
            Self::Html => EscapeStyle::Html,
            Self::Grid => EscapeStyle::Grid,
        }
    }
}

/// One extracted table, borrowing its cell values from the input item.
struct Table<'value> {
    /// Ordered column labels (the extraction's member identities).
    headers: Vec<String>,
    /// Rows, each aligned to the column count; missing members are `null`.
    rows: Vec<Vec<&'value Value>>,
}

/// Extracts one table from an input value.
fn extract(value: &Value) -> Result<Table<'_>, CodecError> {
    match value {
        Value::Object(object) => {
            let mut headers = Vec::new();
            let mut row = Vec::new();
            for entry in object {
                headers.push(String::from(entry.key()));
                row.push(entry.value());
            }
            Ok(Table {
                headers,
                rows: alloc::vec![row],
            })
        }
        Value::Array(array) => {
            // Union of member identities in first-appearance order, collected BEFORE any row is built: a row whose
            // object lacks a member a LATER element introduces must still align to the FULL header set (missing members
            // are `null`). Building the rows during the union left the earlier rows short and panicked the column
            // layout on the first-row-shorter-than-later-row shape.
            //
            // Each member's column resolves once through the keyed map (no linear membership scan over the accumulated
            // headers), and every entry's column is recorded as it is seen, so the row pass places values directly
            // instead of re-walking each element per header. The single shape check below is the only one: a non-object
            // element refuses before any row exists, so the row pass needs no second guard.
            let mut headers: Vec<String> = Vec::new();
            let mut columns: HashMap<&str, usize> = HashMap::new();
            let mut placements: Vec<Vec<(usize, &Value)>> = Vec::with_capacity(array.len());
            for element in array {
                let Value::Object(element) = element else {
                    return Err(unsupported(
                        "table-shape",
                        "table renderers accept an object or an array of objects per item",
                    ));
                };
                let mut placed: Vec<(usize, &Value)> = Vec::with_capacity(element.len());
                for entry in element {
                    let len = headers.len();
                    let existing = columns.get(entry.key()).copied();
                    let column = existing.unwrap_or_else(|| {
                        columns.insert(entry.key(), len);
                        headers.push(String::from(entry.key()));
                        len
                    });
                    placed.push((column, entry.value()));
                }
                placements.push(placed);
            }
            let mut rows: Vec<Vec<&Value>> = Vec::with_capacity(placements.len());
            for placed in placements {
                let mut row = alloc::vec![&Value::Null; headers.len()];
                for (column, value) in placed {
                    // A re-occurrence of an already-placed member overwrites: the final occurrence supplies the value.
                    row[column] = value;
                }
                rows.push(row);
            }
            Ok(Table { headers, rows })
        }
        _ => Err(unsupported(
            "table-shape",
            "table renderers accept an object or an array of objects per item",
        )),
    }
}

/// One laid-out column: its frozen width and alignment.
struct Column {
    /// Frozen display width.
    width: usize,
    /// Right-aligned for numeric columns, left otherwise.
    right: bool,
}

/// One laid-out cell: its escaped visual lines and each line's display width.
struct CellLines {
    /// Escaped visual-line bytes, in display order.
    lines: Vec<String>,
    /// Displayed width of each visual line.
    widths: Vec<usize>,
}

/// A fully laid-out table: columns plus header/row cell lines.
struct LaidOut {
    columns: Vec<Column>,
    /// Header cell lines; empty when the header policy is absent.
    header: Vec<CellLines>,
    rows: Vec<Vec<CellLines>>,
}

/// Renders one input item as one complete table frame.
///
/// The frame carries every interior LF; the facade appends the final one.
///
/// # Errors
///
/// Returns an `UnsupportedShape` reject for a non-table item, a GFM request with an absent header, a nested cell past
/// the crate's depth ceiling, or a cap failure (rows, rendered bytes, or a too-wide atom); every refusal emits no
/// frame.
pub(crate) fn render(
    value: &Value,
    renderer: TableRenderer,
    options: RenderEncodeOptions,
    resources: &ResourceContext<'_>,
) -> Result<String, CodecError> {
    if renderer == TableRenderer::Gfm && options.header == HeaderPolicy::Absent {
        return Err(unsupported(
            "gfm-header",
            "render.gfm-table@1 requires header = present (GFM tables need a header)",
        ));
    }
    let table = extract(value)?;
    if table.headers.is_empty() {
        // An EMPTY table (`[]`, `{}`) has no rows and no columns: its frame is the empty text, not a refusal — an
        // empty input prints an empty table, and there is nothing the dialect could draw anyway.
        return Ok(String::new());
    }
    if table.rows.len() > options.sample_rows {
        return Err(unsupported(
            "table-rows",
            "the table exceeds the sampled layout's row cap",
        ));
    }
    let laid_out = layout(&table, renderer.style(), options, resources)?;
    Ok(match renderer {
        TableRenderer::Gfm => emit_gfm(&laid_out),
        TableRenderer::Html => emit_html(&laid_out, options.header),
        TableRenderer::Grid => emit_grid(&laid_out, options.header),
    })
}

/// Refuses once the rendered-cell byte cap is crossed.
fn check_rendered_bytes(rendered_bytes: usize) -> Result<(), CodecError> {
    if rendered_bytes > DEFAULT_SAMPLE_BYTES {
        return Err(unsupported(
            "table-bytes",
            "the table exceeds the sampled layout's rendered-cell-byte cap",
        ));
    }
    Ok(())
}

/// Lays the extracted table out under the sampled layout law.
fn layout(
    table: &Table<'_>,
    style: EscapeStyle,
    options: RenderEncodeOptions,
    resources: &ResourceContext<'_>,
) -> Result<LaidOut, CodecError> {
    // Atomize every header label and cell once, measuring the rendered-cell byte cap as we go (header labels count only
    // when the header is sampled). The cap is checked INCREMENTALLY, after every atomized cell: a single huge cell
    // refuses at its own accumulation instead of paying the full escaped text of every cell first — the cap exists to
    // bound the work.
    let use_header = options.header == HeaderPolicy::Present;
    let mut header_atoms: Vec<Vec<AtomOrBreak>> = Vec::new();
    let mut row_atoms: Vec<Vec<Vec<AtomOrBreak>>> = Vec::with_capacity(table.rows.len());
    let mut rendered_bytes = 0_usize;
    if use_header {
        header_atoms.reserve(table.headers.len());
        for header in &table.headers {
            let atoms = atomize(header, style, options.width)?;
            rendered_bytes = rendered_bytes
                .checked_add(escaped_bytes(&atoms))
                .ok_or_else(|| unsupported("table-bytes", "rendered cell bytes overflow"))?;
            check_rendered_bytes(rendered_bytes)?;
            header_atoms.push(atoms);
        }
    }
    for row in &table.rows {
        let mut cells = Vec::with_capacity(row.len());
        for cell in row {
            let text = cell_text(cell, resources)?;
            let atoms = atomize(text.as_str(), style, options.width)?;
            rendered_bytes = rendered_bytes
                .checked_add(escaped_bytes(&atoms))
                .ok_or_else(|| unsupported("table-bytes", "rendered cell bytes overflow"))?;
            check_rendered_bytes(rendered_bytes)?;
            cells.push(atoms);
        }
        row_atoms.push(cells);
    }

    // Freeze column widths: natural width, capped by the maximum when bound.
    let mut columns = Vec::with_capacity(table.headers.len());
    for column in 0..table.headers.len() {
        let mut natural = 0_usize;
        if use_header {
            natural = natural.max(logical_line_width(&header_atoms[column]));
        }
        for row in &row_atoms {
            natural = natural.max(logical_line_width(&row[column]));
        }
        let width = if options.max_width == 0 {
            natural
        } else {
            natural.max(1).min(options.max_width)
        };
        let right = table
            .rows
            .iter()
            .all(|row| matches!(row[column].untagged(), Value::Number(_)));
        columns.push(Column { width, right });
    }

    // Wrap each cell into visual lines under its frozen column width.
    let header = if use_header {
        header_atoms
            .iter()
            .zip(&columns)
            .map(|(atoms, column)| wrap(atoms, column.width))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let mut rows = Vec::with_capacity(row_atoms.len());
    for row in row_atoms {
        let mut laid = Vec::with_capacity(row.len());
        for (atoms, column) in row.iter().zip(&columns) {
            laid.push(wrap(atoms, column.width)?);
        }
        rows.push(laid);
    }
    Ok(LaidOut { columns, header, rows })
}

/// The plain text of one table cell (a scalar formatter spelling).
///
/// A table cell is TEXT and a column has no room for a tag, so a tagged cell publishes its payload under the shared
/// publish law and records the event.
fn cell_text(cell: &Value, resources: &ResourceContext<'_>) -> Result<String, CodecError> {
    if let TagLayer::Tagged(_) = value_tag_layer(cell) {
        project_tag(resources);
    }
    match cell.untagged() {
        Value::Array(_) | Value::Object(_) => {
            let mut out = String::new();
            write_compact_json(&mut out, cell.untagged(), 0)?;
            Ok(out)
        }
        _ => {
            let mut out = String::new();
            write_scalar(&mut out, cell.untagged(), StringStyle::Raw)?;
            Ok(out)
        }
    }
}

/// Appends one value's compact JSON spelling to `out`.
///
/// This is the nested-cell arm of [`cell_text`]: a container cell travels as its JSON text. Strings quote per the
/// strict-JSON law; a non-core tag has no JSON spelling, so it descends into its payload (the shared publish law); a
/// non-finite number has no JSON literal and renders the JSON encoder's own non-finite law — `null` for NaN, the
/// clamped widest binary64 for an infinity.
fn write_compact_json(out: &mut String, value: &Value, depth: usize) -> Result<(), CodecError> {
    // The crate's shared nesting law: an owned value can nest arbitrarily deep (the engine sets no construction limit)
    // and this walk recurses once per container level, so past the ceiling it refuses by name instead of overflowing
    // the stack. See [`crate::MAX_NESTING_DEPTH`].
    if depth >= crate::MAX_NESTING_DEPTH {
        return Err(unsupported(
            "table-depth",
            "a nested cell nests past the render package's depth ceiling",
        ));
    }
    match value {
        Value::Array(array) => {
            out.push('[');
            for index in 0..array.len() {
                if index != 0 {
                    out.push(',');
                }
                let Some(element) = array.get(index) else {
                    return Err(contract("an array index past its length"));
                };
                write_compact_json(out, element, depth + 1)?;
            }
            out.push(']');
            Ok(())
        }
        Value::Object(object) => {
            out.push('{');
            for index in 0..object.len() {
                if index != 0 {
                    out.push(',');
                }
                let Some(entry) = object.get_index(index) else {
                    return Err(contract("an object index past its length"));
                };
                write_json_quoted(out, entry.key(), false);
                out.push(':');
                write_compact_json(out, entry.value(), depth + 1)?;
            }
            out.push('}');
            Ok(())
        }
        // A tag layer is not part of the JSON spelling: publish its payload.
        Value::Tagged { payload, .. } => write_compact_json(out, payload, depth + 1),
        Value::Number(number) if number.as_float().is_some_and(|value| !value.get().is_finite()) => {
            out.push_str(match number.as_float().map(Float::get) {
                Some(value) if value.is_nan() => "null",
                Some(value) if value < 0.0 => "-1.7976931348623157e+308",
                _ => "1.7976931348623157e+308",
            });
            Ok(())
        }
        other => write_scalar(out, other, StringStyle::TreeQuoted),
    }
}

/// Greedily wraps one cell's atom stream into visual lines at `width`.
///
/// A `width` of zero disables wrapping: each logical line stays whole. At least one atom is guaranteed per nonempty
/// visual line; an atom wider than the frozen width is `CellTooWide` before this cell could be published.
fn wrap(atoms: &[AtomOrBreak], width: usize) -> Result<CellLines, CodecError> {
    let mut lines: Vec<String> = Vec::new();
    let mut widths: Vec<usize> = Vec::new();
    let mut current: String = String::new();
    let mut current_width = 0_usize;
    for element in atoms {
        match element {
            AtomOrBreak::Break => {
                lines.push(current);
                widths.push(current_width);
                current = String::new();
                current_width = 0;
            }
            AtomOrBreak::Atom(atom) => {
                let atom_width = usize::from(atom.width);
                if width != 0 && atom_width > width {
                    return Err(unsupported(
                        "table-width",
                        "an atom is wider than the frozen column width",
                    ));
                }
                if width != 0
                    && !current.is_empty()
                    && current_width
                        .checked_add(atom_width)
                        .ok_or_else(|| unsupported("table-width", "a cell line exceeds the frozen column width"))?
                        > width
                {
                    lines.push(current);
                    widths.push(current_width);
                    current = String::new();
                    current_width = 0;
                }
                current.try_reserve(atom.bytes.len()).map_err(|_| allocation())?;
                current.push_str(&atom.bytes);
                current_width = current_width.saturating_add(atom_width);
            }
        }
    }
    lines.push(current);
    widths.push(current_width);
    Ok(CellLines { lines, widths })
}

/// The widest single logical line's displayed width.
fn logical_line_width(atoms: &[AtomOrBreak]) -> usize {
    let mut width = 0_usize;
    let mut line = 0_usize;
    for element in atoms {
        match element {
            AtomOrBreak::Break => {
                width = width.max(line);
                line = 0;
            }
            AtomOrBreak::Atom(atom) => line = line.saturating_add(usize::from(atom.width)),
        }
    }
    width.max(line)
}

/// Total escaped byte length of one atom stream.
fn escaped_bytes(atoms: &[AtomOrBreak]) -> usize {
    atoms
        .iter()
        .map(|element| match element {
            AtomOrBreak::Atom(atom) => atom.bytes.len(),
            AtomOrBreak::Break => 0,
        })
        .sum()
}

/// Emits a GFM table: header row, delimiter row, data rows. The facade's LF ends the final line.
fn emit_gfm(laid_out: &LaidOut) -> String {
    let mut out = String::new();
    out.push_str(&gfm_row(&laid_out.header));
    out.push('\n');
    // Delimiter row reflects each column's alignment.
    let mut delimiter = String::from("| ");
    for (index, column) in laid_out.columns.iter().enumerate() {
        if index != 0 {
            delimiter.push_str(" | ");
        }
        delimiter.push_str(if column.right { "---:" } else { ":---" });
    }
    delimiter.push_str(" |");
    out.push_str(&delimiter);
    out.push('\n');
    for row in &laid_out.rows {
        out.push_str(&gfm_row(row));
        out.push('\n');
    }
    out.pop();
    out
}

/// One GFM row: `| ` plus wrapped cells joined by ` | ` plus ` |`.
fn gfm_row(cells: &[CellLines]) -> String {
    let mut out = String::from("| ");
    for (index, cell) in cells.iter().enumerate() {
        if index != 0 {
            out.push_str(" | ");
        }
        out.push_str(&cell.lines.join("<br />"));
    }
    out.push_str(" |");
    out
}

/// Emits an HTML table fragment. Ends in `</table>`; the facade appends the final LF.
fn emit_html(laid_out: &LaidOut, header: HeaderPolicy) -> String {
    let mut out = String::new();
    out.push_str("<table>\n");
    if header == HeaderPolicy::Present {
        out.push_str("<thead>\n<tr>");
        for (index, cell) in laid_out.header.iter().enumerate() {
            write!(
                out,
                "<th scope=\"col\" {style}>{text}</th>",
                style = cell_style(&laid_out.columns, index),
                text = cell.lines.join("<br />"),
            )
            .expect("String writes are infallible");
        }
        out.push_str("</tr>\n</thead>\n");
    }
    out.push_str("<tbody>\n");
    for row in &laid_out.rows {
        out.push_str("<tr>");
        for (index, cell) in row.iter().enumerate() {
            write!(
                out,
                "<td {style}>{text}</td>",
                style = cell_style(&laid_out.columns, index),
                text = cell.lines.join("<br />"),
            )
            .expect("String writes are infallible");
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</tbody>\n</table>");
    out
}

fn cell_style(columns: &[Column], index: usize) -> String {
    let align = if columns.get(index).is_some_and(|column| column.right) {
        "right"
    } else {
        "left"
    };
    alloc::format!("style=\"text-align: {align}; white-space: pre-wrap\"")
}

/// Emits an ASCII grid table. Each border/row is a physical line; the facade appends the final LF.
fn emit_grid(laid_out: &LaidOut, header: HeaderPolicy) -> String {
    let mut out = String::new();
    let mut lines: Vec<String> = Vec::new();
    push_grid_border(&mut lines, &laid_out.columns);
    if header == HeaderPolicy::Present {
        push_grid_row(&mut lines, &laid_out.header, &laid_out.columns);
        push_grid_border(&mut lines, &laid_out.columns);
    }
    for row in &laid_out.rows {
        push_grid_row(&mut lines, row, &laid_out.columns);
        push_grid_border(&mut lines, &laid_out.columns);
    }
    out.push_str(&lines.join("\n"));
    out
}

/// Appends one grid border line: `+` plus `w_i + 2` dashes per column plus `+`.
fn push_grid_border(lines: &mut Vec<String>, columns: &[Column]) {
    let mut border = String::from("+");
    for column in columns {
        border.push_str(&"-".repeat(column.width.saturating_add(2)));
        border.push('+');
    }
    lines.push(border);
}

/// Appends one grid physical row, padding every cell to its column width.
///
/// A logical row uses the maximum wrapped-line count of its cells; exhausted cells supply empty padded segments.
fn push_grid_row(lines: &mut Vec<String>, cells: &[CellLines], columns: &[Column]) {
    let height = cells.iter().map(|cell| cell.lines.len()).max().unwrap_or(1);
    for line_index in 0..height {
        let mut row = String::from("| ");
        for (cell_index, cell) in cells.iter().enumerate() {
            if cell_index != 0 {
                row.push_str(" | ");
            }
            let column = &columns[cell_index];
            let text = cell.lines.get(line_index).map_or("", String::as_str);
            let text_width = cell.widths.get(line_index).copied().unwrap_or(0);
            let padding = column.width.saturating_sub(text_width);
            if column.right {
                row.push_str(&" ".repeat(padding));
            }
            row.push_str(text);
            if !column.right {
                row.push_str(&" ".repeat(padding));
            }
        }
        row.push_str(" |");
        lines.push(row);
    }
}

#[cfg(test)]
mod tests {
    use super::extract;

    #[test]
    fn a_row_shorter_than_a_later_row_still_aligns() {
        // The load-bearing row is the FIRST one: its object lacks `k1`, a member a LATER element introduces, so if any
        // row were built before the header union completed it would be short and panic the column layout with an index
        // out of bounds. The extraction collects the full union BEFORE any row is built, and every row aligns to it
        // with `null` for its missing members.
        let value = jqf_data::Value::Array(
            jqf_data::Array::try_from_vec(alloc::vec![
                jqf_data::Value::Object({
                    let mut b = jqf_data::ObjectBuilder::try_with_capacity(1).expect("b");
                    b.try_insert_last(
                        jqf_data::ObjectKey::try_from_str("k0").expect("k"),
                        jqf_data::Value::Bool(true),
                    )
                    .expect("i");
                    b.try_finish().expect("o")
                }),
                jqf_data::Value::Object({
                    let mut b = jqf_data::ObjectBuilder::try_with_capacity(2).expect("b");
                    b.try_insert_last(
                        jqf_data::ObjectKey::try_from_str("k0").expect("k"),
                        jqf_data::Value::Bool(false),
                    )
                    .expect("i");
                    b.try_insert_last(
                        jqf_data::ObjectKey::try_from_str("k1").expect("k"),
                        jqf_data::Value::Null,
                    )
                    .expect("i");
                    b.try_finish().expect("o")
                }),
            ])
            .expect("a"),
        );
        let table = extract(&value).expect("extract");
        assert_eq!(table.headers, alloc::vec!["k0", "k1"]);
        assert_eq!(table.rows.len(), 2);
        // The FIRST row's missing k1 is `null`, not an absent cell.
        assert_eq!(table.rows[0].len(), 2);
    }

    #[test]
    fn interleaved_members_place_values_in_their_union_columns() {
        // The keyed-column extraction must land each value in the column its member identity holds in the
        // FIRST-APPEARANCE union, with `null` filling every member an element lacks — independent of the order the
        // members appear in any one element.
        let object = |keys: &[(&str, jqf_data::Value)]| {
            let mut b = jqf_data::ObjectBuilder::try_with_capacity(keys.len()).expect("b");
            for (key, value) in keys {
                b.try_insert_last(jqf_data::ObjectKey::try_from_str(key).expect("k"), value.clone())
                    .expect("i");
            }
            b.try_finish().expect("o")
        };
        let value = jqf_data::Value::Array(
            jqf_data::Array::try_from_vec(alloc::vec![
                jqf_data::Value::Object(object(&[("k0", jqf_data::Value::Bool(true))])),
                jqf_data::Value::Object(object(&[
                    ("k2", jqf_data::Value::Bool(false)),
                    ("k0", jqf_data::Value::Null),
                ])),
                jqf_data::Value::Object(object(&[("k1", jqf_data::Value::Bool(true))])),
            ])
            .expect("a"),
        );
        let table = extract(&value).expect("extract");
        assert_eq!(table.headers, alloc::vec!["k0", "k2", "k1"]);
        assert_eq!(table.rows.len(), 3);
        assert!(matches!(table.rows[0][0], jqf_data::Value::Bool(true)));
        assert!(matches!(table.rows[0][1], jqf_data::Value::Null));
        assert!(matches!(table.rows[0][2], jqf_data::Value::Null));
        assert!(matches!(table.rows[1][0], jqf_data::Value::Null));
        assert!(matches!(table.rows[1][1], jqf_data::Value::Bool(false)));
        assert!(matches!(table.rows[1][2], jqf_data::Value::Null));
        assert!(matches!(table.rows[2][0], jqf_data::Value::Null));
        assert!(matches!(table.rows[2][1], jqf_data::Value::Null));
        assert!(matches!(table.rows[2][2], jqf_data::Value::Bool(true)));
    }
}
