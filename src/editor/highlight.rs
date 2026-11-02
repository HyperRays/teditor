//! Asynchronous syntect parsing. The UI draws cached, immutable viewport colors;
//! regexes, grammar detection and parser checkpoints belong to one shared worker.
use super::{
    buffer::{Line, TextBuffer},
    document::Document,
    encoding::Encoding,
};
use crate::renderer::Color;
use std::{
    collections::{BTreeMap, HashMap},
    ops::Range,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Sender},
    },
    time::{Duration, Instant},
};
use syntect::{
    highlighting::{
        Color as SyntaxColor, HighlightIterator, HighlightState, Highlighter, ScopeSelectors,
        StyleModifier, Theme, ThemeItem, ThemeSettings,
    },
    parsing::{ParseState, ScopeStack, SyntaxSet},
};

/// Code colors, taken from the interface's so that the two read as one.
fn syntax_theme() -> Theme {
    let colors = crate::ui::Theme::default();
    let convert = |Color(r, g, b, a): Color| SyntaxColor { r, g, b, a };
    let item = |selectors: &str, color| ThemeItem {
        scope: selectors.parse::<ScopeSelectors>().unwrap(),
        style: StyleModifier {
            foreground: Some(convert(color)),
            ..Default::default()
        },
    };
    Theme {
        settings: ThemeSettings {
            foreground: Some(convert(colors.text)),
            background: Some(convert(colors.panel)),
            ..Default::default()
        },
        scopes: vec![
            item("comment, punctuation.definition.comment", colors.faint),
            item("keyword, storage", colors.danger),
            item("string", Color::rgb(0x94, 0xfa, 0x87)),
            item("constant", Color::rgb(0xfa, 0xa6, 0x7f)),
            item("entity.name, support.type, support.class", colors.accent),
            item(
                "entity.name.function, support.function",
                Color::rgb(0x8a, 0xb0, 0xcf),
            ),
        ],
        ..Default::default()
    }
}

const PREFETCH_ROWS: usize = 64;
const CHECKPOINT_ROWS: usize = 128;
const MAX_CHECKPOINTS: usize = 256;
// Syntect needs a contiguous whole line. Never materialize a giant/minified
// line or pretend its truncated prefix establishes the following parser state.
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_COLOR_SPANS: usize = 65_536;

#[derive(Clone)]
struct Request {
    generation: u64,
    text: TextBuffer,
    path: Option<PathBuf>,
    rows: Range<usize>,
}
#[derive(Debug)]
struct Span {
    start: usize,
    end: usize,
    color: Color,
}
struct Colors {
    generation: u64,
    rows: Range<usize>,
    spans: Vec<Span>,
}
impl Colors {
    fn plain(request: &Request) -> Self {
        Self {
            generation: request.generation,
            rows: request.rows.clone(),
            spans: Vec::new(),
        }
    }
    fn at(&self, byte: usize) -> Option<Color> {
        self.spans
            .get(self.spans.partition_point(|span| span.end <= byte))
            .filter(|span| span.start <= byte)
            .map(|span| span.color)
    }
    fn push(&mut self, start: usize, end: usize, color: Color) {
        if start >= end {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.end == start
            && last.color == color
        {
            last.end = end;
        } else {
            self.spans.push(Span { start, end, color });
        }
    }
    /// Keep presentation colors attached to the surviving text while its new
    /// syntax is being parsed. New text inherits the adjacent token's color.
    /// These spans are provisional; they must never count as a completed parse.
    fn rebase(&mut self, old: &TextBuffer, new: &TextBuffer) {
        let start = old.boundary(old.common_prefix_len(new));
        let mut suffix = old
            .common_suffix_len(new)
            .min(old.len().min(new.len()) - start);
        // A common byte suffix can begin inside two different UTF-8 scalars.
        // Treat those scalars as replaced, rather than splitting their colors.
        while suffix > 0
            && (old.boundary(old.len() - suffix) != old.len() - suffix
                || new.boundary(new.len() - suffix) != new.len() - suffix)
        {
            suffix -= 1;
        }
        let old_end = old.len() - suffix;
        let new_end = new.len() - suffix;
        let inherited = if old_end > start || start == 0 || old.byte(start - 1) == Some(b'\n') {
            self.at(start)
                .or_else(|| start.checked_sub(1).and_then(|b| self.at(b)))
        } else {
            self.at(start - 1).or_else(|| self.at(start))
        };
        let spans = std::mem::take(&mut self.spans);
        self.spans.reserve(spans.len() + 2);
        for span in &spans {
            if span.start >= start {
                break;
            }
            self.push(span.start, span.end.min(start), span.color);
        }
        if let Some(color) = inherited {
            self.push(start, new_end, color);
        }
        for span in &spans {
            if span.end <= old_end {
                continue;
            }
            self.push(
                new_end + (span.start.max(old_end) - old_end),
                new_end + (span.end - old_end),
                span.color,
            );
        }
    }
}
#[derive(Default)]
struct Mailbox {
    request: Mutex<Option<Request>>,
    result: Mutex<Option<Colors>>,
    generation: AtomicU64,
    queued: AtomicBool,
}
enum Work {
    Render(u64, Arc<Mailbox>),
    Close(u64),
}

/// Retains just the current viewport plus a small scroll margin on the UI side.
#[derive(Default)]
pub(super) struct Highlighting {
    client: Option<(u64, Arc<Mailbox>, Sender<Work>)>,
    snapshot: Option<TextBuffer>,
    path: Option<PathBuf>,
    requested: Option<Range<usize>>,
    colors: Option<Colors>,
    colors_current: bool,
    unavailable: bool,
}
impl Highlighting {
    pub fn pending(&self) -> bool {
        self.requested.is_some()
    }

