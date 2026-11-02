use super::buffer::TextBuffer;
use super::*;
use crate::terminal::{Cell, input::*};
use find::FindAction;
use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "teditor-editor-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, text).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn frame() -> Framebuffer {
    Framebuffer::new([100, 30], crate::ui::Theme::default().panel).unwrap()
}
fn key(logical_key: Key, primary: bool, shift: bool) -> InputEvent {
    InputEvent::Key(KeyEvent {
        physical_key: PhysicalKey::Code(KeyCode::KeyA),
        key_without_modifiers: Some(logical_key.clone()),
        logical_key,
        location: KeyLocation::Standard,
        state: KeyState::Pressed,
        repeat: false,
        synthetic: false,
        modifiers: Modifiers {
            super_key: primary,
            shift,
            ..Default::default()
        },
        composing: false,
    })
}
fn named(key_: NamedKey) -> InputEvent {
    key(Key::Named(key_), false, false)
}
fn cmd(ch: &str) -> InputEvent {
    key(Key::Character(ch.into()), true, false)
}
fn text(value: &str) -> InputEvent {
    InputEvent::Text(TextInput {
        text: value.into(),
        source: TextSource::Keyboard,
        modifiers: Modifiers::default(),
    })
}
fn pointer(kind: PointerKind, x: f64, y: f64) -> InputEvent {
    InputEvent::Pointer(PointerEvent {
        kind,
        position: Some(PointerPosition {
            physical: [x * 11.0, y * 26.0],
            grid: [x, y],
            cell: Some([x.max(0.0) as u32, y.max(0.0) as u32]),
        }),
        modifiers: Modifiers::default(),
    })
}
fn send(
    editor: &mut Editor,
    frame: &mut Framebuffer,
    events: impl IntoIterator<Item = InputEvent>,
) {
    for event in events {
        editor.handle(event);
    }
    editor.compose(frame);
    wait_for_io(editor, frame);
}
fn wait_for_io(editor: &mut Editor, frame: &mut Framebuffer) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while editor.has_pending_io() {
        assert!(
            std::time::Instant::now() < deadline,
            "file operation timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
        editor.compose(frame);
    }
}
fn wait_for_background(editor: &mut Editor, frame: &mut Framebuffer) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while editor.has_pending_background_work() {
        assert!(
            std::time::Instant::now() < deadline,
            "background work timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
        editor.compose(frame);
    }
}
fn wheel(x: f64, y: f64, lines: f64) -> InputEvent {
    InputEvent::Scroll(ScrollEvent {
        delta: MouseScrollDelta::LineDelta(0.0, lines as f32),
        cells: [0.0, lines],
        phase: TouchPhase::Moved,
        position: Some(PointerPosition {
            physical: [x * 11.0, y * 26.0],
            grid: [x, y],
            cell: Some([x as u32, y as u32]),
        }),
        modifiers: Modifiers::default(),
    })
}
fn row(frame: &Framebuffer, row: u32) -> String {
    (0..frame.size()[0])
        .map(|x| frame.cell(x, row).unwrap_or(&Cell::default()).character)
        .collect()
}

const TECHNOLOGIST: &str = "\u{1F469}\u{200D}\u{1F4BB}";
const FLAG: &str = "\u{1F1E9}\u{1F1EA}";
const THUMB: &str = "\u{1F44D}\u{1F3FD}";
const ACCENTED: &str = "e\u{301}";

#[test]
fn returning_to_the_saved_text_by_any_route_counts_as_unchanged() {
    let fixture = Fixture::new();
    let path = fixture.file("a.txt", "one two");
    let mut doc = Document::open(&path).unwrap();
    assert!(!doc.dirty());
    // By undo and redo.
    doc.insert("x");
    assert!(doc.dirty());
    doc.undo();
    assert!(!doc.dirty());
    doc.redo();
    assert!(doc.dirty());
    doc.undo();
    // By editing it back, with no undo at all.
    doc.set_cursor(3, false);
    doc.insert("!");
    assert!(doc.dirty());
    doc.delete(true, false);
    assert!(!doc.dirty(), "typing then deleting is no change");
    // By replacing it with identical text of the same length, as a different edit.
    doc.select_all();
    doc.insert("one tw0");
    assert!(doc.dirty());
    doc.select_all();
    doc.insert("one two");
    assert!(!doc.dirty());
    // Saving unchanged text keeps the file byte for byte, from the original.
    doc.save(None).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"one two");
    // And the saved state moves: what was the original is now the change.
    doc.select_all();
    doc.insert("new");
    doc.save(None).unwrap();
    assert!(!doc.dirty());
    doc.undo();
    assert!(doc.dirty());
    doc.redo();
    assert!(!doc.dirty());
}

#[test]
fn typing_groups_into_one_undo_step_per_word() {
    let mut doc = Document::new();
    for ch in "hello big world".chars() {
        doc.type_text(&ch.to_string());
    }
    doc.undo();
    assert_eq!(doc.text.to_text(), "hello big ");
    doc.undo();
    assert_eq!(doc.text.to_text(), "hello ");
    doc.redo();
    assert_eq!(doc.text.to_text(), "hello big ");
    doc.undo();
    doc.undo();
    assert!(doc.text.is_empty() && !doc.dirty());
    // Moving the caret or any other edit starts a new step.
    doc.type_text("ab");
    doc.set_cursor(0, false);
    doc.type_text("c");
    assert_eq!(doc.text.to_text(), "cab");
    doc.undo();
    assert_eq!(doc.text.to_text(), "ab");
}

/// Keep composing until `done`; a followed file is checked ten times a second.
fn wait_for_document(
    editor: &mut Editor,
    frame: &mut Framebuffer,
    done: impl Fn(&Document) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done(editor.active_document().unwrap()) {
        assert!(
            std::time::Instant::now() < deadline,
            "the document never updated: {:?}",
            editor.active_document().unwrap().text.to_text()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        editor.compose(frame);
    }
}
fn append(path: &std::path::Path, bytes: &[u8]) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn an_untouched_file_follows_changes_on_disk() {
    let fixture = Fixture::new();
    let path = fixture.file("log.txt", "first\n");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    // Growth appears by itself, and is not a change to save.
    append(&path, b"second\n");
    wait_for_document(&mut editor, &mut frame, |doc| {
        doc.text.to_text() == "first\nsecond\n"
    });
    assert!(!editor.active_document().unwrap().dirty());
    // A character split across two writes appears only once whole.
    append(&path, &"é".as_bytes()[..1]);
    std::thread::sleep(std::time::Duration::from_millis(250));
    editor.compose(&mut frame);
    assert_eq!(
        editor.active_document().unwrap().text.to_text(),
        "first\nsecond\n"
    );
    append(&path, &"é".as_bytes()[1..]);
    wait_for_document(&mut editor, &mut frame, |doc| {
        doc.text.to_text().ends_with('é')
    });
    // Truncating, as log rotation does, reloads rather than appends.
    fs::write(&path, "new\n").unwrap();
    wait_for_document(&mut editor, &mut frame, |doc| doc.text.to_text() == "new\n");
    // Following continues after a reload.
    append(&path, b"more\n");
    wait_for_document(&mut editor, &mut frame, |doc| {
        doc.text.to_text() == "new\nmore\n"
    });
    // A rewrite that grows is told apart from an append, by what the file now ends with.
    fs::write(&path, "completely different and longer content\n").unwrap();
    wait_for_document(&mut editor, &mut frame, |doc| {
        doc.text.to_text().starts_with("completely")
    });
    // Saving nothing writes what is on disk, including what followed in.
    append(&path, b"tail\n");
    wait_for_document(&mut editor, &mut frame, |doc| {
        doc.text.to_text().ends_with("tail\n")
    });
    let document = editor.active_document().unwrap();
    assert!(!document.dirty());
}

#[test]
fn a_file_in_another_encoding_reloads_only_when_it_changes() {
    let fixture = Fixture::new();
    let path = fixture.0.join("binary.dat");
    fs::write(&path, [0u8, 0xFF, 0xFE, 1, 2, 3]).unwrap();
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    assert_eq!(
        editor.active_document().unwrap().encoding(),
        crate::editor::encoding::Encoding::Bytes
    );
    // Several watch checks pass with nothing changing, so nothing may start loading.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        editor.compose(&mut frame);
        assert!(editor.tabs[0].io.is_none(), "reloaded an unchanged file");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // A real change still comes through, whole.
    append(&path, &[4, 5]);
    wait_for_document(&mut editor, &mut frame, |doc| {
        doc.text.to_text().chars().count() == 8
    });
    assert!(!editor.active_document().unwrap().dirty());
}

#[test]
fn opening_a_file_adds_nothing_to_the_status_row() {
    let fixture = Fixture::new();
    let path = fixture.file("a.txt", "text");
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.request_open(&path).unwrap();
    wait_for_io(&mut editor, &mut frame);
    assert_eq!(editor.status(), "");
    // A note about how it opened is still worth showing.
    let path = fixture.0.join("binary.dat");
    fs::write(&path, b"\xef\xbb\xbf\xff not really utf-8").unwrap();
    editor.request_open(&path).unwrap();
    wait_for_io(&mut editor, &mut frame);
    assert!(
        editor.status().contains("Bytes view"),
        "{}",
        editor.status()
    );
}

#[test]
fn lines_wrap_between_words_and_keep_every_character() {
    use super::wrap;
    let mut doc = Document::new();
    doc.insert("one two three four\nverylongwordthatdoesnotfit\n\tab cd");
    // Rows join back into the whole line, in order, and none exceeds the width.
    let width = 8;
    for line in 0..doc.line_count() {
        let mut row = wrap::Row { line, start: 0 };
        let mut joined = String::new();
        loop {
            let end = wrap::end(&doc, row, width);
            assert!(end - row.start <= width, "{row:?}");
            let a = doc.offset(line, row.start);
            let b = if end == doc.line_columns(line) {
                doc.line_end(line)
            } else {
                doc.offset(line, end)
            };
            joined.push_str(&doc.text.read(a..b));
            match wrap::next(&doc, row, width) {
                Some(next) if next.line == line => row = next,
                _ => break,
            }
        }
        assert_eq!(joined, doc.line(line));
    }
    // Between words when possible, mid-word only when one is wider than the row.
    let starts = |doc: &Document, line, width| {
        let mut row = wrap::Row { line, start: 0 };
        let mut starts = vec![0];
        while let Some(next) = wrap::next(doc, row, width).filter(|next| next.line == line) {
            starts.push(next.start);
            row = next;
        }
        starts
    };
    assert_eq!(starts(&doc, 0, 8), [0, 8, 14]);
    assert_eq!(starts(&doc, 1, 8), [0, 8, 16, 24]);
    // A row exactly full does not spill, and moving back finds the same rows.
    assert_eq!(starts(&doc, 0, 18), [0]);
    let row = wrap::Row { line: 0, start: 14 };
    assert_eq!(
        wrap::previous(&doc, row, 8),
        Some(wrap::Row { line: 0, start: 8 })
    );
    assert_eq!(
        wrap::containing(&doc, 0, 13, 8),
        wrap::Row { line: 0, start: 8 }
    );
    // A wide character is never split, and a width smaller than it still makes progress.
    let mut wide = Document::new();
    wide.insert("ab中cd");
    assert_eq!(starts(&wide, 0, 3), [0, 2, 5]);
    assert_eq!(starts(&wide, 0, 1), [0, 1, 2, 4, 5]);
}

#[test]
fn word_wrap_draws_moves_and_scrolls_by_rows() {
    let fixture = Fixture::new();
    let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa ".repeat(4);
    let path = fixture.file("a.txt", &format!("{long}\nshort\n"));
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    assert_eq!(
        screen_text(&frame).matches("kappa").count(),
        1,
        "starts unwrapped"
    );
    editor.command(Command::WordWrap);
    send(&mut editor, &mut frame, []);
    // The line continues below, the next line follows its last row, and numbers stay true.
    let text = screen_text(&frame);
    let short_row = (0..30).find(|&y| row(&frame, y).contains("short")).unwrap();
    assert!(short_row > 2, "{text}");
    assert!(row(&frame, short_row).contains(" 2 "), "{text}");
    assert!(row(&frame, 1).contains(" 1 "), "{text}");
    assert!(!row(&frame, 2).contains(" 2 "), "{text}");
    // Nothing is lost: every word appears, on some row.
    let words = text.matches("kappa").count();
    assert_eq!(words, 4, "{text}");

    // Down follows rows inside the line, keeping the column.
    let doc = editor.active_document().unwrap();
    let width = doc.wrap_width.unwrap();
    let second_row = super::wrap::next(doc, super::wrap::Row { line: 0, start: 0 }, width).unwrap();
    assert_eq!(second_row.line, 0);
    let second = second_row.start;
    send(&mut editor, &mut frame, [named(NamedKey::ArrowDown)]);
    let doc = editor.active_document().unwrap();
    assert_eq!(doc.row_at(doc.cursor), 0, "still the first line");
    assert_eq!(doc.position()[0], second);
    send(&mut editor, &mut frame, [named(NamedKey::ArrowUp)]);
    assert_eq!(editor.active_document().unwrap().cursor, 0);
    // Home and End stay on the row on screen.
    send(&mut editor, &mut frame, [named(NamedKey::End)]);
    let doc = editor.active_document().unwrap();
    assert!(doc.position()[0] < second && doc.position()[0] > 0);
    send(&mut editor, &mut frame, [named(NamedKey::Home)]);
    assert_eq!(editor.active_document().unwrap().cursor, 0);

    // Clicking a continuation row lands in that part of the line.
    let target = cell_of(&frame, 2, &long[second..second + 3]);
    click(&mut editor, &mut frame, target);
    let doc = editor.active_document().unwrap();
    assert_eq!(doc.position(), [second, 0]);

    // Toggling off restores one row per line, with nothing scrolled away.
    editor.command(Command::WordWrap);
    send(&mut editor, &mut frame, []);
    assert!(row(&frame, 2).contains(" 2 "), "{}", screen_text(&frame));
}

#[test]
fn the_wheel_moves_through_a_wrapped_line_and_stops_at_the_end() {
    let fixture = Fixture::new();
    let lines: String = (0..40)
        .map(|n| format!("L{n} {}\n", "word ".repeat(60)))
        .collect();
    let path = fixture.file("a.txt", &lines);
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    editor.command(Command::WordWrap);
    send(&mut editor, &mut frame, []);
    // One step of the wheel is one row, which can be inside a line.
    send(&mut editor, &mut frame, [wheel(80.0, 10.0, -1.0)]);
    let doc = editor.active_document().unwrap();
    assert_eq!(doc.scroll[1], 0, "still line zero");
    assert!(doc.scroll[0] > 0, "but its second row");
    send(&mut editor, &mut frame, [wheel(80.0, 10.0, 1.0)]);
    assert_eq!(editor.active_document().unwrap().scroll, [0, 0]);
    // Far past the end stops with the last row on the bottom of the pane.
    send(&mut editor, &mut frame, [wheel(80.0, 10.0, -100_000.0)]);
    assert!(editor.active_document().unwrap().scroll[1] > 30);
    assert!(screen_text(&frame).contains("L39"));
    assert!(!row(&frame, 27).trim_end().ends_with("~"));
    // Typing in view keeps the caret on screen at any width.
    send(&mut editor, &mut frame, [cmd("a"), text("x")]);
    assert_eq!(editor.active_document().unwrap().scroll, [0, 0]);
}

#[test]
fn enormous_lines_wrap_without_scanning_them() {
    use super::wrap;
    let mut doc = Document::new();
    doc.insert(&"abcdefghij".repeat(10_000_000));
    assert!(doc.line_columns(0) > 64 * 1024);
    let started = std::time::Instant::now();
    let width = 80;
    let far = 73_000_000;
    for _ in 0..1000 {
        let row = wrap::containing(&doc, 0, far, width);
        assert_eq!(row.start, far / width * width);
        assert_eq!(
            wrap::next(&doc, row, width).unwrap().start,
            row.start + width
        );
        assert_eq!(
            wrap::previous(&doc, row, width).unwrap().start,
            row.start - width
        );
        assert!(wrap::offset_on_row(&doc, row, row.start + 5, width) > 0);
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "wrapping scanned the line: {:?}",
        started.elapsed()
    );
}

#[test]
fn the_tree_highlight_stops_before_its_scrollbar() {
    let fixture = Fixture::new();
    fixture.file("0-first.txt", "");
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let theme = crate::ui::Theme::default();
    let background = |frame: &Framebuffer, x| frame.cell(x, 1).unwrap().style.background;
    let width = editor.context.pane_rect("files").unwrap().columns;
    // With no scrollbar, the highlight reaches the divider's column.
    assert_eq!(background(&frame, width - 2), theme.control);
    assert_eq!(background(&frame, width - 1), theme.panel);
    // With one, it leaves the scrollbar's column alone.
    for n in 0..60 {
        fixture.file(&format!("{n}.txt"), "");
    }
    editor.tree.refresh();
    send(&mut editor, &mut frame, []);
    assert_eq!(background(&frame, width - 3), theme.control);
    assert_eq!(background(&frame, width - 2), theme.panel);
    assert!(frame.graphics().any(|p| p.rect.column == width - 2
        && matches!(p.graphic, crate::renderer::Graphic::Scrollbar { .. })));
}

#[test]
fn the_palette_finds_symbols_and_opens_them_at_their_line() {
    if crate::tags::Ctags::locate().is_none() {
        eprintln!("skipping: this build has no ctags");
        return;
    }
    let fixture = Fixture::new();
    fixture.file("a.txt", "plain\n");
    let source = "// first\n// second\nfn alpha_function() {}\nstruct AlphaType;\n";
    let code = fixture.file("code.rs", source);
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    // Any capitalization finds them, and each result says where it is.
    send(&mut editor, &mut frame, [cmd("p"), text("#ALPHA")]);
    wait_for_screen(&mut editor, &mut frame, "alpha_function");
    let screen = screen_text(&frame);
    assert!(screen.contains("AlphaType"), "{screen}");
    assert!(screen.contains("function  code.rs:3"), "{screen}");
    // Choosing one opens the file with the caret on that line.
    let row_of_type = (0..30)
        .find(|&y| row(&frame, y).contains("AlphaType"))
        .unwrap();
    let chip = cell_of(&frame, row_of_type, "AlphaType");
    click(&mut editor, &mut frame, chip);
    wait_for_io(&mut editor, &mut frame);
    assert!(!editor.palette_open());
    let doc = editor.active_document().unwrap();
    assert_eq!(
        doc.path.as_deref(),
        Some(code.canonicalize().unwrap().as_path())
    );
    assert_eq!(doc.position(), [0, 3]);
    // An open file just moves its caret; Enter takes the first result.
    send(&mut editor, &mut frame, [cmd("p"), text("#alpha_f")]);
    wait_for_screen(&mut editor, &mut frame, "alpha_function");
    let tabs = editor.tab_count();
    send(&mut editor, &mut frame, [named(NamedKey::Enter)]);
    assert_eq!(editor.tab_count(), tabs);
    assert_eq!(editor.active_document().unwrap().position(), [0, 2]);
    // A save brings new symbols into the next search.
    send(
        &mut editor,
        &mut frame,
        [cmd("a"), text("fn renamed_later() {}"), cmd("s")],
    );
    wait_for_io(&mut editor, &mut frame);
    send(&mut editor, &mut frame, [cmd("p"), text("#renamed")]);
    wait_for_screen(&mut editor, &mut frame, "renamed_later");
    // Other palette searches are unaffected, and a miss says so.
    send(
        &mut editor,
        &mut frame,
        [cmd("a"), text("#nothing_like_this")],
    );
    wait_for_screen(&mut editor, &mut frame, "No match");
    send(&mut editor, &mut frame, [cmd("a"), text(">new")]);
    wait_for_screen(&mut editor, &mut frame, "New file");
}

/// A pointer event with Cmd held.
fn cmd_pointer(kind: PointerKind, x: f64, y: f64) -> InputEvent {
    let InputEvent::Pointer(mut event) = pointer(kind, x, y) else {
        unreachable!();
    };
    event.modifiers.super_key = true;
    InputEvent::Pointer(event)
}
fn cmd_click(editor: &mut Editor, frame: &mut Framebuffer, [x, y]: [f64; 2]) {
    let press = cmd_pointer(PointerKind::Down(MouseButton::Left), x, y);
    let release = cmd_pointer(PointerKind::Up(MouseButton::Left), x, y);
    send(editor, frame, [press, release]);
}

#[test]
fn cmd_clicking_a_symbol_follows_it_to_its_definition() {
    if crate::tags::Ctags::locate().is_none() {
        eprintln!("skipping: this build has no ctags");
        return;
    }
    let fixture = Fixture::new();
    let main = fixture.file(
        "main.rs",
        "fn main() {\n    helper();\n    Shape::new();\n    everywhere();\n    missing();\n    both();\n    only_python();\n}\n",
    );
    let helpers = fixture.file(
        "helpers.rs",
        "// about\nfn helper() {}\n\nstruct Shape;\nimpl Shape {\n    fn new() {}\n}\nfn everywhere() {}\nfn both() {}\n",
    );
    fixture.file("other.rs", "fn everywhere() {}\n");
    fixture.file(
        "script.py",
        "def both():\n    pass\ndef only_python():\n    pass\n",
    );
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&main).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let doc_cell = |frame: &Framebuffer, word: &str| {
        let y = (0..30).find(|&y| row(frame, y).contains(word)).unwrap();
        cell_of(frame, y, word)
    };

    // The first click builds the index, then the jump happens by itself.
    let target = doc_cell(&frame, "helper();");
    cmd_click(&mut editor, &mut frame, target);
    wait_for_io(&mut editor, &mut frame);
    editor.compose(&mut frame);
    let doc = editor.active_document().unwrap();
    assert_eq!(
        doc.path.as_deref(),
        Some(helpers.canonicalize().unwrap().as_path())
    );
    assert_eq!(doc.position(), [0, 1]);
    assert_eq!(editor.status(), "");

    // A type and its implementation, both in one file, go to the first of them.
    editor.open(&main).unwrap();
    editor.compose(&mut frame);
    let target = doc_cell(&frame, "Shape");
    cmd_click(&mut editor, &mut frame, target);
    let doc = editor.active_document().unwrap();
    assert_eq!(doc.position(), [0, 3]);

    // Definitions in different files are offered rather than guessed.
    editor.open(&main).unwrap();
    editor.compose(&mut frame);
    let target = doc_cell(&frame, "everywhere");
    cmd_click(&mut editor, &mut frame, target);
    assert!(editor.palette_open());
    let screen = screen_text(&frame);
    assert!(
        screen.contains("helpers.rs:8") && screen.contains("other.rs:1"),
        "{screen}"
    );
    send(&mut editor, &mut frame, [named(NamedKey::Escape)]);

    // A definition in another language is a coincidence, not a choice to offer.
    let target = doc_cell(&frame, "only_python");
    cmd_click(&mut editor, &mut frame, target);
    assert_eq!(editor.status(), "No Rust definition found for only_python");
    assert!(!editor.palette_open());
    let target = doc_cell(&frame, "both()");
    cmd_click(&mut editor, &mut frame, target);
    wait_for_io(&mut editor, &mut frame);
    editor.compose(&mut frame);
    assert_eq!(editor.active_document().unwrap().position(), [0, 8]);
    editor.open(&main).unwrap();
    editor.compose(&mut frame);

    // A name with no definition says so, and leaves the caret alone.
    let before = editor.active_document().unwrap().cursor;
    let target = doc_cell(&frame, "missing");
    cmd_click(&mut editor, &mut frame, target);
    assert_eq!(editor.status(), "No Rust definition found for missing");
    assert_eq!(editor.active_document().unwrap().cursor, before);
    assert_eq!(editor.active_document().unwrap().selection().len(), 0);

    // Cmd+clicking away from any symbol is an ordinary click, and moving after a followed
    // click selects nothing.
    let [x, y] = doc_cell(&frame, "fn main");
    cmd_click(&mut editor, &mut frame, [x + 2.0, y]);
    assert_eq!(
        editor.active_document().unwrap().cursor,
        2,
        "the space after fn"
    );
    let target = doc_cell(&frame, "missing");
    send(
        &mut editor,
        &mut frame,
        [
            cmd_pointer(PointerKind::Down(MouseButton::Left), target[0], target[1]),
            cmd_pointer(PointerKind::Move, target[0] + 3.0, target[1]),
            cmd_pointer(
                PointerKind::Up(MouseButton::Left),
                target[0] + 3.0,
                target[1],
            ),
        ],
    );
    assert_eq!(editor.active_document().unwrap().selection().len(), 0);
}

