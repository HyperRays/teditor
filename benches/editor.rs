//! CPU benchmarks: the piece tree, document edits and whole-frame composition.
//!
//! Run with `cargo bench --bench editor`. No GPU or window is needed.
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use teditor::{
    editor::{Editor, buffer::TextBuffer, document::Document, search},
    terminal::{
        Framebuffer, InputEvent,
        input::{
            Modifiers, MouseScrollDelta, PointerPosition, ScrollEvent, TextInput, TextSource,
            TouchPhase,
        },
    },
    ui::Theme,
};

/// Roughly 1.5 MiB of source-like text across 40,000 lines.
fn source() -> String {
    (0..40_000)
        .map(|n| format!("    let value_{n} = compute(\"α\", {n}); // note\n"))
        .collect()
}

fn buffer(c: &mut Criterion) {
    let text = source();
    let buffer = TextBuffer::from_text(&text);
    c.bench_function("buffer/from_text", |b| {
        b.iter(|| TextBuffer::from_text(black_box(&text)))
    });
    c.bench_function("buffer/line_start", |b| {
        let mut row = 0;
        b.iter(|| {
            row = (row + 7919) % buffer.line_count();
            black_box(buffer.line_start(row))
        })
    });
    c.bench_function("buffer/row_at", |b| {
        let mut offset = 0;
        b.iter(|| {
            offset = (offset + 104_729) % buffer.len();
            black_box(buffer.row_at(offset))
        })
    });
    c.bench_function("buffer/columns_of_line", |b| {
        let start = buffer.line_start(20_000);
        let end = buffer.line_start(20_001) - 1;
        b.iter(|| black_box(buffer.columns(start..end)))
    });
    c.bench_function("buffer/replace", |b| {
        let inserted = TextBuffer::from_text("x");
        b.iter_batched_ref(
            || buffer.clone(),
            |edited| edited.replace(700_000..700_000, &inserted),
            BatchSize::SmallInput,
        )
    });
    c.bench_function("buffer/read_all_chunks", |b| {
        b.iter(|| {
            buffer
                .chunks(0..buffer.len())
                .map(|(_, chunk)| chunk.len())
                .sum::<usize>()
        })
    });
}

fn document(c: &mut Criterion) {
    let text = source();
    c.bench_function("document/typing_with_undo_history", |b| {
        b.iter_batched_ref(
            || {
                let mut doc = Document::new();
                doc.insert(&text);
                doc.set_cursor(700_000, false);
                doc
            },
            |doc| {
                for _ in 0..50 {
                    doc.insert("x");
                }
            },
            BatchSize::LargeInput,
        )
    });
    c.bench_function("document/visible_screen", |b| {
        let mut doc = Document::new();
        doc.insert(&text);
        b.iter(|| {
            for row in 20_000..20_050 {
                black_box(doc.visible_line(row, 0, 120));
            }
        })
    });
}

/// Searching text in memory, by what a query is: a word that is rare, one that is on nearly
/// every line, and a regular expression.
fn find(c: &mut Criterion) {
    let text = TextBuffer::from_text(&source().repeat(64));
    let mut group = c.benchmark_group("find");
    group.throughput(criterion::Throughput::Bytes(text.len() as u64));
    let cases = [
        ("rare_word", "zzzznotpresent", false),
        ("common_word", "let", false),
        ("regex", r"\blet\s+\w+\s*=", true),
    ];
    for (name, query, regex) in cases {
        let options = search::Options {
            regex,
            ..search::Options::default()
        };
        let matcher = search::compile(query, options).unwrap();
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut count = 0usize;
                search::scan(
                    &text,
                    &matcher,
                    &std::sync::atomic::AtomicBool::new(false),
                    |_, _, _| {
                        count += 1;
                        true
                    },
                )
                .unwrap();
                black_box(count)
            })
        });
    }
    group.finish();
}

fn scroll(lines: f64) -> InputEvent {
    InputEvent::Scroll(ScrollEvent {
        delta: MouseScrollDelta::LineDelta(0.0, lines as f32),
        cells: [0.0, lines],
        phase: TouchPhase::Moved,
        position: Some(PointerPosition {
            physical: [880.0, 300.0],
            grid: [80.0, 12.0],
            cell: Some([80, 12]),
        }),
        modifiers: Modifiers::default(),
    })
}

fn typed(text: &str) -> InputEvent {
    InputEvent::Text(TextInput {
        text: text.into(),
        // Keyboard text never contains line breaks; an IME commit may.
        source: TextSource::Ime,
        modifiers: Modifiers::default(),
    })
}

/// An editor showing a highlighted source file, with all background work finished.
fn opened(frame: &mut Framebuffer, workspace: &tempfile::TempDir) -> Editor {
    let path = workspace.path().join("source.rs");
    std::fs::write(&path, source()).unwrap();
    let mut editor = Editor::new(workspace.path()).unwrap();
    editor.open(&path).unwrap();
    settle(&mut editor, frame);
    editor
}

fn settle(editor: &mut Editor, frame: &mut Framebuffer) {
    editor.compose(frame);
    while editor.has_pending_background_work() {
        std::thread::sleep(std::time::Duration::from_micros(200));
        editor.compose(frame);
    }
}

fn compose(c: &mut Criterion) {
    let workspace = tempfile::tempdir().unwrap();
    let mut frame = Framebuffer::new([160, 50], Theme::default().panel).unwrap();
    let mut editor = opened(&mut frame, &workspace);
    c.bench_function("compose/idle_frame", |b| {
        b.iter(|| editor.compose(&mut frame))
    });
    c.bench_function("compose/scroll_four_events", |b| {
        let mut direction = -3.0;
        let mut step = 0;
        b.iter(|| {
            step += 1;
            if step % 200 == 0 {
                direction = -direction;
            }
            for _ in 0..4 {
                editor.handle(scroll(direction));
            }
            editor.compose(&mut frame);
        })
    });
    // Varied text with line breaks, like real typing. Repeating one character would
    // instead measure comparing an ever longer run of identical bytes.
    c.bench_function("compose/typed_character", |b| {
        let mut count = 0usize;
        b.iter(|| {
            count += 1;
            let letter = (b'a' + (count % 26) as u8) as char;
            let text = if count.is_multiple_of(64) {
                "\n".to_owned()
            } else {
                letter.to_string()
            };
            editor.handle(typed(&text));
            editor.compose(&mut frame);
        })
    });
}

criterion_group!(benches, buffer, document, find, compose);
criterion_main!(benches);
