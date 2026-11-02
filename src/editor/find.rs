//! Find and replace within the active document, in a bar at the bottom of its pane.
//!
//! Searching runs on a background thread over a shared copy of the text, so that typing never
//! waits for it, and starts over when the query, its options or the text change.

use super::{
    Editor, Focus,
    buffer::TextBuffer,
    document::{Document, columns},
    search::{self, Options},
    view::edit_key,
};
use crate::{
    renderer::{CellRect, Color, Graphic},
    terminal::{
        InputEvent, Style,
        input::{Key, MouseButton, NamedKey, PointerKind},
    },
    ui::{
        Canvas, Cursor, Ui,
        controls::{Clicks, ICON_COLUMNS, Icon, IconButton, Tone, draw_icons, icons_width},
    },
};
use grep_regex::RegexMatcher;
use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
};
use unicode_segmentation::UnicodeSegmentation;

/// Most match positions kept. Each costs a few bytes, so this stays small however many a file
/// has; those past it are not highlighted or stepped through, and Replace All still finds them.
const MAX_MATCHES: usize = 100_000;
/// Matches handed over at once, so that results appear as a long search goes on.
const BATCH: usize = 1024;
/// Cells kept for the summary or a button's name, so that the input stays still as they change.
const STATUS_CELLS: u32 = "Replace all matches".len() as u32;

/// Where an input's text starts: after its icon, and a cell so that the two do not touch.
const TEXT_X: u32 = ICON_COLUMNS + 1;

// Icons from the renderer's bundled symbol font, where the main font has nothing as clear.
const ICON_SEARCH: Icon<'_> = Icon::Wide('\u{EC0D}');
const ICON_WORD: Icon<'_> = Icon::Wide('\u{eb7e}');

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Query,
    Replacement,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Button {
    Case,
    Word,
    Regex,
    Previous,
    Next,
    Close,
    Replace,
    ReplaceAll,
}

/// What the bar asked for this frame, done once the frame has been drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FindAction {
    Next,
    Previous,
    Replace,
    ReplaceAll,
    Close,
}

/// The bar's inputs and options.
pub(super) struct FindBar {
    pub(super) query: Document,
    pub(super) replacement: Document,
    pub(super) options: Options,
    replacing: bool,
    focus_request: Option<Field>,
    query_buttons: Clicks<Button>,
    replace_buttons: Clicks<Button>,
    /// Where the top edge goes and its color, known once the bar has been laid out.
    edge: Option<(CellRect, Color)>,
}

impl FindBar {
    fn new() -> Self {
        Self {
            query: Document::new(),
            replacement: Document::new(),
            options: Options::default(),
            replacing: false,
            focus_request: None,
            query_buttons: Clicks::default(),
            replace_buttons: Clicks::default(),
            edge: None,
        }
    }

    /// Rows the bar takes from the document.
    pub fn rows(&self) -> u32 {
        if self.replacing { 2 } else { 1 }
    }

    /// Draw the bar, returning what it asked for. `failed` says the summary is bad news.
    pub fn draw(
        &mut self,
        ui: &mut Ui<'_>,
        status: &str,
        failed: bool,
        take_focus: bool,
    ) -> Vec<FindAction> {
        let mut actions = Vec::new();
        let request = self.focus_request.take();
        let focus = |field| {
            request == Some(field) || (take_focus && request.is_none() && field == Field::Query)
        };
        let mut canvas = ui.canvas("query", 1, true);
        if focus(Field::Query) {
            canvas.request_focus();
        }
        let row = canvas.response.rect;
        let theme = canvas.theme;
        let mut focused = canvas.response.focused && canvas.window_focused;
        let input = self.draw_query_row(&mut canvas, status, failed, &mut actions);
        if self.replacing {
            // Directly below: stacked canvases would have a row between them.
            let below = CellRect::new(row.column, row.row + 1, row.columns, 1);
            let mut canvas = ui.canvas_at("replacement", below, true);
            if focus(Field::Replacement) {
                canvas.request_focus();
            }
            focused |= canvas.response.focused && canvas.window_focused;
            self.draw_replacement_row(&mut canvas, input, &mut actions);
        }
        self.edge = row.row.checked_sub(1).map(|above| {
            let color = if focused {
                theme.active_border
            } else {
                theme.border
            };
            (CellRect::new(row.column, above, row.columns, 1), color)
        });
        actions
    }