#[test]
fn cmd_clicking_follows_what_a_name_is_reached_through() {
    if crate::tags::Ctags::locate().is_none() {
        eprintln!("skipping: this build has no ctags");
        return;
    }
    let fixture = Fixture::new();
    let main = fixture.file(
        "a.rs",
        "struct Holder { handle: File }\nfn f() {\n    Kind::File;\n    String::new();\n    Shape::new();\n    shape.draw();\n}\n",
    );
    fixture.file(
        "b.rs",
        "enum Kind {\n    File,\n}\nstruct Shape;\nimpl Shape {\n    fn new() {}\n    fn draw(&self) {}\n}\nstruct Other;\nimpl Other {\n    fn new() {}\n}\n",
    );
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&main).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let mut follow = |text: &str, skip: f64| {
        editor.open(&main).unwrap();
        editor.compose(&mut frame);
        let y = (0..30).find(|&y| row(&frame, y).contains(text)).unwrap();
        let [x, y] = cell_of(&frame, y, text);
        cmd_click(&mut editor, &mut frame, [x + skip, y]);
        wait_for_io(&mut editor, &mut frame);
        editor.compose(&mut frame);
        let doc = editor.active_document().unwrap();
        (
            doc.path.as_ref().unwrap().file_name().unwrap().to_owned(),
            doc.position()[1],
            editor.status().to_owned(),
        )
    };
    // A bare `File` is not the variant in another file, whatever shares its name.
    let (file, _, status) = follow("handle: File", 8.0);
    assert_eq!(file, "a.rs");
    assert_eq!(status, "No Rust definition found for File");
    // Through its enum it is.
    let (file, line, _) = follow("Kind::File", 6.0);
    assert_eq!((file.to_str().unwrap(), line), ("b.rs", 1));
    // `new` belongs to whichever type it is reached through, and `String`'s is not indexed.
    let (file, line, _) = follow("Shape::new", 7.0);
    assert_eq!((file.to_str().unwrap(), line), ("b.rs", 5));
    let (file, _, status) = follow("String::new", 8.0);
    assert_eq!(file, "a.rs");
    assert_eq!(status, "No Rust definition found for new");
    // A method on a variable finds the only method of that name.
    let (file, line, _) = follow("shape.draw", 6.0);
    assert_eq!((file.to_str().unwrap(), line), ("b.rs", 6));
}

#[test]
fn following_a_symbol_puts_its_line_at_the_top_of_the_view() {
    if crate::tags::Ctags::locate().is_none() {
        eprintln!("skipping: this build has no ctags");
        return;
    }
    let fixture = Fixture::new();
    let main = fixture.file("a.rs", "fn f() {\n    middle();\n    near_end();\n}\n");
    let blank = "\n".repeat(59);
    let tail = "\n".repeat(38);
    let other = format!("{blank}fn middle() {{}}\n{tail}fn near_end() {{}}\n");
    fixture.file("b.rs", &other);
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&main).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let mut follow = |text: &str| {
        editor.open(&main).unwrap();
        editor.compose(&mut frame);
        let y = (0..30).find(|&y| row(&frame, y).contains(text)).unwrap();
        let target = cell_of(&frame, y, text);
        cmd_click(&mut editor, &mut frame, target);
        wait_for_io(&mut editor, &mut frame);
        editor.compose(&mut frame);
        let doc = editor.active_document().unwrap();
        (doc.scroll[1], doc.position()[1], row(&frame, 1))
    };
    // A file opened by the jump, then one already open.
    for _ in 0..2 {
        let (top, line, first_row) = follow("middle();");
        assert_eq!((top, line), (59, 59));
        assert!(first_row.contains("fn middle"), "{first_row}");
    }
    // Near the end there are too few lines to reach the top, but it stays in view.
    let (top, line, _) = follow("near_end();");
    assert_eq!(line, 98);
    assert!(top < line && top > 59, "{top}");
    assert!(screen_text(&frame).contains("fn near_end"));
}

/// The mouse rarely stays perfectly still during a click, and a jump must not read that as a
/// drag from wherever it lands.
#[test]
fn moving_during_a_cmd_click_selects_nothing_where_it_lands() {
    if crate::tags::Ctags::locate().is_none() {
        eprintln!("skipping: this build has no ctags");
        return;
    }
    let fixture = Fixture::new();
    let main = fixture.file(
        "a.rs",
        "fn f() {\n    target_function();\n    other_name();\n}\n",
    );
    let other = format!("{}fn target_function() {{}}\n", "\n".repeat(5));
    fixture.file("b.rs", &other);
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&main).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let y = (0..30)
        .find(|&y| row(&frame, y).contains("target_function"))
        .unwrap();
    let [x, y] = cell_of(&frame, y, "target_function");
    let down = PointerKind::Down(MouseButton::Left);
    let up = PointerKind::Up(MouseButton::Left);

    // Pressed, nudged within the name, and released: the jump happens, and only then.
    send(&mut editor, &mut frame, [cmd_pointer(down, x, y)]);
    send(
        &mut editor,
        &mut frame,
        [cmd_pointer(PointerKind::Move, x + 1.0, y)],
    );
    let document = editor.active_document().unwrap();
    assert_eq!(document.path.as_ref().unwrap().file_name().unwrap(), "a.rs");
    send(&mut editor, &mut frame, [cmd_pointer(up, x + 1.0, y)]);
    wait_for_io(&mut editor, &mut frame);
    // Movement arriving just after the jump is not a drag either.
    send(
        &mut editor,
        &mut frame,
        [cmd_pointer(PointerKind::Move, x + 2.0, y)],
    );
    editor.compose(&mut frame);
    let document = editor.active_document().unwrap();
    assert_eq!(document.path.as_ref().unwrap().file_name().unwrap(), "b.rs");
    assert_eq!(document.position(), [0, 5]);
    assert_eq!(document.selection().len(), 0);

    // Dragging off the name before letting go cancels, and selects nothing.
    editor.open(&main).unwrap();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [
            cmd_pointer(down, x, y),
            cmd_pointer(PointerKind::Move, x, y + 1.0),
            cmd_pointer(up, x, y + 1.0),
        ],
    );
    wait_for_io(&mut editor, &mut frame);
    let document = editor.active_document().unwrap();
    assert_eq!(document.path.as_ref().unwrap().file_name().unwrap(), "a.rs");
    assert_eq!(document.selection().len(), 0);
}

/// Runs the real `cargo doc` on a tiny project, which needs a nightly toolchain.
#[test]
fn cmd_click_follows_what_the_compiler_resolved_in_a_rust_project() {
    let nightly = std::process::Command::new("rustup")
        .args(["toolchain", "list"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("nightly"));
    if crate::tags::Ctags::locate().is_none() || !nightly {
        eprintln!("skipping: this needs ctags and a nightly toolchain");
        return;
    }
    let fixture = Fixture::new();
    fixture.file(
        "Cargo.toml",
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    std::fs::create_dir(fixture.0.join("src")).unwrap();
    let source = "pub mod two;\n\
        pub struct Here;\n\
        impl Here {\n    pub fn go(&self) {}\n}\n\
        pub fn run() {\n    let here = Here;\n    here.go();\n    two::There.go();\n\
        let text = String::new();\n    let _ = text.len();\n}\n";
    let lib = fixture.file("src/lib.rs", source);
    let two = fixture.file(
        "src/two.rs",
        "pub struct There;\nimpl There {\n    pub fn go(&self) {}\n}\n",
    );
    let lib = lib.canonicalize().unwrap();
    let two = two.canonicalize().unwrap();
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&lib).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let mut click = |text: &str, skip: f64| {
        editor.open(&lib).unwrap();
        editor.repaint = true;
        editor.compose(&mut frame);
        let y = (0..30).find(|&y| row(&frame, y).contains(text)).unwrap();
        let [x, y] = cell_of(&frame, y, text);
        cmd_click(&mut editor, &mut frame, [x + skip, y]);
        wait_for_io(&mut editor, &mut frame);
        editor.compose(&mut frame);
    };
    // Nothing runs until a click asks, and that click still gets a ctags answer.
    click("here.go", 5.0);
    assert!(editor.rust_links.running());
    // Ctags cannot tell the two `go` methods apart, so that first click offered both.
    assert!(editor.palette_open());
    send(&mut editor, &mut frame, [named(NamedKey::Escape)]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while editor.rust_links.running() {
        assert!(std::time::Instant::now() < deadline, "cargo doc timed out");
        std::thread::sleep(std::time::Duration::from_millis(20));
        editor.rust_links.poll();
    }
    let position = |editor: &Editor| {
        let document = editor.active_document().unwrap();
        (document.path.clone().unwrap(), document.position()[1])
    };
    // Two types have `go`, which ctags could only offer; the compiler knows which is meant.
    let mut click = |text: &str, skip: f64| {
        editor.open(&lib).unwrap();
        editor.repaint = true;
        editor.compose(&mut frame);
        let y = (0..30).find(|&y| row(&frame, y).contains(text)).unwrap();
        let [x, y] = cell_of(&frame, y, text);
        cmd_click(&mut editor, &mut frame, [x + skip, y]);
        wait_for_io(&mut editor, &mut frame);
        editor.compose(&mut frame);
        assert!(!editor.palette_open(), "{text} was answered exactly");
        (position(&editor), editor.status().to_owned())
    };
    assert_eq!(click("here.go", 5.0).0, (lib.clone(), 3));
    assert_eq!(click("There.go", 6.0).0, (two.clone(), 2));
    // A local variable, which ctags does not know about in Rust at all.
    assert_eq!(click("here.go", 0.0).0, (lib.clone(), 6));
    // The standard library opens if its source is installed, and otherwise says so.
    let ((path, _), status) = click("text.len", 5.0);
    assert!(
        path.ends_with("alloc/src/string.rs") || status == "String::len is outside this project",
        "{path:?} {status}"
    );
}

#[test]
fn a_page_older_than_the_file_is_not_trusted() {
    let fixture = Fixture::new();
    fixture.file(
        "Cargo.toml",
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\n",
    );
    std::fs::create_dir_all(fixture.0.join("target/doc/src/tiny")).unwrap();
    std::fs::create_dir(fixture.0.join("src")).unwrap();
    let lib = fixture.file(
        "src/lib.rs",
        "fn caller() { wrong(); }\nfn wrong() {}\nfn right() {}\n",
    );
    fixture.file(
        "target/doc/src/tiny/lib.rs.html",
        "<pre class=\"rust\"><code><a href=#1 id=1 data-nosnippet>1</a>fn caller() { <a href=\"#3\">right</a>(); }\n</code></pre>",
    );
    let lib = lib.canonicalize().unwrap();
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&lib).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let reference = symbols::Reference::new("wrong".into(), "fn caller() { ").at(14);
    // The page's line says `right()` where the file says `wrong()`, so it has no say.
    assert!(!editor.follow_rust_link(&reference));
    assert_eq!(editor.active_document().unwrap().position(), [0, 0]);
}

#[test]
fn holding_cmd_over_a_symbol_marks_it_as_clickable() {
    let fixture = Fixture::new();
    let path = fixture.file("a.rs", "let some_name = 12345;\n");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let [x, y] = cell_of(&frame, 1, "some_name");
    let underlines = |frame: &Framebuffer| {
        frame
            .graphics()
            .filter(|p| {
                p.rect.row == 1
                    && matches!(
                        p.graphic,
                        crate::renderer::Graphic::Divider {
                            vertical: false,
                            ..
                        }
                    )
            })
            .map(|p| p.rect.columns)
            .collect::<Vec<_>>()
    };
    send(&mut editor, &mut frame, [pointer(PointerKind::Move, x, y)]);
    assert_eq!(editor.cursor(), Cursor::Text);
    assert!(underlines(&frame).is_empty(), "nothing without Cmd");
    let held = Modifiers {
        super_key: true,
        ..Default::default()
    };
    send(
        &mut editor,
        &mut frame,
        [InputEvent::ModifiersChanged(held)],
    );
    assert_eq!(editor.cursor(), Cursor::Pointer);
    assert_eq!(underlines(&frame), [9], "the whole name");
    // Numbers and spaces are not names, and letting go ends it.
    let [x, _] = cell_of(&frame, 1, "12345");
    send(&mut editor, &mut frame, [pointer(PointerKind::Move, x, y)]);
    assert_eq!(editor.cursor(), Cursor::Text);
    assert!(underlines(&frame).is_empty());
    let [x, _] = cell_of(&frame, 1, "some_name");
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Move, x, y),
            InputEvent::ModifiersChanged(Modifiers::default()),
        ],
    );
    assert_eq!(editor.cursor(), Cursor::Text);
    assert!(underlines(&frame).is_empty());
}

