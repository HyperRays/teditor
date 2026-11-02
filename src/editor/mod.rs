//! The editor application, composed from the immediate-mode tiled UI.
//!
//! [`Editor`] owns the workspace and its commands. A desktop host calls
//! [`request_open`](Editor::request_open), queues normalized input with
//! [`handle`](Editor::handle), and calls [`compose`](Editor::compose) on the terminal's
//! back framebuffer. While [`has_pending_background_work`](Editor::has_pending_background_work)
//! is true, it should keep composing periodically to receive file results and syntax colors.
//!
//! # Large files
//!
//! There is no application-imposed size limit. A [`document::Document`]'s text is a
//! [`buffer::TextBuffer`], a persistent piece tree over private temporary-file mappings,
//! so edits touch only affected tree paths and undo shares old roots. Opening snapshots the
//! file into an anonymous temporary file, then detects, decodes and indexes it on a
//! two-worker I/O pool; saving streams an immutable buffer snapshot on the same pool, so
//! typing during a save keeps later edits dirty. Bulk reads and the highlighter's scans use
//! bounded positional reads, so memory follows what is on screen rather than the file's size;
//! `tests/memory.rs` and `tests/memory_highlight.rs` enforce this.
//!
//! # Undo and following a file
//!
//! Undo keeps earlier roots of the persistent tree, so each step costs about a kilobyte, and
//! a run of typing shares one step per word. A document is unchanged whenever its text
//! matches what was last saved, however it got there. A document never edited, undone or
//! saved since opening also follows its file live: see the `follow` module.
//!
//! # Saving safely
//!
//! Saves write a temporary sibling that keeps the original's permissions, and compare a
//! streaming hash of the file on disk immediately before atomic replacement, so external
//! changes are detected. Line endings and byte-order marks are preserved. Malformed input
//! is never silently replaced: conversion is strict, and a character the output encoding
//! cannot represent fails the save before the original changes. Binary files open in a
//! lossless Bytes view that saves back byte for byte.
//!
//! # Syntax highlighting
//!
//! One shared worker loads syntect's grammars and parses immutable buffer snapshots, keeping
//! parser checkpoints so multiline state survives. No regex runs in frame composition. The
//! UI caches colors for the visible rows plus a margin, and while a parse is pending, colors
//! stay attached to surviving text as bytes move. Colors depend on every earlier line, so a
//! first jump far into a large file shows plain text until the worker has scanned that far. A line over 16 KiB, or a parser error,
//! leaves that line and the following text uncolored until an edit permits a valid parse.

pub mod buffer;
mod chrome;
mod clipboard;
mod console;
/// Open documents: text, caret, selection, undo and saving.
pub mod document;
pub mod encoding;
mod file_io;
mod find;
mod follow;
mod highlight;
mod jobs;
mod palette;
mod rust_links;
pub mod search;
mod symbols;
mod tree;
mod view;
mod wrap;

use crate::{
    renderer::{CellRect, Graphic},
    terminal::{
        Framebuffer, InputEvent,
        input::{Key, KeyEvent, NamedKey},
    },
    ui::{Context, Cursor, Id, Output, Split, controls::Clicks},
    vt,
};
use chrome::{TabAction, TabBar};
use console::Console;
use document::Document;
use encoding::Encoding;
use find::{FindBar, Found};
use follow::{Followed, Watch};
use jobs::Worker;
use palette::{Action, Choice, Command, Mode, Palette};
use rust_links::RustLinks;
use search::GlobalSearch;
use std::{
    cell::Cell,
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
};
use symbols::{DefinitionQuery, Reference, SymbolSearch};
use tree::FileTree;
use view::{EditorView, letter, primary};