    /// Draw the bar's top edge, lit like a focused pane's. It is the bottom edge of the row
    /// above, which is outside the bar, so this takes the `ui` around it.
    pub fn draw_edge(&self, ui: &mut Ui<'_>) {
        if let Some((rect, color)) = self.edge {
            let vertical = false;
            ui.graphic_at(rect, Graphic::Divider { vertical, color });
        }
    }

    /// Returns the input's width, which the replacement's lines up with.
    fn draw_query_row(
        &mut self,
        canvas: &mut Canvas<'_>,
        status: &str,
        failed: bool,
        actions: &mut Vec<FindAction>,
    ) -> u32 {
        let [width, _] = canvas.size();
        let theme = canvas.theme;
        canvas.fill(
            CellRect::new(0, 0, width, 1),
            Style::new(theme.text, theme.popup),
        );
        let button = |icon, name, on, gap, tone, action| IconButton {
            icon,
            name,
            on,
            gap,
            tone,
            action,
        };
        let options = self.options;
        let icons = [
            button(
                Icon::Text("Aa"),
                "Match case",
                options.case_sensitive,
                0,
                Tone::Plain,
                Button::Case,
            ),
            button(
                ICON_WORD,
                "Whole word",
                options.whole_word,
                0,
                Tone::Plain,
                Button::Word,
            ),
            button(
                Icon::Text(".*"),
                "Regular expression",
                options.regex,
                0,
                Tone::Plain,
                Button::Regex,
            ),
            button(
                Icon::Text("↑"),
                "Previous match",
                false,
                1,
                Tone::Plain,
                Button::Previous,
            ),
            button(
                Icon::Text("↓"),
                "Next match",
                false,
                0,
                Tone::Plain,
                Button::Next,
            ),
            button(
                Icon::Text("✕"),
                "Close",
                false,
                1,
                Tone::Danger,
                Button::Close,
            ),
        ];
        let buttons = icons_width(&icons);
        let slot = STATUS_CELLS.min(width / 3);
        let input = width.saturating_sub(buttons + slot + TEXT_X + 2).max(4);
        let (clicked, hovered) = draw_icons(
            canvas,
            &mut self.query_buttons,
            theme.popup,
            width.saturating_sub(buttons),
            &icons,
        );
        match clicked {
            Some(Button::Case) => self.options.case_sensitive = !self.options.case_sensitive,
            Some(Button::Word) => self.options.whole_word = !self.options.whole_word,
            Some(Button::Regex) => self.options.regex = !self.options.regex,
            Some(Button::Previous) => actions.push(FindAction::Previous),
            Some(Button::Next) => actions.push(FindAction::Next),
            Some(Button::Close) => actions.push(FindAction::Close),
            _ => {}
        }
        self.handle_input(canvas, Field::Query, input, actions);
        draw_input(canvas, &mut self.query, input, ICON_SEARCH, "find");
        let color = match hovered {
            Some(_) => theme.muted,
            None if failed => theme.danger,
            None => theme.accent,
        };
        draw_status(canvas, input, slot, hovered.unwrap_or(status), color);
        input
    }

    fn draw_replacement_row(
        &mut self,
        canvas: &mut Canvas<'_>,
        input: u32,
        actions: &mut Vec<FindAction>,
    ) {
        let [width, _] = canvas.size();
        let theme = canvas.theme;
        canvas.fill(
            CellRect::new(0, 0, width, 1),
            Style::new(theme.text, theme.popup),
        );
        let button = |icon, name, action| IconButton {
            icon,
            name,
            on: false,
            gap: 0,
            tone: Tone::Plain,
            action,
        };
        let icons = [
            button(Icon::Text("⏎"), "Replace this match", Button::Replace),
            button(Icon::Text("⇉"), "Replace all matches", Button::ReplaceAll),
        ];
        let (clicked, hovered) = draw_icons(
            canvas,
            &mut self.replace_buttons,
            theme.popup,
            width.saturating_sub(icons_width(&icons)),
            &icons,
        );
        match clicked {
            Some(Button::Replace) => actions.push(FindAction::Replace),
            Some(Button::ReplaceAll) => actions.push(FindAction::ReplaceAll),
            _ => {}
        }
        self.handle_input(canvas, Field::Replacement, input, actions);
        let placeholder = if self.options.regex {
            "replace, $1 for a group"
        } else {
            "replace"
        };
        draw_input(
            canvas,
            &mut self.replacement,
            input,
            Icon::Text("→"),
            placeholder,
        );
        let slot = STATUS_CELLS.min(width / 3);
        draw_status(canvas, input, slot, hovered.unwrap_or(""), theme.muted);
    }

