//! Draws a document and applies keyboard and pointer input to it.

use std::ops::Range;

use super::symbols::Reference;
use super::{
    clipboard,
    document::Document,
    wrap::{self, Row},
};
use crate::{
    renderer::{CellRect, Graphic},
    terminal::{
        InputEvent, Style,
        input::{Key, KeyEvent, Modifiers, MouseButton, NamedKey, PointerKind, TextSource},
    },
    ui::{Canvas, Cursor, controls::Scroll},
};

/// The platform's primary modifier, as labeled in shortcuts.
pub fn primary_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd"
    } else {
        "Ctrl"
    }
}
pub fn primary(key: &KeyEvent) -> bool {
    primary_held(key.modifiers)
}
/// Whether the platform's primary modifier is among those held.
fn primary_held(modifiers: Modifiers) -> bool {
    modifiers.super_key || (modifiers.control && !modifiers.alt_graph)
}
pub fn letter(key: &KeyEvent, ch: &str) -> bool {
    matches!(key.key_without_modifiers.as_ref().unwrap_or(&key.logical_key), Key::Character(c) if c.eq_ignore_ascii_case(ch))
}

#[derive(Default)]
pub struct EditorView {
    vertical: Scroll,
    horizontal_fraction: f64,
    wrapped_fraction: f64,
    size: [u32; 2],
    /// A symbol that was Cmd+clicked, for the editor to look up.
    definition_request: Option<Reference>,
    /// The symbol under a Cmd+press still held. Releasing on it follows it, and until then
    /// moving is not a drag.
    pressed_symbol: Option<Range<usize>>,
    highlighting: super::highlight::Highlighting,
}
impl EditorView {
    /// Take the symbol whose definition was asked for by a Cmd+click.
    pub fn take_definition_request(&mut self) -> Option<Reference> {
        self.definition_request.take()
    }
    pub fn highlight_pending(&self) -> bool {
        self.highlighting.pending()
    }
    /// Apply this frame's input to the document, then draw its visible rows.
    /// Returns any error from an editing command.
    pub fn draw(
        &mut self,
        canvas: &mut Canvas<'_>,
        doc: &mut Document,
        word_wrap: bool,
        matches: &[Range<usize>],
    ) -> Option<String> {
        let [width, height] = canvas.size();
        let gutter = (doc.line_count().to_string().len() as u32 + 2).min(width.saturating_sub(2));
        let content_width = width.saturating_sub(gutter + 1) as usize;
        doc.wrap_width = word_wrap.then_some(content_width.max(1));
        if self.size != [width, height] {
            doc.reveal = true;
            self.size = [width, height];
        }
        self.vertical.offset = doc.scroll[1];
        self.vertical.maximum = doc.line_count().saturating_sub(height as usize);
        let error = self.handle_events(canvas, doc, gutter);
        let insertion = if canvas.preedit.is_some() {
            doc.selection().start
        } else {
            doc.cursor
        };
        let [column, line] = [doc.column_at(insertion), doc.row_at(insertion)];
        self.settle_scroll(canvas, doc, [column, line], content_width);
        let rows = screen_rows(doc, height as usize);
        let first_line = rows.first().map_or(doc.scroll[1], |row| row.line);
        let last_line = rows.last().map_or(first_line, |row| row.line);
        self.highlighting.update(doc, first_line..last_line + 1);
        let theme = canvas.theme;
        canvas.fill(
            CellRect::new(0, 0, width, height),
            Style::new(theme.text, theme.panel),
        );
        for (screen_row, &row) in rows.iter().enumerate() {
            let current = row.line == line;
            self.draw_line(
                canvas,
                doc,
                screen_row,
                row,
                (current, matches),
                [gutter as usize, content_width],
            );
        }
        let caret_row = match doc.wrap_width {
            Some(wrap_width) => wrap::containing(doc, line, column, wrap_width),
            None => Row {
                line,
                start: doc.scroll[0],
            },
        };
        let screen_row = rows.iter().position(|&row| row == caret_row);
        let x = match doc.wrap_width {
            // A full last row has no cell for the column after it, so the caret waits on the
            // row's final cell.
            Some(_) => Some((column - caret_row.start).min(content_width.saturating_sub(1))),
            None => column
                .checked_sub(caret_row.start)
                .filter(|&x| x < content_width),
        };
        if let (Some(y), Some(x), true) = (
            screen_row,
            x,
            canvas.response.focused && canvas.window_focused,
        ) {
            draw_caret(canvas, doc, insertion, [gutter + x as u32, y as u32]);
        }
        self.vertical.offset = doc.scroll[1].min(self.vertical.maximum);
        self.vertical.draw(canvas, doc.line_count());
        draw_link_hint(canvas, doc, gutter);
        error
    }