#[test]
fn the_path_dropdown_stands_apart_from_the_document() {
    let fixture = Fixture::new();
    let path = fixture.file("only.rs", "fn main() {}\n");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let theme = crate::ui::Theme::default();
    let bottom_lines = |frame: &Framebuffer| {
        frame
            .graphics()
            .filter(|p| {
                p.rect.row > 0
                    && matches!(
                        p.graphic,
                        crate::renderer::Graphic::Divider {
                            vertical: false,
                            ..
                        }
                    )
            })
            .count()
    };
    assert_eq!(frame.cell(60, 2).unwrap().style.background, theme.panel);
    assert_eq!(bottom_lines(&frame), 0);
    send(&mut editor, &mut frame, [cmd("l")]);
    assert_eq!(frame.cell(60, 1).unwrap().style.background, theme.popup);
    assert_eq!(bottom_lines(&frame), 1);
    send(&mut editor, &mut frame, [named(NamedKey::Escape)]);
    // The full palette has edges already, and keeps the plain background.
    send(&mut editor, &mut frame, [cmd("p")]);
    assert_eq!(frame.cell(60, 25).unwrap().style.background, theme.desktop);
}

#[test]
fn opening_a_folder_gives_it_an_ignore_file_that_stays_out_of_sight() {
    let fixture = Fixture::new();
    let ignore = fixture.0.join(crate::tags::IGNORE_FILE);
    assert!(!ignore.exists());
    let mut editor = Editor::new(&fixture.0).unwrap();
    assert!(ignore.exists());
    let mut frame = frame();
    editor.compose(&mut frame);
    assert!(!screen_text(&frame).contains(".teditor"));
    // A folder inside the project uses the project's file.
    let inner = fixture.0.join("inner");
    std::fs::create_dir(&inner).unwrap();
    editor.open(&inner).unwrap();
    assert!(!inner.join(".teditor").exists());
    // Changing to another project does the same as opening it, and keeps what the user wrote.
    let other = Fixture::new();
    let kept = other.0.join(crate::tags::IGNORE_FILE);
    std::fs::create_dir(other.0.join(".teditor")).unwrap();
    std::fs::write(&kept, "mine\n").unwrap();
    editor.open(&other.0).unwrap();
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), "mine\n");
    let fresh = Fixture::new();
    editor.open(&fresh.0).unwrap();
    assert!(fresh.0.join(crate::tags::IGNORE_FILE).exists());
}

#[test]
fn the_tree_follows_changes_on_disk_by_itself() {
    let fixture = Fixture::new();
    let keep = fixture.file("keep.txt", "");
    std::fs::create_dir(fixture.0.join("open")).unwrap();
    std::fs::create_dir(fixture.0.join("shut")).unwrap();
    let root = fixture.0.canonicalize().unwrap();
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.tree.reveal_folder(&root.join("open"));
    editor.tree.reveal(&keep.canonicalize().unwrap());
    let mut frame = frame();
    editor.compose(&mut frame);
    let selected = |editor: &Editor| editor.tree.entries[editor.tree.selected].path.clone();
    let chosen = selected(&editor);

    // Added at the top, added in an expanded folder, renamed and removed, with no input.
    fixture.file("added.txt", "");
    wait_for_screen(&mut editor, &mut frame, "added.txt");
    std::fs::write(fixture.0.join("open/inner.txt"), "").unwrap();
    wait_for_screen(&mut editor, &mut frame, "inner.txt");
    std::fs::rename(fixture.0.join("added.txt"), fixture.0.join("moved.txt")).unwrap();
    wait_for_screen(&mut editor, &mut frame, "moved.txt");
    assert!(!screen_text(&frame).contains("added.txt"));
    std::fs::remove_file(fixture.0.join("moved.txt")).unwrap();
    wait_until(&mut editor, &mut frame, |editor| {
        !editor
            .tree
            .entries
            .iter()
            .any(|e| e.path.ends_with("moved.txt"))
    });
    // The selection stays on the same file as rows move around it.
    assert_eq!(selected(&editor), chosen);

    // A folder expanded later is followed, and one collapsed again is let go.
    editor.tree.reveal_folder(&root.join("shut"));
    editor.compose(&mut frame);
    assert_eq!(editor.folder_watches.len(), 3);
    std::fs::write(fixture.0.join("shut/late.txt"), "").unwrap();
    wait_for_screen(&mut editor, &mut frame, "late.txt");
    editor.tree.expanded.remove(&root.join("shut"));
    editor.compose(&mut frame);
    assert_eq!(editor.folder_watches.len(), 2);

    // A new workspace swaps every watch.
    let other = Fixture::new();
    editor.open(&other.0).unwrap();
    editor.compose(&mut frame);
    assert_eq!(editor.folder_watches.len(), 1);
    other.file("elsewhere.txt", "");
    wait_for_screen(&mut editor, &mut frame, "elsewhere.txt");
}

#[test]
fn folder_changes_reach_quick_open_and_symbols() {
    let fixture = Fixture::new();
    fixture.file("first.txt", "");
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    // Nothing is rebuilt until something has used the list.
    fixture.file("before.txt", "");
    wait_for_screen(&mut editor, &mut frame, "before.txt");
    assert!(editor.index.is_none() && editor.files.is_empty());
    send(&mut editor, &mut frame, [cmd("p")]);
    wait_for_io(&mut editor, &mut frame);
    send(&mut editor, &mut frame, [named(NamedKey::Escape)]);
    assert!(editor.files.iter().any(|f| f.ends_with("before.txt")));
    // Afterwards it keeps up.
    fixture.file("after.txt", "");
    wait_until(&mut editor, &mut frame, |editor| {
        editor.files.iter().any(|f| f.ends_with("after.txt"))
    });
}

fn found(editor: &Editor) -> Vec<std::ops::Range<usize>> {
    editor.found.matches.clone()
}
/// Open a file and search it, waiting for the search to finish.
fn find_in(fixture: &Fixture, contents: &str, query: &str) -> (Editor, Framebuffer) {
    let path = fixture.file("find.txt", contents);
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [cmd("f"), text(query)]);
    settle_find(&mut editor, &mut frame);
    (editor, frame)
}
fn settle_find(editor: &mut Editor, frame: &mut Framebuffer) {
    wait_for_io(editor, frame);
    editor.compose(frame);
    wait_for_io(editor, frame);
    editor.compose(frame);
}
fn selected(editor: &Editor) -> std::ops::Range<usize> {
    editor.active_document().unwrap().selection()
}

/// An idle window draws nothing further, so one frame after a key has to get things going.
#[test]
fn a_search_starts_in_the_frame_that_typed_it() {
    let fixture = Fixture::new();
    let path = fixture.file("find.txt", "needle\n");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    editor.handle(cmd("f"));
    editor.compose(&mut frame);
    editor.handle(text("needle"));
    editor.compose(&mut frame);
    assert!(
        editor.has_pending_io(),
        "the host keeps drawing until it is done"
    );
    wait_for_io(&mut editor, &mut frame);
    assert_eq!(found(&editor).len(), 1);
    assert_eq!(selected(&editor), 0..6);
    // The same goes for the palette.
    editor.handle(named(NamedKey::Escape));
    editor.compose(&mut frame);
    editor.handle(cmd("p"));
    editor.handle(text("?needle"));
    editor.compose(&mut frame);
    assert!(editor.has_pending_io());
    wait_for_io(&mut editor, &mut frame);
    assert_eq!(editor.text_search.hits().len(), 1);
}

#[test]
fn find_steps_through_matches_and_wraps() {
    let fixture = Fixture::new();
    let (mut editor, mut frame) = find_in(&fixture, "one two one\nONE three\none\n", "one");
    assert_eq!(found(&editor), [0..3, 8..11, 12..15, 22..25]);
    assert_eq!(selected(&editor), 0..3, "typing lands on the first match");
    assert!(screen_text(&frame).contains("1 of 4"));

    let previous = key(Key::Named(NamedKey::Enter), false, true);
    send(&mut editor, &mut frame, [enter()]);
    assert_eq!(selected(&editor), 8..11);
    send(&mut editor, &mut frame, [enter(), enter()]);
    assert_eq!(selected(&editor), 22..25);
    send(&mut editor, &mut frame, [enter()]);
    assert_eq!(selected(&editor), 0..3, "wraps to the start");
    send(&mut editor, &mut frame, [previous.clone()]);
    assert_eq!(selected(&editor), 22..25, "wraps to the end");
    send(&mut editor, &mut frame, [previous]);
    assert_eq!(selected(&editor), 12..15);
    // Escape closes the bar, keeps the selection and hands back the keyboard.
    send(&mut editor, &mut frame, [named(NamedKey::Escape)]);
    assert!(editor.find.is_none() && found(&editor).is_empty());
    assert_eq!(selected(&editor), 12..15);
    send(&mut editor, &mut frame, [text("x")]);
    assert!(
        editor
            .active_document()
            .unwrap()
            .text
            .to_text()
            .contains("x")
    );
}

#[test]
#[allow(clippy::single_range_in_vec_init)]
fn find_options_and_typing_change_the_matches() {
    let fixture = Fixture::new();
    let (mut editor, mut frame) = find_in(&fixture, "cat Cat concat\n", "cat");
    assert_eq!(found(&editor).len(), 3);
    let set = |editor: &mut Editor, change: fn(&mut crate::editor::search::Options)| {
        change(&mut editor.find.as_mut().unwrap().options);
    };
    set(&mut editor, |o| o.case_sensitive = true);
    settle_find(&mut editor, &mut frame);
    assert_eq!(found(&editor), [0..3, 11..14]);
    set(&mut editor, |o| o.whole_word = true);
    settle_find(&mut editor, &mut frame);
    assert_eq!(found(&editor), [0..3]);
    set(&mut editor, |o| {
        o.whole_word = false;
        o.regex = true;
    });
    send(&mut editor, &mut frame, [text("$")]);
    settle_find(&mut editor, &mut frame);
    assert_eq!(found(&editor), [11..14]);
    // A broken pattern says why, and finds nothing.
    send(&mut editor, &mut frame, [text("(")]);
    settle_find(&mut editor, &mut frame);
    assert!(found(&editor).is_empty());
    assert!(editor.found.status(false, &(0..0)).len() > 3);
    // No match says so, and an empty query says nothing.
    let bar = editor.find.as_mut().unwrap();
    bar.options.regex = false;
    bar.query.select_all();
    bar.query.insert("nothing here");
    settle_find(&mut editor, &mut frame);
    assert!(screen_text(&frame).contains("No match"));
    let bar = editor.find.as_mut().unwrap();
    bar.query.select_all();
    bar.query.insert("");
    settle_find(&mut editor, &mut frame);
    assert!(!screen_text(&frame).contains("No match"));
}

#[test]
fn the_find_bar_has_its_own_look_and_named_icon_buttons() {
    let fixture = Fixture::new();
    let (mut editor, mut frame) = find_in(&fixture, "cat Cat\n", "cat");
    let theme = crate::ui::Theme::default();
    let bar = (0..30)
        .find(|&y| row(&frame, y).contains('\u{eb7e}'))
        .unwrap();
    let top = editor.context.pane_rect("editor").unwrap();
    let y = f64::from(bar) + 0.5;
    let background = |frame: &Framebuffer, x: u32| frame.cell(x, bar).unwrap().style.background;
    let [case_x, _] = cell_of(&frame, bar, "Aa");
    let close_x = cell_of(&frame, bar, "✕")[0];
    assert_eq!(background(&frame, top.column + 60), theme.popup);
    assert_eq!(background(&frame, case_x as u32), theme.popup, "off");
    assert_eq!(
        background(&frame, top.column + 2),
        theme.control,
        "the input"
    );

    // Its top edge shows that it has the keyboard.
    let line = |frame: &Framebuffer| {
        frame
            .graphics()
            .filter(|p| p.rect.row == bar - 1)
            .find_map(|p| match p.graphic {
                crate::renderer::Graphic::Divider {
                    vertical: false,
                    color,
                } => Some(color),
                _ => None,
            })
    };
    send(&mut editor, &mut frame, [InputEvent::Focused(true)]);
    assert_eq!(line(&frame), Some(theme.active_border));
    click(&mut editor, &mut frame, [50.5, 3.5]);
    assert_eq!(line(&frame), Some(theme.border));

    // Hovering a button says what it does, in place of the count.
    assert!(row(&frame, bar).contains("of 2") || row(&frame, bar).contains('2'));
    send(
        &mut editor,
        &mut frame,
        [pointer(PointerKind::Move, case_x + 0.5, y)],
    );
    assert!(
        row(&frame, bar).contains("Match case"),
        "{}",
        row(&frame, bar)
    );
    assert_eq!(background(&frame, case_x as u32), theme.hover);
    // A toggle stays filled while it is on.
    click(&mut editor, &mut frame, [case_x + 0.5, y]);
    settle_find(&mut editor, &mut frame);
    assert!(editor.find.as_ref().unwrap().options.case_sensitive);
    assert_eq!(found(&editor).len(), 1);
    assert_eq!(background(&frame, case_x as u32), theme.accent);
    // Closing warns on hover, and closes.
    send(
        &mut editor,
        &mut frame,
        [pointer(PointerKind::Move, close_x + 0.5, y)],
    );
    assert_eq!(background(&frame, close_x as u32), theme.danger);
    click(&mut editor, &mut frame, [close_x + 0.5, y]);
    assert!(editor.find.is_none());
}

#[test]
#[allow(clippy::single_range_in_vec_init)]
fn find_opens_with_the_selection_and_follows_edits() {
    let fixture = Fixture::new();
    let path = fixture.file("find.txt", "alpha beta alpha\n");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    editor.tabs[editor.active].document.select_range(6..10);
    send(&mut editor, &mut frame, [cmd("f")]);
    settle_find(&mut editor, &mut frame);
    assert_eq!(found(&editor), [6..10]);
    // Typing replaces a query that was just opened.
    send(&mut editor, &mut frame, [text("alpha")]);
    settle_find(&mut editor, &mut frame);
    assert_eq!(found(&editor), [0..5, 11..16]);
    // Matches stay right as the document changes under them.
    let document = &mut editor.tabs[editor.active].document;
    document.set_cursor(0, false);
    document.insert("xx");
    settle_find(&mut editor, &mut frame);
    assert_eq!(found(&editor), [2..7, 13..18]);
}

#[test]
fn the_current_match_stands_out_from_the_others() {
    let fixture = Fixture::new();
    let (mut editor, mut frame) = find_in(&fixture, "ab ab\n", "ab");
    send(&mut editor, &mut frame, [enter()]);
    let theme = crate::ui::Theme::default();
    let screen = screen_text(&frame);
    let y = screen
        .lines()
        .position(|line| line.contains("ab ab"))
        .unwrap() as u32;
    let [x, _] = cell_of(&frame, y, "ab ab");
    let background = |column: f64| frame.cell(column as u32, y).unwrap().style.background;
    assert_eq!(background(x), theme.hover, "another match");
    assert_eq!(background(x + 2.0), theme.control, "the current line");
    assert_eq!(background(x + 3.0), theme.accent, "the current match");
}