    fn handle_input(
        &mut self,
        canvas: &mut Canvas<'_>,
        field: Field,
        input: u32,
        actions: &mut Vec<FindAction>,
    ) {
        if canvas
            .pointer()
            .is_some_and(|point| point[0] < f64::from(input + TEXT_X))
        {
            canvas.set_cursor(Cursor::Text);
        }
        for event in std::mem::take(&mut canvas.events) {
            let document = match field {
                Field::Query => &mut self.query,
                Field::Replacement => &mut self.replacement,
            };
            match event {
                InputEvent::Text(text) => {
                    let text: String = text.text.chars().filter(|c| !c.is_control()).collect();
                    if !text.is_empty() {
                        document.insert(&text);
                    }
                }
                InputEvent::Key(key) => match key.logical_key {
                    Key::Named(NamedKey::Escape) => actions.push(FindAction::Close),
                    Key::Named(NamedKey::Enter) => actions.push(match field {
                        Field::Query if key.modifiers.shift => FindAction::Previous,
                        Field::Query => FindAction::Next,
                        Field::Replacement => FindAction::Replace,
                    }),
                    Key::Named(NamedKey::Tab) if self.replacing => {
                        self.focus_request = Some(match field {
                            Field::Query => Field::Replacement,
                            Field::Replacement => Field::Query,
                        });
                    }
                    Key::Named(NamedKey::Tab) => {}
                    _ => {
                        // The clipboard failing leaves the input as it was.
                        let _ = edit_key(document, &key, 1);
                    }
                },
                InputEvent::Pointer(pointer) => {
                    let Some(point) = pointer.position.map(|p| canvas.local(p.grid)) else {
                        continue;
                    };
                    if !matches!(pointer.kind, PointerKind::Down(MouseButton::Left))
                        || point[0] >= f64::from(input + TEXT_X)
                    {
                        continue;
                    }
                    let column =
                        document.scroll[0] + (point[0] - f64::from(TEXT_X)).max(0.0) as usize;
                    let offset = document.offset(0, column);
                    document.set_cursor(offset, false);
                }
                _ => {}
            }
        }
    }
}

/// Text in the space after an input, cut to fit.
fn draw_status(canvas: &mut Canvas<'_>, input: u32, slot: u32, text: &str, color: Color) {
    let shown: String = text.chars().take(slot as usize).collect();
    let style = Style::new(color, canvas.theme.popup);
    canvas.text((input + TEXT_X + 1) as i32, 0, &shown, style);
}

/// One line of editable text with a prompt marker, its selection and its caret.
fn draw_input(
    canvas: &mut Canvas<'_>,
    document: &mut Document,
    width: u32,
    marker: Icon<'_>,
    placeholder: &str,
) {
    let theme = canvas.theme;
    let text = document.text.to_text();
    let caret = columns(&text[..document.cursor]);
    let room = width.max(1) as usize;
    if caret < document.scroll[0] {
        document.scroll[0] = caret;
    }
    if caret >= document.scroll[0] + room {
        document.scroll[0] = caret + 1 - room;
    }
    let plain = Style::new(theme.text, theme.control);
    canvas.fill(CellRect::new(0, 0, width + TEXT_X, 1), plain);
    let marker_style = Style::new(theme.accent, theme.control);
    marker.draw(canvas, 0, 0, marker_style);
    let selection = document.selection();
    let selected = Style::new(theme.desktop, theme.accent);
    let mut x = 0;
    for (byte, cluster) in text.grapheme_indices(true) {
        let cells = columns(cluster);
        let position = x as i64 - document.scroll[0] as i64;
        x += cells;
        if position < 0 {
            continue;
        }
        if position as usize + cells > room {
            break;
        }
        let style = if selection.contains(&byte) {
            selected
        } else {
            plain
        };
        canvas.text(TEXT_X as i32 + position as i32, 0, cluster, style);
    }
    if text.is_empty() && canvas.preedit.is_none() {
        let shown: String = placeholder.chars().take(room.saturating_sub(1)).collect();
        let x = TEXT_X as i32 + 1;
        canvas.text(x, 0, &shown, Style::new(theme.muted, theme.control));
    }
    if canvas.response.focused && canvas.window_focused {
        let x = TEXT_X + (caret - document.scroll[0]).min(room - 1) as u32;
        canvas.caret(x, 0);
        let under = text[document.cursor..]
            .graphemes(true)
            .next()
            .unwrap_or(" ");
        canvas.text(x as i32, 0, under, selected);
    }
}

