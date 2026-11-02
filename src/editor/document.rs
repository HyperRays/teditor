//! A document: text, caret, selection, undo history, encoding and save state.
//!
//! ```
//! use teditor::editor::document::Document;
//!
//! let mut doc = Document::new();
//! doc.insert("hello");
//! assert!(doc.dirty());
//! doc.undo();
//! assert_eq!(doc.text.to_text(), "");
//! ```

use super::{
    buffer::{Snapshot, TextBuffer},
    encoding::Encoding,
    file_io::{self, Progress, SaveRequest, Saved},
    follow::{Followed, Tail},
    wrap,
};
use crate::renderer::Columns;
use std::{
    collections::VecDeque,
    fs,
    io::{self, BufWriter, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use unicode_segmentation::UnicodeSegmentation;

/// Columns between tab stops.
pub const TAB_WIDTH: usize = 4;
const HISTORY_STATES: usize = 1000;
/// Inserts at least this large go to a temporary file instead of memory.
const LARGE_INSERT_BYTES: usize = 1024 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 1024;
/// Bytes read around the caret to find a grapheme cluster's edges. Far longer than any
/// cluster in real text, and bounded so a pathological one cannot stall a keystroke.
const CLUSTER_WINDOW: usize = 256;

#[derive(Clone)]
struct State {
    text: TextBuffer,
    cursor: usize,
    anchor: usize,
    encoding: Encoding,
    bom: bool,
}

/// A document's text is a persistent disk-backed piece tree. Coordinates are UTF-8
/// byte offsets; edits touch only affected pieces, and undo retains shared roots.
/// File encoding is independent of the internal Unicode representation.
pub struct Document {
    /// Where the document lives on disk; `None` until first saved.
    pub path: Option<PathBuf>,
    /// The current text.
    pub text: TextBuffer,
    /// The caret's byte offset.
    pub cursor: usize,
    /// The other end of the selection; equal to `cursor` when nothing is selected.
    pub anchor: usize,
    /// The first visible `[column, row]`. With word wrap, the column is where the top row
    /// starts within its line, rather than a horizontal scroll.
    pub scroll: [usize; 2],
    /// The width lines wrap at, when word wrap is on. Vertical movement follows rows then.
    pub(super) wrap_width: Option<usize>,
    /// Bring the caret into view on the next draw.
    pub reveal: bool,
    /// A message about how the file was opened, such as falling back to a bytes view.
    pub notice: Option<String>,
    encoding: Encoding,
    bom: bool,
    saved_encoding: Encoding,
    saved_bom: bool,
    /// The text as last loaded or saved. It shares storage with `text`, so comparing the
    /// two costs little, and returning to this text by any route means unchanged.
    saved_text: TextBuffer,
    dirty: bool,
    original: Option<Arc<Snapshot>>,
    disk_hash: Option<blake3::Hash>,
    crlf: bool,
    preferred_column: Option<usize>,
    /// Set until the first edit, for following the file as it changes.
    tail: Option<Tail>,
    /// Where the last typed text ended, while further typing can join its undo step.
    typing_end: Option<usize>,
    joins_last_step: bool,
    undo: VecDeque<State>,
    redo: Vec<State>,
}
impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}
impl Document {
    /// An empty, unsaved UTF-8 document.
    pub fn new() -> Self {
        Self {
            path: None,
            text: TextBuffer::new(),
            cursor: 0,
            anchor: 0,
            scroll: [0, 0],
            wrap_width: None,
            reveal: true,
            notice: None,
            encoding: Encoding::Utf8,
            bom: false,
            saved_encoding: Encoding::Utf8,
            saved_bom: false,
            saved_text: TextBuffer::new(),
            dirty: false,
            original: None,
            disk_hash: None,
            crlf: false,
            preferred_column: None,
            tail: None,
            typing_end: None,
            joins_last_step: false,
            undo: VecDeque::new(),
            redo: Vec::new(),
        }
    }
    /// Read a file synchronously, detecting its encoding.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with_encoding(path, None, &Progress::default())
    }
    /// Read a file, decoding with `encoding` or detecting one. `progress` reports and cancels.
    pub fn open_with_encoding(
        path: &Path,
        encoding: Option<Encoding>,
        progress: &Progress,
    ) -> io::Result<Self> {
        let path = crate::paths::canonicalize(path)?;
        let loaded = file_io::load(&path, encoding, progress)?;
        let crlf = loaded.text.line_count() > 1
            && loaded
                .text
                .byte(loaded.text.line_start(1).saturating_sub(2))
                == Some(b'\r');
        Ok(Self {
            path: Some(path),
            saved_text: loaded.text.clone(),
            text: loaded.text,
            notice: loaded.notice,
            encoding: loaded.encoding,
            bom: loaded.bom,
            saved_encoding: loaded.encoding,
            saved_bom: loaded.bom,
            original: Some(loaded.original),
            disk_hash: Some(loaded.tail.hash()),
            tail: Some(loaded.tail),
            crlf,
            ..Self::new()
        })
    }
    /// Whether the text or output encoding differs from what was last saved.
    pub fn dirty(&self) -> bool {
        self.dirty
    }
    /// Whether this document still follows its file: it has never been edited, undone or
    /// saved since it was opened.
    pub(crate) fn following(&self) -> bool {
        self.tail.is_some()
    }
    /// Take what the file has gained since it was opened. Only a document that is still
    /// [following](Self::following) has anything to do here.
    pub(crate) fn follow(&mut self) -> io::Result<Followed> {
        let (Some(tail), Some(path)) = (&mut self.tail, &self.path) else {
            return Ok(Followed::Current);
        };
        // Other encodings would need the text decoded again, so they load whole.
        if self.encoding != Encoding::Utf8 {
            return Ok(if tail.file_changed(path)? {
                Followed::Rewritten
            } else {
                Followed::Current
            });
        }
        let old_length = self.text.len();
        let followed = tail.read_new(path, &mut self.text)?;
        if self.text.len() != old_length {
            self.disk_hash = Some(tail.hash());
            self.saved_text = self.text.clone();
            // The stored original no longer has everything, so saving must write the text.
            self.original = None;
            // A caret at the end stays at the end, which is how to watch a log.
            if self.cursor == old_length && self.anchor == old_length {
                self.cursor = self.text.len();
                self.anchor = self.cursor;
                self.reveal = true;
            }
        }
        Ok(followed)
    }
    /// Keep the caret and view of the document this one reloads.
    pub(crate) fn keep_position_of(&mut self, old: &Self) {
        let at_end = old.cursor == old.text.len();
        let cursor = if at_end { self.text.len() } else { old.cursor };
        self.set_cursor(cursor, false);
        self.scroll = old.scroll;
        self.reveal = at_end;
    }
    /// The encoding used for saving.
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }
    /// Whether saving writes a byte-order mark.
    pub fn has_bom(&self) -> bool {
        self.bom
    }
    /// Choose the encoding for the next save. This can be undone like an edit.
    pub fn set_encoding(&mut self, encoding: Encoding) {
        if self.encoding != encoding {
            self.checkpoint();
            self.encoding = encoding;
            self.bom = encoding.default_bom();
            self.changed();
        }
    }
    /// The file name, or `Untitled`.
    pub fn name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or_else(|| "Untitled".into(), |n| n.to_string_lossy().into_owned())
    }
    /// Number of lines.
    pub fn line_count(&self) -> usize {
        self.text.line_count()
    }
    /// The offset where line `row` starts, clamped to the last line.
    pub fn line_start(&self, row: usize) -> usize {
        self.text.line_start(row.min(self.line_count() - 1))
    }
    /// The offset where line `row` ends, before its line ending.
    pub fn line_end(&self, row: usize) -> usize {
        if row + 1 >= self.line_count() {
            return self.text.len();
        }
        let mut end = self.text.line_start(row + 1) - 1;
        if end > 0 && self.text.byte(end - 1) == Some(b'\r') {
            end -= 1;
        }
        end
    }
    /// Explicitly materialize one line. Rendering uses visible_line instead.
    pub fn line(&self, row: usize) -> String {
        self.text.read(self.line_start(row)..self.line_end(row))
    }
    /// The zero-based line containing `offset`.
    pub fn row_at(&self, offset: usize) -> usize {
        self.text.row_at(offset)
    }
    /// The display column of `offset` within its line.
    pub fn column_at(&self, offset: usize) -> usize {
        self.text
            .columns(self.line_start(self.row_at(offset))..offset)
    }
    /// The caret's `[column, row]`.
    pub fn position(&self) -> [usize; 2] {
        [self.column_at(self.cursor), self.row_at(self.cursor)]
    }
    /// The display width of line `row`.
    pub fn line_columns(&self, row: usize) -> usize {
        self.text.columns(self.line_start(row)..self.line_end(row))
    }
    /// The offset of the character covering `column` on line `row`.
    pub fn offset(&self, row: usize, column: usize) -> usize {
        let row = row.min(self.line_count() - 1);
        self.text
            .offset_at_column(self.line_start(row)..self.line_end(row), column)
    }
    /// Return only the scalars intersecting the requested horizontal viewport, each as its
    /// offset, column, width in cells, and the scalar. Combining scalars have width zero.
    pub fn visible_line(
        &self,
        row: usize,
        column: usize,
        width: usize,
    ) -> Vec<(usize, usize, usize, char)> {
        let line_start = self.line_start(row);
        let end = self.line_end(row);
        let start = if column == 0 {
            line_start
        } else {
            self.text.offset_at_column(line_start..end, column)
        };
        let mut x = self.text.columns(line_start..start);
        let mut visible = Vec::with_capacity(width);
        let mut measure = Columns::default();
        for (base, chunk) in self.text.chunks(start..end) {
            for (byte, ch) in chunk.char_indices() {
                let cells = cell_width(ch, x, &mut measure);
                if x >= column.saturating_add(width) && cells > 0 {
                    return visible;
                }
                visible.push((base + byte, x, cells, ch));
                x += cells;
            }
        }
        if x < column.saturating_add(width) {
            visible.push((end, x, 1, ' '));
        }
        visible
    }
    /// The selected byte range, in ascending order.
    pub fn selection(&self) -> Range<usize> {
        self.cursor.min(self.anchor)..self.cursor.max(self.anchor)
    }
    /// Copy the selected text into a `String`.
    pub fn selected_text(&self) -> String {
        self.text.read(self.selection())
    }
    /// Stream the selected text without first copying it into memory.
    pub fn write_selection(&self, out: &mut dyn Write) -> io::Result<()> {
        self.text
            .read_chunks(self.selection(), |_, chunk| out.write_all(chunk.as_bytes()))
    }
    /// Select the whole document.
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
        self.reveal = true;
    }
    /// Move the caret to the nearest valid position, keeping the anchor if `extend` is set.
    pub fn set_cursor(&mut self, offset: usize, extend: bool) {
        self.cursor = self.text.boundary(offset);
        if self.cursor > 0
            && self.text.byte(self.cursor) == Some(b'\n')
            && self.text.byte(self.cursor - 1) == Some(b'\r')
        {
            self.cursor -= 1;
        }
        if !extend {
            self.anchor = self.cursor;
        }
        self.preferred_column = None;
        self.reveal = true;
    }
    /// The start of the grapheme cluster before `offset`. A line ending is one cluster.
    fn previous(&self, offset: usize) -> usize {
        let start = self.text.boundary(offset.saturating_sub(CLUSTER_WINDOW));
        let window = self.text.read(start..offset);
        offset - window.graphemes(true).next_back().map_or(0, str::len)
    }
    /// The end of the grapheme cluster starting at `offset`.
    fn next(&self, offset: usize) -> usize {
        let end = self.text.boundary(offset + CLUSTER_WINDOW);
        let window = self.text.read(offset..end);
        offset + window.graphemes(true).next().map_or(0, str::len)
    }
    /// The whole grapheme cluster starting at `offset`.
    pub fn cluster_at(&self, offset: usize) -> String {
        self.text.read(offset..self.next(offset))
    }
    /// Move the caret up or down `delta` lines, keeping its preferred column.
    pub fn move_vertical(&mut self, delta: isize, extend: bool) {
        if let Some(width) = self.wrap_width {
            return self.move_vertical_wrapped(delta, extend, width);
        }
        let [column, row] = self.position();
        let column = self.preferred_column.unwrap_or(column);
        self.set_cursor(
            self.offset(
                row.saturating_add_signed(delta).min(self.line_count() - 1),
                column,
            ),
            extend,
        );
        self.preferred_column = Some(column);
    }
    fn move_vertical_wrapped(&mut self, delta: isize, extend: bool, width: usize) {
        let [column, line] = self.position();
        let mut row = wrap::containing(self, line, column, width);
        let x = self.preferred_column.unwrap_or(column - row.start);
        for _ in 0..delta.unsigned_abs() {
            let step = if delta < 0 {
                wrap::previous(self, row, width)
            } else {
                wrap::next(self, row, width)
            };
            let Some(step) = step else {
                break;
            };
            row = step;
        }
        self.set_cursor(wrap::offset_on_row(self, row, row.start + x, width), extend);
        self.preferred_column = Some(x);
    }
    /// Move the caret to the start of a line, counting from one, and scroll that line to the
    /// top of the view, or as near as the end of the file allows.
    pub(super) fn go_to_line(&mut self, line: usize) {
        self.set_cursor(self.line_start(line.saturating_sub(1)), false);
        self.scroll = [0, self.row_at(self.cursor)];
    }
    /// Where Home goes: the start of the row on screen when wrapping, else of the line.
    pub(super) fn home(&self) -> usize {
        let [column, line] = self.position();
        match self.wrap_width {
            Some(width) => self.offset(line, wrap::containing(self, line, column, width).start),
            None => self.line_start(line),
        }
    }
    /// Where End goes: the end of the row on screen when wrapping, else of the line.
    pub(super) fn end(&self) -> usize {
        let [column, line] = self.position();
        match self.wrap_width {
            Some(width) => {
                let row = wrap::containing(self, line, column, width);
                wrap::offset_on_row(self, row, wrap::end(self, row, width), width)
            }
            None => self.line_end(line),
        }
    }
    /// Move the caret one character or word, or collapse the selection.
    pub fn move_horizontal(&mut self, right: bool, word: bool, extend: bool) {
        let selected = self.selection();
        let offset = if !extend && !selected.is_empty() {
            if right { selected.end } else { selected.start }
        } else if word {
            self.word_edge(right)
        } else if right {
            self.next(self.cursor)
        } else {
            self.previous(self.cursor)
        };
        self.set_cursor(offset, extend);
    }
    /// The offset one word from the caret, in the given direction.
    pub fn word_edge(&self, right: bool) -> usize {
        let mut whitespace = false;
        if right {
            for (base, chunk) in self.text.chunks(self.cursor..self.text.len()) {
                for (i, ch) in chunk.char_indices() {
                    if ch.is_whitespace() {
                        whitespace = true;
                    } else if whitespace {
                        return base + i;
                    }
                }
            }
            self.text.len()
        } else {
            for (base, chunk) in self.text.chunks_rev(0..self.cursor) {
                for (i, ch) in chunk.char_indices().rev() {
                    if !ch.is_whitespace() {
                        whitespace = true;
                    } else if whitespace {
                        return base + i + ch.len_utf8();
                    }
                }
            }
            0
        }
    }
    /// Add typed text. A run of typing is one undo step, ending at each new word.
    pub fn type_text(&mut self, text: &str) {
        let at_word_start = !text.starts_with(char::is_whitespace)
            && self
                .text
                .char_at(self.text.previous(self.cursor))
                .is_some_and(char::is_whitespace);
        self.joins_last_step = self.typing_end == Some(self.cursor)
            && self.anchor == self.cursor
            && !at_word_start
            && !self.undo.is_empty();
        self.insert(text);
        self.joins_last_step = false;
        self.typing_end = Some(self.cursor);
    }
    /// `text` with its line endings converted to the document's.
    pub(super) fn normalized_text(&self, text: &str) -> String {
        if self.encoding == Encoding::Bytes {
            return text.to_owned();
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if self.crlf {
            text.replace('\n', "\r\n")
        } else {
            text
        }
    }
    /// Select a range and bring it into view.
    pub(super) fn select_range(&mut self, range: Range<usize>) {
        self.set_cursor(range.end, false);
        self.anchor = range.start;
    }
    /// Take on text built elsewhere from this document's, as one undo step.
    pub(super) fn replace_whole_text(&mut self, text: TextBuffer) {
        self.checkpoint();
        self.text = text;
        self.cursor = self.text.boundary(self.cursor.min(self.text.len()));
        self.anchor = self.cursor;
        self.changed();
    }
    /// Replace the selection with `text`, converting line endings to the document's.
    pub fn insert(&mut self, text: &str) {
        let text = &self.normalized_text(text);
        let range = self.selection();
        if self.text.equals(range.clone(), text) {
            return;
        }
        // Large pastes stay on disk, as undo may keep them for a long time.
        let inserted = if text.len() >= LARGE_INSERT_BYTES {
            TextBuffer::from_text_on_disk(text).unwrap_or_else(|_| TextBuffer::from_text(text))
        } else {
            TextBuffer::from_text(text)
        };
        self.replace(range, &inserted);
    }
    /// Start a new line, carrying over the current line's indentation.
    pub fn newline(&mut self) {
        let range = self.selection();
        let start = self.line_start(self.row_at(range.start));
        let mut end = start;
        'indent: for (base, chunk) in self.text.chunks(start..range.start) {
            for (byte, ch) in chunk.char_indices() {
                if !matches!(ch, ' ' | '\t') {
                    break 'indent;
                }
                end = base + byte + ch.len_utf8();
            }
        }
        let mut inserted = TextBuffer::from_text(if self.crlf { "\r\n" } else { "\n" });
        inserted.append(&self.text.slice(start..end));
        self.replace(range, &inserted);
    }
    /// Indent the selected lines, or outdent them; with no selection and no outdent, insert
    /// spaces to the next tab stop.
    pub fn indent(&mut self, outdent: bool) -> io::Result<()> {
        let selected = self.selection();
        if selected.is_empty() && !outdent {
            self.insert(&" ".repeat(TAB_WIDTH - self.position()[0] % TAB_WIDTH));
            return Ok(());
        }
        let first = self.row_at(selected.start);
        let last = self.row_at(if selected.is_empty() {
            selected.end
        } else {
            self.text.previous(selected.end)
        });
        let range = self.line_start(first)..self.line_end(last);
        let mut file = BufWriter::with_capacity(64 * 1024, tempfile::tempfile()?);
        let mut at_start = true;
        let mut removed = 0;
        let mut skipped = 0;
        let mut bytes = [0; 4];
        for (_, chunk) in self.text.chunks(range.clone()) {
            for ch in chunk.chars() {
                if at_start {
                    if !outdent {
                        file.write_all(b"    ")?;
                        at_start = false;
                    } else if ch == ' ' && skipped < TAB_WIDTH {
                        skipped += 1;
                        removed += 1;
                        continue;
                    } else if ch == '\t' && skipped == 0 {
                        removed += 1;
                        at_start = false;
                        continue;
                    } else {
                        at_start = false;
                    }
                }
                file.write_all(ch.encode_utf8(&mut bytes).as_bytes())?;
                if ch == '\n' {
                    at_start = true;
                    skipped = 0;
                }
            }
        }
        if outdent && removed == 0 {
            return Ok(());
        }
        let mapped = Snapshot::seal(file.into_inner().map_err(|e| e.into_error())?)?;
        let inserted = TextBuffer::from_snapshot(mapped, 0, |_| Ok(()))?;
        let old_cursor = self.cursor;
        let start = range.start;
        let length = inserted.len();
        self.replace(range, &inserted);
        if selected.is_empty() {
            self.set_cursor(old_cursor.saturating_sub(removed).max(start), false);
        } else {
            self.anchor = start;
            self.cursor = start + length;
        }
        Ok(())
    }
    /// Select the run of word, space or symbol characters around `offset`.
    pub fn select_word(&mut self, offset: usize) {
        match self.word_range(offset) {
            Some(range) => {
                self.set_cursor(range.end, false);
                self.anchor = range.start;
            }
            None => self.set_cursor(offset, false),
        }
    }
    /// The run of similar characters around `offset`: word characters, spaces or symbols.
    fn word_range(&self, offset: usize) -> Option<Range<usize>> {
        let offset = self.text.boundary(offset);
        let class = classify(self.text.char_at(offset)?);
        let mut start = offset;
        let mut end = offset;
        'back: for (base, chunk) in self.text.chunks_rev(0..offset) {
            for (i, c) in chunk.char_indices().rev() {
                if matches!(c, '\r' | '\n') || classify(c) != class {
                    break 'back;
                }
                start = base + i;
            }
        }
        'forward: for (base, chunk) in self.text.chunks(offset..self.text.len()) {
            for (i, c) in chunk.char_indices() {
                if matches!(c, '\r' | '\n') || classify(c) != class {
                    break 'forward;
                }
                end = base + i + c.len_utf8();
            }
        }
        Some(start..end)
    }
    /// Up to a short way of the text on `offset`'s line before it, enough to see a qualifier.
    pub(super) fn text_before(&self, offset: usize) -> String {
        let line_start = self.line_start(self.row_at(offset));
        let start = self
            .text
            .boundary(offset.saturating_sub(MAX_IDENTIFIER_BYTES).max(line_start));
        self.text.read(start..offset)
    }
    /// The range of the identifier at `offset`, if there is one to look up: bare numbers are
    /// words but not names. A pathological one is too long to be a symbol, and is not read.
    pub(super) fn identifier_at(&self, offset: usize) -> Option<Range<usize>> {
        let range = self.word_range(offset)?;
        let is_name = classify(self.text.char_at(range.start)?) == WORD_CLASS
            && range.len() <= MAX_IDENTIFIER_BYTES
            && !self
                .text
                .read(range.clone())
                .chars()
                .all(|c| c.is_ascii_digit());
        is_name.then_some(range)
    }
    /// Delete the selection, or else one character or word next to the caret.
    pub fn delete(&mut self, backwards: bool, word: bool) {
        let mut range = self.selection();
        if range.is_empty() {
            if backwards {
                range.start = if word {
                    self.word_edge(false)
                } else {
                    self.previous(self.cursor)
                };
            } else {
                range.end = if word {
                    self.word_edge(true)
                } else {
                    self.next(self.cursor)
                };
            }
        }
        if !range.is_empty() {
            self.replace(range, &TextBuffer::new());
        }
    }
    fn state(&self) -> State {
        State {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
            encoding: self.encoding,
            bom: self.bom,
        }
    }
    fn checkpoint(&mut self) {
        if self.joins_last_step {
            return;
        }
        self.undo.push_back(self.state());
        self.redo.clear();
        if self.undo.len() > HISTORY_STATES {
            self.undo.pop_front();
        }
    }
    fn changed(&mut self) {
        self.update_dirty();
        self.tail = None;
        self.typing_end = None;
        self.preferred_column = None;
        self.reveal = true;
    }
    fn update_dirty(&mut self) {
        self.dirty = self.encoding != self.saved_encoding
            || self.bom != self.saved_bom
            || !self.text.same_text(&self.saved_text);
    }
    fn replace(&mut self, range: Range<usize>, inserted: &TextBuffer) {
        self.checkpoint();
        self.cursor = range.start + inserted.len();
        self.anchor = self.cursor;
        self.text.replace(range, inserted);
        self.changed();
    }
    fn restore(&mut self, state: State) {
        self.text = state.text;
        self.cursor = state.cursor;
        self.anchor = state.anchor;
        self.encoding = state.encoding;
        self.bom = state.bom;
        self.changed();
    }
    /// Restore the state before the last edit.
    pub fn undo(&mut self) {
        if let Some(state) = self.undo.pop_back() {
            self.redo.push(self.state());
            self.restore(state);
        }
    }
    /// Reapply the last undone edit.
    pub fn redo(&mut self) {
        if let Some(state) = self.redo.pop() {
            self.undo.push_back(self.state());
            self.restore(state);
        }
    }
    pub(crate) fn prepare_save(&self, destination: Option<&Path>) -> io::Result<SaveRequest> {
        let path = destination
            .or(self.path.as_deref())
            .ok_or_else(|| io::Error::other("Choose a path with Save As"))?;
        let parent = crate::paths::canonicalize(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?;
        let path = parent.join(
            path.file_name()
                .ok_or_else(|| io::Error::other("Choose a file name"))?,
        );
        let same = self.path.as_ref() == Some(&path);
        if !same && fs::symlink_metadata(&path).is_ok() {
            return Err(io::Error::other(
                "That path already exists; choose a new file name",
            ));
        }
        Ok(SaveRequest {
            destination: path,
            expected: if same { self.disk_hash } else { None },
            unchanged: if !self.dirty() {
                self.original.clone()
            } else {
                None
            },
            text: self.text.clone(),
            encoding: self.encoding,
            bom: self.bom,
        })
    }
    pub(crate) fn saved(&mut self, saved: Saved) {
        let original_unchanged = saved.text.same_text(&self.saved_text)
            && saved.encoding == self.saved_encoding
            && saved.bom == self.saved_bom;
        self.path = Some(saved.path);
        self.disk_hash = Some(saved.hash);
        self.saved_text = saved.text;
        self.saved_encoding = saved.encoding;
        self.saved_bom = saved.bom;
        if !original_unchanged {
            self.original = None;
        }
        self.tail = None;
        self.update_dirty();
    }
    /// Synchronous API for embedding/tests. The desktop host runs the same save
    /// request on a worker; edits made during that save retain their dirty state.
    pub fn save(&mut self, destination: Option<&Path>) -> io::Result<()> {
        let saved = self.prepare_save(destination)?.run(&Progress::default())?;
        self.saved(saved);
        Ok(())
    }
}

const WORD_CLASS: u8 = 0;
/// Which kind of run a character belongs to, for selecting words.
fn classify(character: char) -> u8 {
    if character.is_alphanumeric() || character == '_' {
        WORD_CLASS
    } else if character.is_whitespace() {
        1
    } else {
        2
    }
}

/// Cells `ch` adds when drawn at `column`: tabs depend on the column, and `measure` tracks
/// the scalars before it. See [`Columns`].
pub fn cell_width(ch: char, column: usize, measure: &mut Columns) -> usize {
    let width = measure.next(ch) as usize;
    if ch == '\t' {
        TAB_WIDTH - column % TAB_WIDTH
    } else {
        width
    }
}
/// The display width of `text`, starting at column zero.
pub fn columns(text: &str) -> usize {
    let mut measure = Columns::default();
    text.chars()
        .fold(0, |n, c| n + cell_width(c, n, &mut measure))
}