#[test]
fn hovering_any_find_button_shows_its_whole_name() {
    let fixture = Fixture::new();
    let (mut editor, mut frame) = find_in(&fixture, "cat\n", "cat");
    editor.command(Command::Replace);
    editor.repaint = true;
    editor.compose(&mut frame);
    let bar = (0..30)
        .find(|&y| row(&frame, y).contains('\u{eb7e}'))
        .unwrap();
    for (y, icon, name) in [
        (bar, "Aa", "Match case"),
        (bar, "\u{eb7e}", "Whole word"),
        (bar, ".*", "Regular expression"),
        (bar, "↑", "Previous match"),
        (bar, "↓", "Next match"),
        (bar, "✕", "Close"),
        (bar + 1, "⏎", "Replace this match"),
        (bar + 1, "⇉", "Replace all matches"),
    ] {
        let [x, _] = cell_of(&frame, y, icon);
        send(
            &mut editor,
            &mut frame,
            [pointer(PointerKind::Move, x + 0.5, f64::from(y) + 0.5)],
        );
        let shown = row(&frame, y);
        assert!(shown.contains(name), "{name} in {shown}");
    }
}

#[test]
fn replace_changes_one_match_at_a_time() {
    let fixture = Fixture::new();
    let (mut editor, mut frame) = find_in(&fixture, "one one one\n", "one");
    editor.command(Command::Replace);
    editor.repaint = true;
    editor.compose(&mut frame);
    let screen = screen_text(&frame);
    assert!(screen.contains('⏎') && screen.contains('⇉'), "{screen}");
    // The query already has text, so the replacement has the keyboard, and Enter replaces.
    send(&mut editor, &mut frame, [text("two"), enter()]);
    settle_find(&mut editor, &mut frame);
    let text = |editor: &Editor| editor.active_document().unwrap().text.to_text();
    assert_eq!(text(&editor), "two one one\n");
    assert_eq!(selected(&editor), 4..7, "goes on to the next");
    send(&mut editor, &mut frame, [enter()]);
    settle_find(&mut editor, &mut frame);
    assert_eq!(text(&editor), "two two one\n");
    // Each replacement is its own undo step.
    send(&mut editor, &mut frame, [named(NamedKey::Escape)]);
    send(&mut editor, &mut frame, [cmd("z")]);
    assert_eq!(text(&editor), "two one one\n");
}

#[test]
fn replace_all_is_one_undo_step_and_uses_capture_groups() {
    let fixture = Fixture::new();
    let contents = "a=1 b=2\nc=3\n";
    let (mut editor, mut frame) = find_in(&fixture, contents, "x");
    let bar = editor.find.as_mut().unwrap();
    bar.options.regex = true;
    bar.query.select_all();
    bar.query.insert(r"(\w)=(\d)");
    bar.replacement.insert("$2:$1");
    settle_find(&mut editor, &mut frame);
    editor.apply_find(FindAction::ReplaceAll);
    settle_find(&mut editor, &mut frame);
    let text = |editor: &Editor| editor.active_document().unwrap().text.to_text();
    assert_eq!(text(&editor), "1:a 2:b\n3:c\n");
    assert_eq!(editor.status(), "Replaced 3 matches");
    assert!(editor.active_document().unwrap().dirty());
    editor.tabs[editor.active].document.undo();
    assert_eq!(text(&editor), contents);
    assert!(
        !editor.active_document().unwrap().dirty(),
        "back to unchanged"
    );

    // A document edited while replacing keeps the edit.
    editor.apply_find(FindAction::ReplaceAll);
    let document = &mut editor.tabs[editor.active].document;
    document.set_cursor(0, false);
    document.insert("new ");
    settle_find(&mut editor, &mut frame);
    assert_eq!(text(&editor), format!("new {contents}"));
    assert!(editor.status().contains("changed"));
}

#[test]
fn replace_keeps_a_documents_line_endings_and_handles_many_lengths() {
    let fixture = Fixture::new();
    let (mut editor, mut frame) = find_in(&fixture, "aa\r\naa\r\n", "aa");
    editor.find.as_mut().unwrap().replacement.insert("longer");
    editor.apply_find(FindAction::ReplaceAll);
    settle_find(&mut editor, &mut frame);
    let text = editor.active_document().unwrap().text.to_text();
    assert_eq!(text, "longer\r\nlonger\r\n");
}

#[test]
fn the_palette_searches_the_text_of_files() {
    let fixture = Fixture::new();
    let target = fixture.file("src.txt", "first\n    second needle here\n");
    fixture.file("other.txt", "no match\n");
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [cmd("p"), text("?needle")]);
    wait_for_screen(&mut editor, &mut frame, "second needle here");
    let screen = screen_text(&frame);
    assert!(screen.contains("src.txt:2"), "{screen}");
    assert!(!screen.contains("no match"));
    // Enter opens the file at that line.
    send(&mut editor, &mut frame, [enter()]);
    wait_for_io(&mut editor, &mut frame);
    assert!(!editor.palette_open());
    let document = editor.active_document().unwrap();
    assert_eq!(
        document.path.as_deref(),
        Some(target.canonicalize().unwrap().as_path())
    );
    assert_eq!(document.position(), [0, 1]);
    // A command leads there, and closing forgets the results.
    assert!(editor.text_search.hits().is_empty());
    editor.command(Command::FindInFiles);
    editor.repaint = true;
    editor.compose(&mut frame);
    assert!(editor.palette_open());
    assert!(screen_text(&frame).contains('?'));
    send(&mut editor, &mut frame, [text("nothing like this")]);
    wait_for_screen(&mut editor, &mut frame, "No match");
    send(&mut editor, &mut frame, [named(NamedKey::Escape)]);
    send(&mut editor, &mut frame, [cmd("f"), named(NamedKey::Escape)]);
    let shift_f = key(Key::Character("f".into()), true, true);
    send(&mut editor, &mut frame, [shift_f]);
    assert!(editor.palette_open());
}

#[test]
fn symbol_search_has_a_palette_command() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [cmd("p"), text(">find sym")]);
    wait_for_screen(&mut editor, &mut frame, "Find symbol");
    send(&mut editor, &mut frame, [named(NamedKey::Enter)]);
    assert!(editor.palette_open());
    let bottom = row(&frame, 29);
    assert!(bottom.contains('#'), "{bottom}");
}

#[test]
fn a_caret_at_the_end_stays_at_the_end_while_following() {
    let fixture = Fixture::new();
    let path = fixture.file("log.txt", "first\n");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    append(&path, b"second\n");
    wait_for_document(&mut editor, &mut frame, |doc| doc.text.len() > 6);
    assert_eq!(editor.active_document().unwrap().cursor, 0, "stays put");
    send(
        &mut editor,
        &mut frame,
        [key(Key::Named(NamedKey::End), true, false)],
    );
    assert_eq!(editor.active_document().unwrap().cursor, 13);
    append(&path, b"third\n");
    wait_for_document(&mut editor, &mut frame, |doc| doc.text.len() > 13);
    assert_eq!(editor.active_document().unwrap().cursor, 19, "follows");
}

#[test]
fn editing_or_undoing_stops_following_for_good() {
    let fixture = Fixture::new();
    let path = fixture.file("log.txt", "first\n");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    assert!(editor.tabs[0].watch.is_some());
    send(&mut editor, &mut frame, [text("x"), cmd("z")]);
    assert!(!editor.active_document().unwrap().dirty());
    editor.compose(&mut frame);
    assert!(
        editor.tabs[0].watch.is_none(),
        "the watch ends with the edit"
    );
    append(&path, b"second\n");
    std::thread::sleep(std::time::Duration::from_millis(250));
    editor.compose(&mut frame);
    assert_eq!(editor.active_document().unwrap().text.to_text(), "first\n");
    // The earlier protection still applies: saving would overwrite that change.
    send(&mut editor, &mut frame, [text("y")]);
    let saved = editor.tabs[0].document.save(None);
    assert!(
        saved.is_err(),
        "the file changed on disk, so this must not overwrite"
    );
}

#[test]
fn the_caret_and_deleting_treat_grapheme_clusters_as_one_character() {
    let text = format!("a{ACCENTED}{TECHNOLOGIST}{FLAG}{THUMB}b");
    let mut doc = Document::new();
    doc.insert(&text);
    doc.set_cursor(0, false);
    let mut stops = vec![0];
    for _ in 0..6 {
        doc.move_horizontal(true, false, false);
        stops.push(doc.cursor);
    }
    let expected: Vec<_> = ["", "a", ACCENTED, TECHNOLOGIST, FLAG, THUMB, "b"]
        .iter()
        .scan(0, |offset, part| {
            *offset += part.len();
            Some(*offset)
        })
        .collect();
    assert_eq!(stops, expected);
    for &stop in expected.iter().rev().skip(1) {
        doc.move_horizontal(false, false, false);
        assert_eq!(doc.cursor, stop);
    }
    // Columns count what is seen: a, é, two for each emoji, then b.
    assert_eq!(doc.line_columns(0), 1 + 1 + 2 + 2 + 2 + 1);
    assert_eq!(doc.offset(0, 2), expected[2]);
    assert_eq!(doc.offset(0, 3), expected[2], "inside the wide emoji");
    assert_eq!(doc.offset(0, 4), expected[3]);
    // Backspace and Delete remove a whole cluster, and undo restores it.
    doc.set_cursor(expected[3], false);
    doc.delete(true, false);
    assert_eq!(doc.text.to_text(), format!("a{ACCENTED}{FLAG}{THUMB}b"));
    doc.delete(false, false);
    assert_eq!(doc.text.to_text(), format!("a{ACCENTED}{THUMB}b"));
    doc.undo();
    doc.undo();
    assert_eq!(doc.text.to_text(), text);
    // A line ending is still one step, and clusters around it stay whole.
    let mut doc = Document::new();
    doc.insert(ACCENTED);
    doc.text.replace(
        doc.text.len()..doc.text.len(),
        &TextBuffer::from_text("\r\nz"),
    );
    doc.set_cursor(ACCENTED.len() + 2, false);
    doc.move_horizontal(false, false, false);
    assert_eq!(doc.cursor, ACCENTED.len());
    doc.move_horizontal(false, false, false);
    assert_eq!(doc.cursor, 0);
}

#[test]
fn documents_draw_clusters_in_the_cells_they_measure() {
    use crate::renderer::{cluster_text, is_symbol};
    let fixture = Fixture::new();
    let path = fixture.file("a.txt", &format!("{ACCENTED}x{TECHNOLOGIST}y{FLAG}z\n"));
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let top = (0..30).find(|&y| row(&frame, y).contains('y')).unwrap();
    let cells: Vec<char> = row(&frame, top).chars().collect();
    let start = cells.iter().position(|&c| is_symbol(c)).unwrap();
    assert_eq!(&*cluster_text(cells[start]).unwrap(), ACCENTED);
    assert_eq!(cells[start + 1], 'x');
    assert_eq!(&*cluster_text(cells[start + 2]).unwrap(), TECHNOLOGIST);
    assert_eq!(cells[start + 3], Cell::CONTINUATION);
    assert_eq!(cells[start + 4], 'y');
    assert_eq!(&*cluster_text(cells[start + 5]).unwrap(), FLAG);
    assert_eq!(cells[start + 6], Cell::CONTINUATION);
    assert_eq!(cells[start + 7], 'z');
    // Typing a combining mark composes with the character before it, in the same cell.
    send(
        &mut editor,
        &mut frame,
        [named(NamedKey::End), text("\u{301}")],
    );
    let cells: Vec<char> = row(&frame, top).chars().collect();
    assert_eq!(&*cluster_text(cells[start + 7]).unwrap(), "z\u{301}");
    // Scrolling so a cluster straddles the edge leaves blanks, never part of it.
    let document = editor.active_document().unwrap();
    assert_eq!(document.position()[0], 8);
}

#[test]
fn the_palette_query_measures_and_draws_whole_clusters() {
    use crate::renderer::{cluster_text, is_symbol};
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [cmd("p"), text(&format!("{TECHNOLOGIST}{ACCENTED}"))],
    );
    let cells: Vec<char> = row(&frame, 29).chars().collect();
    assert_eq!(&*cluster_text(cells[2]).unwrap(), TECHNOLOGIST);
    assert_eq!(cells[3], Cell::CONTINUATION);
    assert!(
        is_symbol(cells[4]) || cells[4] == ' ',
        "the accent's cell or the caret"
    );
    send(&mut editor, &mut frame, [named(NamedKey::Backspace)]);
    let cells: Vec<char> = row(&frame, 29).chars().collect();
    assert_eq!(&*cluster_text(cells[2]).unwrap(), TECHNOLOGIST);
    assert!(
        !cells[4..10].iter().any(|&c| is_symbol(c)),
        "the accent went"
    );
}

#[test]
fn unicode_selection_edit_undo_and_tab_navigation() {
    let mut doc = Document::new();
    doc.insert("α\t🙂\nsecond\n");
    assert_eq!(doc.line_count(), 3);
    assert_eq!(doc.offset(0, 4), "α\t".len());
    // The emoji covers columns 4 and 5, so either one finds it.
    assert_eq!(doc.offset(0, 5), "α\t".len());
    doc.set_cursor(doc.offset(0, 6), false);
    doc.move_horizontal(false, false, true);
    assert_eq!(doc.selected_text(), "🙂");
    doc.insert("é");
    assert_eq!(doc.line(0), "α\té");
    doc.undo();
    assert_eq!(doc.selected_text(), "🙂");
    doc.redo();
    assert_eq!(doc.line(0), "α\té");
    doc.move_vertical(1, false);
    assert_eq!(doc.position(), [5, 1]);
    doc.move_vertical(1, false);
    assert_eq!(doc.position(), [0, 2]);
    doc.move_vertical(-1, false);
    assert_eq!(doc.position(), [5, 1]);
    doc.select_all();
    doc.delete(false, false);
    assert_eq!(doc.text.to_text(), "");
    doc.undo();
    assert_eq!(doc.text.to_text(), "α\té\nsecond\n");
}

#[test]
fn save_preserves_crlf_bom_and_rejects_external_changes() {
    let fixture = Fixture::new();
    let path = fixture.file("file.txt", "\u{feff}one\r\ntwo\r\n");
    let mut doc = Document::open(&path).unwrap();
    assert!(!doc.dirty());
    assert_eq!(doc.text.to_text(), "one\r\ntwo\r\n");
    doc.insert("hello ");
    doc.save(None).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\u{feff}hello one\r\ntwo\r\n"
    );
    assert!(!doc.dirty());
    doc.undo();
    assert!(doc.dirty());
    doc.redo();
    assert!(!doc.dirty());
    fs::write(&path, "external").unwrap();
    doc.insert("edit");
    assert!(doc.save(None).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "external");
    let copy = fixture.0.join("copy.txt");
    doc.save(Some(&copy)).unwrap();
    assert!(!doc.dirty());
    assert!(doc.save(Some(&path)).is_err());
    assert!(fs::read_dir(&fixture.0).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
}

#[test]
fn binary_and_non_utf8_open_without_replacing_original_bytes() {
    let fixture = Fixture::new();
    let binary = fixture.file("binary", "a\0b");
    let invalid = fixture.0.join("invalid");
    fs::write(&invalid, [0xff, 0xfe, 0xff]).unwrap();
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&binary).unwrap();
    assert_eq!(editor.active_document().unwrap().text.to_text(), "a\0b");
    editor.tabs[editor.active].document.save(None).unwrap();
    assert_eq!(fs::read(&binary).unwrap(), b"a\0b");
    editor.open(&invalid).unwrap();
    assert_eq!(
        editor.active_document().unwrap().encoding(),
        Encoding::Bytes
    );
    editor.tabs[editor.active].document.save(None).unwrap();
    assert_eq!(fs::read(&invalid).unwrap(), [0xff, 0xfe, 0xff]);
    assert_eq!(editor.tab_count(), 2);
}

#[test]
fn tree_has_branches_directory_first_and_preserves_selection_on_refresh() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("src")).unwrap();
    fixture.file("src/main.rs", "main");
    fixture.file("a.txt", "a");
    fixture.file(".hidden", "h");
    let mut tree = FileTree::new(&fixture.0).unwrap();
    assert_eq!(tree.entries[0].label, "src/");
    assert_eq!(tree.entries[0].prefix, "├─ ");
    assert_eq!(tree.entries[1].prefix, "└─ ");
    tree.reveal(&fixture.0.join("src/main.rs"));
    assert_eq!(tree.entries[tree.selected].label, "main.rs");
    assert_eq!(tree.entries[tree.selected].prefix, "│  └─ ");
    fixture.file("0.txt", "0");
    tree.refresh();
    assert_eq!(tree.entries[tree.selected].label, "main.rs");
    tree.show_hidden = true;
    tree.refresh();
    assert!(tree.entries.iter().any(|e| e.label == ".hidden"));
}

#[test]
fn editing_and_save_are_ordered_within_one_input_batch() {
    let fixture = Fixture::new();
    let path = fixture.file("a.txt", "one\ntwo");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [
            text("hi "),
            named(NamedKey::End),
            named(NamedKey::Enter),
            text("three"),
            cmd("s"),
        ],
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "hi one\nthree\ntwo");
    assert!(!editor.active_document().unwrap().dirty());
    send(&mut editor, &mut frame, [cmd("z")]);
    assert!(editor.active_document().unwrap().dirty());
}