struct Tab {
    id: u64,
    document: Document,
    view: EditorView,
    io: Option<IoJob>,
    /// Present while the document still follows its file.
    watch: Option<Arc<Watch>>,
    /// The line to show once the file has finished loading.
    go_to_line: Option<usize>,
}
/// One shell in the terminal panel. Only the active tab is drawn, but all of them run.
struct TerminalTab {
    id: u64,
    console: Console,
}
impl Tab {
    /// Closing this tab now would lose edits or interrupt a save.
    fn unsaved(&self) -> bool {
        self.document.dirty() || matches!(self.io, Some(IoJob::Save(_)))
    }
}
enum IoJob {
    Load {
        worker: Worker<Document>,
        new: bool,
    },
    /// The followed file was rewritten, so it loads again behind the visible text.
    Reload(Worker<Document>),
    Save(Worker<file_io::Saved>),
}
impl IoJob {
    fn label(&self) -> String {
        match self {
            Self::Load { worker, .. } | Self::Reload(worker) => worker.progress.label(),
            Self::Save(worker) => worker.progress.label(),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Editor,
    Files,
    Palette,
    Terminal,
    Find,
}
#[derive(Clone, Copy)]
enum Pending {
    Close(u64),
    Quit,
}

const TERMINAL_MIN_ROWS: u32 = 4;
const WORKSPACE_MIN_ROWS: u32 = 4;
/// Programs can set very long titles; a tab shows the start of one.
const TERMINAL_LABEL_CHARS: usize = 24;

/// Owns documents and workspace state. The host queues normalized terminal input
/// and composes into its writable framebuffer; GPU presentation stays with Terminal.
pub struct Editor {
    context: Context,
    tree: FileTree,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    events: Vec<InputEvent>,
    output: Output,
    repaint: bool,
    palette: Option<Palette>,
    focus: Option<Focus>,
    pending: Option<Pending>,
    pending_save: bool,
    status: String,
    quit: bool,
    tab_bar: TabBar,
    status_buttons: Clicks<chrome::StatusButton>,
    title_click: Clicks<()>,
    terminal_resizing: bool,
    terminals: Vec<TerminalTab>,
    active_terminal: usize,
    terminal_tabs: TabBar,
    terminal_backlog: bool,
    /// A followed file has more to read than one frame takes.
    follow_backlog: bool,
    /// Whether long lines continue on the next row, for every tab.
    word_wrap: bool,
    symbols: SymbolSearch,
    /// The find bar, while open, and what it has found.
    find: Option<FindBar>,
    found: Found,
    text_search: GlobalSearch,
    rust_links: RustLinks,
    /// A symbol to follow once the index is up to date.
    pending_definition: Option<DefinitionQuery>,
    /// One for each folder the tree shows.
    folder_watches: HashMap<PathBuf, Arc<Watch>>,
    /// Folders changed since the quick-open list was built.
    files_stale: bool,
    terminal_open: bool,
    /// Panel height including its header; kept between toggles.
    terminal_rows: u32,
    /// Refocus the terminal when a palette opened from it closes.
    palette_from_terminal: bool,
    shell: Vec<String>,
    waker: vt::Waker,
    files: Vec<PathBuf>,
    index: Option<Receiver<Vec<PathBuf>>>,
    focused: bool,
}

impl Editor {
    /// An editor for the workspace at `root`, holding one new document.
    pub fn new(root: &Path) -> io::Result<Self> {
        // A read-only folder can still be browsed; indexing falls back to the defaults.
        let _ = crate::tags::ensure_ignore_file(root);
        let mut editor = Self {
            context: Context::new(),
            tree: FileTree::new(root)?,
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            events: Vec::new(),
            output: Output::default(),
            repaint: false,
            palette: None,
            focus: Some(Focus::Editor),
            pending: None,
            pending_save: false,
            status: String::new(),
            quit: false,
            tab_bar: TabBar::default(),
            status_buttons: Clicks::default(),
            title_click: Clicks::default(),
            terminal_resizing: false,
            terminals: Vec::new(),
            active_terminal: 0,
            terminal_tabs: TabBar::default(),
            terminal_backlog: false,
            follow_backlog: false,
            word_wrap: false,
            symbols: SymbolSearch::default(),
            find: None,
            found: Found::default(),
            text_search: GlobalSearch::default(),
            rust_links: RustLinks::default(),
            pending_definition: None,
            folder_watches: HashMap::new(),
            files_stale: false,
            terminal_open: false,
            terminal_rows: 12,
            palette_from_terminal: false,
            shell: default_shell(),
            waker: Arc::new(|| {}),
            files: Vec::new(),
            index: None,
            focused: true,
        };
        editor.new_document();
        Ok(editor)
    }
    /// The document in the active tab.
    pub fn active_document(&self) -> Option<&Document> {
        self.tabs.get(self.active).map(|t| &t.document)
    }
    /// Number of open tabs.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }
    /// The workspace folder.
    pub fn workspace(&self) -> &Path {
        &self.tree.root
    }
    /// Whether the palette or a prompt is showing.
    pub fn palette_open(&self) -> bool {
        self.palette.is_some()
    }
    /// The current status-bar message.
    pub fn status(&self) -> &str {
        &self.status
    }
    /// The window title for the active document.
    pub fn title(&self) -> String {
        self.active_document().map_or_else(
            || "teditor".into(),
            |doc| {
                format!(
                    "{}{} — teditor",
                    if doc.dirty() { "● " } else { "" },
                    doc.name()
                )
            },
        )
    }
    /// Lets background output wake a host that waits for events rather than polling.
    pub fn set_waker(&mut self, waker: vt::Waker) {
        self.waker = waker;
    }
    /// Whether the terminal panel is showing.
    pub fn terminal_open(&self) -> bool {
        self.terminal_open
    }
    fn terminal_focused(&self) -> bool {
        self.context.focused() == Some(Id::new("terminal").with("screen"))
    }
    /// Show or hide the panel. Hiding keeps the shells running; each ends only on exit.
    fn toggle_terminal(&mut self) {
        if self.terminal_open {
            self.terminal_open = false;
            self.focus = Some(Focus::Editor);
        } else if self.terminals.is_empty() {
            self.new_terminal();
        } else {
            self.terminal_open = true;
            self.focus = Some(Focus::Terminal);
        }
    }
    /// Start another shell in a new tab, opening the panel if it is hidden.
    fn new_terminal(&mut self) {
        let cwd = self.tree.root.clone();
        match Console::spawn(&self.shell, &cwd, self.context.theme, self.waker.clone()) {
            Ok(console) => {
                let id = self.next_id;
                self.next_id += 1;
                self.terminals.push(TerminalTab { id, console });
                self.terminal_open = true;
                self.switch_terminal(self.terminals.len() - 1);
            }
            Err(e) => self.report_error(format!("Could not start the terminal: {e}")),
        }
    }
    fn switch_terminal(&mut self, index: usize) {
        self.active_terminal = index;
        self.focus = Some(Focus::Terminal);
        // A composition in progress belongs to the shell it started in.
        self.context.reset_text_input();
    }
    /// Remove a shell's tab, ending it if it is still running. Without tabs the panel closes.
    fn close_terminal(&mut self, id: u64) {
        let Some(index) = self.terminals.iter().position(|tab| tab.id == id) else {
            return;
        };
        self.terminals.remove(index);
        if index == self.active_terminal {
            self.context.reset_text_input();
        }
        if index < self.active_terminal {
            self.active_terminal -= 1;
        }
        self.active_terminal = self
            .active_terminal
            .min(self.terminals.len().saturating_sub(1));
        if self.terminals.is_empty() && self.terminal_open {
            self.terminal_open = false;
            self.focus = Some(Focus::Editor);
        }
    }
    /// Parse every shell's output, including hidden tabs, so none stalls on a full queue.
    fn pump_terminals(&mut self) {
        let mut exited = Vec::new();
        self.terminal_backlog = false;
        for tab in &mut self.terminals {
            let more = tab.console.pump();
            self.terminal_backlog |= more;
            if tab.console.exited() && !more {
                exited.push(tab.id);
            }
        }
        for id in exited {
            self.close_terminal(id);
            self.status = "Terminal exited".into();
        }
    }
    /// Queue input for the next [`compose`](Self::compose).
    pub fn handle(&mut self, event: InputEvent) {
        self.events.push(event);
    }
    /// The native cursor to show.
    pub fn cursor(&self) -> Cursor {
        self.output.cursor
    }
    /// Whether a text field or document has the keyboard, so native IME should be on.
    pub fn text_input_active(&self) -> bool {
        self.output.ime_cursor.is_some()
    }
    /// The cells the IME candidate window should stay near.
    pub fn ime_cursor_cells(&self) -> Option<CellRect> {
        self.output.ime_cursor
    }
    /// Whether native IME must be turned off before it is next enabled. Clears the request.
    pub fn take_ime_reset(&mut self) -> bool {
        std::mem::take(&mut self.output.reset_ime)
    }
    /// Whether another frame is needed even without new input.
    pub fn needs_repaint(&self) -> bool {
        self.repaint
    }
    /// Whether file loading, saving or indexing is still running.
    pub fn has_pending_io(&self) -> bool {
        self.index.is_some()
            || self.symbols.indexing()
            || self.found.working()
            || self.text_search.searching()
            || self.tabs.iter().any(|t| t.io.is_some())
    }
    /// Hosts should poll while file work or visible syntax coloring is pending.
    pub fn has_pending_background_work(&self) -> bool {
        self.has_pending_io()
            || self
                .tabs
                .get(self.active)
                .is_some_and(|t| t.view.highlight_pending())
    }
    /// Whether the application should exit.
    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// Open a file synchronously, or change workspace for a folder.
    pub fn open(&mut self, path: &Path) -> io::Result<()> {
        let path = crate::paths::canonicalize(path)?;
        if path.is_dir() {
            self.tree = FileTree::new(&path)?;
            // A read-only folder can still be browsed; indexing falls back to the defaults.
            let _ = crate::tags::ensure_ignore_file(&path);
            self.files.clear();
            self.index = None;
            self.symbols = SymbolSearch::default();
            self.focus = Some(Focus::Files);
            self.status = format!("Workspace: {}", path.display());
            return Ok(());
        }
        if self.activate_path(&path) {
            return Ok(());
        }
        let document = Document::open(&path)?;
        if self.tabs.len() == 1 && pristine(&self.tabs[0].document) {
            self.tabs.clear();
        }
        self.add_tab(document, None);
        self.tree.reveal(&path);
        self.status.clear();
        Ok(())
    }
    /// Desktop entry point: schedule snapshotting, decoding and indexing on the
    /// I/O pool. Closing a loading tab cancels it; completion cannot resurrect it.
    pub fn request_open(&mut self, path: &Path) -> io::Result<()> {
        let path = crate::paths::canonicalize(path)?;
        if path.is_dir() {
            return self.open(&path);
        }
        if !std::fs::metadata(&path)?.is_file() {
            return Err(io::Error::other("Choose a regular file"));
        }
        if self.activate_path(&path) {
            return Ok(());
        }
        let source = path.clone();
        let worker =
            Worker::start(move |progress| Document::open_with_encoding(&source, None, progress))?;
        let mut document = Document::new();
        document.path = Some(path);
        self.add_tab(document, Some(IoJob::Load { worker, new: true }));
        Ok(())
    }
    /// Switch to the tab already showing `path`, if there is one.
    fn activate_path(&mut self, path: &Path) -> bool {
        let tab = self
            .tabs
            .iter()
            .position(|tab| tab.document.path.as_deref() == Some(path));
        if let Some(index) = tab {
            self.activate(index);
        }
        tab.is_some()
    }
    fn add_tab(&mut self, document: Document, io: Option<IoJob>) {
        let id = self.next_id;
        self.next_id += 1;
        let watch = self.watch_for(&document);
        self.tabs.push(Tab {
            id,
            document,
            view: EditorView::default(),
            io,
            watch,
            go_to_line: None,
        });
        self.activate(self.tabs.len() - 1);
    }
    fn reopen(&mut self, id: u64, encoding: Option<Encoding>) -> io::Result<()> {
        let tab = self
            .tabs
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| io::Error::other("Tab is no longer open"))?;
        if tab.document.dirty() {
            return Err(io::Error::other(
                "Save or undo edits before reopening; use Save with encoding to convert current edits",
            ));
        }
        if tab.io.is_some() {
            return Err(io::Error::other(
                "Wait for the current file operation first",
            ));
        }
        let path = tab
            .document
            .path
            .clone()
            .ok_or_else(|| io::Error::other("Save this new document before reopening it"))?;
        let worker =
            Worker::start(move |progress| Document::open_with_encoding(&path, encoding, progress))?;
        tab.io = Some(IoJob::Load { worker, new: false });
        self.context.reset_text_input();
        Ok(())
    }
    fn watch_for(&self, document: &Document) -> Option<Arc<Watch>> {
        let path = document.path.as_ref().filter(|_| document.following())?;
        Some(Watch::start(path, self.waker.clone()))
    }
    /// Bring untouched documents up to date with their files, and stop watching the rest.
    fn follow_files(&mut self) {
        self.follow_backlog = false;
        for tab in &mut self.tabs {
            if !tab.document.following() {
                tab.watch = None;
                continue;
            }
            let Some(watch) = tab.watch.as_ref().filter(|_| tab.io.is_none()) else {
                continue;
            };
            if !watch.take_changed() {
                continue;
            }
            match tab.document.follow() {
                Ok(Followed::Current) => {}
                Ok(Followed::More) => {
                    watch.mark_changed();
                    self.follow_backlog = true;
                }
                Ok(Followed::Rewritten) => {
                    if let Some(path) = tab.document.path.clone() {
                        let encoding = Some(tab.document.encoding());
                        tab.io = Worker::start(move |progress| {
                            Document::open_with_encoding(&path, encoding, progress)
                        })
                        .ok()
                        .map(IoJob::Reload);
                    }
                }
                // Gone for now; it is noticed again if it comes back.
                Err(_) => {}
            }
        }
    }
    /// Bring the tree, and what depends on the folders it shows, up to date with the disk.
    fn follow_folders(&mut self) {
        let tree = &self.tree;
        self.folder_watches
            .retain(|path, _| *path == tree.root || tree.expanded.contains(path));
        let mut started = false;
        for path in tree.shown_folders() {
            if !self.folder_watches.contains_key(path) {
                let watch = Watch::start_from_now(path, self.waker.clone());
                self.folder_watches.insert(path.clone(), watch);
                started = true;
            }
        }
        let mut changed = false;
        for watch in self.folder_watches.values() {
            changed |= watch.take_changed();
        }
        // Reading after a watch starts means nothing can change unseen.
        if started || changed {
            self.tree.refresh();
        }
        if changed {
            self.symbols.invalidate();
            self.files_stale |= !self.files.is_empty() || self.index.is_some();
        }
        // One walk at a time however often folders change, with a last one after the changes.
        if self.files_stale && self.index.is_none() {
            self.files_stale = false;
            self.start_index();
        }
    }
    /// Follow a symbol to where it is defined, once the index allows.
    fn go_to_definition(&mut self, reference: Reference) {
        if self.follow_rust_link(&reference) {
            return;
        }
        self.symbols.request(&self.tree.root);
        self.status = format!("Finding {}…", reference.name);
        let file = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.document.path.clone());
        let language = file.as_deref().and_then(|file| {
            let ctags = crate::tags::Ctags::locate()?;
            ctags.language_of(file).ok()?
        });
        self.pending_definition = Some(DefinitionQuery {
            reference,
            file,
            language,
        });
        self.resolve_definition();
    }
    /// Act on the waiting symbol if the index is ready. A single definition opens directly;
    /// several could each be what was meant, so the palette offers them.
    fn resolve_definition(&mut self) {
        let Some(query) = self.pending_definition.clone() else {
            return;
        };
        if let Some(problem) = self.symbols.problem().filter(|_| !self.symbols.indexing()) {
            let problem = problem.to_owned();
            self.pending_definition = None;
            return self.report_error(problem);
        }
        let Some(index) = self.symbols.index().filter(|_| self.symbols.ready()) else {
            return;
        };
        self.pending_definition = None;
        let name = &query.reference.name;
        let tags = match index.find(name) {
            Ok(tags) => query.narrow(tags),
            Err(e) => return self.report_error(e.to_string()),
        };
        self.status.clear();
        match tags.as_slice() {
            [] => {
                let language = query.language.as_deref().unwrap_or("");
                let space = if language.is_empty() { "" } else { " " };
                self.status = format!("No {language}{space}definition found for {name}");
            }
            [only] => {
                let (path, line) = (only.path.clone(), only.line);
                self.open_at_line(&path, line);
            }
            _ => self.show_palette(Mode::Search, &format!("#{name}")),
        }
    }
    /// Open a file at a line, now or once it has finished loading.
    fn open_at_line(&mut self, path: &Path, line: usize) {
        if let Err(e) = self.request_open(path) {
            return self.report_error(e.to_string());
        }
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if matches!(tab.io, Some(IoJob::Load { .. })) {
            tab.go_to_line = Some(line);
        } else {
            tab.document.go_to_line(line);
        }
    }
    fn new_document(&mut self) {
        self.add_tab(Document::new(), None);
    }
    fn activate(&mut self, index: usize) {
        self.active = index.min(self.tabs.len().saturating_sub(1));
        self.focus = Some(Focus::Editor);
        self.context.reset_text_input();
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.document.reveal = true;
        }
    }
    fn start_index(&mut self) {
        let (send, receive) = mpsc::channel();
        let root = self.tree.root.clone();
        let hidden = self.tree.show_hidden;
        std::thread::spawn(move || {
            let _ = send.send(tree::index_files(&root, hidden));
        });
        self.index = Some(receive);
    }
    fn show_palette(&mut self, mode: Mode, initial: &str) {
        if matches!(mode, Mode::Search) && self.files.is_empty() && self.index.is_none() {
            self.start_index();
        }
        self.palette_from_terminal = self.palette.is_none() && self.terminal_focused();
        self.palette = Some(Palette::new(mode, initial));
        self.focus = Some(Focus::Palette);
        self.context.reset_text_input();
    }
    /// Edit the active document's path in its title bar, selected so typing replaces it.
    fn show_path_bar(&mut self) {
        let initial = self
            .active_document()
            .and_then(|doc| doc.path.as_ref())
            .map(|path| {
                path.strip_prefix(&self.tree.root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default();
        let hidden = self.tree.show_hidden;
        self.show_palette(Mode::Path { hidden }, &initial);
        if let Some(palette) = &mut self.palette {
            palette.query.select_all();
        }
    }
    /// Open what the path bar names, creating a file or trailing-slash folder if missing.
    fn submit_path(&mut self, text: &str) {
        let path = palette::resolve_path(&self.tree.root, text);
        let result = match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                self.close_palette();
                self.show_folder(&path)
            }
            Ok(_) => {
                self.close_palette();
                self.request_open(&path)
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            Err(_) if text.ends_with(std::path::is_separator) => std::fs::create_dir_all(&path)
                .and_then(|()| {
                    self.files.clear();
                    self.close_palette();
                    self.show_folder(&path)
                }),
            Err(_) => path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                })
                .and_then(|_| {
                    self.files.clear();
                    self.close_palette();
                    self.request_open(&path)
                }),
        };
        if let Err(e) = result {
            self.report_error(e.to_string());
        }
    }
    /// Show a folder in the tree, switching workspace if it lies outside this one.
    fn show_folder(&mut self, path: &Path) -> io::Result<()> {
        let path = crate::paths::canonicalize(path)?;
        if !path.starts_with(&self.tree.root) {
            return self.open(&path);
        }
        self.tree.reveal_folder(&path);
        self.focus = Some(Focus::Files);
        Ok(())
    }
    fn close_palette(&mut self) {
        self.palette = None;
        let terminal = std::mem::take(&mut self.palette_from_terminal) && self.terminal_open;
        self.focus = Some(if terminal {
            Focus::Terminal
        } else {
            Focus::Editor
        });
        self.context.reset_text_input();
    }

    /// A native close request takes the same unsaved-change path as Cmd+Q.
    pub fn request_quit(&mut self) {
        if self.tabs.iter().any(Tab::unsaved) {
            self.pending = Some(Pending::Quit);
            self.pending_save = false;
            self.show_palette(Mode::Confirm("Unsaved changes.".into()), "");
        } else {
            self.quit = true;
        }
    }
    fn request_close(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        self.request_close_tab(tab.id);
    }
    fn request_close_tab(&mut self, id: u64) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        if tab.unsaved() {
            let message = format!("Save {}?", tab.document.name());
            self.pending = Some(Pending::Close(id));
            self.pending_save = false;
            self.show_palette(Mode::Confirm(message), "");
        } else {
            self.remove_tab(id);
        }
    }
    fn remove_tab(&mut self, id: u64) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.tabs.remove(index);
            self.activate(if index < self.active {
                self.active.saturating_sub(1)
            } else {
                self.active
            });
        }
    }
    fn save_as(&mut self, id: u64) {
        let name = self.tabs.iter().find(|t| t.id == id).map_or_else(
            || "untitled.txt".into(),
            |t| {
                if t.document.path.is_some() {
                    format!("{}.copy", t.document.name())
                } else {
                    "untitled.txt".into()
                }
            },
        );
        self.status = "Save as: enter a new file path, then Enter (Esc cancels)".into();
        self.show_palette(Mode::SaveAs(id), &name);
        if let Some(palette) = &mut self.palette {
            palette.query.select_all();
        }
    }
    fn report_error(&mut self, message: String) {
        if let Some(palette) = &mut self.palette {
            palette.notice = Some(message.clone());
        }
        self.status = message;
    }
    /// Save a tab, asking for a path first when it has none.
    fn save(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.document.path.is_none() {
            self.save_as(tab.id);
        } else if let Err(e) = self.start_save(index, None) {
            self.report_error(e.to_string());
        }
    }
    fn start_save(&mut self, index: usize, destination: Option<&Path>) -> io::Result<()> {
        let tab = &mut self.tabs[index];
        if tab.io.is_some() {
            return Err(io::Error::other("A file operation is already in progress"));
        }
        let request = tab.document.prepare_save(destination)?;
        tab.io = Some(IoJob::Save(Worker::start(move |progress| {
            request.run(progress)
        })?));
        Ok(())
    }
    fn poll_io(&mut self) {
        enum Finished {
            Load(bool, io::Result<Box<Document>>),
            Reload(io::Result<Box<Document>>),
            Save(io::Result<file_io::Saved>),
        }
        let mut finished = Vec::new();
        for tab in &mut self.tabs {
            let result = match &tab.io {
                Some(IoJob::Load { worker, new }) => {
                    worker.poll().map(|r| Finished::Load(*new, r.map(Box::new)))
                }
                Some(IoJob::Reload(worker)) => {
                    worker.poll().map(|r| Finished::Reload(r.map(Box::new)))
                }
                Some(IoJob::Save(worker)) => worker.poll().map(Finished::Save),
                None => None,
            };
            if let Some(result) = result {
                tab.io = None;
                finished.push((tab.id, result));
            }
        }
        for (id, result) in finished {
            let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
                continue;
            };
            match result {
                Finished::Load(_, Ok(document)) => {
                    self.status = document.notice.clone().unwrap_or_default();
                    if let Some(path) = &document.path {
                        self.tree.reveal(path);
                    }
                    self.tabs[index].watch = self.watch_for(&document);
                    self.tabs[index].document = *document;
                    self.tabs[index].view = EditorView::default();
                    if let Some(line) = self.tabs[index].go_to_line.take() {
                        self.tabs[index].document.go_to_line(line);
                    }
                    if index == self.active && self.palette.is_none() {
                        self.activate(index);
                    }
                    // Remove only the pristine startup document, never another tab's edits.
                    if self.tabs.len() > 1
                        && self.tabs[0].id == 0
                        && pristine(&self.tabs[0].document)
                    {
                        self.remove_tab(0);
                    }
                }
                Finished::Reload(Ok(mut document)) => {
                    // An edit made while it loaded takes priority over the file.
                    if self.tabs[index].document.following() {
                        document.keep_position_of(&self.tabs[index].document);
                        self.tabs[index].watch = self.watch_for(&document);
                        self.tabs[index].document = *document;
                    }
                }
                // Keep showing the last good text; the watch notices any later change.
                Finished::Reload(Err(_)) => {}
                Finished::Load(new, Err(e)) => {
                    if new {
                        self.remove_tab(id);
                    }
                    self.report_error(e.to_string());
                }
                Finished::Save(Ok(saved)) => {
                    self.rust_links.saved();
                    self.tree.reveal(&saved.path);
                    self.files.clear();
                    self.symbols.invalidate();
                    self.tabs[index].document.saved(saved);
                    self.status = format!("Saved {}", self.tabs[index].document.name());
                    if matches!(self.palette.as_ref().map(|p| &p.mode), Some(Mode::SaveAs(tab)) if *tab == id)
                    {
                        self.close_palette();
                    }
                    if self.pending.is_some() && self.pending_save {
                        self.finish_pending(true);
                    }
                }
                Finished::Save(Err(e)) => self.report_error(e.to_string()),
            }
        }
    }
    /// Answer the unsaved-changes prompt. Saving runs on the I/O pool, one tab at a
    /// time, and each completed save comes back here until none are left.
    fn finish_pending(&mut self, save: bool) {
        self.pending_save = save;
        if save {
            let pending = self.pending;
            let applies = |tab: &&Tab| match pending {
                Some(Pending::Close(id)) => tab.id == id,
                Some(Pending::Quit) => true,
                None => false,
            };
            if self
                .tabs
                .iter()
                .filter(applies)
                .any(|tab| matches!(tab.io, Some(IoJob::Save(_))))
            {
                return;
            }
            let dirty = self
                .tabs
                .iter()
                .filter(applies)
                .find(|t| t.document.dirty());
            if let Some(id) = dirty.map(|tab| tab.id) {
                if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                    self.save(index);
                }
                return;
            }
        }
        let pending = self.pending.take();
        self.pending_save = false;
        self.close_palette();
        match pending {
            Some(Pending::Close(id)) => self.remove_tab(id),
            Some(Pending::Quit) => self.quit = true,
            None => {}
        }
    }
    fn command(&mut self, command: Command) {
        match command {
            Command::New => self.new_document(),
            Command::Palette => self.show_palette(Mode::Search, ""),
            Command::PathBar => self.show_path_bar(),
            Command::Terminal => self.toggle_terminal(),
            Command::NewTerminal => self.new_terminal(),
            Command::CloseTerminal => {
                if let Some(tab) = self.terminals.get(self.active_terminal) {
                    self.close_terminal(tab.id);
                }
            }
            Command::Save => {
                self.save(self.active);
            }
            Command::SaveAs => {
                if let Some(tab) = self.tabs.get(self.active) {
                    self.save_as(tab.id);
                }
            }
            Command::Close => self.request_close(),
            Command::Quit => self.request_quit(),
            Command::WordWrap => {
                self.word_wrap = !self.word_wrap;
                // A scroll column means different things in each mode, so start afresh.
                for tab in &mut self.tabs {
                    tab.document.scroll[0] = 0;
                    tab.document.reveal = true;
                }
            }
            Command::Symbols => self.show_palette(Mode::Search, "#"),
            Command::FindInFiles => self.show_palette(Mode::Search, "?"),
            Command::Find => self.open_find(false),
            Command::Replace => self.open_find(true),
            Command::Refresh | Command::Hidden => {
                if command == Command::Hidden {
                    self.tree.show_hidden = !self.tree.show_hidden;
                }
                self.tree.refresh();
                self.start_index();
                self.symbols.invalidate();
                self.status = self
                    .tree
                    .error
                    .clone()
                    .unwrap_or_else(|| "Files refreshed".into());
            }
            Command::Files => self.focus = Some(Focus::Files),
            Command::Editor => self.focus = Some(Focus::Editor),
            Command::Undo => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.document.undo();
                }
            }
            Command::Redo => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.document.redo();
                }
            }
            Command::ReopenEncoding | Command::SaveEncoding => {
                if let Some(tab) = self.tabs.get(self.active) {
                    if tab.io.is_some() {
                        self.report_error("Wait for the current file operation first".into());
                    } else {
                        self.show_palette(
                            Mode::Encoding {
                                tab: tab.id,
                                save: command == Command::SaveEncoding,
                            },
                            "",
                        );
                    }
                }
            }
        }
    }
    fn global(&mut self, event: &InputEvent) -> bool {
        if let InputEvent::Focused(focused) = event {
            self.focused = *focused;
        }
        if let InputEvent::FileDropped { path, .. } = event {
            if self.pending.is_none()
                && let Err(e) = self.request_open(path)
            {
                self.report_error(e.to_string());
            }
            return true;
        }
        let InputEvent::Key(key) = event else {
            return false;
        };
        if !key.is_actionable() || !self.focused {
            return false;
        }
        if self.palette.is_some() {
            if primary(key)
                && (letter(key, "p") || letter(key, "l"))
                && !key.repeat
                && self.pending.is_none()
            {
                self.close_palette();
                return true;
            }
            return false;
        }
        // Inside the terminal Ctrl belongs to the program, so only the application's
        // own modifier reaches these shortcuts.
        let shortcut = if self.terminal_focused() {
            key.modifiers.super_key
                || (cfg!(not(target_os = "macos")) && key.modifiers.control && key.modifiers.shift)
        } else {
            primary(key)
        };
        if shortcut {
            let in_terminal = self.terminal_focused();
            if let Some(command) = shortcut_command(key) {
                if !key.repeat {
                    // Closing acts on whichever kind of tab has the keyboard.
                    self.command(match command {
                        Command::Close if in_terminal => Command::CloseTerminal,
                        other => other,
                    });
                }
                return true;
            }
            let (count, current) = if in_terminal {
                (self.terminals.len(), self.active_terminal)
            } else {
                (self.tabs.len(), self.active)
            };
            if let Some(index) = tab_shortcut(key, count, current, in_terminal) {
                if in_terminal {
                    self.switch_terminal(index);
                } else {
                    self.activate(index);
                }
                return true;
            }
        }
        if key.logical_key == Key::Named(NamedKey::F6) {
            self.focus = Some(
                if self.context.focused() == Some(Id::new("files").with("tree")) {
                    Focus::Editor
                } else {
                    Focus::Files
                },
            );
            return true;
        }
        false
    }

    /// Process queued input and background results, then draw a complete frame.
    pub fn compose(&mut self, framebuffer: &mut Framebuffer) {
        self.poll_io();
        self.symbols.poll();
        self.resolve_definition();
        self.follow_files();
        self.pump_terminals();
        if let Some(index) = &self.index {
            match index.try_recv() {
                Ok(files) => {
                    self.files = files;
                    self.index = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.index = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.follow_folders();
        self.poll_find();
        self.text_search.poll();
        self.poll_rust_links();
        let mut reset_ime = false;
        // Process in order so a click followed by typing, or editing followed by
        // Save in the same native batch, always targets the current document.
        for event in std::mem::take(&mut self.events) {
            if self.focus.is_some() {
                self.frame(framebuffer, &[]);
                reset_ime |= self.output.reset_ime;
            }
            let consumed = self.global(&event);
            self.frame(
                framebuffer,
                if consumed {
                    &[]
                } else {
                    std::slice::from_ref(&event)
                },
            );
            reset_ime |= self.output.reset_ime;
        }
        // A query typed just now must start searching, with no later frame to count on.
        self.poll_find();
        // Resolve effects from tree/tab/palette actions before presenting once.
        self.frame(framebuffer, &[]);
        self.output.reset_ime |= reset_ime;
        self.repaint |= self.terminal_backlog || self.follow_backlog;
        match self.palette.as_ref().and_then(Palette::text_query) {
            Some(query) => self
                .text_search
                .request(&query, &self.tree.root, &self.waker),
            None => self.text_search.clear(),
        }
        if self.palette.as_ref().is_some_and(Palette::wants_symbols) {
            self.symbols.request(&self.tree.root);
        }
    }
    /// Rebuild the whole interface once, then apply what its controls asked for.
    fn frame(&mut self, framebuffer: &mut Framebuffer, events: &[InputEvent]) {
        let active_path = self.active_document().and_then(|d| d.path.clone());
        let title = active_path.as_ref().map_or_else(
            || "Untitled".into(),
            |p| {
                p.strip_prefix(&self.tree.root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            },
        );
        let root = self.tree.root.clone();
        let root_name = root.file_name().unwrap_or_default().to_string_lossy();
        let labels: Vec<_> = self
            .tabs
            .iter()
            .map(|tab| {
                let dot = if tab.document.dirty() { "● " } else { "" };
                (tab.id, format!("{dot}{}", tab.document.name()))
            })
            .collect();
        let status = chrome::StatusBar::new(self);
        let terminal_labels: Vec<_> = self
            .terminals
            .iter()
            .map(|tab| {
                let title = tab.console.title();
                (tab.id, title.chars().take(TERMINAL_LABEL_CHARS).collect())
            })
            .collect();
        let terminal_title = terminal_labels
            .get(self.active_terminal)
            .map_or("", |(_, title): &(u64, String)| title.as_str());
        let [screen_width, screen_height] = framebuffer.size();
        let panel_rows = if self.terminal_open {
            let available = screen_height.saturating_sub(1);
            self.terminal_rows
                .min(available.saturating_sub(WORKSPACE_MIN_ROWS))
                .max(TERMINAL_MIN_ROWS.min(available))
        } else {
            0
        };
        let focus = self.focus.take();
        let modal = self.palette.is_some();
        // A later error replaces an earlier one; the status row shows just one.
        let error = Cell::new(None);
        let report = |message: Option<String>| {
            if message.is_some() {
                error.set(message);
            }
        };
        let mut open = None;
        let mut tab_action = None;
        let mut action = None;
        let mut editor_rect = None;
        let mut terminal_rect = None;
        let mut keep_focus = None;
        let mut terminal_focused = false;
        let mut title_clicked = false;
        let mut new_terminal_rows = None;
        let mut terminal_tab_action = None;
        let find_rows = self.find.as_ref().map_or(0, FindBar::rows);
        let (find_status, find_failed) = self.find_status();
        let mut find_actions = Vec::new();
        let output = self.context.run(framebuffer, events, |ui| {
            ui.footer(
                "application",
                1,
                |ui| {
                    ui.disabled(modal, |ui| {
                        ui.footer(
                            "panel",
                            panel_rows,
                            |ui| {
                                ui.split(
                                    "workspace",
                                    Split::columns(0.26),
                                    |ui| {
                                        ui.pane("files", &root_name, |ui| {
                                            let height = ui.available_height();
                                            let mut canvas = ui.canvas("tree", height, false);
                                            if focus == Some(Focus::Files) {
                                                canvas.request_focus();
                                            }
                                            open =
                                                self.tree.draw(&mut canvas, active_path.as_deref());
                                            report(self.tree.error.take());
                                        });
                                    },
                                    |ui| {
                                        ui.footer(
                                            "documents",
                                            1,
                                            |ui| {
                                                let rect = ui.pane("editor", &title, |ui| {
                                                    ui.footer(
                                                        "find",
                                                        find_rows,
                                                        |ui| {
                                                            report(chrome::draw_document(
                                                                ui,
                                                                self.tabs.get_mut(self.active),
                                                                focus == Some(Focus::Editor),
                                                                self.word_wrap,
                                                                &self.found.matches,
                                                            ));
                                                        },
                                                        |ui| {
                                                            if let Some(bar) = &mut self.find {
                                                                find_actions = bar.draw(
                                                                    ui,
                                                                    &find_status,
                                                                    find_failed,
                                                                    focus == Some(Focus::Find),
                                                                );
                                                            }
                                                        },
                                                    );
                                                    if let Some(bar) = &self.find {
                                                        bar.draw_edge(ui);
                                                    }
                                                });
                                                editor_rect = Some(rect);
                                            },
                                            |ui| {
                                                let mut canvas = ui.canvas("tabs", 1, false);
                                                let [width, _] = canvas.size();
                                                tab_action = chrome::tabs(
                                                    &mut canvas,
                                                    width,
                                                    &labels,
                                                    self.active,
                                                    &mut self.tab_bar,
                                                );
                                            },
                                        );
                                    },
                                );
                            },
                            |ui| {
                                if !self.terminal_open {
                                    return;
                                }
                                let rect = ui.pane("terminal", terminal_title, |ui| {
                                    let height = ui.available_height();
                                    let mut canvas = ui.canvas("screen", height, true);
                                    if focus == Some(Focus::Terminal) {
                                        canvas.request_focus();
                                    }
                                    terminal_focused = canvas.response.focused;
                                    if let Some(tab) = self.terminals.get_mut(self.active_terminal)
                                    {
                                        report(tab.console.draw(&mut canvas));
                                    }
                                });
                                terminal_rect = Some(rect);
                            },
                        );
                    });
                },
                |ui| {
                    ui.disabled(modal, |ui| {
                        let mut canvas = ui.canvas("status", 1, false);
                        // These rows act on the document; they never keep the keyboard.
                        if canvas.response.focused {
                            keep_focus = Some(Focus::Editor);
                        }
                        action = status.draw(&mut canvas, &mut self.status_buttons).map(run);
                    });
                },
            );
            let header = editor_rect
                .filter(|rect| rect.rows > 0)
                .map(|rect| CellRect::new(rect.column, rect.row, rect.columns, 1));
            if let Some(header) = header.filter(|_| !modal) {
                let mut canvas = ui.canvas_at("title", header, false);
                if canvas.response.focused {
                    keep_focus = Some(Focus::Editor);
                }
                title_clicked = chrome::draw_title(&mut canvas, &title, &mut self.title_click);
            }
            if let Some(rect) = terminal_rect.filter(|rect| !modal && rect.rows > 0) {
                let header = CellRect::new(rect.column, rect.row, rect.columns, 1);
                let mut canvas = ui.canvas_at("terminal-bar", header, false);
                if canvas.response.focused {
                    keep_focus = Some(Focus::Terminal);
                    terminal_focused = true;
                }
                let bar = chrome::draw_terminal_bar(
                    &mut canvas,
                    &terminal_labels,
                    self.active_terminal,
                    &mut self.terminal_tabs,
                    &mut self.terminal_resizing,
                    screen_height,
                    terminal_focused,
                );
                new_terminal_rows = bar.rows;
                terminal_tab_action = bar.tab;
                // The terminal's top edge. It is the bottom edge of the row above, as a
                // horizontal divider only draws on a bottom edge.
                if let Some(row) = rect.row.checked_sub(1) {
                    ui.graphic_at(
                        CellRect::new(rect.column, row, rect.columns, 1),
                        Graphic::Divider {
                            vertical: false,
                            color: bar.edge,
                        },
                    );
                }
            }
            if let Some(palette) = &mut self.palette {
                let (chosen, message) = palette.draw(
                    ui,
                    palette::Placement {
                        screen: [screen_width, screen_height],
                        header,
                        editor: editor_rect,
                        take_focus: focus == Some(Focus::Palette),
                    },
                    &root,
                    &self.files,
                    self.index.is_some(),
                    (&self.symbols, &self.text_search),
                );
                if chosen.is_some() {
                    action = chosen;
                }
                report(message);
            }
        });
        self.output = output;
        self.repaint = false;
        if keep_focus.is_some() {
            self.focus = keep_focus;
        }
        if let Some(rows) = new_terminal_rows {
            self.terminal_rows = rows.max(TERMINAL_MIN_ROWS);
            self.repaint = true;
        }
        if !find_actions.is_empty() {
            for action in find_actions {
                self.apply_find(action);
            }
            self.poll_find();
            self.repaint = true;
        }
        if title_clicked {
            self.show_path_bar();
            self.repaint = true;
        }
        if let Some(error) = error.take() {
            self.report_error(error);
            self.repaint = true;
        }
        if let Some(path) = open {
            if let Err(e) = self.request_open(&path) {
                self.report_error(e.to_string());
            }
            self.repaint = true;
        }
        if let Some(action) = tab_action {
            match action {
                TabAction::Activate(index) => self.activate(index),
                TabAction::Close(id) => self.request_close_tab(id),
                TabAction::New => self.new_document(),
            }
            self.repaint = true;
        }
        let request = self
            .tabs
            .get_mut(self.active)
            .and_then(|tab| tab.view.take_definition_request());
        if let Some(name) = request {
            self.go_to_definition(name);
            self.repaint = true;
        }
        if let Some(action) = terminal_tab_action {
            match action {
                TabAction::Activate(index) => self.switch_terminal(index),
                TabAction::Close(id) => self.close_terminal(id),
                TabAction::New => self.new_terminal(),
            }
            self.repaint = true;
        }
        if let Some(action) = action {
            self.palette_action(action);
            self.repaint = true;
        }
    }
    fn palette_action(&mut self, action: Action) {
        match action {
            Action::Cancel => {
                self.pending = None;
                self.pending_save = false;
                self.close_palette();
                self.status.clear();
            }
            Action::Choose(Choice::Encoding(encoding)) => {
                let Some(Mode::Encoding { tab, save }) = self.palette.as_ref().map(|p| &p.mode)
                else {
                    return;
                };
                let (tab, save) = (*tab, *save);
                let Some(index) = self.tabs.iter().position(|t| t.id == tab) else {
                    return;
                };
                if save {
                    if let Some(encoding) = encoding {
                        self.tabs[index].document.set_encoding(encoding);
                        self.close_palette();
                        self.save(index);
                    }
                } else {
                    match self.reopen(tab, encoding) {
                        Ok(()) => self.close_palette(),
                        Err(e) => self.report_error(e.to_string()),
                    }
                }
            }
            Action::Choose(Choice::Path {
                path,
                directory: true,
            }) => {
                if let Some(palette) = &mut self.palette {
                    palette.complete_path(&path, true);
                }
            }
            Action::Choose(Choice::Command(command)) => {
                self.close_palette();
                self.command(command);
            }
            Action::Choose(Choice::Symbol { path, line }) => {
                self.close_palette();
                self.open_at_line(&path, line);
            }
            Action::Choose(Choice::File(path) | Choice::Path { path, .. }) => {
                self.close_palette();
                if let Err(e) = self.request_open(&path) {
                    self.report_error(e.to_string());
                }
            }
            Action::SubmitPath(text) => self.submit_path(&text),
            Action::Save => self.finish_pending(true),
            Action::Discard => self.finish_pending(false),
            Action::SaveAs(name) => {
                let Some(Palette {
                    mode: Mode::SaveAs(id),
                    ..
                }) = &self.palette
                else {
                    return;
                };
                let Some(index) = self.tabs.iter().position(|tab| tab.id == *id) else {
                    return;
                };
                let path = self.tree.root.join(name);
                match self.start_save(index, Some(&path)) {
                    Ok(()) => {
                        if let Some(palette) = &mut self.palette {
                            palette.notice = Some("Saving…".into());
                        }
                    }
                    Err(e) => self.report_error(e.to_string()),
                }
            }
        }
    }
}

/// A status-bar click, as the palette action it stands for.
fn run(command: Command) -> Action {
    Action::Choose(Choice::Command(command))
}

/// An untouched new document, which opening a file may replace.
fn pristine(document: &Document) -> bool {
    document.path.is_none() && !document.dirty()
}

/// The command bound to a primary-modifier shortcut.
fn shortcut_command(key: &KeyEvent) -> Option<Command> {
    let shift = key.modifiers.shift;
    [
        ("p", Command::Palette),
        (
            "s",
            if shift {
                Command::SaveAs
            } else {
                Command::Save
            },
        ),
        ("n", Command::New),
        ("w", Command::Close),
        ("q", Command::Quit),
        ("l", Command::PathBar),
        ("t", Command::NewTerminal),
        ("b", Command::Files),
    ]
    .into_iter()
    .find(|(name, _)| letter(key, name))
    .map(|(_, command)| command)
    .or((shift && letter(key, "e")).then_some(Command::ReopenEncoding))
    // Shift keeps this working in a focused terminal, where Ctrl+J is the program's line feed.
    .or((shift && letter(key, "j")).then_some(Command::Terminal))
    .or(letter(key, "f").then_some(if shift {
        Command::FindInFiles
    } else if key.modifiers.alt {
        Command::Replace
    } else {
        Command::Find
    }))
    // On a Mac, Cmd+H hides the application.
    .or((cfg!(not(target_os = "macos")) && letter(key, "h")).then_some(Command::Replace))
}

/// The tab a shortcut selects: Tab cycles, a digit picks one, and inside the terminal
/// Shift+[ and Shift+] also cycle, since Ctrl+Tab belongs to the program there.
fn tab_shortcut(key: &KeyEvent, count: usize, current: usize, in_terminal: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let shift = key.modifiers.shift;
    let backward = in_terminal && shift && letter(key, "[");
    let forward = in_terminal && shift && letter(key, "]");
    if key.logical_key == Key::Named(NamedKey::Tab) || backward || forward {
        let previous = backward || (shift && !forward);
        let step = if previous { count - 1 } else { 1 };
        return Some((current + step) % count);
    }
    let Key::Character(digit) = &key.logical_key else {
        return None;
    };
    let number = digit.parse::<usize>().ok()?;
    (number > 0 && number <= count).then(|| number - 1)
}

/// The user's login shell, so the panel has the same environment as their terminal.
fn default_shell() -> Vec<String> {
    if cfg!(windows) {
        return vec![std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into())];
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    vec![shell, "-l".into()]
}

#[cfg(test)]
mod tests;