    pub fn update(&mut self, doc: &Document, visible: Range<usize>) {
        if doc.encoding() == Encoding::Bytes {
            // Raw bytes have no programming-language semantics, even for a .rs name.
            self.cancel();
            self.snapshot = None;
            self.colors = None;
            self.colors_current = false;
            return;
        }
        let changed = self
            .snapshot
            .as_ref()
            .is_none_or(|old| !old.same_snapshot(&doc.text))
            || self.path != doc.path;
        if changed {
            self.cancel();
            // Preserve the display while invalidating the parser result. Byte
            // ranges follow insertions/deletions, including changes in line count.
            if let (Some(old), Some(colors)) = (&self.snapshot, &mut self.colors)
                && self.path == doc.path
            {
                colors.rebase(old, &doc.text);
            } else {
                self.colors = None;
            }
            self.snapshot = Some(doc.text.clone());
            self.path = doc.path.clone();
            self.colors_current = false;
        }
        if let Some((_, mailbox, _)) = &self.client {
            let result = mailbox.result.lock().unwrap().take();
            if let Some(result) = result
                && result.generation == mailbox.generation.load(Ordering::Relaxed)
            {
                self.colors = Some(result);
                self.colors_current = true;
                self.requested = None;
            }
        }
        if self.colors_current
            && self
                .colors
                .as_ref()
                .is_some_and(|c| covers(&c.rows, &visible))
        {
            // A scroll back into cached rows need not wait for an obsolete request.
            if self
                .requested
                .as_ref()
                .is_some_and(|r| !covers(r, &visible))
            {
                self.cancel();
            }
            return;
        }
        if self.unavailable || self.requested.as_ref().is_some_and(|r| covers(r, &visible)) {
            return;
        }
        if self.client.is_none() {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let Some(queue) = worker() else {
                self.unavailable = true;
                return;
            };
            self.client = Some((NEXT.fetch_add(1, Ordering::Relaxed), Arc::default(), queue));
        }
        let (id, mailbox, queue) = self.client.as_ref().unwrap();
        let rows = visible.start.saturating_sub(PREFETCH_ROWS)
            ..visible
                .end
                .saturating_add(PREFETCH_ROWS)
                .min(doc.line_count());
        let generation = mailbox.generation.fetch_add(1, Ordering::Relaxed) + 1;
        *mailbox.request.lock().unwrap() = Some(Request {
            generation,
            text: doc.text.clone(),
            path: doc.path.clone(),
            rows: rows.clone(),
        });
        self.requested = Some(rows);
        if !mailbox.queued.swap(true, Ordering::Relaxed)
            && queue.send(Work::Render(*id, mailbox.clone())).is_err()
        {
            self.requested = None;
            self.unavailable = true;
        }
    }
    fn cancel(&mut self) {
        if let Some((_, mailbox, _)) = &self.client {
            mailbox.generation.fetch_add(1, Ordering::Relaxed);
            mailbox.request.lock().unwrap().take();
        }
        self.requested = None;
    }
    pub fn color(&self, byte: usize, fallback: Color) -> Color {
        self.colors
            .as_ref()
            .and_then(|colors| colors.at(byte))
            .unwrap_or(fallback)
    }
}
impl Drop for Highlighting {
    fn drop(&mut self) {
        self.cancel();
        if let Some((id, _, queue)) = &self.client {
            let _ = queue.send(Work::Close(*id));
        }
    }
}
fn covers(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    outer.start <= inner.start && outer.end >= inner.end
}

fn worker() -> Option<Sender<Work>> {
    static QUEUE: OnceLock<Option<Sender<Work>>> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            let (send, receive) = mpsc::channel::<Work>();
            let reschedule = send.clone();
            std::thread::Builder::new()
                .name("teditor-syntax".into())
                .spawn(move || {
                    // Definitions and lazy regex compilation never run on the UI thread.
                    let syntaxes = two_face::syntax::extra_newlines();
                    let theme = &syntax_theme();
                    let mut engines = HashMap::<u64, Engine>::new();
                    while let Ok(work) = receive.recv() {
                        let (id, mailbox) = match work {
                            Work::Render(id, mailbox) => (id, mailbox),
                            Work::Close(id) => {
                                engines.remove(&id);
                                continue;
                            }
                        };
                        mailbox.queued.store(false, Ordering::Relaxed);
                        let Some(request) = mailbox.request.lock().unwrap().take() else {
                            continue;
                        };
                        let cancelled =
                            || mailbox.generation.load(Ordering::Relaxed) != request.generation;
                        if cancelled() {
                            continue;
                        }
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            engines
                                .entry(id)
                                .or_default()
                                .render(&request, &syntaxes, theme, cancelled)
                        }));
                        if cancelled() {
                            continue;
                        }
                        match result {
                            Ok(Some(colors)) => *mailbox.result.lock().unwrap() = Some(colors),
                            Ok(None) => {
                                // Yield during a long seek so another tab's request can run.
                                let mut pending = mailbox.request.lock().unwrap();
                                if pending.is_none() && !cancelled() {
                                    *pending = Some(request.clone());
                                    if !mailbox.queued.swap(true, Ordering::Relaxed) {
                                        let _ = reschedule.send(Work::Render(id, mailbox.clone()));
                                    }
                                }
                            }
                            Err(_) => {
                                engines.remove(&id);
                                *mailbox.result.lock().unwrap() = Some(Colors::plain(&request));
                            }
                        }
                    }
                })
                .ok()?;
            Some(send)
        })
        .clone()
}