    fn handle_events(
        &mut self,
        canvas: &mut Canvas<'_>,
        doc: &mut Document,
        gutter: u32,
    ) -> Option<String> {
        let [width, height] = canvas.size();
        let mut error = None;
        for event in std::mem::take(&mut canvas.events) {
            if let (InputEvent::Scroll(scroll), Some(wrap_width)) = (&event, doc.wrap_width) {
                self.wrapped_fraction -= scroll.cells[1];
                let steps = self.wrapped_fraction.trunc() as isize;
                self.wrapped_fraction -= steps as f64;
                scroll_rows(doc, steps, wrap_width, height as usize);
                doc.reveal = false;
                continue;
            }
            if let InputEvent::Scroll(scroll) = &event {
                self.horizontal_fraction -= scroll.cells[0];
                let steps = self.horizontal_fraction.trunc() as isize;
                self.horizontal_fraction -= steps as f64;
                doc.scroll[0] = doc.scroll[0].saturating_add_signed(steps);
                doc.reveal = false;
            }
            if self.vertical.event(&event, canvas, doc.line_count()) {
                // The scrollbar counts lines, so with word wrap it lands on a line's first row.
                if doc.wrap_width.is_some() && doc.scroll[1] != self.vertical.offset {
                    doc.scroll[0] = 0;
                }
                doc.scroll[1] = self.vertical.offset;
                doc.reveal = false;
                continue;
            }
            match event {
                InputEvent::Text(text) => {
                    // Named Enter/Tab are handled as editing commands; keyboard
                    // text contains printable scalars only. IME/paste may be multiline.
                    if text.source == TextSource::Keyboard {
                        let value: String = text.text.chars().filter(|c| !c.is_control()).collect();
                        if !value.is_empty() {
                            doc.type_text(&value);
                        }
                    } else if !text.text.is_empty() {
                        doc.insert(&text.text);
                    }
                }
                InputEvent::Pointer(p) if matches!(p.kind, PointerKind::Click { button: MouseButton::Left, count } if count >= 2) => {
                    if let Some(point) = p.position.map(|p| canvas.local(p.grid)) {
                        let (offset, row) = offset_at_point(doc, point, gutter, height);
                        if matches!(p.kind, PointerKind::Click { count: 2, .. }) {
                            doc.select_word(offset);
                        } else {
                            doc.set_cursor(doc.line_start(row), false);
                            doc.set_cursor(
                                doc.line_end(row).saturating_add(1).min(doc.text.len()),
                                true,
                            );
                        }
                    }
                }
                InputEvent::Key(key) => {
                    if let Err(e) = edit_key(doc, &key, height as usize) {
                        error = Some(e.to_string());
                    }
                }
                // Followed on release, as the jump must not happen with the button still down:
                // what opens would take any movement before the release for a drag.
                InputEvent::Pointer(p) if p.kind == PointerKind::Up(MouseButton::Left) => {
                    let pressed = self.pressed_symbol.take();
                    if let (Some(word), Some(position)) = (pressed, p.position) {
                        let point = canvas.local(position.grid);
                        let (offset, _) = offset_at_point(doc, point, gutter, height);
                        // Moving off the symbol first cancels, as with any button.
                        if word.contains(&offset) {
                            let before = doc.text_before(word.start);
                            let name = doc.text.read(word.clone());
                            self.definition_request =
                                Some(Reference::new(name, &before).at(word.start));
                        }
                    }
                }
                InputEvent::Pointer(p)
                    if matches!(
                        p.kind,
                        PointerKind::Down(MouseButton::Left) | PointerKind::Move
                    ) =>
                {
                    if let Some(position) = p.position {
                        let point = canvas.local(position.grid);
                        let dragging = matches!(p.kind, PointerKind::Move);
                        if dragging && (!canvas.response.active || self.pressed_symbol.is_some()) {
                            continue;
                        }
                        self.pressed_symbol = None;
                        if !dragging && primary_held(p.modifiers) {
                            let (offset, _) = offset_at_point(doc, point, gutter, height);
                            if let Some(word) = doc.identifier_at(offset) {
                                self.pressed_symbol = Some(word);
                                continue;
                            }
                        }
                        if let Some(wrap_width) = doc.wrap_width {
                            if dragging && point[1] < 0.0 {
                                scroll_rows(doc, -1, wrap_width, height as usize);
                            }
                            if dragging && point[1] >= f64::from(height) {
                                scroll_rows(doc, 1, wrap_width, height as usize);
                            }
                        } else {
                            if dragging && point[1] < 0.0 {
                                doc.scroll[1] = doc.scroll[1].saturating_sub(1);
                            }
                            if dragging && point[1] >= f64::from(height) {
                                doc.scroll[1] = (doc.scroll[1] + 1).min(self.vertical.maximum);
                            }
                            if dragging && point[0] < f64::from(gutter) {
                                doc.scroll[0] = doc.scroll[0].saturating_sub(1);
                            }
                            if dragging && point[0] >= f64::from(width.saturating_sub(1)) {
                                doc.scroll[0] += 1;
                            }
                        }
                        let (offset, _) = offset_at_point(doc, point, gutter, height);
                        doc.set_cursor(offset, dragging || p.modifiers.shift);
                    }
                }
                _ => {}
            }
        }
        error
    }