/// Where to go once matches are known.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Jump {
    /// The first match at or after this offset, wrapping to the start.
    Next(usize),
    /// The last match before this offset, wrapping to the end.
    Previous(usize),
}

/// A search or Replace All running in the background. Dropping it stops the work.
struct Background<T> {
    cancel: Arc<AtomicBool>,
    results: Receiver<T>,
}
impl<T> Drop for Background<T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

enum Scanned {
    Matches(Vec<Range<usize>>),
    Finished(Result<(), String>),
}

struct ReplaceAll {
    /// The text it started from; a document that has moved on keeps its edits.
    original: TextBuffer,
    job: Background<Result<(TextBuffer, usize), String>>,
}

/// What the search has found in the active document.
#[derive(Default)]
pub(super) struct Found {
    /// In document order.
    pub matches: Vec<Range<usize>>,
    /// The text, query and options these matches are for.
    searched: Option<(TextBuffer, String, Options)>,
    scan: Option<Background<Scanned>>,
    full: bool,
    problem: Option<String>,
    jump: Option<Jump>,
    /// Where typing a query searches from, so that it can narrow and widen in place.
    origin: usize,
    replace_all: Option<ReplaceAll>,
}

impl Found {
    fn is_finished(&self) -> bool {
        self.scan.is_none()
    }

    /// Whether there is still background work to wait for.
    pub fn working(&self) -> bool {
        self.scan.is_some() || self.replace_all.is_some()
    }

    /// The bar's summary of where things stand, given the current selection.
    pub fn status(&self, query_empty: bool, selection: &Range<usize>) -> String {
        if self.replace_all.is_some() {
            return "Replacing…".into();
        }
        if let Some(problem) = &self.problem {
            return problem.clone();
        }
        if query_empty {
            return String::new();
        }
        let total = self.matches.len();
        if total == 0 {
            return if self.is_finished() {
                "No match".into()
            } else {
                "Searching…".into()
            };
        }
        let more = if self.full || !self.is_finished() {
            "+"
        } else {
            ""
        };
        match self
            .matches
            .binary_search_by_key(&selection.start, |m| m.start)
        {
            Ok(index) if self.matches[index] == *selection => {
                format!("{} of {total}{more}", index + 1)
            }
            _ => format!("{total}{more}"),
        }
    }

    /// Whether the summary is bad news: the query is broken or finds nothing.
    pub fn failed(&self, query_empty: bool) -> bool {
        self.problem.is_some() || (!query_empty && self.is_finished() && self.matches.is_empty())
    }

    /// Whether the selection is exactly one of the matches.
    fn holds(&self, selection: &Range<usize>) -> bool {
        self.matches
            .binary_search_by_key(&selection.start, |m| m.start)
            .is_ok_and(|index| self.matches[index] == *selection)
    }