#[test]
fn tabs_are_below_editor_and_palette_occupies_last_row() {
    let fixture = Fixture::new();
    let a = fixture.file("a.txt", "first");
    let b = fixture.file("b.txt", "second");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&a).unwrap();
    editor.open(&b).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    assert!(row(&frame, 28).contains("a.txt"));
    assert!(row(&frame, 28).contains("b.txt"));
    assert!(row(&frame, 1).contains("second"));
    send(&mut editor, &mut frame, [cmd("p"), text(">new")]);
    assert!(editor.palette_open());
    assert!(row(&frame, 29).contains(">new"));
    assert!(row(&frame, 19).contains("New file"));
    assert_eq!(editor.ime_cursor_cells().unwrap().row, 29);
    assert_eq!(editor.active_document().unwrap().text.to_text(), "second");
    send(
        &mut editor,
        &mut frame,
        [named(NamedKey::Enter), text("fresh")],
    );
    assert_eq!(editor.tab_count(), 3);
    assert_eq!(editor.active_document().unwrap().text.to_text(), "fresh");
    assert!(!editor.palette_open());
    assert!(row(&frame, 28).contains("● Untitled"));
}

#[test]
fn mouse_tree_open_drag_selection_and_tab_switch() {
    let fixture = Fixture::new();
    let a = fixture.file("a.txt", "first\nline");
    fixture.file("b.txt", "second");
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), 5.0, 1.0),
            pointer(PointerKind::Up(MouseButton::Left), 5.0, 1.0),
        ],
    );
    assert_eq!(
        editor.active_document().unwrap().path,
        Some(a.canonicalize().unwrap())
    );
    // Editor starts at column 26, with three columns of line-number gutter.
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), 29.0, 1.0),
            pointer(PointerKind::Move, 32.0, 1.0),
            pointer(PointerKind::Up(MouseButton::Left), 32.0, 1.0),
        ],
    );
    assert_eq!(editor.active_document().unwrap().selected_text(), "fir");
    send(&mut editor, &mut frame, [text("new")]);
    assert_eq!(
        editor.active_document().unwrap().text.to_text(),
        "newst\nline"
    );
    editor.open(&fixture.0.join("b.txt")).unwrap();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), 29.0, 28.0),
            pointer(PointerKind::Up(MouseButton::Left), 29.0, 28.0),
        ],
    );
    assert_eq!(editor.active_document().unwrap().name(), "a.txt");
    assert!(editor.active_document().unwrap().dirty());
}

fn close_button(frame: &Framebuffer, index: usize) -> [f64; 2] {
    let y = frame.size()[1] - 2;
    let x = row(frame, y)
        .chars()
        .enumerate()
        .filter_map(|(x, ch)| (ch == '×').then_some(x))
        .nth(index)
        .expect("visible tab close button");
    [x as f64 + 0.5, f64::from(y) + 0.5]
}

fn click_close(editor: &mut Editor, frame: &mut Framebuffer, index: usize) {
    let [x, y] = close_button(frame, index);
    send(
        editor,
        frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), x, y),
            pointer(PointerKind::Up(MouseButton::Left), x, y),
        ],
    );
}

#[test]
fn tab_cross_closes_background_active_and_last_tabs_on_release() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        editor.open(&fixture.file(name, name)).unwrap();
    }
    let mut frame = frame();
    editor.compose(&mut frame);
    let [x, y] = close_button(&frame, 0);
    send(
        &mut editor,
        &mut frame,
        [pointer(PointerKind::Down(MouseButton::Left), x, y)],
    );
    assert_eq!(editor.tab_count(), 3);
    assert_eq!(editor.active_document().unwrap().name(), "c.txt");
    send(
        &mut editor,
        &mut frame,
        [pointer(PointerKind::Up(MouseButton::Left), x, y)],
    );
    assert_eq!(editor.tab_count(), 2);
    assert_eq!(editor.active_document().unwrap().name(), "c.txt");
    click_close(&mut editor, &mut frame, 1);
    assert_eq!(editor.active_document().unwrap().name(), "b.txt");
    click_close(&mut editor, &mut frame, 0);
    assert_eq!(editor.tab_count(), 0);
    assert!(editor.active_document().is_none());
}

#[test]
fn background_tab_cross_save_discard_and_cancel_target_the_clicked_document() {
    for decision in ["save", "discard", "cancel"] {
        let fixture = Fixture::new();
        let a = fixture.file("a.txt", "original");
        let b = fixture.file("b.txt", "keep");
        let mut editor = Editor::new(&fixture.0).unwrap();
        editor.open(&a).unwrap();
        let mut frame = frame();
        editor.compose(&mut frame);
        send(&mut editor, &mut frame, [text("edited ")]);
        editor.open(&b).unwrap();
        editor.compose(&mut frame);
        click_close(&mut editor, &mut frame, 0);
        assert!(editor.palette_open());
        assert_eq!(editor.tab_count(), 2);
        assert_eq!(editor.active_document().unwrap().name(), "b.txt");
        assert!(row(&frame, 29).contains("Save a.txt?"));
        let event = match decision {
            "save" => text("s"),
            "discard" => text("d"),
            _ => named(NamedKey::Escape),
        };
        send(&mut editor, &mut frame, [event]);
        assert!(!editor.palette_open());
        assert_eq!(editor.tab_count(), if decision == "cancel" { 2 } else { 1 });
        assert_eq!(editor.active_document().unwrap().name(), "b.txt");
        assert_eq!(
            fs::read_to_string(a).unwrap(),
            if decision == "save" {
                "edited original"
            } else {
                "original"
            }
        );
        assert_eq!(fs::read_to_string(b).unwrap(), "keep");
        if decision == "cancel" {
            assert!(editor.tabs[0].document.dirty());
        }
    }
}

fn cell_of(frame: &Framebuffer, y: u32, needle: &str) -> [f64; 2] {
    let line: Vec<char> = row(frame, y).chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    let x = line
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap_or_else(|| panic!("{needle:?} not on row {y}"));
    [x as f64 + 0.5, f64::from(y) + 0.5]
}

fn click(editor: &mut Editor, frame: &mut Framebuffer, [x, y]: [f64; 2]) {
    send(
        editor,
        frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), x, y),
            pointer(PointerKind::Up(MouseButton::Left), x, y),
        ],
    );
}

#[test]
fn unsaved_prompt_buttons_answer_with_the_mouse() {
    for (button, tabs, contents) in [
        ("S  Save", 1, "edited original"),
        ("D  Discard", 1, "original"),
        ("Esc  Cancel", 2, "original"),
    ] {
        let fixture = Fixture::new();
        let a = fixture.file("a.txt", "original");
        let b = fixture.file("b.txt", "keep");
        let mut editor = Editor::new(&fixture.0).unwrap();
        editor.open(&a).unwrap();
        let mut frame = frame();
        editor.compose(&mut frame);
        send(&mut editor, &mut frame, [text("edited ")]);
        editor.open(&b).unwrap();
        editor.compose(&mut frame);
        click_close(&mut editor, &mut frame, 0);
        assert!(editor.palette_open());
        let target = cell_of(&frame, 29, button);
        click(&mut editor, &mut frame, target);
        assert!(!editor.palette_open(), "{button} should close the prompt");
        assert_eq!(editor.tab_count(), tabs, "{button}");
        assert_eq!(fs::read_to_string(&a).unwrap(), contents, "{button}");
    }
}

#[test]
fn prompt_buttons_cancel_on_outside_release_and_prompt_ignores_stray_clicks() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [text("unsaved"), cmd("w")]);
    let [x, y] = cell_of(&frame, 29, "Discard");
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), x, y),
            pointer(PointerKind::Move, 2.0, y),
            pointer(PointerKind::Up(MouseButton::Left), 2.0, y),
        ],
    );
    assert_eq!(editor.tab_count(), 1);
    assert!(editor.palette_open());
    // Clicking the workspace behind an unsaved prompt neither answers nor dismisses it.
    click(&mut editor, &mut frame, [50.0, 10.0]);
    assert!(editor.palette_open());
    assert_eq!(editor.tab_count(), 1);
    send(&mut editor, &mut frame, [text("d")]);
    assert_eq!(editor.tab_count(), 0);
}

#[test]
fn palette_rows_run_on_click_and_keep_keyboard_navigation() {
    let fixture = Fixture::new();
    fixture.file("alpha.txt", "alpha");
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [cmd("p")]);
    assert!(row(&frame, 18).contains("Files and commands"));
    assert!(row(&frame, 19).contains("New file"));
    assert!(row(&frame, 19).contains("+N"));
    // Hovering highlights a row without choosing it.
    let target = cell_of(&frame, 20, "Save file");
    send(
        &mut editor,
        &mut frame,
        [pointer(PointerKind::Move, target[0], target[1])],
    );
    assert!(editor.palette_open());
    assert_eq!(
        frame.cell(40, 20).unwrap().style.background,
        crate::ui::Theme::default().hover
    );
    assert_eq!(editor.cursor(), Cursor::Pointer);
    let target = cell_of(&frame, 19, "New file");
    click(&mut editor, &mut frame, target);
    assert!(!editor.palette_open());
    assert_eq!(editor.tab_count(), 2);
    // The keyboard still works and moves through results past the visible rows.
    send(&mut editor, &mut frame, [cmd("p")]);
    send(
        &mut editor,
        &mut frame,
        std::iter::repeat_n(named(NamedKey::ArrowDown), 12),
    );
    assert!(row(&frame, 29).contains("13/"));
    send(&mut editor, &mut frame, [text("alpha")]);
    wait_for_background(&mut editor, &mut frame);
    editor.compose(&mut frame);
    assert!(row(&frame, 19).contains("alpha.txt"));
    send(&mut editor, &mut frame, [named(NamedKey::Enter)]);
    assert_eq!(editor.active_document().unwrap().name(), "alpha.txt");
}

#[test]
fn palette_scrolls_with_the_wheel_and_click_away_or_buttons_dismiss_it() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [cmd("p")]);
    let top = row(&frame, 19);
    send(&mut editor, &mut frame, [wheel(20.0, 22.5, -2.0)]);
    assert!(editor.palette_open());
    assert_ne!(row(&frame, 19), top);
    click(&mut editor, &mut frame, [50.0, 5.0]);
    assert!(!editor.palette_open());
    assert_eq!(editor.tab_count(), 1, "the click must not reach the tree");
    send(&mut editor, &mut frame, [cmd("p")]);
    let target = cell_of(&frame, 29, "Close");
    click(&mut editor, &mut frame, target);
    assert!(!editor.palette_open());
    // The Enter button submits the selected row.
    send(&mut editor, &mut frame, [cmd("p"), text(">new file")]);
    let target = cell_of(&frame, 29, "Open");
    click(&mut editor, &mut frame, target);
    assert_eq!(editor.tab_count(), 2);
}

#[test]
fn status_buttons_open_the_palette_save_and_return_the_keyboard() {
    let fixture = Fixture::new();
    let path = fixture.file("file.txt", "text");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let target = cell_of(&frame, 29, "Commands");
    click(&mut editor, &mut frame, target);
    assert!(editor.palette_open());
    send(&mut editor, &mut frame, [named(NamedKey::Escape)]);
    send(&mut editor, &mut frame, [text("new ")]);
    let target = cell_of(&frame, 29, "Save");
    click(&mut editor, &mut frame, target);
    assert_eq!(fs::read_to_string(&path).unwrap(), "new text");
    // Clicking the empty middle of the row must not strand typing.
    click(&mut editor, &mut frame, [70.0, 29.5]);
    send(&mut editor, &mut frame, [text("more ")]);
    assert_eq!(
        editor.active_document().unwrap().text.to_text(),
        "new more text"
    );
    let target = cell_of(&frame, 29, "UTF-8");
    click(&mut editor, &mut frame, target);
    assert!(matches!(
        editor.palette.as_ref().unwrap().mode,
        Mode::Encoding { .. }
    ));
}

#[test]
fn new_tab_button_and_hover_states_follow_the_pointer() {
    let fixture = Fixture::new();
    fixture.file("a.txt", "a");
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let plus = cell_of(&frame, 28, "+");
    click(&mut editor, &mut frame, plus);
    assert_eq!(editor.tab_count(), 2);
    send(
        &mut editor,
        &mut frame,
        [pointer(PointerKind::Move, 5.5, 1.5)],
    );
    assert_eq!(
        frame.cell(20, 1).unwrap().style.background,
        crate::ui::Theme::default().hover
    );
    assert_eq!(editor.cursor(), Cursor::Pointer);
}

#[test]
fn title_bar_opens_on_click_and_creates_missing_files_and_folders() {
    let fixture = Fixture::new();
    let a = fixture.file("a.txt", "first");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&a).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    // Hovering advertises it; a press released elsewhere does not open it.
    send(
        &mut editor,
        &mut frame,
        [pointer(PointerKind::Move, 60.0, 0.5)],
    );
    assert!(row(&frame, 0).contains("Click to edit"));
    assert_eq!(editor.cursor(), Cursor::Text);
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), 60.0, 0.5),
            pointer(PointerKind::Move, 60.0, 5.0),
            pointer(PointerKind::Up(MouseButton::Left), 60.0, 5.0),
        ],
    );
    assert!(!editor.palette_open());
    click(&mut editor, &mut frame, [60.0, 0.5]);
    assert!(editor.palette_open());
    assert!(row(&frame, 0).contains("a.txt"));
    assert_eq!(editor.ime_cursor_cells().unwrap().row, 0);
    // The initial selection means typing replaces the path.
    send(&mut editor, &mut frame, [text("deep/er/new.rs")]);
    assert!(row(&frame, 0).contains("deep/er/new.rs"));
    assert!(row(&frame, 0).contains("New file"));
    assert!(row(&frame, 0).contains("Create"));
    send(&mut editor, &mut frame, [named(NamedKey::Enter)]);
    assert!(!editor.palette_open());
    assert_eq!(editor.active_document().unwrap().name(), "new.rs");
    assert_eq!(
        fs::read_to_string(fixture.0.join("deep/er/new.rs")).unwrap(),
        ""
    );
    // The new document takes typing straight away.
    send(&mut editor, &mut frame, [text("hello")]);
    assert_eq!(editor.active_document().unwrap().text.to_text(), "hello");
    // A trailing slash makes a folder and shows it in the tree.
    send(
        &mut editor,
        &mut frame,
        [cmd("l"), text("made/"), named(NamedKey::Enter)],
    );
    assert!(fixture.0.join("made").is_dir());
    assert!(!editor.palette_open());
    assert_eq!(editor.tree.entries[editor.tree.selected].label, "made/");
    assert!(row(&frame, 1).contains("deep/") || row(&frame, 2).contains("deep/"));
    assert_eq!(editor.tab_count(), 2);
}

#[test]
fn title_bar_navigates_with_suggestions_without_overriding_typed_names() {
    let fixture = Fixture::new();
    let a = fixture.file("a.txt", "first");
    fs::create_dir(fixture.0.join("src")).unwrap();
    let main = fixture.0.join("src/main.rs");
    fs::write(&main, "fn main() {}").unwrap();
    fs::write(fixture.0.join("src/mainline.rs"), "").unwrap();
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&a).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    // Unedited, the bar lists the file's siblings.
    send(&mut editor, &mut frame, [cmd("l")]);
    assert!(row(&frame, 1).contains("src/"));
    assert!(row(&frame, 2).contains("a.txt"));
    // Clicking a folder enters it and keeps the bar open.
    let target = cell_of(&frame, 1, "src/");
    let target = [target[0].max(30.0), target[1]];
    click(&mut editor, &mut frame, target);
    assert!(editor.palette_open());
    assert!(row(&frame, 0).contains("src/"));
    assert!(row(&frame, 1).contains("main.rs"));
    // A typed prefix of a suggestion still means that exact new name.
    send(&mut editor, &mut frame, [text("mai")]);
    assert!(row(&frame, 1).contains("main.rs"));
    assert!(row(&frame, 0).contains("New file"));
    // Tab completes to the first suggestion; Enter then opens it, creating nothing.
    send(&mut editor, &mut frame, [named(NamedKey::Tab)]);
    assert!(row(&frame, 0).contains("src/main.rs"));
    send(&mut editor, &mut frame, [named(NamedKey::Enter)]);
    assert_eq!(
        editor.active_document().unwrap().path,
        Some(main.canonicalize().unwrap())
    );
    assert!(!fixture.0.join("src/mai").exists());
    // The arrows pick a row, and so does clicking a file.
    send(
        &mut editor,
        &mut frame,
        [
            cmd("l"),
            named(NamedKey::ArrowDown),
            named(NamedKey::ArrowDown),
        ],
    );
    assert!(row(&frame, 0).contains("Open"));
    send(&mut editor, &mut frame, [named(NamedKey::Enter)]);
    assert_eq!(editor.active_document().unwrap().name(), "mainline.rs");
    send(&mut editor, &mut frame, [cmd("l")]);
    let target = cell_of(&frame, 1, "main.rs");
    click(&mut editor, &mut frame, target);
    assert!(!editor.palette_open());
    assert_eq!(editor.active_document().unwrap().name(), "main.rs");
    // Escape and clicking away leave the disk alone.
    send(
        &mut editor,
        &mut frame,
        [cmd("l"), text("nope.txt"), named(NamedKey::Escape)],
    );
    assert!(!editor.palette_open());
    send(&mut editor, &mut frame, [cmd("l"), text("nope.txt")]);
    click(&mut editor, &mut frame, [5.0, 20.0]);
    assert!(!editor.palette_open());
    assert!(!fixture.0.join("nope.txt").exists());
}