    /// Bring the caret into view if it moved, then clamp the scroll position.
    fn settle_scroll(
        &mut self,
        canvas: &Canvas<'_>,
        doc: &mut Document,
        [column, row]: [usize; 2],
        content_width: usize,
    ) {
        let height = canvas.size()[1] as usize;
        if let Some(wrap_width) = doc.wrap_width {
            let reveal = doc.reveal || canvas.preedit.is_some();
            settle_wrapped_scroll(doc, [column, row], wrap_width, height, reveal);
            doc.reveal = false;
            return;
        }
        if doc.reveal || canvas.preedit.is_some() {
            self.vertical.offset = doc.scroll[1];
            self.vertical.reveal(row, height);
            doc.scroll[1] = self.vertical.offset;
            if column < doc.scroll[0] {
                doc.scroll[0] = column;
            }
            if column >= doc.scroll[0] + content_width.max(1) {
                doc.scroll[0] = column.saturating_sub(content_width.saturating_sub(1));
            }
            doc.reveal = false;
        }
        doc.scroll[1] = doc.scroll[1].min(self.vertical.maximum);
        // Clamp horizontal scrolling to the visible lines (and caret line).
        if doc.scroll[0] > 0 {
            let max_column = (doc.scroll[1]..(doc.scroll[1] + height).min(doc.line_count()))
                .map(|r| doc.line_columns(r))
                .max()
                .unwrap_or(0)
                .max(column);
            doc.scroll[0] =
                doc.scroll[0].min(max_column.saturating_sub(content_width.saturating_sub(1)));
        }
        self.vertical.offset = doc.scroll[1];
    }