#[derive(Clone)]
struct Checkpoint {
    /// Where this row starts, so resuming a scan need not look it up in the mapping.
    /// Edits discard every checkpoint after them, which keeps it valid.
    offset: usize,
    parse: ParseState,
    highlight: HighlightState,
}
#[derive(Default)]
struct Engine {
    text: TextBuffer,
    syntax: Option<usize>,
    checkpoints: BTreeMap<usize, Checkpoint>,
    plain_from: Option<usize>,
    #[cfg(test)]
    parsed_lines: usize,
}
impl Engine {
    fn checkpoint(&mut self, row: usize, state: &Checkpoint) {
        self.checkpoints.insert(row, state.clone());
        if self.checkpoints.len() > MAX_CHECKPOINTS {
            // Keep the initial state and the most recently reached region.
            let farthest = *self
                .checkpoints
                .keys()
                .filter(|&&r| r != 0 && r != row)
                .max_by_key(|&&r| r.abs_diff(row))
                .unwrap();
            self.checkpoints.remove(&farthest);
        }
    }
    fn render(
        &mut self,
        request: &Request,
        syntaxes: &SyntaxSet,
        theme: &Theme,
        cancelled: impl Fn() -> bool,
    ) -> Option<Colors> {
        let syntax = detect(request, syntaxes);
        if self.syntax != syntax {
            self.checkpoints.clear();
            self.plain_from = None;
        } else if !self.text.same_snapshot(&request.text) {
            let first = self.text.common_prefix_len(&request.text);
            let row = request.text.row_at(first);
            self.checkpoints.split_off(&row.saturating_add(1));
            if self.plain_from.is_some_and(|failed| row <= failed) {
                self.plain_from = None;
            }
        }
        self.text = request.text.clone();
        self.syntax = syntax;
        let mut colors = Colors::plain(request);
        let Some(syntax) = syntax else {
            return Some(colors);
        };
        if self.plain_from.is_some_and(|row| row <= request.rows.start) {
            return Some(colors);
        }
        let highlighter = Highlighter::new(theme);
        if self.checkpoints.is_empty() {
            self.checkpoints.insert(
                0,
                Checkpoint {
                    offset: 0,
                    parse: ParseState::new(&syntaxes.syntaxes()[syntax]),
                    highlight: HighlightState::new(&highlighter, ScopeStack::new()),
                },
            );
        }
        let (&first, state) = self
            .checkpoints
            .range(..=request.rows.start)
            .next_back()
            .unwrap();
        let mut state = state.clone();
        let deadline = Instant::now() + Duration::from_millis(8);
        // Colors depend on every earlier line, so a long seek scans them all; reading
        // them through one block keeps that from pulling the file into memory.
        let text = self.text.clone();
        let mut lines = text.lines_from(state.offset);
        for row in first..request.rows.end {
            if cancelled() {
                self.checkpoint(row, &state);
                return None;
            }
            if row < request.rows.start && Instant::now() >= deadline {
                self.checkpoint(row, &state);
                return None;
            }
            if row == request.rows.start {
                self.checkpoint(row, &state);
            }
            let line = match lines.next_line(MAX_LINE_BYTES) {
                Line::Text(line) if self.plain_from.is_none_or(|r| row < r) => line,
                Line::End => break,
                _ => {
                    self.plain_from = Some(row);
                    break;
                }
            };
            #[cfg(test)]
            {
                self.parsed_lines += 1;
            }
            let Ok(ops) = state.parse.parse_line(&line, syntaxes) else {
                self.plain_from = Some(row);
                break;
            };
            let mut byte = state.offset;
            for (style, token) in
                HighlightIterator::new(&mut state.highlight, &ops, &line, &highlighter)
            {
                let start = byte;
                byte += token.len();
                if row < request.rows.start
                    || token.is_empty()
                    || colors.spans.len() >= MAX_COLOR_SPANS
                {
                    continue;
                }
                let fg = style.foreground;
                let color = Color::rgb(fg.r, fg.g, fg.b);
                colors.push(start, byte, color);
            }
            state.offset += line.len();
            if (row + 1) % CHECKPOINT_ROWS == 0 || row + 1 == request.rows.end {
                self.checkpoint(row + 1, &state);
            }
        }
        Some(colors)
    }
}
fn detect(request: &Request, syntaxes: &SyntaxSet) -> Option<usize> {
    let path = request.path.as_deref();
    let syntax = path
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .and_then(|name| syntaxes.find_syntax_by_extension(name))
        .or_else(|| {
            path.and_then(|p| p.extension())
                .and_then(|s| s.to_str())
                .and_then(|extension| syntaxes.find_syntax_by_extension(extension))
        })
        .or_else(|| {
            let end = request.text.boundary(request.text.line_start(1).min(4096));
            syntaxes.find_syntax_by_first_line(&request.text.read(0..end))
        })?;
    if syntax.name == "Plain Text" {
        return None;
    }
    syntaxes
        .syntaxes()
        .iter()
        .position(|s| std::ptr::eq(s, syntax))
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntect::{easy::HighlightLines, util::LinesWithEndings};

    fn definitions() -> (SyntaxSet, Theme) {
        (two_face::syntax::extra_newlines(), syntax_theme())
    }
    fn request(text: &str, name: &str, rows: Range<usize>) -> Request {
        Request {
            generation: 1,
            text: TextBuffer::from_text(text),
            path: Some(name.into()),
            rows,
        }
    }
    fn render(
        engine: &mut Engine,
        request: &Request,
        syntaxes: &SyntaxSet,
        theme: &Theme,
    ) -> Colors {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(colors) = engine.render(request, syntaxes, theme, || false) {
                return colors;
            }
            assert!(Instant::now() < deadline, "syntax seek did not finish");
        }
    }
    fn compare_reference(request: &Request, colors: &Colors, syntaxes: &SyntaxSet, theme: &Theme) {
        let syntax = &syntaxes.syntaxes()[detect(request, syntaxes).unwrap()];
        let mut reference = HighlightLines::new(syntax, theme);
        let source = request.text.to_text();
        let mut byte = 0;
        for (row, line) in LinesWithEndings::from(&source).enumerate() {
            for (style, token) in reference.highlight_line(line, syntaxes).unwrap() {
                if request.rows.contains(&row) {
                    for (offset, _) in token.char_indices() {
                        assert_eq!(
                            colors.at(byte + offset),
                            Some(Color::rgb(
                                style.foreground.r,
                                style.foreground.g,
                                style.foreground.b
                            )),
                            "incorrect syntax at row {row}, byte {}",
                            byte + offset
                        );
                    }
                }
                byte += token.len();
            }
        }
    }
    #[test]
    fn multiline_state_edits_and_undo_match_a_complete_parse() {
        let (syntaxes, theme) = definitions();
        let source = format!(
            "fn main() {{\r\n/* comment\r\n{}*/ let s = \"α\\tβ\";\r\n}}\r\n",
            "inside\r\n".repeat(300)
        );
        let mut request = request(&source, "code.rs", 270..304);
        let original = request.text.clone();
        let mut engine = Engine::default();
        let colors = render(&mut engine, &request, &syntaxes, &theme);
        compare_reference(&request, &colors, &syntaxes, &theme);
        let parsed = engine.parsed_lines;
        request.rows = 280..304;
        let colors = render(&mut engine, &request, &syntaxes, &theme);
        compare_reference(&request, &colors, &syntaxes, &theme);
        assert!(
            engine.parsed_lines - parsed < CHECKPOINT_ROWS,
            "cached scroll reparsed the prefix"
        );
        let start = source.find("/*").unwrap();
        request
            .text
            .replace(start..start + 2, &TextBuffer::from_text("//"));
        let edited = request.text.clone();
        for text in [edited.clone(), original, edited] {
            request.text = text;
            let colors = render(&mut engine, &request, &syntaxes, &theme);
            compare_reference(&request, &colors, &syntaxes, &theme);
        }
        // A late edit preserves the earlier checkpoints, even with shifted UTF-8 bytes.
        let start = request.text.line_start(290);
        request
            .text
            .replace(start..start, &TextBuffer::from_text("/* é */"));
        let parsed = engine.parsed_lines;
        let colors = render(&mut engine, &request, &syntaxes, &theme);
        compare_reference(&request, &colors, &syntaxes, &theme);
        assert!(engine.parsed_lines - parsed < CHECKPOINT_ROWS);
    }
    #[test]
    fn language_detection_handles_names_shebangs_and_plain_files() {
        let (syntaxes, theme) = definitions();
        for (name, source, expected) in [
            ("main.RS", "fn main() {}\n", "Rust"),
            ("Makefile", "all:\n\techo hello\n", "Makefile"),
            (
                "script",
                "#!/usr/bin/env python3\nprint('hello')\n",
                "Python",
            ),
            ("data.json", "{\"value\": 123}\n", "JSON"),
            (
                "solver.f90",
                "program solver\nend program solver\n",
                "Fortran (Modern)",
            ),
            ("Cargo.toml", "[package]\nname = \"demo\"\n", "TOML"),
            ("app.ts", "const value: number = 1;\n", "TypeScript"),
        ] {
            let request = request(source, name, 0..source.lines().count());
            assert_eq!(
                syntaxes.syntaxes()[detect(&request, &syntaxes).unwrap()].name,
                expected
            );
            let colors = render(&mut Engine::default(), &request, &syntaxes, &theme);
            compare_reference(&request, &colors, &syntaxes, &theme);
        }
        for name in ["note.txt", "unknown.extension"] {
            let request = request("ordinary words\n", name, 0..1);
            assert!(detect(&request, &syntaxes).is_none());
            assert!(
                render(&mut Engine::default(), &request, &syntaxes, &theme)
                    .spans
                    .is_empty()
            );
        }
    }
    #[test]
    fn giant_lines_fall_back_without_allocating_or_inventing_parser_state() {
        let (syntaxes, theme) = definitions();
        let source = format!("{}\nfn later() {{}}\n", "x".repeat(1024 * 1024));
        let mut request = request(&source, "large.rs", 0..2);
        let mut engine = Engine::default();
        assert!(
            render(&mut engine, &request, &syntaxes, &theme)
                .spans
                .is_empty()
        );
        assert_eq!(engine.parsed_lines, 0);
        assert_eq!(engine.plain_from, Some(0));
        request
            .text
            .replace(0..1024 * 1024, &TextBuffer::from_text("// shorter"));
        let colors = render(&mut engine, &request, &syntaxes, &theme);
        assert!(engine.plain_from.is_none());
        compare_reference(&request, &colors, &syntaxes, &theme);
    }
    #[test]
    fn stale_results_and_byte_views_cannot_recolor_new_text() {
        let mut highlighting = Highlighting::default();
        let mut doc = Document::new();
        doc.path = Some("code.rs".into());
        doc.insert(&"/* comment\n".repeat(2000));
        highlighting.update(&doc, 1900..1920);
        doc.select_all();
        doc.insert("fn main() { let s = \"α\"; }\n");
        highlighting.update(&doc, 0..2);
        let deadline = Instant::now() + Duration::from_secs(10);
        while highlighting.pending() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
            highlighting.update(&doc, 0..2);
        }
        let (syntaxes, theme) = definitions();
        let request = request(&doc.text.to_text(), "code.rs", 0..2);
        compare_reference(
            &request,
            highlighting.colors.as_ref().unwrap(),
            &syntaxes,
            &theme,
        );
        doc.set_encoding(Encoding::Bytes);
        highlighting.update(&doc, 0..2);
        assert!(!highlighting.pending());
        assert!(highlighting.colors.is_none());
    }
    #[test]
    fn checkpoint_eviction_keeps_reverse_seek_progress() {
        let (syntaxes, theme) = definitions();
        let mut engine = Engine::default();
        let request = request("fn x() {}\n", "code.rs", 0..1);
        render(&mut engine, &request, &syntaxes, &theme);
        let state = engine.checkpoints[&0].clone();
        for row in 1..MAX_CHECKPOINTS * 2 {
            engine.checkpoint(row * 128, &state);
        }
        engine.checkpoint(5, &state);
        assert_eq!(engine.checkpoints.len(), MAX_CHECKPOINTS);
        assert!(engine.checkpoints.contains_key(&0));
        assert!(engine.checkpoints.contains_key(&5));
    }

    #[test]
    fn editing_keeps_token_colors_while_the_worker_is_pending() {
        let (syntaxes, theme) = definitions();
        let mut doc = Document::new();
        doc.path = Some("code.rs".into());
        doc.insert("fn main() {\n    let α = \"hello\";\n    // note\n    let n = 123;\n}\n");
        let request = request(&doc.text.to_text(), "code.rs", 0..doc.line_count());
        let colors = render(&mut Engine::default(), &request, &syntaxes, &theme);
        // A held mailbox makes the intermediate frame deterministic: parsing
        // cannot happen to finish before we inspect the colors shown on screen.
        let (queue, _held_worker) = mpsc::channel();
        let mailbox = Arc::new(Mailbox::default());
        mailbox.generation.store(1, Ordering::Relaxed);
        let mut highlighting = Highlighting {
            client: Some((0, mailbox.clone(), queue)),
            snapshot: Some(doc.text.clone()),
            path: doc.path.clone(),
            colors: Some(colors),
            colors_current: true,
            requested: None,
            unavailable: false,
        };
        let color = |highlighting: &Highlighting, doc: &Document, token: &str| {
            let byte = doc.text.to_text().find(token).unwrap();
            highlighting.color(byte, Color::rgb(1, 2, 3))
        };
        let before =
            ["let α", "hello", "// note", "123"].map(|token| color(&highlighting, &doc, token));
        let byte = doc.text.to_text().find("hello").unwrap() + 2;
        doc.set_cursor(byte, false);
        doc.insert("é");
        highlighting.update(&doc, 0..doc.line_count());
        assert!(highlighting.pending());
        for (token, expected) in ["let α", "heéllo", "// note", "123"]
            .into_iter()
            .zip(before)
        {
            assert_eq!(
                color(&highlighting, &doc, token),
                expected,
                "{token} lost its color while parsing"
            );
        }
        assert_eq!(color(&highlighting, &doc, "é"), before[1]);
        let obsolete = mailbox.request.lock().unwrap().as_ref().unwrap().clone();
        // Another keystroke, line insertion, deletion and undo arrive before
        // the first reparse finishes. Surviving tokens must follow their bytes.
        doc.insert("🙂");
        highlighting.update(&doc, 0..doc.line_count());
        doc.set_cursor(0, false);
        doc.insert("\n\n");
        highlighting.update(&doc, 0..doc.line_count());
        doc.delete(true, false);
        highlighting.update(&doc, 0..doc.line_count());
        doc.undo();
        highlighting.update(&doc, 0..doc.line_count());
        doc.redo();
        highlighting.update(&doc, 0..doc.line_count());
        for (token, expected) in ["let α", "heé🙂llo", "// note", "123"]
            .into_iter()
            .zip(before)
        {
            assert_eq!(
                color(&highlighting, &doc, token),
                expected,
                "{token} lost its color during rapid edits"
            );
        }
        // A semantic change should replace the provisional colors directly,
        // once the newest generation is ready, without accepting older results.
        doc.set_cursor(0, false);
        doc.insert("/*");
        highlighting.update(&doc, 0..doc.line_count());
        *mailbox.result.lock().unwrap() =
            Some(render(&mut Engine::default(), &obsolete, &syntaxes, &theme));
        highlighting.update(&doc, 0..doc.line_count());
        assert!(
            highlighting.pending(),
            "an obsolete parse ended the current request"
        );
        assert_eq!(color(&highlighting, &doc, "123"), before[3]);
        let latest = mailbox.request.lock().unwrap().as_ref().unwrap().clone();
        *mailbox.result.lock().unwrap() =
            Some(render(&mut Engine::default(), &latest, &syntaxes, &theme));
        highlighting.update(&doc, 0..doc.line_count());
        assert!(
            !highlighting.pending(),
            "retained colors suppressed the replacement parse"
        );
        compare_reference(
            &latest,
            highlighting.colors.as_ref().unwrap(),
            &syntaxes,
            &theme,
        );
        assert_ne!(color(&highlighting, &doc, "123"), before[3]);
    }

    #[test]
    fn provisional_colors_keep_utf8_boundaries_and_uncached_gaps() {
        let (syntaxes, theme) = definitions();
        for replacement in ["ê", "ĩ", "🙂", "", "α\r\nβ"] {
            let source = "// before\r\nlet é = \"string\";\r\n// after\r\n";
            let request = request(source, "code.rs", 1..2);
            let mut colors = render(&mut Engine::default(), &request, &syntaxes, &theme);
            let mut edited = request.text.clone();
            let start = source.find('é').unwrap();
            let expected = colors.at(source.find("string").unwrap());
            edited.replace(
                start..start + 'é'.len_utf8(),
                &TextBuffer::from_text(replacement),
            );
            colors.rebase(&request.text, &edited);
            let text = edited.to_text();
            assert_eq!(colors.at(text.find("string").unwrap()), expected);
            assert_eq!(colors.at(0), None);
            assert_eq!(colors.at(text.find("// after").unwrap()), None);
            for span in &colors.spans {
                assert!(text.is_char_boundary(span.start));
                assert!(text.is_char_boundary(span.end));
                assert!(span.start < span.end);
            }
            assert!(
                colors
                    .spans
                    .windows(2)
                    .all(|pair| pair[0].end <= pair[1].start)
            );
            // Edits entirely before the cached region also shift its colors.
            let old = edited.clone();
            edited.replace(0..0, &TextBuffer::from_text("\n🙂\n"));
            colors.rebase(&old, &edited);
            assert_eq!(
                colors.at(edited.to_text().find("string").unwrap()),
                expected
            );
            assert_eq!(
                colors.at(1),
                None,
                "insertion outside the cache invented a token color"
            );
        }
    }
}