#[test]
fn title_bar_reports_paths_it_cannot_create() {
    let fixture = Fixture::new();
    let a = fixture.file("a.txt", "first");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&a).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [cmd("l"), text("a.txt/child.rs"), named(NamedKey::Enter)],
    );
    assert!(
        editor.palette_open(),
        "the bar stays open to correct the path"
    );
    assert!(!editor.status().is_empty());
    assert_eq!(fs::read_to_string(a).unwrap(), "first");
}

fn screen_text(frame: &Framebuffer) -> String {
    (0..frame.size()[1]).map(|y| row(frame, y) + "\n").collect()
}
fn wait_for_screen(editor: &mut Editor, frame: &mut Framebuffer, needle: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        editor.compose(frame);
        if screen_text(frame).contains(needle) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{needle:?} never appeared:\n{}",
            screen_text(frame)
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
fn enter() -> InputEvent {
    InputEvent::Key(KeyEvent {
        physical_key: PhysicalKey::Code(KeyCode::Enter),
        key_without_modifiers: None,
        logical_key: Key::Named(NamedKey::Enter),
        location: KeyLocation::Standard,
        state: KeyState::Pressed,
        repeat: false,
        synthetic: false,
        modifiers: Modifiers::default(),
        composing: false,
    })
}
/// The row of the terminal panel's header, which holds its tabs.
fn terminal_header(editor: &Editor) -> u32 {
    editor.context.pane_rect("terminal").unwrap().row
}
fn terminal_editor(fixture: &Fixture) -> Editor {
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.shell = vec!["/bin/sh".into()];
    editor
}

#[test]
fn a_line_above_the_terminal_lights_up_like_a_split_divider() {
    let fixture = Fixture::new();
    let mut editor = terminal_editor(&fixture);
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [cmd("j")]);
    let top = terminal_header(&editor);
    let line = |frame: &Framebuffer, row: u32| {
        frame
            .graphics()
            .find(|p| {
                p.rect.row == row
                    && matches!(
                        p.graphic,
                        crate::renderer::Graphic::Divider {
                            vertical: false,
                            ..
                        }
                    )
            })
            .map(|p| (p.graphic, p.rect))
    };
    let edge = |frame: &Framebuffer, row: u32| match line(frame, row).unwrap().0 {
        crate::renderer::Graphic::Divider { color, .. } => color,
        _ => unreachable!(),
    };
    let theme = crate::ui::Theme::default();
    let y = f64::from(top) + 0.5;
    click(&mut editor, &mut frame, [50.5, 3.5]);
    // The line is the bottom edge of the row above the terminal's tabs, across the whole
    // panel, and there is none beneath the tabs.
    let span = line(&frame, top - 1).unwrap().1;
    let panel = editor.context.pane_rect("terminal").unwrap();
    assert_eq!((span.column, span.columns), (panel.column, panel.columns));
    assert_eq!(line(&frame, top), None);
    assert_eq!(edge(&frame, top - 1), theme.border);
    // Over empty header space a press would resize, so it is lit.
    send(
        &mut editor,
        &mut frame,
        [pointer(PointerKind::Move, 30.5, y)],
    );
    assert_eq!(edge(&frame, top - 1), theme.accent);
    // Over a tab a press would click instead.
    send(
        &mut editor,
        &mut frame,
        [pointer(PointerKind::Move, 2.5, y)],
    );
    assert_eq!(edge(&frame, top - 1), theme.border);
    // A drag stays lit once the pointer has moved off.
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Move, 30.5, y),
            pointer(PointerKind::Down(MouseButton::Left), 30.5, y),
            pointer(PointerKind::Move, 30.5, 10.5),
        ],
    );
    let top = terminal_header(&editor);
    assert_eq!(top, 10);
    assert_eq!(edge(&frame, top - 1), theme.accent);
    send(
        &mut editor,
        &mut frame,
        [pointer(PointerKind::Up(MouseButton::Left), 30.5, 10.5)],
    );
    click(&mut editor, &mut frame, [50.5, 3.5]);
    assert_eq!(edge(&frame, top - 1), theme.border);
}

#[test]
fn only_the_focused_pane_has_a_lit_header_line() {
    let fixture = Fixture::new();
    let mut editor = terminal_editor(&fixture);
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [InputEvent::Focused(true), cmd("j")],
    );
    let top = terminal_header(&editor);
    // The line drawn last at a cell is the one on screen.
    let line = |frame: &Framebuffer, column: u32, row: u32| {
        frame
            .graphics()
            .filter(|p| {
                p.rect.row == row
                    && (p.rect.column..p.rect.column + p.rect.columns).contains(&column)
            })
            .filter_map(|p| match p.graphic {
                crate::renderer::Graphic::Divider {
                    vertical: false,
                    color,
                } => Some(color),
                _ => None,
            })
            .last()
            .unwrap()
    };
    let theme = crate::ui::Theme::default();
    let lit = |frame: &Framebuffer| {
        [[5, 0], [50, 0], [50, top - 1]].map(|[x, y]| line(frame, x, y) == theme.active_border)
    };
    assert!(editor.terminal_focused());
    assert_eq!(lit(&frame), [false, false, true], "terminal");
    click(&mut editor, &mut frame, [50.5, 3.5]);
    assert_eq!(lit(&frame), [false, true, false], "editor");
    click(&mut editor, &mut frame, [5.5, 3.5]);
    assert_eq!(lit(&frame), [true, false, false], "files");
    // Nothing is focused while another window is in front.
    send(&mut editor, &mut frame, [InputEvent::Focused(false)]);
    assert_eq!(lit(&frame), [false, false, false], "unfocused window");
}

/// Keys that move in a line reach the program, including those with Cmd.
#[test]
fn line_editing_keys_reach_the_terminal_program() {
    let fixture = Fixture::new();
    let mut editor = terminal_editor(&fixture);
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [cmd("j")]);
    wait_for_screen(&mut editor, &mut frame, "$");
    let arrow = |code, named, modifiers| {
        InputEvent::Key(KeyEvent {
            physical_key: PhysicalKey::Code(code),
            key_without_modifiers: Some(Key::Named(named)),
            logical_key: Key::Named(named),
            location: KeyLocation::Standard,
            state: KeyState::Pressed,
            repeat: false,
            synthetic: false,
            modifiers,
            composing: false,
        })
    };
    send(&mut editor, &mut frame, [text("cat -v"), enter()]);
    let shift = Modifiers {
        shift: true,
        ..Default::default()
    };
    send(
        &mut editor,
        &mut frame,
        [
            arrow(KeyCode::ArrowLeft, NamedKey::ArrowLeft, shift),
            enter(),
        ],
    );
    wait_for_screen(&mut editor, &mut frame, "^[[D");
    if cfg!(target_os = "macos") {
        let option = Modifiers {
            alt: true,
            ..Default::default()
        };
        let command = Modifiers {
            super_key: true,
            ..Default::default()
        };
        send(
            &mut editor,
            &mut frame,
            [
                arrow(KeyCode::ArrowLeft, NamedKey::ArrowLeft, option),
                enter(),
            ],
        );
        wait_for_screen(&mut editor, &mut frame, "^[b");
        send(
            &mut editor,
            &mut frame,
            [
                arrow(KeyCode::ArrowLeft, NamedKey::ArrowLeft, command),
                enter(),
            ],
        );
        wait_for_screen(&mut editor, &mut frame, "^A");
    }
}

#[test]
fn terminal_toggles_runs_commands_and_keeps_its_session_while_hidden() {
    let fixture = Fixture::new();
    let path = fixture.file("a.txt", "file text");
    let mut editor = terminal_editor(&fixture);
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    assert!(!editor.terminal_open());
    send(&mut editor, &mut frame, [cmd("j")]);
    assert!(editor.terminal_open());
    assert!(editor.terminal_focused());
    // Typing reaches the shell, not the document, and starts in the workspace.
    send(
        &mut editor,
        &mut frame,
        [text("echo out-$((40+2)); pwd"), enter()],
    );
    wait_for_screen(&mut editor, &mut frame, "out-42");
    wait_for_screen(&mut editor, &mut frame, "teditor-editor-");
    assert_eq!(
        editor.active_document().unwrap().text.to_text(),
        "file text"
    );
    // The panel fits under the workspace, above the status row.
    assert!(row(&frame, 29).contains("Terminal"));
    assert_eq!(terminal_header(&editor), 17);
    assert!(
        !row(&frame, 17).contains("Hide"),
        "the status bar's chip hides it"
    );
    // Hiding keeps the shell; state set before hiding is still there afterward.
    send(
        &mut editor,
        &mut frame,
        [text("KEPT=yes"), enter(), cmd("j")],
    );
    assert!(!editor.terminal_open());
    assert!(!screen_text(&frame).contains("out-42"));
    send(&mut editor, &mut frame, [text("typed in file ")]);
    assert_eq!(
        editor.active_document().unwrap().text.to_text(),
        "typed in file file text"
    );
    send(
        &mut editor,
        &mut frame,
        [cmd("j"), text("echo was-$KEPT"), enter()],
    );
    wait_for_screen(&mut editor, &mut frame, "was-yes");
    // Exiting the shell closes the panel and returns the keyboard; the next toggle is fresh.
    send(&mut editor, &mut frame, [text("exit"), enter()]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while editor.terminal_open() {
        assert!(
            std::time::Instant::now() < deadline,
            "the panel stayed open"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        editor.compose(&mut frame);
    }
    assert_eq!(editor.status(), "Terminal exited");
    send(&mut editor, &mut frame, [text("back ")]);
    assert!(
        editor
            .active_document()
            .unwrap()
            .text
            .to_text()
            .contains("back ")
    );
    send(
        &mut editor,
        &mut frame,
        [cmd("j"), text("echo fresh-$KEPT."), enter()],
    );
    wait_for_screen(&mut editor, &mut frame, "fresh-.");
}

/// Cmd+Shift plus a key, as a layout reports it.
fn cmd_shift(ch: &str) -> InputEvent {
    key(Key::Character(ch.into()), true, true)
}
fn wait_until(editor: &mut Editor, frame: &mut Framebuffer, done: impl Fn(&Editor) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done(editor) {
        assert!(std::time::Instant::now() < deadline, "condition never met");
        std::thread::sleep(std::time::Duration::from_millis(2));
        editor.compose(frame);
    }
}

#[test]
fn terminal_tabs_run_separate_shells_that_keep_working_in_the_background() {
    let fixture = Fixture::new();
    let mut editor = terminal_editor(&fixture);
    let mut frame = frame();
    editor.compose(&mut frame);
    // Cmd+T opens the panel when it is hidden, then adds tabs; each shell has its own state.
    send(
        &mut editor,
        &mut frame,
        [cmd("t"), text("FIRST=one; echo tab-$FIRST"), enter()],
    );
    assert!(editor.terminal_open());
    wait_for_screen(&mut editor, &mut frame, "tab-one");
    send(
        &mut editor,
        &mut frame,
        [cmd("t"), text("echo second-[$FIRST]"), enter()],
    );
    assert_eq!(editor.terminals.len(), 2);
    wait_for_screen(&mut editor, &mut frame, "second-[]");
    assert!(
        !screen_text(&frame).contains("tab-one"),
        "only one tab shows"
    );

    // A command started in a background tab keeps running and its output is waiting.
    send(
        &mut editor,
        &mut frame,
        [
            cmd("1"),
            text("sleep 0.2; echo finished-in-background"),
            enter(),
        ],
    );
    wait_for_screen(&mut editor, &mut frame, "tab-one");
    send(&mut editor, &mut frame, [cmd("2")]);
    wait_for_screen(&mut editor, &mut frame, "second-[]");
    std::thread::sleep(std::time::Duration::from_millis(400));
    editor.compose(&mut frame);
    assert!(!screen_text(&frame).contains("finished-in-background"));
    send(&mut editor, &mut frame, [cmd("1")]);
    wait_for_screen(&mut editor, &mut frame, "finished-in-background");

    // Cycling wraps both ways, and the brackets arrive however the layout reports them.
    send(&mut editor, &mut frame, [cmd_shift("]")]);
    assert_eq!(editor.active_terminal, 1);
    send(&mut editor, &mut frame, [cmd_shift("]")]);
    assert_eq!(editor.active_terminal, 0);
    send(&mut editor, &mut frame, [cmd_shift("[")]);
    assert_eq!(editor.active_terminal, 1);

    // Hiding keeps every tab, and showing returns to the same one.
    send(&mut editor, &mut frame, [cmd("j")]);
    assert!(!editor.terminal_open());
    assert_eq!(editor.terminals.len(), 2);
    send(&mut editor, &mut frame, [cmd("j")]);
    assert_eq!(editor.active_terminal, 1);
    wait_for_screen(&mut editor, &mut frame, "second-[]");

    // Exiting one shell closes only its tab; exiting the last closes the panel.
    send(&mut editor, &mut frame, [text("exit"), enter()]);
    wait_until(&mut editor, &mut frame, |e| e.terminals.len() == 1);
    assert!(editor.terminal_open());
    assert!(editor.terminal_focused());
    wait_for_screen(&mut editor, &mut frame, "finished-in-background");
    send(&mut editor, &mut frame, [text("exit"), enter()]);
    wait_until(&mut editor, &mut frame, |e| !e.terminal_open());
    assert!(editor.terminals.is_empty());
}

#[test]
fn terminal_tab_row_works_with_the_mouse_and_cmd_w_closes_the_focused_kind_of_tab() {
    let fixture = Fixture::new();
    let path = fixture.file("a.txt", "file text");
    let mut editor = terminal_editor(&fixture);
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [cmd("j")]);
    let header = terminal_header(&editor);
    assert!(
        row(&frame, header).contains('×'),
        "one shell still has a tab"
    );
    // The + starts another shell.
    let plus = cell_of(&frame, header, "+");
    click(&mut editor, &mut frame, plus);
    assert_eq!(editor.terminals.len(), 2);
    assert_eq!(editor.active_terminal, 1);
    // Clicking the first tab's label switches to it and returns the keyboard.
    click(&mut editor, &mut frame, [3.5, f64::from(header) + 0.5]);
    assert_eq!(editor.active_terminal, 0);
    assert!(editor.terminal_focused());
    // Its × closes that shell only.
    let close = cell_of(&frame, header, "×");
    click(&mut editor, &mut frame, close);
    assert_eq!(editor.terminals.len(), 1);
    assert_eq!(editor.tab_count(), 1, "the document's tab stays");
    // Cmd+W closes the terminal tab while the terminal is focused, not the document.
    send(&mut editor, &mut frame, [cmd("w")]);
    assert!(editor.terminals.is_empty() && !editor.terminal_open());
    assert_eq!(editor.tab_count(), 1);
    assert!(!editor.terminal_focused());
    // With the document focused, it closes the document instead.
    send(&mut editor, &mut frame, [cmd("w")]);
    assert_eq!(editor.tab_count(), 0);
}

#[test]
fn the_terminal_shows_whole_graphemes_from_a_real_shell() {
    let fixture = Fixture::new();
    let mut editor = terminal_editor(&fixture);
    let mut frame = frame();
    editor.compose(&mut frame);
    // Octal escapes, so the clusters only exist once the shell has printed them.
    let printf = "printf 'cafe\\314\\201.\\360\\237\\221\\251\\342\\200\\215\\360\\237\\222\\273.\\360\\237\\207\\251\\360\\237\\207\\252.\\n'";
    send(&mut editor, &mut frame, [cmd("j"), text(printf), enter()]);
    let shows = |frame: &Framebuffer, wanted: &str| {
        (0..30).any(|y| {
            row(frame, y)
                .chars()
                .any(|c| crate::renderer::cluster_text(c).is_some_and(|text| &*text == wanted))
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !shows(&frame, TECHNOLOGIST) {
        assert!(
            std::time::Instant::now() < deadline,
            "no joined emoji:\n{}",
            screen_text(&frame)
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        editor.compose(&mut frame);
    }
    assert!(shows(&frame, ACCENTED), "{}", screen_text(&frame));
    assert!(shows(&frame, FLAG), "{}", screen_text(&frame));
}

#[test]
fn terminal_panel_works_with_the_mouse_and_keeps_ctrl_for_the_program() {
    let fixture = Fixture::new();
    let mut editor = terminal_editor(&fixture);
    let mut frame = frame();
    editor.compose(&mut frame);
    let target = cell_of(&frame, 29, "Terminal");
    click(&mut editor, &mut frame, target);
    assert!(editor.terminal_open());
    // Before its prompt, the shell has no handlers yet and an interrupt would end it.
    wait_for_screen(&mut editor, &mut frame, "$ ");
    // Ctrl+C interrupts in the shell rather than copying in the editor.
    // Printing first proves the line is running, so the interrupt cannot arrive too early.
    send(
        &mut editor,
        &mut frame,
        [text("echo go-$((1+1)); sleep 100"), enter()],
    );
    wait_for_screen(&mut editor, &mut frame, "go-2");
    let ctrl_c = InputEvent::Key(KeyEvent {
        physical_key: PhysicalKey::Code(KeyCode::KeyC),
        key_without_modifiers: Some(Key::Character("c".into())),
        logical_key: Key::Character("c".into()),
        location: KeyLocation::Standard,
        state: KeyState::Pressed,
        repeat: false,
        synthetic: false,
        modifiers: Modifiers {
            control: true,
            ..Default::default()
        },
        composing: false,
    });
    // One press can land just as the shell hands the terminal to the new process, where
    // it reaches only the shell, so press again as a person would. The limit is far
    // shorter than the sleep, so a returning prompt proves the interrupt worked.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while screen_text(&frame).matches("$ ").count() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "sleep was not interrupted"
        );
        send(&mut editor, &mut frame, [ctrl_c.clone()]);
        std::thread::sleep(std::time::Duration::from_millis(50));
        editor.compose(&mut frame);
    }
    send(
        &mut editor,
        &mut frame,
        [text("echo after-$((1+1))"), enter()],
    );
    wait_for_screen(&mut editor, &mut frame, "after-2");
    // Clicking the editor moves the keyboard there; clicking the panel moves it back.
    click(&mut editor, &mut frame, [50.0, 3.5]);
    assert!(!editor.terminal_focused());
    click(&mut editor, &mut frame, [50.0, 25.5]);
    assert!(editor.terminal_focused());
    // Dragging the header resizes; the height survives hiding and is clamped.
    let top = terminal_header(&editor);
    // Empty header space, clear of the tabs and their buttons.
    let [x, y] = [30.5, f64::from(top) + 0.5];
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), x, y),
            pointer(PointerKind::Move, x, 10.5),
            pointer(PointerKind::Up(MouseButton::Left), x, 10.5),
        ],
    );
    assert_eq!(terminal_header(&editor), 10);
    assert!(editor.terminal_open());
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), x, 10.5),
            pointer(PointerKind::Move, x, 0.5),
            pointer(PointerKind::Up(MouseButton::Left), x, 0.5),
        ],
    );
    assert_eq!(terminal_header(&editor), 4, "keeps workspace rows");
    // One status-bar chip both hides and shows the panel, at the size it was.
    let chip = cell_of(&frame, 29, "Terminal");
    click(&mut editor, &mut frame, chip);
    assert!(!editor.terminal_open());
    click(&mut editor, &mut frame, chip);
    assert!(editor.terminal_open());
    assert_eq!(terminal_header(&editor), 4);
}