    /// Draw one row of a document line: its number, background and colored text runs.
    fn draw_line(
        &self,
        canvas: &mut Canvas<'_>,
        doc: &Document,
        screen_row: usize,
        row: Row,
        (current, matches): (bool, &[Range<usize>]),
        [gutter, content_width]: [usize; 2],
    ) {
        let theme = canvas.theme;
        let Row {
            line,
            start: scroll,
        } = row;
        let gutter = gutter as u32;
        let background = if current { theme.control } else { theme.panel };
        canvas.fill(
            CellRect::new(gutter, screen_row as u32, content_width as u32, 1),
            Style::new(theme.text, background),
        );
        // A wrapped row shows only the text up to where the next row takes over.
        let content_width = match doc.wrap_width {
            Some(wrap_width) => {
                let end = wrap::end(doc, row, wrap_width);
                // The last row also has the cell after its text, to show a selected ending.
                let ending = usize::from(end == doc.line_columns(line));
                (end - scroll + ending).min(content_width)
            }
            None => content_width,
        };
        let number_width = gutter.saturating_sub(1) as usize;
        let number_color = if current { theme.accent } else { theme.muted };
        // Only the first row of a wrapped line carries its number.
        if doc.wrap_width.is_none() || scroll == 0 {
            canvas.text(
                0,
                screen_row as i32,
                &format!("{:>number_width$} ", line + 1),
                Style::new(number_color, theme.panel),
            );
        }
        // Submit text runs rather than allocating two Strings and a paint
        // command for every cell. Selection boundaries split the runs.
        let selection = doc.selection();
        let line_start = doc.line_start(line);
        let mut next_match = matches.partition_point(|found| found.end <= line_start);
        let mut run = String::with_capacity(content_width);
        let mut run_start = 0;
        let mut run_style = Style::new(theme.text, background);
        let mut base_shown = false;
        for (byte, x, cells, ch) in doc.visible_line(line, scroll, content_width) {
            // A combining scalar joins the text of the character it modifies.
            if cells == 0 {
                if base_shown && !run.is_empty() {
                    run.push(ch);
                }
                continue;
            }
            while matches
                .get(next_match)
                .is_some_and(|found| found.end <= byte)
            {
                next_match += 1;
            }
            let found = matches
                .get(next_match)
                .filter(|found| found.contains(&byte));
            let foreground = self.highlighting.color(byte, theme.text);
            let cell_style = if found == Some(&selection) {
                Style::new(theme.desktop, theme.accent)
            } else if found.is_some() || selection.contains(&byte) {
                Style::new(foreground, theme.hover)
            } else {
                Style::new(foreground, background)
            };
            let wide = ch != '\t' && cells > 1;
            let whole = x >= scroll && x + cells <= scroll + content_width;
            base_shown = !ch.is_control() && (!wide || whole);
            for dx in 0..cells {
                // The text run advances over a whole wide character itself.
                if (wide && whole && dx > 0)
                    || !(scroll..scroll + content_width).contains(&(x + dx))
                {
                    continue;
                }
                let display = if ch == '\t' || (wide && !whole) {
                    ' '
                } else if ch.is_control() {
                    '·'
                } else {
                    ch
                };
                if !run.is_empty() && cell_style != run_style {
                    canvas.text(run_start, screen_row as i32, &run, run_style);
                    run.clear();
                }
                if run.is_empty() {
                    run_start = gutter as i32 + (x + dx - scroll) as i32;
                    run_style = cell_style;
                }
                run.push(display);
            }
        }
        if !run.is_empty() {
            canvas.text(run_start, screen_row as i32, &run, run_style);
        }
    }
}

/// While Cmd is held over a symbol, show that clicking it follows it: underline it and
/// change the cursor. On a wrapped line this covers the part on the row under the pointer.
fn draw_link_hint(canvas: &mut Canvas<'_>, doc: &Document, gutter: u32) {
    let [width, height] = canvas.size();
    let Some(point) = canvas.pointer().filter(|_| primary_held(canvas.modifiers)) else {
        return;
    };
    let in_text = point[0] >= f64::from(gutter) && point[0] < f64::from(width.saturating_sub(1));
    if !in_text || point[1] < 0.0 {
        return;
    }
    let (offset, line) = offset_at_point(doc, point, gutter, height);
    let Some(word) = doc.identifier_at(offset) else {
        return;
    };
    let y = point[1] as usize;
    let Some(row) = screen_rows(doc, y + 1).pop().filter(|row| row.line == line) else {
        return;
    };
    let start = doc.column_at(word.start).max(row.start) - row.start;
    let end = doc.column_at(word.end).saturating_sub(row.start);
    let content = width.saturating_sub(gutter + 1) as usize;
    let end = end.min(content);
    if start >= end {
        return;
    }
    canvas.set_cursor(Cursor::Pointer);
    let color = canvas.theme.accent;
    canvas.graphic(
        CellRect::new(gutter + start as u32, y as u32, (end - start) as u32, 1),
        Graphic::Divider {
            vertical: false,
            color,
        },
    );
}

