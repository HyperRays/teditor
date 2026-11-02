//! Word wrap: how one document line divides into rows on screen.
//!
//! A row is identified by its line and the column it starts at, computed on demand from the
//! rows next to it. Nothing is stored, and the work is proportional to what is on screen, so
//! wrapping costs the same however large the file is.

use super::document::Document;

/// Lines wider than this break at exact multiples of the width rather than between words,
/// because finding word boundaries takes a scan from the start of the line. Any position in
/// a gigantic line can then be found directly.
const WORD_WRAP_COLUMNS: usize = 64 * 1024;

/// One row on screen: part of a line, starting at a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Row {
    pub line: usize,
    pub start: usize,
}

/// Where the row after `start` begins, or `None` if the rest of the line fits.
fn next_start(doc: &Document, line: usize, start: usize, width: usize) -> Option<usize> {
    let limit = start + width;
    let columns = doc.line_columns(line);
    if columns <= limit {
        return None;
    }
    if columns > WORD_WRAP_COLUMNS {
        return Some(limit);
    }
    let mut after_space = None;
    for (_, x, cells, ch) in doc.visible_line(line, start, width + 1) {
        if cells == 0 {
            continue;
        }
        if x + cells > limit {
            // Break after the last space, or else mid-word: a character wider than the
            // row still has to go somewhere, so it takes a row by itself.
            let split = if x > start { x } else { x + cells };
            return Some(after_space.unwrap_or(split));
        }
        if ch.is_whitespace() {
            after_space = Some(x + cells);
        }
    }
    Some(limit)
}

/// The row after this one, on the next line if this is the last of its line.
pub(super) fn next(doc: &Document, row: Row, width: usize) -> Option<Row> {
    match next_start(doc, row.line, row.start, width) {
        Some(start) => Some(Row { start, ..row }),
        None => (row.line + 1 < doc.line_count()).then_some(Row {
            line: row.line + 1,
            start: 0,
        }),
    }
}

/// The row before this one, on the previous line if this is the first of its line.
pub(super) fn previous(doc: &Document, row: Row, width: usize) -> Option<Row> {
    if row.start > 0 {
        return Some(containing(doc, row.line, row.start - 1, width));
    }
    let line = row.line.checked_sub(1)?;
    Some(containing(doc, line, doc.line_columns(line), width))
}

/// The row of `line` that shows `column`.
pub(super) fn containing(doc: &Document, line: usize, column: usize, width: usize) -> Row {
    if doc.line_columns(line) > WORD_WRAP_COLUMNS {
        let last = doc.line_columns(line).saturating_sub(1);
        return Row {
            line,
            start: column.min(last) / width * width,
        };
    }
    let mut start = 0;
    while let Some(next) = next_start(doc, line, start, width).filter(|&next| next <= column) {
        start = next;
    }
    Row { line, start }
}

/// The column just past the end of a row.
pub(super) fn end(doc: &Document, row: Row, width: usize) -> usize {
    next_start(doc, row.line, row.start, width).unwrap_or_else(|| doc.line_columns(row.line))
}

/// The offset of a column on a row. Past a row's end that is its last character, as the
/// column there belongs to the next row; the last row of a line can reach the line's end.
pub(super) fn offset_on_row(doc: &Document, row: Row, column: usize, width: usize) -> usize {
    let last = next_start(doc, row.line, row.start, width)
        .map_or(column, |next| column.min(next.saturating_sub(1)));
    doc.offset(row.line, last)
}