#[test]
fn palette_opened_from_the_terminal_returns_there_and_small_windows_survive() {
    let fixture = Fixture::new();
    let mut editor = terminal_editor(&fixture);
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [cmd("j"), cmd("p")]);
    assert!(editor.palette_open());
    assert!(screen_text(&frame).contains("Toggle terminal"));
    send(&mut editor, &mut frame, [named(NamedKey::Escape)]);
    assert!(editor.terminal_focused());
    for size in [[35, 8], [10, 3], [1, 1], [0, 0], [100, 30]] {
        let mut small = Framebuffer::new(size, crate::ui::Theme::default().panel).unwrap();
        editor.compose(&mut small);
    }
}

#[test]
fn terminal_cursor_follows_a_program_that_only_moves_it() {
    let fixture = Fixture::new();
    let mut editor = terminal_editor(&fixture);
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [cmd("j")]);
    wait_for_screen(&mut editor, &mut frame, "$ ");
    send(&mut editor, &mut frame, [text("abcd")]);
    wait_for_screen(&mut editor, &mut frame, "abcd");
    let wait_for_column = |editor: &mut Editor, frame: &mut Framebuffer, column: u32| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            editor.compose(frame);
            let caret = editor.ime_cursor_cells().unwrap().column;
            if caret == column {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cursor stayed at {caret}, expected {column}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    };
    let start = editor.ime_cursor_cells().unwrap().column;
    let left = InputEvent::Key(KeyEvent {
        physical_key: PhysicalKey::Code(KeyCode::ArrowLeft),
        key_without_modifiers: None,
        logical_key: Key::Named(NamedKey::ArrowLeft),
        location: KeyLocation::Standard,
        state: KeyState::Pressed,
        repeat: false,
        synthetic: false,
        modifiers: Modifiers::default(),
        composing: false,
    });
    // The shell redraws nothing for this, only moving the cursor back.
    send(&mut editor, &mut frame, [left]);
    wait_for_column(&mut editor, &mut frame, start - 1);
}

#[test]
fn wide_characters_take_two_columns_in_documents_and_the_terminal() {
    let fixture = Fixture::new();
    let path = fixture.file("party.txt", "a\u{1F389}b\nsecond line\n");
    let mut editor = terminal_editor(&fixture);
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let text_row = row(&frame, 1);
    let start = text_row.find("a\u{1F389}").unwrap();
    let cells: Vec<char> = text_row[start..].chars().take(4).collect();
    assert_eq!(cells, ['a', '\u{1F389}', Cell::CONTINUATION, 'b']);
    let doc = editor.active_document().unwrap();
    assert_eq!(doc.line_columns(0), 4);
    // Clicking either half lands on the emoji; moving past it counts two columns.
    let column = text_row[..start].chars().count() as f64;
    for half in [1.5, 2.5] {
        click(&mut editor, &mut frame, [column + half, 1.5]);
        assert_eq!(editor.active_document().unwrap().position(), [1, 0]);
    }
    send(&mut editor, &mut frame, [named(NamedKey::ArrowRight)]);
    assert_eq!(editor.active_document().unwrap().position(), [3, 0]);
    // Column positions stay aligned between lines around the emoji.
    send(&mut editor, &mut frame, [named(NamedKey::ArrowDown)]);
    assert_eq!(editor.active_document().unwrap().position(), [3, 1]);
    // The terminal takes the width from Ghostty and keeps the next character aligned.
    send(&mut editor, &mut frame, [cmd("j")]);
    wait_for_screen(&mut editor, &mut frame, "$ ");
    send(
        &mut editor,
        &mut frame,
        [text("printf 'x\\360\\237\\216\\211y\\n'"), enter()],
    );
    wait_for_screen(&mut editor, &mut frame, "x\u{1F389}\0y");
}

#[test]
fn wide_characters_are_clipped_whole_at_the_edge_of_a_window() {
    let fixture = Fixture::new();
    let path = fixture.file("edge.txt", &format!("{}\u{1F389}", "x".repeat(200)));
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = Framebuffer::new([50, 10], crate::ui::Theme::default().panel).unwrap();
    // Every horizontal scroll position leaves no half glyph behind.
    for scroll in 150..=205 {
        editor.tabs[0].document.scroll[0] = scroll;
        editor.tabs[0].document.reveal = false;
        editor.compose(&mut frame);
        let line: Vec<char> = row(&frame, 1).chars().collect();
        for (x, ch) in line.iter().enumerate() {
            if *ch == Cell::CONTINUATION {
                assert_eq!(line[x - 1], '\u{1F389}', "scroll {scroll}");
            }
            if *ch == '\u{1F389}' {
                assert_eq!(line[x + 1], Cell::CONTINUATION, "scroll {scroll}");
            }
        }
    }
}

#[test]
fn tab_cross_cancels_outside_release_and_remains_reachable_with_overflow() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    for name in ["first-long-document.txt", "second-long-document.txt"] {
        editor.open(&fixture.file(name, "text")).unwrap();
    }
    let mut frame = Framebuffer::new([35, 8], crate::ui::Theme::default().panel).unwrap();
    editor.compose(&mut frame);
    let [x, y] = close_button(&frame, 0);
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), x, y),
            pointer(PointerKind::Move, x, y - 1.0),
            pointer(PointerKind::Up(MouseButton::Left), x, y - 1.0),
        ],
    );
    assert_eq!(editor.tab_count(), 2);
    click_close(&mut editor, &mut frame, 0);
    assert_eq!(editor.tab_count(), 1);
    assert_eq!(
        editor.active_document().unwrap().name(),
        "first-long-document.txt"
    );
    click_close(&mut editor, &mut frame, 0);
    assert_eq!(editor.tab_count(), 0);
}

#[test]
fn unsaved_close_and_quit_require_save_or_discard_and_escape_cancels() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [text("unsaved"), cmd("w")]);
    assert!(editor.palette_open());
    assert_eq!(editor.tab_count(), 1);
    send(&mut editor, &mut frame, [named(NamedKey::Escape)]);
    assert!(!editor.palette_open());
    editor.request_quit();
    editor.compose(&mut frame);
    assert!(!editor.quit_requested());
    send(&mut editor, &mut frame, [text("s")]);
    assert!(matches!(
        editor.palette.as_ref().unwrap().mode,
        Mode::SaveAs(_)
    ));
    send(
        &mut editor,
        &mut frame,
        [text("saved.txt"), named(NamedKey::Enter)],
    );
    assert_eq!(
        fs::read_to_string(fixture.0.join("saved.txt")).unwrap(),
        "unsaved"
    );
    assert!(editor.quit_requested());
}

#[test]
fn ime_preedit_is_temporary_and_document_switch_blocks_late_commit() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [
            InputEvent::Ime(ImeEvent::Enabled),
            InputEvent::Ime(ImeEvent::Preedit(Preedit {
                text: "é".into(),
                cursor: Some((2, 2)),
            })),
        ],
    );
    assert_eq!(editor.active_document().unwrap().text.to_text(), "");
    assert!(row(&frame, 1).contains('é'));
    send(
        &mut editor,
        &mut frame,
        [InputEvent::Text(TextInput {
            text: "é".into(),
            source: TextSource::Ime,
            modifiers: Modifiers::default(),
        })],
    );
    assert_eq!(editor.active_document().unwrap().text.to_text(), "é");
    send(
        &mut editor,
        &mut frame,
        [
            cmd("n"),
            InputEvent::Text(TextInput {
                text: "late".into(),
                source: TextSource::Ime,
                modifiers: Modifiers::default(),
            }),
        ],
    );
    assert_eq!(editor.active_document().unwrap().text.to_text(), "");
}

#[test]
fn shrinking_layout_keeps_tabs_palette_and_cursor_within_frame() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    for size in [[100, 30], [35, 8], [10, 3], [1, 1], [0, 0], [80, 20]] {
        let mut frame = Framebuffer::new(size, crate::ui::Theme::default().panel).unwrap();
        editor.compose(&mut frame);
        if let Some(caret) = editor.ime_cursor_cells() {
            assert!(caret.column < size[0] && caret.row < size[1]);
        }
    }
}

#[test]
fn indentation_is_one_undo_step_and_double_click_selects_unicode_words() {
    let mut doc = Document::new();
    doc.insert("α_beta\n  second\n");
    doc.select_word(2);
    assert_eq!(doc.selected_text(), "α_beta");
    doc.select_all();
    doc.indent(false).unwrap();
    assert_eq!(doc.text.to_text(), "    α_beta\n      second\n");
    doc.undo();
    assert_eq!(doc.text.to_text(), "α_beta\n  second\n");
    doc.redo();
    doc.indent(true).unwrap();
    assert_eq!(doc.text.to_text(), "α_beta\n  second\n");
    doc.set_cursor(doc.offset(1, 2), false);
    doc.indent(true).unwrap();
    assert_eq!(doc.line(1), "second");
    assert_eq!(doc.position(), [0, 1]);
    doc.undo();
    assert_eq!(doc.position(), [2, 1]);
}

#[test]
fn syntax_colors_follow_save_as_decoded_text_selection_and_edits() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.tabs[editor.active]
        .document
        .insert("fn main() {\n    let s = \"αβ\";\n}\n");
    editor.compose(&mut frame);
    wait_for_background(&mut editor, &mut frame);
    let theme = crate::ui::Theme::default();
    assert_eq!(frame.cell(29, 1).unwrap().style.foreground, theme.text);
    let path = fixture.0.join("saved.rs");
    editor.tabs[editor.active]
        .document
        .save(Some(&path))
        .unwrap();
    editor.compose(&mut frame);
    wait_for_background(&mut editor, &mut frame);
    let keyword = frame.cell(29, 1).unwrap().style.foreground;
    assert_ne!(keyword, theme.text);
    let string = frame.cell(43, 2).unwrap().style.foreground;
    assert_ne!(string, keyword);
    let doc = &mut editor.tabs[editor.active].document;
    let start = doc.line_start(1) + "    let s = \"".len();
    doc.anchor = start;
    doc.cursor = start + "αβ".len();
    doc.reveal = false;
    editor.focus = Some(Focus::Files);
    editor.compose(&mut frame);
    assert_eq!(frame.cell(42, 2).unwrap().character, 'α');
    assert_eq!(frame.cell(42, 2).unwrap().style.foreground, string);
    assert_eq!(frame.cell(42, 2).unwrap().style.background, theme.hover);
    editor.tabs[editor.active].document.insert("new");
    editor.compose(&mut frame);
    // Editing line two must not remove the cached color from line one.
    assert_eq!(frame.cell(29, 1).unwrap().style.foreground, keyword);
    // Inspect the immediate edit frame as well as the settled parser result.
    // The edited string and other tokens on its line retain their colors.
    for (i, ch) in "new".chars().enumerate() {
        assert_eq!(frame.cell(42 + i as u32, 2).unwrap().character, ch);
        assert_eq!(
            frame.cell(42 + i as u32, 2).unwrap().style.foreground,
            string
        );
    }
    assert_eq!(frame.cell(33, 2).unwrap().style.foreground, keyword);
    wait_for_background(&mut editor, &mut frame);
    editor.tabs[editor.active].document.undo();
    editor.compose(&mut frame);
    wait_for_background(&mut editor, &mut frame);
    assert_eq!(frame.cell(42, 2).unwrap().character, 'α');
    assert_eq!(frame.cell(42, 2).unwrap().style.foreground, string);
    // The parser consumes decoded UTF-8 offsets, not the original file's units.
    let utf16 = fixture.0.join("utf16.rs");
    let mut bytes = vec![0xff, 0xfe];
    for unit in "fn main() { let s = \"αβ\"; }\r\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(&utf16, bytes).unwrap();
    editor.request_open(&utf16).unwrap();
    editor.compose(&mut frame);
    wait_for_background(&mut editor, &mut frame);
    // The focused caret covers the first cell; inspect the second keyword cell.
    assert_eq!(frame.cell(30, 1).unwrap().style.foreground, keyword);
    assert_eq!(
        editor.active_document().unwrap().encoding(),
        Encoding::Utf16Le
    );
}

#[test]
fn text_runs_preserve_tab_clipping_selection_and_control_characters() {
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.tabs[editor.active]
        .document
        .insert(&format!("a\tβ\0xyz{}\nsecond", ".".repeat(100)));
    let mut frame = frame();
    editor.compose(&mut frame);
    let doc = &mut editor.tabs[editor.active].document;
    doc.anchor = 1;
    doc.cursor = "a\tβ\0".len();
    doc.scroll = [2, 0];
    doc.reveal = false;
    // Move focus away so the caret does not cover the selection boundary.
    editor.focus = Some(Focus::Files);
    editor.compose(&mut frame);
    let theme = crate::ui::Theme::default();
    for (i, expected) in "  β·xyz.".chars().enumerate() {
        let cell = frame.cell(29 + i as u32, 1).unwrap();
        assert_eq!(cell.character, expected);
        assert_eq!(
            cell.style.background,
            if i < 4 { theme.hover } else { theme.control }
        );
    }
    assert_eq!(frame.cell(98, 1).unwrap().character, '.');
    assert_eq!(frame.cell(99, 1).unwrap().character, ' ');
}

#[test]
fn fractional_wheel_and_scrollbar_drag_move_viewport_without_moving_caret() {
    let fixture = Fixture::new();
    let path = fixture.file(
        "long.txt",
        &(0..100).map(|i| format!("line {i}\n")).collect::<String>(),
    );
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    let InputEvent::Pointer(p) = pointer(PointerKind::Move, 50.0, 10.0) else {
        unreachable!()
    };
    let wheel = || {
        InputEvent::Scroll(ScrollEvent {
            delta: MouseScrollDelta::LineDelta(0.0, -0.4),
            cells: [0.0, -0.4],
            phase: TouchPhase::Moved,
            position: p.position,
            modifiers: Modifiers::default(),
        })
    };
    send(&mut editor, &mut frame, [wheel(), wheel()]);
    assert_eq!(editor.active_document().unwrap().scroll[1], 0);
    send(&mut editor, &mut frame, [wheel()]);
    assert_eq!(editor.active_document().unwrap().scroll[1], 1);
    assert_eq!(editor.active_document().unwrap().cursor, 0);
    send(
        &mut editor,
        &mut frame,
        [
            pointer(PointerKind::Down(MouseButton::Left), 99.3, 3.0),
            pointer(PointerKind::Move, 99.3, 26.0),
            pointer(PointerKind::Up(MouseButton::Left), 99.3, 26.0),
        ],
    );
    assert!(editor.active_document().unwrap().scroll[1] > 60);
    assert_eq!(editor.active_document().unwrap().cursor, 0);
    send(&mut editor, &mut frame, [text("X")]);
    assert_eq!(editor.active_document().unwrap().scroll[1], 0);
    assert!(
        editor
            .active_document()
            .unwrap()
            .text
            .to_text()
            .starts_with("Xline 0")
    );
}