/// The rows on screen, top to bottom, for at most `height` rows.
fn screen_rows(doc: &Document, height: usize) -> Vec<Row> {
    let top = Row {
        line: doc.scroll[1],
        start: doc.scroll[0],
    };
    match doc.wrap_width {
        Some(width) => std::iter::successors(Some(top), |&row| wrap::next(doc, row, width))
            .take(height)
            .collect(),
        None => (top.line..(top.line + height).min(doc.line_count()))
            .map(|line| Row {
                line,
                start: top.start,
            })
            .collect(),
    }
}

/// The offset under a point in local cells, and the line it is on.
fn offset_at_point(doc: &Document, point: [f64; 2], gutter: u32, height: u32) -> (usize, usize) {
    let y = point[1].clamp(0.0, f64::from(height.saturating_sub(1))) as usize;
    let x = (point[0] - f64::from(gutter)).max(0.0) as usize;
    let rows = screen_rows(doc, y + 1);
    let last = doc.line_count() - 1;
    let row = rows.last().copied().unwrap_or(Row {
        line: last,
        start: doc.scroll[0],
    });
    let column = row.start + x;
    match doc.wrap_width {
        Some(width) => (wrap::offset_on_row(doc, row, column, width), row.line),
        None => {
            let line = (doc.scroll[1] + y).min(last);
            (doc.offset(line, column), line)
        }
    }
}

/// Move the top of the screen by whole rows, stopping with the last row at the bottom.
fn scroll_rows(doc: &mut Document, steps: isize, width: usize, height: usize) {
    let mut top = Row {
        line: doc.scroll[1],
        start: doc.scroll[0],
    };
    for _ in 0..steps.unsigned_abs() {
        let step = if steps < 0 {
            wrap::previous(doc, top, width)
        } else {
            wrap::next(doc, top, width)
        };
        let Some(step) = step else {
            break;
        };
        top = step;
    }
    doc.scroll = [top.start, top.line];
    fill_screen(doc, width, height);
}

/// Scroll back just enough that the last row is not above the bottom of the screen.
fn fill_screen(doc: &mut Document, width: usize, height: usize) {
    let shown = screen_rows(doc, height).len();
    let mut top = Row {
        line: doc.scroll[1],
        start: doc.scroll[0],
    };
    for _ in shown..height {
        let Some(step) = wrap::previous(doc, top, width) else {
            break;
        };
        top = step;
    }
    doc.scroll = [top.start, top.line];
}

/// Line the top row up with the current width, and bring the caret into view if it moved.
fn settle_wrapped_scroll(
    doc: &mut Document,
    [column, line]: [usize; 2],
    width: usize,
    height: usize,
    reveal: bool,
) {
    let line_at_top = doc.scroll[1].min(doc.line_count() - 1);
    let mut top = wrap::containing(doc, line_at_top, doc.scroll[0], width);
    doc.scroll = [top.start, top.line];
    if reveal {
        let caret = wrap::containing(doc, line, column, width);
        if caret < top {
            top = caret;
        } else if !screen_rows(doc, height.max(1)).contains(&caret) {
            top = caret;
            for _ in 1..height {
                let Some(step) = wrap::previous(doc, top, width) else {
                    break;
                };
                top = step;
            }
        }
        doc.scroll = [top.start, top.line];
    }
    fill_screen(doc, width, height);
}

