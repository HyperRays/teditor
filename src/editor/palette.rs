//! The command palette, path bar and unsaved-changes prompt.

use super::{
    document::{Document, columns},
    encoding::Encoding,
    search::GlobalSearch,
    symbols::SymbolSearch,
    tree::children,
    view::{edit_key, primary_name},
};
use crate::{
    renderer::{CellRect, Graphic},
    terminal::{
        InputEvent, Style,
        input::{Key, MouseButton, NamedKey, PointerKind},
    },
    ui::{
        Canvas, Cursor, Ui,
        controls::{Chip, Clicks, Scroll, Tone, chips_width, compact, draw_chips, hit, place},
    },
};
use std::path::{Path, PathBuf, is_separator};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    New,
    Palette,
    PathBar,
    Terminal,
    NewTerminal,
    CloseTerminal,
    Save,
    SaveAs,
    Close,
    Quit,
    Refresh,
    Hidden,
    Symbols,
    Find,
    Replace,
    FindInFiles,
    WordWrap,
    Files,
    Editor,
    Undo,
    Redo,
    ReopenEncoding,
    SaveEncoding,
}
impl Command {
    pub fn all() -> [(Self, &'static str); 22] {
        [
            (Self::New, "New file"),
            (Self::Save, "Save file"),
            (Self::SaveAs, "Save as…"),
            (Self::PathBar, "Go to path…"),
            (Self::Find, "Find in file…"),
            (Self::Replace, "Replace in file…"),
            (Self::FindInFiles, "Find in files…"),
            (Self::Symbols, "Find symbol…"),
            (Self::Terminal, "Toggle terminal"),
            (Self::NewTerminal, "New terminal tab"),
            (Self::CloseTerminal, "Close terminal tab"),
            (Self::Close, "Close tab"),
            (Self::Quit, "Quit editor"),
            (Self::Refresh, "Refresh files"),
            (Self::Hidden, "Toggle hidden files"),
            (Self::WordWrap, "Toggle word wrap"),
            (Self::Files, "Focus file tree"),
            (Self::Editor, "Focus editor"),
            (Self::Undo, "Undo"),
            (Self::Redo, "Redo"),
            (Self::ReopenEncoding, "Reopen with encoding…"),
            (Self::SaveEncoding, "Save with encoding…"),
        ]
    }
    pub fn shortcut(self) -> Option<String> {
        let keys = match self {
            Self::New => "N",
            Self::Save => "S",
            Self::SaveAs => "Shift+S",
            Self::Close => "W",
            Self::Quit => "Q",
            Self::Files => "B",
            Self::Undo => "Z",
            Self::Redo => "Shift+Z",
            Self::ReopenEncoding => "Shift+E",
            Self::Editor => return Some("F6".into()),
            Self::Palette => "P",
            Self::PathBar => "L",
            Self::Terminal => "Shift+J",
            Self::Find => "F",
            Self::Replace => "Alt+F",
            Self::FindInFiles => "Shift+F",
            Self::NewTerminal => "T",
            Self::Refresh
            | Self::Hidden
            | Self::WordWrap
            | Self::Symbols
            | Self::SaveEncoding
            | Self::CloseTerminal => return None,
        };
        Some(format!("{}+{keys}", primary_name()))
    }
}
#[derive(Clone)]
pub enum Choice {
    Command(Command),
    File(PathBuf),
    Encoding(Option<Encoding>),
    Path { path: PathBuf, directory: bool },
    Symbol { path: PathBuf, line: usize },
}
pub enum Action {
    Choose(Choice),
    SaveAs(String),
    SubmitPath(String),
    Save,
    Discard,
    Cancel,
}
pub enum Mode {
    Search,
    SaveAs(u64),
    Confirm(String),
    Encoding { tab: u64, save: bool },
    Path { hidden: bool },
}
/// A query starting with this searches for symbols, as `>` does for commands.
const SYMBOL_PREFIX: u8 = b'#';
/// A query starting with this searches the text of the workspace's files.
const TEXT_PREFIX: u8 = b'?';
/// Results shown for a symbol search; typing more letters narrows them.
const SYMBOL_ROWS: usize = 200;
/// What pressing Enter on the typed path would do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PathTarget {
    Empty,
    File,
    Folder,
    NewFile,
    NewFolder,
}
/// Resolve typed text against the workspace, expanding a leading `~`.
pub fn resolve_path(root: &Path, text: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (text.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home,
        (Some(rest), Some(home)) if rest.starts_with(is_separator) => {
            home.join(rest.trim_start_matches(is_separator))
        }
        _ => root.join(text),
    }
}
fn path_target(root: &Path, query: &str) -> PathTarget {
    if query.trim().is_empty() {
        return PathTarget::Empty;
    }
    match std::fs::metadata(resolve_path(root, query)) {
        Ok(metadata) if metadata.is_dir() => PathTarget::Folder,
        Ok(_) => PathTarget::File,
        Err(_) if query.ends_with(is_separator) => PathTarget::NewFolder,
        Err(_) => PathTarget::NewFile,
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Button {
    Submit,
    Save,
    Discard,
    Cancel,
}
struct Row {
    score: usize,
    label: String,
    hint: Option<String>,
    choice: Choice,
}
impl Row {
    fn new(score: usize, label: String, choice: Choice) -> Self {
        Self {
            score,
            label,
            hint: None,
            choice,
        }
    }
}
/// What one frame of keyboard and pointer input asked the input row to do.
#[derive(Default)]
struct InputEffects {
    action: Option<Action>,
    error: Option<String>,
    choose: bool,
    delta: isize,
}
/// Where the palette goes on screen this frame.
pub struct Placement {
    pub screen: [u32; 2],
    /// The editor pane's header, which the path bar replaces.
    pub header: Option<CellRect>,
    /// The editor pane, which the path bar's suggestions drop down over.
    pub editor: Option<CellRect>,
    pub take_focus: bool,
}
pub struct Palette {
    pub mode: Mode,
    pub query: Document,
    pub notice: Option<String>,
    selected: usize,
    /// Path mode only: Enter acts on the typed text until the arrows pick a row.
    highlighted: bool,
    initial: String,
    target: PathTarget,
    last_query: String,
    rows: Vec<Row>,
    /// Query, file-list, symbol-index and text-search identity that `rows` was filtered from.
    filtered: Option<(String, usize, usize, usize, usize)>,
    scroll: Scroll,
    reveal: bool,
    buttons: Clicks<Button>,
    row_clicks: Clicks<usize>,
    dragging: bool,
}
impl Palette {
    pub fn new(mode: Mode, initial: &str) -> Self {
        let mut query = Document::new();
        query.insert(initial);
        Self {
            mode,
            query,
            notice: None,
            selected: 0,
            highlighted: false,
            initial: initial.to_owned(),
            target: PathTarget::Empty,
            last_query: String::new(),
            rows: Vec::new(),
            filtered: None,
            scroll: Scroll::default(),
            reveal: false,
            buttons: Clicks::default(),
            row_clicks: Clicks::default(),
            dragging: false,
        }
    }
    /// Result rows shown above the input, given the window height in cells.
    pub fn list_rows(&self, height: u32) -> u32 {
        if matches!(self.mode, Mode::Search | Mode::Encoding { .. }) {
            (height.saturating_sub(2) / 2).min(10)
        } else {
            0
        }
    }
    /// Suggestion rows under the path bar, given the cells below it.
    pub fn dropdown_rows(&self, height: u32) -> u32 {
        if matches!(self.mode, Mode::Path { .. }) {
            (self.rows.len() as u32).min(10).min(height)
        } else {
            0
        }
    }
    pub fn is_path_bar(&self) -> bool {
        matches!(self.mode, Mode::Path { .. })
    }
    /// Replace the path's last component, entering it when it is a folder.
    pub fn complete_path(&mut self, path: &Path, directory: bool) {
        let query = self.query.text.to_text();
        let parent = query.rfind(is_separator).map_or(0, |at| at + 1);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let completed = format!(
            "{}{name}{}",
            &query[..parent],
            if directory { "/" } else { "" }
        );
        self.query.select_all();
        self.query.insert(&completed);
    }
    /// Clicking outside dismisses a search, but an unsaved-changes prompt must be answered.
    pub fn dismissable(&self) -> bool {
        !matches!(self.mode, Mode::Confirm(_))
    }
    /// Recompute the result rows when the query or file list has changed.
    /// Whether the query is asking for symbols, which need an index.
    /// What the query is asking to find in files' text, if it is.
    pub fn text_query(&self) -> Option<String> {
        let wanted =
            matches!(self.mode, Mode::Search) && self.query.text.byte(0) == Some(TEXT_PREFIX);
        wanted.then(|| self.query.text.read(1..self.query.text.len()))
    }
    pub fn wants_symbols(&self) -> bool {
        matches!(self.mode, Mode::Search) && self.query.text.byte(0) == Some(SYMBOL_PREFIX)
    }
    fn filter(
        &mut self,
        query: &str,
        root: &Path,
        files: &[PathBuf],
        symbols: &SymbolSearch,
        text: &GlobalSearch,
    ) {
        let key = (
            query.to_owned(),
            files.as_ptr() as usize,
            files.len(),
            symbols.generation(),
            text.generation(),
        );
        if self.filtered.as_ref() == Some(&key) {
            return;
        }
        self.filtered = Some(key);
        let mut rows = match self.mode {
            Mode::Path { hidden } => {
                self.target = path_target(root, query);
                self.path_rows(query, root, hidden)
            }
            Mode::Encoding { save, .. } => encoding_rows(query, save),
            Mode::Search if self.wants_symbols() => symbol_rows(&query[1..], root, symbols),
            Mode::Search if self.text_query().is_some() => text_rows(root, text),
            Mode::Search => search_rows(query, root, files),
            Mode::SaveAs(_) | Mode::Confirm(_) => Vec::new(),
        };
        // Stable: equally good commands keep their declared order, ahead of files.
        rows.sort_by(|a, b| {
            a.score
                .cmp(&b.score)
                .then_with(|| match (&a.choice, &b.choice) {
                    (Choice::File(_), Choice::File(_)) => a
                        .label
                        .len()
                        .cmp(&b.label.len())
                        .then(a.label.cmp(&b.label)),
                    (Choice::File(_), _) => std::cmp::Ordering::Greater,
                    (_, Choice::File(_)) => std::cmp::Ordering::Less,
                    _ => std::cmp::Ordering::Equal,
                })
        });
        self.rows = rows;
    }
    /// Entries of the folder the query is in, matching its last component.
    fn path_rows(&self, query: &str, root: &Path, hidden: bool) -> Vec<Row> {
        let parent = query.rfind(is_separator).map_or(0, |at| at + 1);
        // Until edited, the bar lists the current file's siblings unfiltered.
        let pattern = if query == self.initial {
            String::new()
        } else {
            query[parent..].to_lowercase()
        };
        let listing = children(
            &resolve_path(root, &query[..parent]),
            hidden || pattern.starts_with('.'),
        )
        .unwrap_or_default();
        let mut rows = Vec::new();
        for (path, directory) in listing {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if let Some(score) = score(&name.to_lowercase(), &pattern) {
                let label = format!("{name}{}", if directory { "/" } else { "" });
                rows.push(Row::new(score, label, Choice::Path { path, directory }));
            }
        }
        rows
    }
    /// Draw the whole palette above everything else: a click-catching scrim, the
    /// input row and its result list. Returns the chosen action and any error.
    pub fn draw(
        &mut self,
        ui: &mut Ui<'_>,
        place: Placement,
        root: &Path,
        files: &[PathBuf],
        indexing: bool,
        searches: (&SymbolSearch, &GlobalSearch),
    ) -> (Option<Action>, Option<String>) {
        let [width, height] = place.screen;
        let scrim = ui.canvas_at("scrim", CellRect::new(0, 0, width, height), false);
        let outside = scrim.events.iter().any(|event| {
            matches!(event, InputEvent::Pointer(p) if matches!(p.kind, PointerKind::Down(MouseButton::Left)))
        });
        let mut action = (outside && self.dismissable()).then_some(Action::Cancel);
        let bottom = height.saturating_sub(1);
        let bottom_row = CellRect::new(0, bottom, width, height.min(1));
        let input = place
            .header
            .filter(|_| self.is_path_bar())
            .unwrap_or(bottom_row);
        let mut canvas = ui.canvas_at("palette", input, true);
        if place.take_focus || !canvas.response.focused {
            canvas.request_focus();
        }
        let (chosen, error) = self.draw_input(&mut canvas, root, files, indexing, searches);
        action = chosen.or(action);
        if let Some(pane) = place.editor.filter(|_| self.is_path_bar()) {
            let rows = self.dropdown_rows(pane.rows.saturating_sub(1));
            if rows > 0 {
                let rect = CellRect::new(pane.column, pane.row + 1, pane.columns, rows);
                let mut canvas = ui.canvas_at("palette-list", rect, false);
                action = self.draw_list(&mut canvas).or(action);
            }
        }
        let rows = self.list_rows(height);
        if rows > 0 {
            let header = CellRect::new(0, bottom - rows - 1, width, 1);
            self.draw_header(&mut ui.canvas_at("palette-header", header, false));
            let list = CellRect::new(0, bottom - rows, width, rows);
            let mut canvas = ui.canvas_at("palette-list", list, false);
            action = self.draw_list(&mut canvas).or(action);
        }
        (action, error)
    }
    /// The input row: the query field or prompt, its status and its buttons.
    fn draw_input(
        &mut self,
        canvas: &mut Canvas<'_>,
        root: &Path,
        files: &[PathBuf],
        indexing: bool,
        searches: (&SymbolSearch, &GlobalSearch),
    ) -> (Option<Action>, Option<String>) {
        let [width, _] = canvas.size();
        let theme = canvas.theme;
        canvas.fill(
            CellRect::new(0, 0, width, 1),
            Style::new(theme.text, theme.title),
        );
        let group = compact(&self.chips(), width.saturating_sub(2));
        let chips_total = chips_width(&group);
        let placed = place(&group, width.saturating_sub(chips_total + 1));
        let content = width.saturating_sub(chips_total + 2);
        let query_width = (if self.is_path_bar() && self.notice.is_none() {
            content * 2 / 3
        } else {
            content / if self.notice.is_some() { 3 } else { 2 }
        })
        .max(4)
        .min(width.saturating_sub(2));
        let confirm = matches!(self.mode, Mode::Confirm(_));
        if let Some(point) = canvas.pointer() {
            canvas.set_cursor(if !confirm && point[0] < f64::from(query_width + 2) {
                Cursor::Text
            } else {
                Cursor::Default
            });
        }
        let clicked = draw_chips(canvas, &mut self.buttons, &placed);
        let mut input = self.handle_events(canvas, query_width);
        match clicked {
            Some(Button::Save) => input.action = Some(Action::Save),
            Some(Button::Discard) => input.action = Some(Action::Discard),
            Some(Button::Cancel) => input.action = Some(Action::Cancel),
            Some(Button::Submit) => input.choose = true,
            None => {}
        }
        if let Mode::Confirm(message) = &self.mode {
            let room = content.saturating_sub(1) as usize;
            let notice = self.notice.as_deref().unwrap_or(message);
            let message: String = notice.chars().take(room).collect();
            canvas.text(1, 0, &message, Style::new(theme.accent, theme.title));
            canvas.caret(width.saturating_sub(1), 0);
            return (input.action, input.error);
        }
        let query = self.query.text.to_text();
        if query != self.last_query {
            self.selected = 0;
            self.highlighted = false;
            self.scroll.offset = 0;
            self.last_query.clone_from(&query);
        }
        let (symbols, text) = searches;
        self.filter(&query, root, files, symbols, text);
        if let Some(action) = self.navigate(&query, input.delta, input.choose) {
            input.action = Some(action);
        }
        self.draw_query(canvas, &query, query_width);
        self.draw_hint(canvas, content, query_width, indexing, searches);
        self.draw_query_caret(canvas, &query, query_width);
        if self.is_path_bar() {
            // Text masks overlays, so the header's rule is restored last.
            canvas.graphic(
                CellRect::new(0, 0, width, 1),
                Graphic::Divider {
                    vertical: false,
                    color: theme.active_border,
                },
            );
        }
        (input.action, input.error)
    }
    /// The input row's buttons for the current mode.
    fn chips(&self) -> Vec<Chip<'static, Button>> {
        let chip = |keys, label, tone, action| Chip {
            keys,
            label,
            tone,
            action,
        };
        let submit = |label| chip("Enter", label, Tone::Primary, Button::Submit);
        let cancel = |label| chip("Esc", label, Tone::Plain, Button::Cancel);
        match self.mode {
            Mode::Confirm(_) => vec![
                chip("S", "Save", Tone::Primary, Button::Save),
                chip("D", "Discard", Tone::Danger, Button::Discard),
                cancel("Cancel"),
            ],
            Mode::Search => vec![submit("Open"), cancel("Close")],
            Mode::Encoding { save, .. } => {
                vec![
                    submit(if save { "Save" } else { "Reopen" }),
                    cancel("Cancel"),
                ]
            }
            Mode::Path { .. } => vec![
                submit(match self.target {
                    _ if self.highlighted => "Open",
                    PathTarget::NewFile | PathTarget::NewFolder => "Create",
                    PathTarget::Folder => "Go to",
                    PathTarget::File | PathTarget::Empty => "Open",
                }),
                cancel("Cancel"),
            ],
            Mode::SaveAs(_) => vec![submit("Save"), cancel("Cancel")],
        }
    }
    /// Apply this frame's events to the query, collecting what they ask for.
    fn handle_events(&mut self, canvas: &mut Canvas<'_>, query_width: u32) -> InputEffects {
        let confirm = matches!(self.mode, Mode::Confirm(_));
        let in_query = |point: [f64; 2]| point[0] < f64::from(query_width + 2);
        let mut input = InputEffects::default();
        for event in std::mem::take(&mut canvas.events) {
            if let InputEvent::Key(key) = &event
                && key.logical_key == Key::Named(NamedKey::Escape)
            {
                input.action = Some(Action::Cancel);
                continue;
            }
            match event {
                InputEvent::Text(text) if confirm => {
                    if text.text.eq_ignore_ascii_case("s") {
                        input.action = Some(Action::Save);
                    } else if text.text.eq_ignore_ascii_case("d") {
                        input.action = Some(Action::Discard);
                    }
                }
                InputEvent::Key(key) if confirm => {
                    if key.logical_key == Key::Named(NamedKey::Enter) {
                        input.action = Some(Action::Save);
                    }
                }
                _ if confirm => {}
                InputEvent::Text(text) => {
                    let text: String = text.text.chars().filter(|c| !c.is_control()).collect();
                    if !text.is_empty() {
                        self.query.insert(&text);
                    }
                }
                InputEvent::Key(key) => match key.logical_key {
                    Key::Named(NamedKey::Enter) => input.choose = true,
                    Key::Named(NamedKey::ArrowDown) => input.delta += 1,
                    Key::Named(NamedKey::ArrowUp) => input.delta -= 1,
                    Key::Named(NamedKey::Tab) if self.is_path_bar() && !key.modifiers.shift => {
                        let row = self
                            .rows
                            .get(if self.highlighted { self.selected } else { 0 });
                        if let Some(Choice::Path { path, directory }) =
                            row.map(|row| row.choice.clone())
                        {
                            self.complete_path(&path, directory);
                        }
                    }
                    Key::Named(NamedKey::Tab) => {
                        input.delta += if key.modifiers.shift { -1 } else { 1 }
                    }
                    _ => {
                        if let Err(e) = edit_key(&mut self.query, &key, 1) {
                            input.error = Some(e.to_string());
                        }
                    }
                },
                InputEvent::Pointer(p) => {
                    let Some(point) = p.position.map(|p| canvas.local(p.grid)) else {
                        continue;
                    };
                    let column = self.query.scroll[0] + (point[0] - 2.0).max(0.0) as usize;
                    let offset = self.query.offset(0, column);
                    match p.kind {
                        PointerKind::Down(MouseButton::Left) if in_query(point) => {
                            self.dragging = true;
                            self.query.set_cursor(offset, p.modifiers.shift);
                        }
                        PointerKind::Move if self.dragging => self.query.set_cursor(offset, true),
                        PointerKind::Up(MouseButton::Left) => self.dragging = false,
                        PointerKind::Click {
                            button: MouseButton::Left,
                            count: 2..,
                        } if in_query(point) => self.query.select_word(offset),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        if !canvas.response.active {
            self.dragging = false;
        }
        if self.query.text.line_count() > 1 {
            let clean = self.query.text.to_text().replace('\n', " ");
            self.query.select_all();
            self.query.insert(&clean);
        }
        input
    }
    /// Move the selection and turn Enter into an action for the current mode.
    fn navigate(&mut self, query: &str, delta: isize, choose: bool) -> Option<Action> {
        let count = self.rows.len() as isize;
        let chosen = |palette: &Self| {
            let row = palette.rows.get(palette.selected)?;
            Some(Action::Choose(row.choice.clone()))
        };
        match self.mode {
            Mode::Path { .. } => {
                if delta != 0 && count > 0 {
                    self.selected = if self.highlighted {
                        (self.selected as isize + delta).rem_euclid(count) as usize
                    } else if delta > 0 {
                        0
                    } else {
                        self.rows.len() - 1
                    };
                    self.highlighted = true;
                    self.reveal = true;
                }
                if !choose {
                    None
                } else if self.highlighted {
                    chosen(self)
                } else {
                    (!query.trim().is_empty()).then(|| Action::SubmitPath(query.to_owned()))
                }
            }
            Mode::Search | Mode::Encoding { .. } => {
                self.selected = if count > 0 {
                    (self.selected as isize + delta).rem_euclid(count) as usize
                } else {
                    0
                };
                self.reveal |= delta != 0;
                if choose { chosen(self) } else { None }
            }
            Mode::SaveAs(_) | Mode::Confirm(_) => {
                (choose && !query.trim().is_empty()).then(|| Action::SaveAs(query.to_owned()))
            }
        }
    }
    /// The prompt marker and the visible part of the query, with its selection.
    fn draw_query(&mut self, canvas: &mut Canvas<'_>, query: &str, query_width: u32) {
        let theme = canvas.theme;
        let caret = columns(&query[..self.query.cursor]);
        if caret < self.query.scroll[0] {
            self.query.scroll[0] = caret;
        }
        if caret >= self.query.scroll[0] + query_width.max(1) as usize {
            self.query.scroll[0] = caret.saturating_sub(query_width.saturating_sub(1) as usize);
        }
        canvas.fill(
            CellRect::new(0, 0, query_width + 2, 1),
            Style::new(theme.text, theme.control),
        );
        let searching = !matches!(self.mode, Mode::SaveAs(_));
        canvas.text(
            0,
            0,
            if searching { "› " } else { "↳ " },
            Style::new(theme.accent, theme.control),
        );
        let selection = self.query.selection();
        let mut x = 0;
        for (byte, cluster) in query.grapheme_indices(true) {
            let width = columns(cluster);
            let position = x as i64 - self.query.scroll[0] as i64;
            x += width;
            if position < 0 {
                continue;
            }
            if position as usize + width > query_width as usize {
                break;
            }
            canvas.text(
                2 + position as i32,
                0,
                cluster,
                if selection.contains(&byte) {
                    Style::new(theme.desktop, theme.accent)
                } else {
                    Style::new(theme.text, theme.control)
                },
            );
        }
        if searching && query.is_empty() && canvas.preedit.is_none() {
            let placeholder = match self.mode {
                Mode::Encoding { .. } => "encoding name",
                Mode::Path { .. } => "path to open or create",
                _ => "file, >command, #symbol or ?text",
            };
            let room = query_width.saturating_sub(1) as usize;
            let shown: String = placeholder.chars().take(room).collect();
            // One cell in, so the caret does not cover the first letter.
            canvas.text(3, 0, &shown, Style::new(theme.muted, theme.control));
        }
    }
    /// The status text to the right of the query.
    fn draw_hint(
        &self,
        canvas: &mut Canvas<'_>,
        content: u32,
        query_width: u32,
        indexing: bool,
        (symbols, text): (&SymbolSearch, &GlobalSearch),
    ) {
        let theme = canvas.theme;
        let (hint, color) = if let Some(notice) = &self.notice {
            (notice.clone(), theme.accent)
        } else {
            match self.mode {
                Mode::Path { .. } => match self.target {
                    _ if self.highlighted => (String::new(), theme.muted),
                    PathTarget::Empty => (String::new(), theme.muted),
                    PathTarget::File => ("File".into(), theme.muted),
                    PathTarget::Folder => ("Folder".into(), theme.muted),
                    PathTarget::NewFile => ("New file".into(), theme.accent),
                    PathTarget::NewFolder => ("New folder".into(), theme.accent),
                },
                Mode::SaveAs(_) | Mode::Confirm(_) => ("Save as".into(), theme.muted),
                _ if !self.rows.is_empty() => (
                    format!("{}/{}", self.selected + 1, self.rows.len()),
                    theme.muted,
                ),
                _ if self.wants_symbols() && symbols.indexing() => {
                    ("Indexing symbols…".into(), theme.muted)
                }
                _ if self.wants_symbols() && symbols.problem().is_some() => {
                    let problem = symbols.problem().unwrap_or_default();
                    (problem.to_owned(), theme.accent)
                }
                _ if self.text_query().is_some() => match text {
                    _ if text.problem().is_some() => {
                        (text.problem().unwrap_or_default().to_owned(), theme.accent)
                    }
                    _ if text.searching() => ("Searching…".into(), theme.muted),
                    _ if text.full() => (format!("First {}", text.hits().len()), theme.muted),
                    _ if !text.hits().is_empty() => {
                        (format!("{} lines", text.hits().len()), theme.muted)
                    }
                    _ if self.query.text.len() <= 1 => (String::new(), theme.muted),
                    _ => ("No match".into(), theme.muted),
                },
                _ if indexing && !self.wants_symbols() => ("Indexing files…".into(), theme.muted),
                _ => ("No match".into(), theme.muted),
            }
        };
        let room = content.saturating_sub(query_width + 3) as usize;
        let hint: String = hint.chars().take(room).collect();
        canvas.text(
            (query_width + 3) as i32,
            0,
            &hint,
            Style::new(color, theme.title),
        );
    }
    /// The block caret, showing any IME composition in progress.
    fn draw_query_caret(&self, canvas: &mut Canvas<'_>, query: &str, query_width: u32) {
        let [width, _] = canvas.size();
        if !canvas.response.focused || !canvas.window_focused || width <= 2 {
            return;
        }
        let theme = canvas.theme;
        let caret = columns(&query[..self.query.cursor]);
        let mut x = 2 + caret.saturating_sub(self.query.scroll[0]) as u32;
        let rest = &query[self.query.cursor..];
        let mut character = rest.graphemes(true).next().unwrap_or(" ").to_owned();
        if let Some(preedit) = canvas.preedit.clone() {
            let room = query_width.saturating_sub(x.saturating_sub(2)) as usize;
            let shown: String = preedit.text.chars().take(room).collect();
            canvas.text(x as i32, 0, &shown, Style::new(theme.accent, theme.control));
            if let Some((start, _)) = preedit.cursor {
                let n = preedit.text.get(..start).unwrap_or("").chars().count();
                x += n as u32;
                character = shown.chars().nth(n).unwrap_or(' ').to_string();
            }
        }
        x = x.min((query_width + 1).min(width - 1));
        canvas.caret(x, 0);
        if canvas.preedit.as_ref().is_none_or(|p| p.cursor.is_some()) {
            canvas.text(
                x as i32,
                0,
                &character,
                Style::new(theme.desktop, theme.accent),
            );
        }
    }
    /// The flat title row above the results.
    pub fn draw_header(&self, canvas: &mut Canvas<'_>) {
        let [width, _] = canvas.size();
        let theme = canvas.theme;
        canvas.fill(
            CellRect::new(0, 0, width, 1),
            Style::new(theme.muted, theme.title),
        );
        let title = match self.mode {
            Mode::Encoding { save: true, .. } => "Save with encoding",
            Mode::Encoding { save: false, .. } => "Reopen with encoding",
            _ => "Files and commands",
        };
        canvas.text(1, 0, title, Style::new(theme.accent, theme.title));
        let hint = "↑↓ select  click to run";
        let left = title.chars().count() as u32 + 3;
        let size = hint.chars().count() as u32 + 1;
        if width >= left + size {
            canvas.text(
                (width - size) as i32,
                0,
                hint,
                Style::new(theme.muted, theme.title),
            );
        }
        canvas.graphic(
            CellRect::new(0, 0, width, 1),
            Graphic::Divider {
                vertical: false,
                color: theme.active_border,
            },
        );
    }
    /// The results: click to run, wheel or scrollbar to move through them.
    pub fn draw_list(&mut self, canvas: &mut Canvas<'_>) -> Option<Action> {
        let [width, height] = canvas.size();
        let theme = canvas.theme;
        let total = self.rows.len();
        self.scroll.maximum = total.saturating_sub(height as usize);
        for event in &canvas.events {
            self.scroll.event(event, canvas, total);
        }
        if std::mem::take(&mut self.reveal) {
            self.scroll.reveal(self.selected, height as usize);
        }
        self.scroll.offset = self.scroll.offset.min(self.scroll.maximum);
        let row_width = width.saturating_sub(u32::from(self.scroll.maximum > 0));
        let regions: Vec<_> = (0..height)
            .map(|row| self.scroll.offset + row as usize)
            .take_while(|index| *index < total)
            .enumerate()
            .map(|(row, index)| (CellRect::new(0, row as u32, row_width, 1), index))
            .collect();
        let clicked = self.row_clicks.update(canvas, &regions);
        let hovered = canvas.pointer().and_then(|point| hit(&regions, point));
        if hovered.is_some() {
            canvas.set_cursor(Cursor::Pointer);
        }
        // The path bar's list opens over the document, so it needs edges of its own.
        let backdrop = if self.is_path_bar() {
            theme.popup
        } else {
            theme.desktop
        };
        canvas.fill(
            CellRect::new(0, 0, width, height),
            Style::new(theme.text, backdrop),
        );
        if total == 0 {
            canvas.text(2, 0, "Nothing to show", Style::new(theme.muted, backdrop));
        }
        for (rect, index) in &regions {
            let row = &self.rows[*index];
            let y = rect.row as i32;
            let selected = *index == self.selected && (self.highlighted || !self.is_path_bar());
            let pressed = self.row_clicks.pressed() == Some(*index);
            let background = if pressed || hovered == Some(*index) {
                theme.hover
            } else if selected {
                theme.control
            } else {
                backdrop
            };
            canvas.fill(*rect, Style::new(theme.text, background));
            if selected {
                canvas.graphic(
                    CellRect::new(0, rect.row, 1, 1),
                    Graphic::Selection(theme.accent),
                );
            }
            if matches!(row.choice, Choice::Command(_)) {
                canvas.text(2, y, ">", Style::new(theme.muted, background));
            }
            let hint = row.hint.as_deref().unwrap_or("");
            let hint_width = hint.chars().count() as u32;
            let mut label_width = row_width.saturating_sub(5);
            if hint_width > 0 && label_width > hint_width + 2 {
                label_width -= hint_width + 2;
                canvas.text(
                    (row_width - hint_width - 1) as i32,
                    y,
                    hint,
                    Style::new(theme.muted, background),
                );
            }
            // Keep the file name when a long path has to give way.
            let count = row.label.chars().count();
            let label = if count > label_width as usize && label_width > 0 {
                let tail: String = row
                    .label
                    .chars()
                    .skip(count - (label_width as usize - 1))
                    .collect();
                format!("…{tail}")
            } else {
                row.label.clone()
            };
            let name = match row.choice {
                Choice::File(_) => label.rfind(['/', '\\']).map_or(0, |at| at + 1),
                _ => 0,
            };
            canvas.text(4, y, &label[..name], Style::new(theme.muted, background));
            canvas.text(
                4 + label[..name].chars().count() as i32,
                y,
                &label[name..],
                Style::new(
                    if matches!(
                        row.choice,
                        Choice::Path {
                            directory: true,
                            ..
                        }
                    ) {
                        theme.accent
                    } else {
                        theme.text
                    },
                    background,
                ),
            );
        }
        if self.is_path_bar() {
            canvas.graphic(
                CellRect::new(0, height.saturating_sub(1), width, 1),
                Graphic::Divider {
                    vertical: false,
                    color: theme.border,
                },
            );
        }
        self.scroll.draw(canvas, total);
        clicked
            .and_then(|index| self.rows.get(index))
            .map(|row| Action::Choose(row.choice.clone()))
    }
}
/// Symbols starting with the query, ignoring case, each with where it is defined.
/// The lines found by a search across files; choosing one opens it there.
fn text_rows(root: &Path, search: &GlobalSearch) -> Vec<Row> {
    search
        .hits()
        .iter()
        .map(|hit| {
            let file = hit.path.strip_prefix(root).unwrap_or(&hit.path);
            let choice = Choice::Symbol {
                path: hit.path.clone(),
                line: hit.line,
            };
            Row {
                hint: Some(format!("{}:{}", file.display(), hit.line)),
                ..Row::new(0, hit.text.clone(), choice)
            }
        })
        .collect()
}
fn symbol_rows(prefix: &str, root: &Path, symbols: &SymbolSearch) -> Vec<Row> {
    let Some(index) = symbols.index() else {
        return Vec::new();
    };
    let tags = index
        .find_prefix(prefix.trim(), SYMBOL_ROWS)
        .unwrap_or_default();
    tags.into_iter()
        .map(|tag| {
            let file = tag.path.strip_prefix(root).unwrap_or(&tag.path);
            let hint = format!("{}  {}:{}", tag.kind, file.display(), tag.line);
            let choice = Choice::Symbol {
                path: tag.path,
                line: tag.line,
            };
            Row {
                hint: Some(hint),
                ..Row::new(0, tag.name, choice)
            }
        })
        .collect()
}
/// Encodings matching the query, led by auto-detection when reopening.
fn encoding_rows(query: &str, save: bool) -> Vec<Row> {
    let pattern = query.trim().to_lowercase();
    let mut rows = Vec::new();
    if !save && let Some(score) = score("auto-detect encoding", &pattern) {
        let label = "Auto-detect encoding".into();
        rows.push(Row::new(score, label, Choice::Encoding(None)));
    }
    for encoding in Encoding::choices() {
        if let Some(score) = score(&encoding.name().to_lowercase(), &pattern) {
            let choice = Choice::Encoding(Some(encoding));
            rows.push(Row::new(score, encoding.name().into(), choice));
        }
    }
    rows
}
/// Commands and files matching the query; a leading `>` restricts it to commands.
fn search_rows(query: &str, root: &Path, files: &[PathBuf]) -> Vec<Row> {
    let pattern = query.trim_start_matches('>').trim().to_lowercase();
    let mut rows = Vec::new();
    for (command, label) in Command::all() {
        if let Some(score) = score(&label.to_lowercase(), &pattern) {
            rows.push(Row {
                hint: command.shortcut(),
                ..Row::new(score, label.to_owned(), Choice::Command(command))
            });
        }
    }
    if query.starts_with('>') {
        return rows;
    }
    for path in files {
        let label = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
        if let Some(score) = score(&label.to_lowercase(), &pattern) {
            let label = label.into_owned();
            rows.push(Row::new(score, label, Choice::File(path.clone())));
        }
    }
    // Also accept an explicit path, including outside the workspace.
    let path = root.join(query);
    if !query.is_empty()
        && path.is_file()
        && !rows
            .iter()
            .any(|row| matches!(&row.choice, Choice::File(p) if p == &path))
    {
        rows.push(Row::new(0, query.to_owned(), Choice::File(path)));
    }
    rows
}
fn score(label: &str, query: &str) -> Option<usize> {
    if query.is_empty() || label.starts_with(query) {
        return Some(0);
    }
    if label.contains(query) {
        return Some(1);
    }
    let mut chars = label.chars();
    query
        .chars()
        .all(|q| chars.by_ref().any(|ch| ch == q))
        .then_some(2)
}