#[test]
fn failed_save_as_is_visible_and_does_not_overwrite_another_file() {
    let fixture = Fixture::new();
    let path = fixture.file("existing.txt", "preserve");
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [
            text("my work"),
            cmd("s"),
            text("existing.txt"),
            named(NamedKey::Enter),
        ],
    );
    assert!(editor.palette_open());
    assert_eq!(fs::read_to_string(path).unwrap(), "preserve");
    assert!(row(&frame, 29).contains("already exists"));
    send(
        &mut editor,
        &mut frame,
        [named(NamedKey::Escape), cmd("w"), text("d")],
    );
    assert_eq!(editor.tab_count(), 0);
    assert!(!editor.palette_open());
}

#[test]
fn dropped_file_opens_once_and_focus_loss_does_not_insert_text() {
    let fixture = Fixture::new();
    let path = fixture.file("drop.txt", "text");
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [
            InputEvent::FileDropped {
                path: path.clone(),
                position: None,
            },
            InputEvent::FileDropped {
                path,
                position: None,
            },
            InputEvent::Focused(false),
            text("late"),
        ],
    );
    assert_eq!(editor.tab_count(), 1);
    assert_eq!(editor.active_document().unwrap().text.to_text(), "text");
    send(
        &mut editor,
        &mut frame,
        [InputEvent::Focused(true), text("new ")],
    );
    assert_eq!(editor.active_document().unwrap().text.to_text(), "new text");
}

#[test]
fn unicode_encodings_detect_boms_and_preserve_mixed_newlines() {
    let fixture = Fixture::new();
    let text = "Café 日本 🙂\r\nsecond\nthird\r\n";
    for encoding in [
        Encoding::Utf8,
        Encoding::Utf16Le,
        Encoding::Utf16Be,
        Encoding::Utf32Le,
        Encoding::Utf32Be,
    ] {
        let mut bytes = encoding.bom().to_vec();
        match encoding {
            Encoding::Utf8 => bytes.extend_from_slice(text.as_bytes()),
            Encoding::Utf16Le | Encoding::Utf16Be => {
                for unit in text.encode_utf16() {
                    bytes.extend_from_slice(&if encoding == Encoding::Utf16Le {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    });
                }
            }
            _ => {
                for ch in text.chars() {
                    bytes.extend_from_slice(&if encoding == Encoding::Utf32Le {
                        (ch as u32).to_le_bytes()
                    } else {
                        (ch as u32).to_be_bytes()
                    });
                }
            }
        }
        let path = fixture.0.join("encoded");
        fs::write(&path, &bytes).unwrap();
        let mut doc = Document::open(&path).unwrap();
        assert_eq!(doc.encoding(), encoding);
        assert_eq!(doc.text.to_text(), text);
        assert!(doc.has_bom());
        assert_eq!(doc.line_count(), 4);
        doc.save(None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        doc.set_cursor(doc.line_end(0), false);
        doc.move_horizontal(true, false, false);
        assert_eq!(doc.position(), [0, 1]);
        doc.move_horizontal(false, false, false);
        assert_eq!(doc.cursor, doc.line_end(0));
        doc.set_cursor(0, false);
        doc.insert("X");
        doc.save(None).unwrap();
        let reopened = Document::open(&path).unwrap();
        assert_eq!(reopened.text.to_text(), format!("X{text}"));
        assert_eq!(reopened.encoding(), encoding);
    }
}

#[test]
fn legacy_decoding_byte_view_and_strict_encoding_conversion() {
    let fixture = Fixture::new();
    let cp1252 = fixture.0.join("western.txt");
    let western = b"This is a French cafe: Caf\xe9. The price is 20 \x80.\r\n";
    fs::write(&cp1252, western).unwrap();
    let mut doc = Document::open_with_encoding(
        &cp1252,
        Some(Encoding::Legacy(encoding_rs::WINDOWS_1252)),
        &file_io::Progress::default(),
    )
    .unwrap();
    assert_eq!(
        doc.text.to_text(),
        "This is a French cafe: Café. The price is 20 €.\r\n"
    );
    doc.insert("🙂");
    assert!(
        doc.save(None)
            .unwrap_err()
            .to_string()
            .contains("cannot represent")
    );
    assert_eq!(fs::read(&cp1252).unwrap(), western);
    doc.set_encoding(Encoding::Utf8);
    doc.save(None).unwrap();
    assert_eq!(
        fs::read_to_string(&cp1252).unwrap(),
        "🙂This is a French cafe: Café. The price is 20 €.\r\n"
    );

    let japanese = fixture.0.join("japanese.txt");
    fs::write(&japanese, [0x93, 0xfa, 0x96, 0x7b]).unwrap();
    let mut doc = Document::open_with_encoding(
        &japanese,
        Some(Encoding::Legacy(encoding_rs::SHIFT_JIS)),
        &file_io::Progress::default(),
    )
    .unwrap();
    assert_eq!(doc.text.to_text(), "日本");
    doc.save(None).unwrap();
    assert_eq!(fs::read(&japanese).unwrap(), [0x93, 0xfa, 0x96, 0x7b]);

    let binary = fixture.0.join("all-bytes.bin");
    let bytes: Vec<_> = (0..=255).collect();
    fs::write(&binary, &bytes).unwrap();
    let mut doc = Document::open(&binary).unwrap();
    assert_eq!(doc.encoding(), Encoding::Bytes);
    assert_eq!(doc.text.to_text().chars().count(), 256);
    doc.insert("X");
    doc.save(None).unwrap();
    assert_eq!(
        fs::read(&binary).unwrap(),
        [b"X".as_slice(), &bytes].concat()
    );
    assert!(
        Document::open_with_encoding(&binary, Some(Encoding::Utf8), &file_io::Progress::default())
            .is_err()
    );
}

#[test]
fn repeated_unchanged_saves_keep_noncanonical_legacy_bytes() {
    let fixture = Fixture::new();
    let path = fixture.0.join("stateful.txt");
    let bytes = b"\x1b(Bhello\x1b(B";
    fs::write(&path, bytes).unwrap();
    let mut doc = Document::open_with_encoding(
        &path,
        Some(Encoding::Legacy(encoding_rs::ISO_2022_JP)),
        &file_io::Progress::default(),
    )
    .unwrap();
    assert_eq!(doc.text.to_text(), "hello");
    for _ in 0..2 {
        doc.save(None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn decoding_handles_split_scalars_and_malformed_input_without_data_loss() {
    let fixture = Fixture::new();
    let path = fixture.0.join("boundaries.txt");
    let text = format!("{}🙂é\n", "a".repeat(32_767));
    let mut bytes = vec![0xff, 0xfe];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(&path, &bytes).unwrap();
    let doc = Document::open(&path).unwrap();
    assert_eq!(doc.text.to_text(), text);
    fs::write(&path, format!("{}🙂é\n", "a".repeat(65_535))).unwrap();
    let doc = Document::open(&path).unwrap();
    assert_eq!(doc.encoding(), Encoding::Utf8);
    assert_eq!(doc.text.char_at(65_535), Some('🙂'));
    let mut invalid = b"\xef\xbb\xbfvalid".to_vec();
    invalid.extend_from_slice(&[0xff, 0x80]);
    fs::write(&path, &invalid).unwrap();
    let mut doc = Document::open(&path).unwrap();
    assert_eq!(doc.encoding(), Encoding::Bytes);
    assert!(doc.notice.is_some());
    doc.save(None).unwrap();
    assert_eq!(fs::read(&path).unwrap(), invalid);
}

fn large_fixture(fixture: &Fixture, name: &str, mib: usize, newlines: bool) -> PathBuf {
    use std::io::Write;
    let path = fixture.0.join(name);
    let mut file = std::io::BufWriter::new(fs::File::create(&path).unwrap());
    let mut block = vec![b'a'; 1024 * 1024];
    if newlines {
        for i in (79..block.len()).step_by(80) {
            block[i] = b'\n';
        }
    }
    for _ in 0..mib {
        file.write_all(&block).unwrap();
    }
    file.flush().unwrap();
    path
}

#[test]
fn large_documents_use_mapped_pieces_for_edit_render_undo_and_save() {
    let fixture = Fixture::new();
    let path = large_fixture(&fixture, "large.txt", 24, true);
    let mut doc = Document::open(&path).unwrap();
    assert_eq!(doc.text.len(), 24 * 1024 * 1024);
    assert_eq!(doc.line_count(), 24 * (1024 * 1024 / 80) + 1);
    let stats = doc.text.stats();
    assert_eq!(stats.memory_bytes, 0);
    assert_eq!(stats.mapped_bytes, doc.text.len());
    assert!(stats.height < 20);
    let offset = doc.offset(doc.line_count() / 2, 5);
    doc.set_cursor(offset, false);
    for _ in 0..200 {
        doc.insert("é");
    }
    assert_eq!(doc.text.stats().memory_bytes, 400);
    for _ in 0..200 {
        doc.undo();
    }
    assert!(!doc.dirty());
    assert_eq!(doc.cursor, offset);
    doc.select_all();
    doc.delete(false, false);
    assert!(doc.text.is_empty());
    doc.undo();
    assert_eq!(doc.text.len(), 24 * 1024 * 1024);
    doc.set_cursor(doc.text.len(), false);
    doc.insert("tail");
    doc.save(None).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), 24 * 1024 * 1024 + 4);
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.tabs[0].document = doc;
    let mut frame = frame();
    editor.compose(&mut frame);
    assert!(editor.active_document().unwrap().scroll[1] > 300_000);
    assert!(row(&frame, 27).contains("tail"));
}

#[test]
fn giant_single_line_renders_only_its_viewport_and_background_load_can_be_cancelled() {
    let fixture = Fixture::new();
    let path = large_fixture(&fixture, "one-line.txt", 24, false);
    let mut doc = Document::open(&path).unwrap();
    let end = doc.text.len();
    doc.set_cursor(end, false);
    assert_eq!(doc.position(), [end, 0]);
    assert_eq!(doc.offset(0, end - 10), end - 10);
    assert_eq!(doc.visible_line(0, end - 10, 80).len(), 11);
    let mut editor = Editor::new(&fixture.0).unwrap();
    let mut frame = frame();
    editor.request_open(&path).unwrap();
    assert!(editor.has_pending_io());
    editor.request_close();
    assert_eq!(editor.tab_count(), 1);
    wait_for_io(&mut editor, &mut frame);
    assert!(editor.active_document().unwrap().path.is_none());
    // Completing an unrelated loading tab must not steal the document being edited.
    editor.request_open(&path).unwrap();
    editor.new_document();
    editor.compose(&mut frame);
    send(&mut editor, &mut frame, [text("keep my focus")]);
    assert_eq!(
        editor.active_document().unwrap().text.to_text(),
        "keep my focus"
    );
    assert_eq!(
        editor
            .tabs
            .iter()
            .filter(|t| t.document.path.is_some())
            .count(),
        1
    );
}

#[test]
fn background_save_uses_a_snapshot_and_preserves_edits_made_during_save() {
    let fixture = Fixture::new();
    let path = fixture.file("file.txt", "original");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [text("first "), cmd("s"), text("later ")],
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "first original");
    assert_eq!(
        editor.active_document().unwrap().text.to_text(),
        "first later original"
    );
    assert!(editor.active_document().unwrap().dirty());
    send(&mut editor, &mut frame, [cmd("z")]);
    assert!(!editor.active_document().unwrap().dirty());
}

#[test]
fn a_finishing_save_does_not_answer_an_unsaved_close_prompt() {
    let fixture = Fixture::new();
    let path = fixture.file("file.txt", "original");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [text("saved "), cmd("s"), text("later "), cmd("w")],
    );
    assert!(editor.palette_open());
    assert_eq!(editor.tab_count(), 1);
    assert_eq!(fs::read_to_string(&path).unwrap(), "saved original");
    send(&mut editor, &mut frame, [text("d")]);
    assert_eq!(editor.tab_count(), 0);
    assert_eq!(fs::read_to_string(path).unwrap(), "saved original");
}

#[test]
fn closing_after_undo_during_save_still_waits_for_a_save_decision() {
    let fixture = Fixture::new();
    let path = fixture.file("file.txt", "original");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [text("saved "), cmd("s"), cmd("z"), cmd("w")],
    );
    assert!(editor.palette_open());
    assert_eq!(editor.tab_count(), 1);
    send(&mut editor, &mut frame, [text("s")]);
    assert_eq!(editor.tab_count(), 0);
    assert_eq!(fs::read_to_string(path).unwrap(), "original");
}

#[test]
fn encoding_palette_converts_saves_and_refuses_to_reopen_unsaved_edits() {
    let fixture = Fixture::new();
    let path = fixture.file("file.txt", "hello\n");
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = frame();
    editor.compose(&mut frame);
    send(
        &mut editor,
        &mut frame,
        [
            cmd("p"),
            text(">save with encoding"),
            named(NamedKey::Enter),
            text("utf-16 le"),
            named(NamedKey::Enter),
        ],
    );
    assert_eq!(
        editor.active_document().unwrap().encoding(),
        Encoding::Utf16Le
    );
    assert!(!editor.active_document().unwrap().dirty());
    assert_eq!(fs::read(&path).unwrap(), b"\xff\xfeh\0e\0l\0l\0o\0\n\0");
    send(
        &mut editor,
        &mut frame,
        [
            text("changed "),
            key(Key::Character("e".into()), true, true),
            text("bytes"),
            named(NamedKey::Enter),
        ],
    );
    assert!(editor.palette_open());
    assert!(row(&frame, 29).contains("Save or undo"));
    assert_eq!(
        editor.active_document().unwrap().text.to_text(),
        "changed hello\n"
    );
    send(
        &mut editor,
        &mut frame,
        [
            named(NamedKey::Escape),
            cmd("s"),
            key(Key::Character("e".into()), true, true),
            text("utf-16 le"),
            named(NamedKey::Enter),
        ],
    );
    assert_eq!(
        editor.active_document().unwrap().encoding(),
        Encoding::Utf16Le
    );
}

#[test]
#[ignore = "scrolling performance probe using tests/renderer_gpu.rs"]
fn source_file_scrolling_performance_probe() {
    use std::{hint::black_box, time::Instant};
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/renderer_gpu.rs");
    let fixture = Fixture::new();
    let mut editor = Editor::new(&fixture.0).unwrap();
    editor.open(&path).unwrap();
    let mut frame = Framebuffer::new([160, 50], crate::ui::Theme::default().panel).unwrap();
    editor.compose(&mut frame);
    wait_for_io(&mut editor, &mut frame);
    let mut samples = Vec::new();
    let mut viewport_samples = Vec::new();
    let mut edit_samples = Vec::new();
    for step in 0..120 {
        let top = step * (editor.active_document().unwrap().line_count() - 50) / 120;
        let doc = &mut editor.tabs[editor.active].document;
        doc.scroll = [0, top];
        doc.reveal = false;
        let now = Instant::now();
        for row in top..top + 45 {
            black_box(doc.line_columns(row));
            black_box(doc.visible_line(row, 0, 110));
        }
        viewport_samples.push(now.elapsed());
        // Measure fully colored frames, with grammar work outside the UI timing.
        editor.compose(&mut frame);
        wait_for_background(&mut editor, &mut frame);
        editor.handle(InputEvent::Scroll(ScrollEvent {
            position: Some(PointerPosition {
                physical: [800.0, 260.0],
                grid: [80.0, 10.0],
                cell: Some([80, 10]),
            }),
            cells: [0.0, -1.0],
            delta: MouseScrollDelta::LineDelta(0.0, -1.0),
            phase: TouchPhase::Moved,
            modifiers: Modifiers::default(),
        }));
        let now = Instant::now();
        editor.compose(&mut frame);
        samples.push(now.elapsed());
        black_box(&frame);
        let doc = &mut editor.tabs[editor.active].document;
        doc.set_cursor(doc.offset(top + 10, 10), false);
        doc.reveal = false;
        editor.handle(text("é"));
        let now = Instant::now();
        editor.compose(&mut frame);
        edit_samples.push(now.elapsed());
        editor.tabs[editor.active].document.undo();
        editor.tabs[editor.active].document.reveal = false;
        editor.compose(&mut frame);
    }
    for (label, mut samples) in [
        ("viewport", viewport_samples),
        ("scroll compose", samples),
        ("edit compose", edit_samples),
    ] {
        samples.sort();
        eprintln!(
            "{label}: median {:?}, p95 {:?}, max {:?}",
            samples[60], samples[114], samples[119]
        );
    }
}

#[test]
#[ignore = "large-file performance probe: writes a 1 GiB temporary file"]
fn large_file_performance_probe() {
    let fixture = Fixture::new();
    let path = large_fixture(&fixture, "gigabyte.txt", 1024, false);
    let start = std::time::Instant::now();
    let mut doc = Document::open(&path).unwrap();
    let load = start.elapsed();
    let mut worst = std::time::Duration::ZERO;
    for index in 0..1000 {
        let position = (doc.text.len() / 1000) * index;
        let start = std::time::Instant::now();
        doc.set_cursor(position, false);
        doc.insert("é");
        assert!(doc.visible_line(0, position.saturating_sub(20), 120).len() <= 120);
        doc.undo();
        worst = worst.max(start.elapsed());
    }
    eprintln!(
        "1 GiB load: {load:?}; worst edit + viewport + undo: {worst:?}; {:?}",
        doc.text.stats()
    );
    assert_eq!(doc.text.stats().memory_bytes, 0);
    assert!(!doc.dirty());
}