/// Draw the block caret at a cell, showing any IME composition in progress.
fn draw_caret(canvas: &mut Canvas<'_>, doc: &Document, insertion: usize, [x, y]: [u32; 2]) {
    let theme = canvas.theme;
    let [width, _] = canvas.size();
    let mut caret_x = x;
    let mut character = doc.cluster_at(insertion);
    if character.chars().any(char::is_control) || character.is_empty() {
        character = " ".into();
    }
    if let Some(preedit) = canvas.preedit.clone() {
        let shown: String = preedit
            .text
            .chars()
            .filter(|c| !c.is_control())
            .take(width.saturating_sub(x + 1) as usize)
            .collect();
        canvas.text(
            x as i32,
            y as i32,
            &shown,
            Style::new(theme.accent, theme.control),
        );
        if let Some((start, _)) = preedit.cursor {
            let n = preedit.text.get(..start).unwrap_or("").chars().count() as u32;
            caret_x = (x + n).min(width.saturating_sub(2));
            character = shown.chars().nth(n as usize).unwrap_or(' ').to_string();
        }
    }
    canvas.caret(caret_x, y);
    if canvas.preedit.as_ref().is_none_or(|p| p.cursor.is_some()) {
        canvas.text(
            caret_x as i32,
            y as i32,
            &character,
            Style::new(theme.desktop, theme.accent),
        );
    }
}

pub(super) fn edit_key(doc: &mut Document, key: &KeyEvent, height: usize) -> std::io::Result<()> {
    let extend = key.modifiers.shift;
    if primary(key) {
        if letter(key, "a") {
            doc.select_all();
            return Ok(());
        }
        if letter(key, "z") {
            if extend {
                doc.redo();
            } else {
                doc.undo();
            }
            return Ok(());
        }
        if letter(key, "y") {
            doc.redo();
            return Ok(());
        }
        if letter(key, "c") || letter(key, "x") {
            if !doc.selection().is_empty() {
                clipboard::write_with(|out| doc.write_selection(out))?;
                if letter(key, "x") {
                    doc.insert("");
                }
            }
            return Ok(());
        }
        if letter(key, "v") {
            doc.insert(&clipboard::read()?);
            return Ok(());
        }
    }
    let word = key.modifiers.alt || (key.modifiers.control && !key.modifiers.alt_graph);
    match key.logical_key {
        Key::Named(NamedKey::Enter) => doc.newline(),
        Key::Named(NamedKey::Tab) => doc.indent(extend)?,
        Key::Named(NamedKey::Backspace) => doc.delete(true, word),
        Key::Named(NamedKey::Delete) => doc.delete(false, word),
        Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight) => {
            let right = key.logical_key == Key::Named(NamedKey::ArrowRight);
            if key.modifiers.super_key {
                doc.set_cursor(if right { doc.end() } else { doc.home() }, extend);
            } else {
                doc.move_horizontal(right, word, extend);
            }
        }
        Key::Named(NamedKey::ArrowUp | NamedKey::ArrowDown) => {
            let down = key.logical_key == Key::Named(NamedKey::ArrowDown);
            if key.modifiers.super_key {
                doc.set_cursor(if down { doc.text.len() } else { 0 }, extend);
            } else {
                doc.move_vertical(if down { 1 } else { -1 }, extend);
            }
        }
        Key::Named(NamedKey::PageUp) => doc.move_vertical(-(height.max(1) as isize), extend),
        Key::Named(NamedKey::PageDown) => doc.move_vertical(height.max(1) as isize, extend),
        Key::Named(NamedKey::Home) => {
            doc.set_cursor(if primary(key) { 0 } else { doc.home() }, extend)
        }
        Key::Named(NamedKey::End) => doc.set_cursor(
            if primary(key) {
                doc.text.len()
            } else {
                doc.end()
            },
            extend,
        ),
        Key::Named(NamedKey::Escape) => doc.set_cursor(doc.cursor, false),
        _ => {}
    }
    Ok(())
}