    /// Search `text` from the start, replacing anything already under way.
    fn start(
        &mut self,
        text: &TextBuffer,
        query: &str,
        options: Options,
        waker: &crate::vt::Waker,
    ) {
        self.scan = None;
        self.matches.clear();
        self.full = false;
        self.problem = None;
        self.searched = Some((text.clone(), query.to_owned(), options));
        if query.is_empty() {
            self.jump = None;
            return;
        }
        let matcher = match search::compile(query, options) {
            Ok(matcher) => matcher,
            Err(problem) => {
                self.problem = Some(problem);
                self.jump = None;
                return;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (send, results) = mpsc::channel();
        let (flag, text, waker) = (cancel.clone(), text.clone(), waker.clone());
        std::thread::spawn(move || {
            let mut batch = Vec::new();
            let mut total = 0;
            let result = search::scan(&text, &matcher, &flag, |range, _, _| {
                batch.push(range);
                total += 1;
                if batch.len() == BATCH {
                    let _ = send.send(Scanned::Matches(std::mem::take(&mut batch)));
                    waker();
                }
                total < MAX_MATCHES
            });
            let _ = send.send(Scanned::Matches(batch));
            let _ = send.send(Scanned::Finished(result));
            waker();
        });
        self.scan = Some(Background { cancel, results });
    }

    /// Take what the search has found since the last call.
    fn receive(&mut self) {
        let Some(scan) = &self.scan else {
            return;
        };
        let mut finished = None;
        loop {
            match scan.results.try_recv() {
                Ok(Scanned::Matches(found)) => self.matches.extend(found),
                Ok(Scanned::Finished(result)) => {
                    finished = Some(result);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = Some(Ok(()));
                    break;
                }
            }
        }
        if let Some(result) = finished {
            self.scan = None;
            self.full = self.matches.len() >= MAX_MATCHES;
            self.problem = result.err();
        }
    }

    /// The match a waiting jump lands on, once enough is known to say.
    fn resolve_jump(&mut self) -> Option<Range<usize>> {
        let jump = self.jump?;
        let finished = self.is_finished();
        let matches = &self.matches;
        let found = match jump {
            Jump::Next(offset) => {
                let after = matches.partition_point(|m| m.start < offset);
                match matches.get(after) {
                    Some(found) => Some(found.clone()),
                    None if finished => matches.first().cloned(),
                    None => return None,
                }
            }
            Jump::Previous(offset) => {
                let after = matches.partition_point(|m| m.start < offset);
                // Matches arrive in order, so a later one shows the earlier ones are all here.
                if !finished && after == matches.len() {
                    return None;
                }
                match after.checked_sub(1) {
                    Some(before) => Some(matches[before].clone()),
                    None if finished => matches.last().cloned(),
                    None => return None,
                }
            }
        };
        self.jump = None;
        found
    }
}

impl Editor {
    /// The bar's summary of where things stand, and whether that is bad news.
    pub(super) fn find_status(&self) -> (String, bool) {
        let Some(bar) = &self.find else {
            return (String::new(), false);
        };
        let selection = self
            .tabs
            .get(self.active)
            .map_or(0..0, |tab| tab.document.selection());
        let empty = bar.query.text.is_empty();
        (
            self.found.status(empty, &selection),
            self.found.failed(empty),
        )
    }

    /// Open the bar, or return to it, with any selected line of text as the query.
    pub(super) fn open_find(&mut self, replacing: bool) {
        let Some(document) = self.tabs.get(self.active).map(|tab| &tab.document) else {
            return;
        };
        let selection = document.selection();
        let seed = (!selection.is_empty() && selection.len() <= 1024)
            .then(|| document.selected_text())
            .filter(|text| !text.contains('\n'));
        let origin = selection.start;
        let bar = self.find.get_or_insert_with(FindBar::new);
        if let Some(seed) = seed {
            bar.query.select_all();
            bar.query.insert(&seed);
        }
        bar.query.select_all();
        bar.replacing |= replacing;
        bar.focus_request = Some(if replacing && !bar.query.text.is_empty() {
            Field::Replacement
        } else {
            Field::Query
        });
        self.found.origin = origin;
        self.focus = Some(Focus::Find);
    }

    /// Bring the search up to date with the bar and the document.
    pub(super) fn poll_find(&mut self) {
        self.finish_replace_all();
        let (Some(bar), Some(tab)) = (&self.find, self.tabs.get(self.active)) else {
            self.found = Found::default();
            return;
        };
        let query = bar.query.text.to_text();
        let found = &mut self.found;
        let (same_query, same_text) = match &found.searched {
            Some((text, searched, options)) => (
                *searched == query && *options == bar.options,
                text.same_text(&tab.document.text),
            ),
            None => (false, false),
        };
        if !same_query || !same_text {
            if !same_query {
                found.jump = Some(Jump::Next(found.origin));
            }
            found.start(&tab.document.text, &query, bar.options, &self.waker);
        }
        found.receive();
        if let Some(target) = found.resolve_jump()
            && let Some(tab) = self.tabs.get_mut(self.active)
        {
            tab.document.select_range(target);
        }
    }

    /// Do what the bar asked for.
    pub(super) fn apply_find(&mut self, action: FindAction) {
        match action {
            FindAction::Close => {
                self.find = None;
                self.found = Found::default();
                self.focus = Some(Focus::Editor);
            }
            FindAction::Next | FindAction::Previous => {
                let Some(tab) = self.tabs.get(self.active) else {
                    return;
                };
                let selection = tab.document.selection();
                self.found.jump = Some(if action == FindAction::Next {
                    Jump::Next(selection.end.max(tab.document.cursor))
                } else {
                    Jump::Previous(selection.start)
                });
                self.found.origin = selection.start;
            }
            FindAction::Replace => self.replace_current(),
            FindAction::ReplaceAll => self.start_replace_all(),
        }
    }

    /// The matcher and options for what the bar now says.
    fn matcher(&self) -> Option<(RegexMatcher, Options, String)> {
        let bar = self.find.as_ref()?;
        let query = bar.query.text.to_text();
        if query.is_empty() {
            return None;
        }
        let matcher = search::compile(&query, bar.options).ok()?;
        Some((matcher, bar.options, bar.replacement.text.to_text()))
    }

    /// Replace the selected match and move to the next; with none selected, just find it.
    fn replace_current(&mut self) {
        let Some((matcher, options, template)) = self.matcher() else {
            return;
        };
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let document = &mut tab.document;
        let selection = document.selection();
        if self.found.holds(&selection) {
            let start = document.line_start(document.row_at(selection.start));
            let end = document.line_end(document.row_at(selection.start));
            let line = document.text.read(start..end);
            let replaced = search::expand(
                &matcher,
                options.regex,
                &template,
                line.as_bytes(),
                selection.start - start,
            );
            document.insert(&replaced);
        }
        self.found.jump = Some(Jump::Next(document.cursor));
        self.found.origin = document.cursor;
    }

    /// Replace every match on a copy of the text, which then takes over as one undo step.
    fn start_replace_all(&mut self) {
        if self.found.replace_all.is_some() {
            return;
        }
        let Some((matcher, options, template)) = self.matcher() else {
            return;
        };
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let original = tab.document.text.clone();
        let template = tab.document.normalized_text(&template);
        let cancel = Arc::new(AtomicBool::new(false));
        let (send, results) = mpsc::channel();
        let (flag, text, waker) = (cancel.clone(), original.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let mut replaced = text.clone();
            let mut count = 0;
            let mut shift = 0isize;
            let scanned = search::scan(&text, &matcher, &flag, |range, line, start| {
                let new = search::expand(
                    &matcher,
                    options.regex,
                    &template,
                    line,
                    range.start - start,
                );
                let at = range.start.wrapping_add_signed(shift);
                replaced.replace(at..at + range.len(), &TextBuffer::from_text(&new));
                shift += new.len() as isize - range.len() as isize;
                count += 1;
                true
            });
            let _ = send.send(scanned.map(|()| (replaced, count)));
            waker();
        });
        self.found.replace_all = Some(ReplaceAll {
            original,
            job: Background { cancel, results },
        });
    }

    /// Take over the result of Replace All once it is ready.
    fn finish_replace_all(&mut self) {
        let Some(running) = &self.found.replace_all else {
            return;
        };
        let result = match running.job.results.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("Replace stopped".into()),
        };
        let original = running.original.clone();
        self.found.replace_all = None;
        match result {
            Ok((text, count)) => {
                let document = self.tabs.get_mut(self.active).map(|tab| &mut tab.document);
                match document {
                    Some(document) if document.text.same_text(&original) => {
                        if count > 0 {
                            document.replace_whole_text(text);
                        }
                        let plural = if count == 1 { "" } else { "es" };
                        self.status = format!("Replaced {count} match{plural}");
                    }
                    // Changed or switched away in the meantime: the newer state wins.
                    _ => self.status = "The file changed, so nothing was replaced".into(),
                }
            }
            Err(problem) => self.report_error(problem),
        }
    }
}
