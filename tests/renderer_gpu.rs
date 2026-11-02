use std::{fs::File, io::BufWriter, time::Duration};
use teditor::renderer::{CellRect, Color, DEFAULT_FONT, Frame, RenderError, Renderer};

#[path = "support/renderer_demo.rs"]
mod demo;

#[path = "support/terminal_demo.rs"]
mod terminal_demo;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

#[test]
#[ignore = "requires a GPU"]
fn editor_renders_tree_documents_tabs_palette_and_incremental_edits() {
    use teditor::{
        editor::Editor,
        terminal::{InputEvent, Terminal, TerminalConfig, input::*},
    };
    let (device, queue) = gpu();
    let viewport = [1100, 780];
    let mut terminal = Terminal::new(
        &device,
        &queue,
        FORMAT,
        TerminalConfig {
            viewport,
            ..Default::default()
        },
    )
    .unwrap();
    let fixture = std::env::temp_dir().join(format!("teditor-gpu-editor-{}", std::process::id()));
    let root = fixture.join("workspace");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let source = "// A cell-based editor, with room to think.\n\nuse teditor::editor::Editor;\nuse std::path::Path;\n\nfn main() {\n    let workspace = Path::new(\".\");\n    let editor = Editor::new(workspace).unwrap();\n\n    // The file tree and documents share one grid.\n    println!(\"{}\", editor.workspace().display());\n}\n\n// Open files from the tree on the left.\n// Switch documents using the tabs below.\n// Cmd+P opens the command palette.\n";
    std::fs::write(root.join("src/main.rs"), source).unwrap();
    std::fs::write(
        root.join("README.md"),
        "# Workspace\n\nA small editor project.\n",
    )
    .unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"workspace\"\n").unwrap();
    let mut editor = Editor::new(&root).unwrap();
    editor.open(&root.join("README.md")).unwrap();
    editor.open(&root.join("src/main.rs")).unwrap();
    editor.compose(terminal.back_mut());
    check_incremental(&device, &queue, &mut terminal);
    wait_editor_background(&mut editor, &mut terminal);
    let keyword = terminal.back().cell(30, 6).unwrap().style.foreground;
    let comment = terminal.back().cell(31, 1).unwrap().style.foreground;
    assert_ne!(
        keyword, comment,
        "Rust keywords and comments must have distinct colors"
    );
    assert_ne!(keyword, teditor::ui::Theme::default().text);
    check_incremental(&device, &queue, &mut terminal);
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    write_png(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/editor-preview.png"),
        viewport,
        &pixels,
        png::ColorType::Rgba,
    );

    let key = |logical_key: Key, command| {
        InputEvent::Key(KeyEvent {
            physical_key: PhysicalKey::Code(KeyCode::KeyP),
            key_without_modifiers: Some(logical_key.clone()),
            logical_key,
            location: KeyLocation::Standard,
            state: KeyState::Pressed,
            repeat: false,
            synthetic: false,
            modifiers: Modifiers {
                super_key: command,
                ..Default::default()
            },
            composing: false,
        })
    };
    editor.handle(key(Key::Character("p".into()), true));
    editor.handle(InputEvent::Text(TextInput {
        text: "main".into(),
        source: TextSource::Keyboard,
        modifiers: Modifiers::default(),
    }));
    editor.compose(terminal.back_mut());
    assert!(editor.palette_open());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        editor.compose(terminal.back_mut());
        let [columns, rows] = terminal.back().size();
        // The results drawer sits directly above the input row.
        let found = (rows.saturating_sub(11)..rows - 1).any(|y| {
            let line: String = (0..columns)
                .map(|x| terminal.back().cell(x, y).unwrap().character)
                .collect();
            line.contains("src/main.rs")
        });
        if found {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "quick-open index did not produce the file"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        editor.ime_cursor_cells().unwrap().row,
        terminal.back().size()[1] - 1
    );
    let stats = check_incremental(&device, &queue, &mut terminal);
    assert!(stats.dirty_cells < (terminal.back().size()[0] * terminal.back().size()[1]) as usize);
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    write_png(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/editor-palette-preview.png"),
        viewport,
        &pixels,
        png::ColorType::Rgba,
    );
    editor.handle(key(Key::Named(NamedKey::Enter), false));
    editor.compose(terminal.back_mut());
    assert!(!editor.palette_open());
    assert_eq!(editor.tab_count(), 2);
    editor.handle(InputEvent::Text(TextInput {
        text: "// edited\n".into(),
        source: TextSource::Keyboard,
        modifiers: Modifiers::default(),
    }));
    editor.compose(terminal.back_mut());
    assert!(editor.active_document().unwrap().dirty());
    assert_eq!(
        terminal.back().cell(30, 6).unwrap().style.foreground,
        keyword,
        "typing in the first line must not clear subsequent syntax colors"
    );
    check_incremental(&device, &queue, &mut terminal);
    wait_editor_background(&mut editor, &mut terminal);
    check_incremental(&device, &queue, &mut terminal);
    editor.handle(key(Key::Character("z".into()), true));
    editor.compose(terminal.back_mut());
    assert_eq!(editor.active_document().unwrap().text.to_text(), source);
    assert_eq!(
        terminal.back().cell(30, 6).unwrap().style.foreground,
        keyword
    );
    check_incremental(&device, &queue, &mut terminal);
    wait_editor_background(&mut editor, &mut terminal);
    check_incremental(&device, &queue, &mut terminal);
    editor.handle(key(Key::Character("p".into()), true));
    editor.handle(InputEvent::Text(TextInput {
        text: ">reopen with encoding".into(),
        source: TextSource::Keyboard,
        modifiers: Modifiers::default(),
    }));
    editor.handle(key(Key::Named(NamedKey::Enter), false));
    editor.handle(InputEvent::Text(TextInput {
        text: "utf-8".into(),
        source: TextSource::Keyboard,
        modifiers: Modifiers::default(),
    }));
    editor.compose(terminal.back_mut());
    check_incremental(&device, &queue, &mut terminal);
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    write_png(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/editor-encoding-preview.png"),
        viewport,
        &pixels,
        png::ColorType::Rgba,
    );
    editor.handle(key(Key::Named(NamedKey::Enter), false));
    editor.compose(terminal.back_mut());
    check_incremental(&device, &queue, &mut terminal);
    let wait = |editor: &mut Editor, terminal: &mut Terminal| {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while editor.has_pending_background_work() {
            assert!(std::time::Instant::now() < deadline, "editor I/O timed out");
            std::thread::sleep(Duration::from_millis(2));
            editor.compose(terminal.back_mut());
        }
    };
    wait(&mut editor, &mut terminal);
    assert_eq!(editor.active_document().unwrap().text.to_text(), source);
    check_incremental(&device, &queue, &mut terminal);
    let binary = root.join("sample.png");
    let bytes = [0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 255];
    std::fs::write(&binary, bytes).unwrap();
    editor.request_open(&binary).unwrap();
    editor.compose(terminal.back_mut());
    wait(&mut editor, &mut terminal);
    assert_eq!(
        editor.active_document().unwrap().encoding(),
        teditor::editor::encoding::Encoding::Bytes
    );
    check_incremental(&device, &queue, &mut terminal);
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    write_png(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/editor-bytes-preview.png"),
        viewport,
        &pixels,
        png::ColorType::Rgba,
    );
    editor.handle(key(Key::Character("s".into()), true));
    editor.compose(terminal.back_mut());
    wait(&mut editor, &mut terminal);
    assert_eq!(std::fs::read(&binary).unwrap(), bytes);
    let preview = |terminal: &mut Terminal, name: &str| {
        check_incremental(&device, &queue, terminal);
        let (_, pixels) = terminal_frame(&device, &queue, terminal);
        write_png(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(name),
            viewport,
            &pixels,
            png::ColorType::Rgba,
        );
    };
    // The command drawer with a hovered row, then the clickable unsaved prompt.
    let [_, rows] = terminal.back().size();
    editor.handle(key(Key::Character("p".into()), true));
    editor.handle(InputEvent::Pointer(PointerEvent {
        kind: PointerKind::Move,
        position: Some(PointerPosition {
            physical: [0.0, 0.0],
            grid: [20.5, f64::from(rows) - 8.5],
            cell: Some([20, rows - 9]),
        }),
        modifiers: Modifiers::default(),
    }));
    editor.compose(terminal.back_mut());
    assert!(editor.palette_open());
    preview(&mut terminal, "editor-commands-preview.png");
    editor.handle(key(Key::Named(NamedKey::Escape), false));
    editor.handle(InputEvent::Text(TextInput {
        text: "x".into(),
        source: TextSource::Keyboard,
        modifiers: Modifiers::default(),
    }));
    editor.handle(key(Key::Character("w".into()), true));
    editor.compose(terminal.back_mut());
    assert!(editor.palette_open());
    preview(&mut terminal, "editor-prompt-preview.png");
    editor.handle(key(Key::Named(NamedKey::Escape), false));
    editor.handle(key(Key::Character("z".into()), true));
    editor.compose(terminal.back_mut());
    assert!(!editor.palette_open());
    // The editable title bar, listing a folder's contents and naming a new file.
    editor.handle(key(Key::Character("l".into()), true));
    editor.handle(InputEvent::Text(TextInput {
        text: "src/ma".into(),
        source: TextSource::Keyboard,
        modifiers: Modifiers::default(),
    }));
    editor.compose(terminal.back_mut());
    assert!(editor.palette_open());
    preview(&mut terminal, "editor-path-preview.png");
    editor.handle(key(Key::Named(NamedKey::Escape), false));
    editor.compose(terminal.back_mut());
    assert!(!editor.palette_open());
    // The embedded terminal, running a real shell with colored output.
    editor.handle(key(Key::Character("j".into()), true));
    editor.compose(terminal.back_mut());
    assert!(editor.terminal_open());
    editor.handle(InputEvent::Text(TextInput {
        text: "printf '\\033[1;32mgreen\\033[0m \\033[4;35munderlined\\033[0m \\033[41;97m on red \\033[0m \\xf0\\x9f\\x8e\\x89 party\\n'; ls\n".into(),
        source: TextSource::Keyboard,
        modifiers: Modifiers::default(),
    }));
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        editor.compose(terminal.back_mut());
        let [columns, rows] = terminal.back().size();
        let found = (0..rows).any(|y| {
            let line: String = (0..columns)
                .map(|x| terminal.back().cell(x, y).unwrap().character)
                .collect();
            // Only the rendered output has these words adjacent; the echoed command
            // has escape sequences between them.
            line.contains("green underlined")
        });
        if found {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the shell produced no output"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    preview(&mut terminal, "editor-terminal-preview.png");
    // The find bar with its replace row, under the document.
    let mut replace = key(Key::Character("f".into()), true);
    if let InputEvent::Key(key) = &mut replace {
        key.modifiers.alt = true;
    }
    editor.handle(replace);
    editor.handle(InputEvent::Text(TextInput {
        text: "PNG".into(),
        source: TextSource::Keyboard,
        modifiers: Modifiers::default(),
    }));
    editor.compose(terminal.back_mut());
    wait_editor_background(&mut editor, &mut terminal);
    editor.compose(terminal.back_mut());
    let size = terminal.back().size();
    let screen: String = (0..size[1])
        .map(|y| {
            let cell = |x| {
                terminal
                    .back()
                    .cell(x, y)
                    .map_or(' ', |cell| cell.character)
            };
            (0..size[0]).map(cell).collect::<String>() + "\n"
        })
        .collect();
    assert!(screen.contains("1 of 1"), "the find bar's match:\n{screen}");
    assert!(screen.contains('⇉'), "the replace row:\n{screen}");
    preview(&mut terminal, "editor-find-preview.png");
    editor.handle(key(Key::Named(NamedKey::Escape), false));
    editor.compose(terminal.back_mut());
    editor.handle(key(Key::Character("j".into()), true));
    editor.compose(terminal.back_mut());
    assert!(!editor.terminal_open());
    for viewport in [[550, 260], [1100, 780]] {
        terminal.resize(viewport).unwrap();
        editor.compose(terminal.back_mut());
        check_incremental(&device, &queue, &mut terminal);
    }
    terminal.set_scale_factor(1.5).unwrap();
    editor.compose(terminal.back_mut());
    check_incremental(&device, &queue, &mut terminal);
    editor.compose(terminal.back_mut());
    assert_eq!(
        terminal_frame(&device, &queue, &mut terminal).0.dirty_cells,
        0
    );
    std::fs::remove_dir_all(fixture).unwrap();
}

#[test]
#[ignore = "requires a GPU"]
fn emoji_draw_in_color_across_two_cells_and_repaint_as_one_piece() {
    use teditor::{
        renderer::Color,
        terminal::{Cell, Style, Terminal, TerminalConfig},
    };
    let (device, queue) = gpu();
    let (foreground, background) = (Color::rgb(220, 229, 238), Color::rgb(17, 23, 33));
    let style = Style {
        foreground,
        background,
    };
    let viewport = [1000, 300];
    for scale in [1.0, 2.0] {
        let mut terminal = Terminal::new(
            &device,
            &queue,
            FORMAT,
            TerminalConfig {
                viewport,
                scale_factor: scale,
                ..Default::default()
            },
        )
        .unwrap();
        let metrics = terminal.cell_metrics();
        let (cw, ch) = (metrics.width, metrics.height);
        terminal.back_mut().clear(Cell::new(' ', style));
        terminal.back_mut().write(0, 0, "a\u{1F389}b", style);
        let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
        let color_pixels = |pixels: &[u8], column: u32, columns: u32| {
            let mut colored = 0;
            for y in 0..ch {
                for x in column * cw..(column + columns) * cw {
                    let p = &pixels[((y * viewport[0] + x) * 4) as usize..];
                    // Text and backdrop here are nearly neutral; emoji are saturated.
                    let (max, min) = (p[0].max(p[1]).max(p[2]), p[0].min(p[1]).min(p[2]));
                    if max - min > 60 {
                        colored += 1;
                    }
                }
            }
            colored
        };
        let area = cw * ch;
        assert!(
            color_pixels(&pixels, 1, 1) > area / 8,
            "scale {scale}: left half"
        );
        assert!(
            color_pixels(&pixels, 2, 1) > area / 8,
            "scale {scale}: right half"
        );
        assert_eq!(color_pixels(&pixels, 0, 1), 0, "a is untouched");
        assert_eq!(color_pixels(&pixels, 3, 1), 0, "b follows after both cells");
        // The two-cell slot keeps the following text in its own column.
        assert_eq!(terminal.back().cell(3, 0).unwrap().character, 'b');
        check_incremental(&device, &queue, &mut terminal);
        // Changing only the head must repaint the whole glyph, not half of it.
        terminal.back_mut().write(1, 0, "\u{1F680}", style);
        check_incremental(&device, &queue, &mut terminal);
        // Replacing it with narrow text must erase both halves.
        terminal.back_mut().write(1, 0, "cd", style);
        check_incremental(&device, &queue, &mut terminal);
        let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
        assert_eq!(color_pixels(&pixels, 1, 2), 0, "no emoji remains");
        // Colors are the image's own, unaffected by the cell's foreground.
        let tinted = Style {
            foreground: Color::rgb(0, 0, 0),
            ..style
        };
        terminal.back_mut().write(5, 0, "\u{1F389}", tinted);
        let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
        assert!(color_pixels(&pixels, 5, 2) > area / 4);
        // Scale keeps the emoji sharp and proportioned: previews show it directly.
        if scale == 2.0 {
            terminal.back_mut().write(
                0,
                2,
                "\u{1F389}\u{1F600}\u{1F680}\u{1F44D}\u{2764} ok",
                style,
            );
            let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
            write_png(
                &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/emoji-preview.png"),
                viewport,
                &pixels,
                png::ColorType::Rgba,
            );
        }
    }
}

#[test]
#[ignore = "requires a GPU"]
fn unicode_text_draws_with_fallback_fonts_and_shaped_clusters() {
    use teditor::{
        renderer::Color,
        terminal::{Cell, Style, Terminal, TerminalConfig},
    };
    let (device, queue) = gpu();
    let style = Style {
        foreground: Color::rgb(220, 229, 238),
        background: Color::rgb(17, 23, 33),
    };
    let viewport = [1100, 560];
    let mut terminal = Terminal::new(
        &device,
        &queue,
        FORMAT,
        TerminalConfig {
            viewport,
            ..Default::default()
        },
    )
    .unwrap();
    let metrics = terminal.cell_metrics();
    let (cw, ch) = (metrics.width, metrics.height);
    terminal.back_mut().clear(Cell::new(' ', style));
    for (row, line) in [
        "Latin      cafe\u{301} naïve Ω Я",
        "CJK        漢字 かな カナ 한글",
        "Hebrew     שלום",
        "Arabic     سلام",
        "Devanagari नमस्ते क्ष",
        "Thai       สวัสดี",
        "Emoji      \u{1F600} \u{1F44D}\u{1F3FD} \u{1F469}\u{200D}\u{1F4BB} \u{1F1E9}\u{1F1EA} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        "Symbols    ✓ ⌘ ∀ ∑ ← → ⠿ ☃",
        "Icons       \u{f002} \u{f015} \u{f07b} \u{e725} \u{e0b0}\u{e0b2} \u{e0b6}\u{e0b4} \u{eab1}",
    ]
    .into_iter()
    .enumerate()
    {
        terminal.back_mut().write(0, row as u32 * 2, line, style);
    }
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/unicode-preview.png");
    write_png(&path, viewport, &pixels, png::ColorType::Rgba);
    check_incremental(&device, &queue, &mut terminal);
    let inked = |column: u32, row: u32, columns: u32, pixels: &[u8]| {
        let mut count = 0;
        for y in row * ch..(row + 1) * ch {
            for x in column * cw..(column + columns) * cw {
                let p = &pixels[((y * viewport[0] + x) * 4) as usize..];
                if p[0]
                    .abs_diff(17)
                    .max(p[1].abs_diff(23))
                    .max(p[2].abs_diff(33))
                    > 40
                {
                    count += 1;
                }
            }
        }
        count
    };
    // Bundled icons draw, whatever is installed.
    assert!(inked(12, 16, 1, &pixels) > 10, "an icon");
    assert!(inked(20, 16, 1, &pixels) > 10, "a powerline symbol");
    // A compound emoji is one picture in two cells, with nothing after it.
    let cells: Vec<_> = (0..terminal.back().size()[0])
        .map(|x| terminal.back().cell(x, 12).unwrap().character)
        .collect();
    let joined = cells
        .iter()
        .position(|c| teditor::renderer::is_symbol(*c))
        .unwrap();
    assert_eq!(cells[joined + 1], Cell::CONTINUATION);
    assert!(inked(joined as u32, 12, 2, &pixels) > cw * ch / 4);
}

#[test]
#[ignore = "requires a GPU"]
fn emoji_atlas_evicts_between_frames_instead_of_running_out() {
    use teditor::{
        renderer::Color,
        terminal::{Cell, Style, Terminal, TerminalConfig},
    };
    let (device, queue) = gpu();
    let style = Style {
        foreground: Color::rgb(220, 229, 238),
        background: Color::rgb(17, 23, 33),
    };
    // A huge font makes each image large, so few of them fill the atlas.
    let viewport = [1000, 500];
    let mut terminal = Terminal::new(
        &device,
        &queue,
        FORMAT,
        TerminalConfig {
            viewport,
            font_height: 200.0,
            ..Default::default()
        },
    )
    .unwrap();
    let metrics = terminal.cell_metrics();
    let last = 0x1F600 + 150;
    for code in 0x1F600..=last {
        terminal.back_mut().clear(Cell::new(' ', style));
        terminal
            .back_mut()
            .write(0, 0, &char::from_u32(code).unwrap().to_string(), style);
        terminal_frame(&device, &queue, &mut terminal);
    }
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    let drawn = (0..metrics.height).any(|y| {
        (0..2 * metrics.width).any(|x| {
            let p = &pixels[((y * viewport[0] + x) * 4) as usize..];
            p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]) > 60
        })
    });
    assert!(drawn, "the final emoji still renders after eviction");
}

#[test]
#[ignore = "requires a GPU"]
fn block_and_box_characters_fill_their_cells_at_every_scale() {
    use teditor::{
        renderer::Color,
        terminal::{Cell, Style, Terminal, TerminalConfig},
    };
    let (device, queue) = gpu();
    let (foreground, background) = (Color::rgb(220, 120, 90), Color::rgb(10, 15, 25));
    let style = Style {
        foreground,
        background,
    };
    for scale in [1.0, 1.25, 1.5, 2.0, 3.0] {
        let viewport = [1000, 700];
        let mut terminal = Terminal::new(
            &device,
            &queue,
            FORMAT,
            TerminalConfig {
                viewport,
                scale_factor: scale,
                ..Default::default()
            },
        )
        .unwrap();
        let back = terminal.back_mut();
        back.clear(Cell::new(' ', style));
        // Solid blocks over two rows and columns, then a tree guide down one column.
        back.write(0, 0, "███", style);
        back.write(0, 1, "███", style);
        back.write(0, 3, "▛▜", style);
        for y in 5..8 {
            back.write(1, y, "│", style);
        }
        let metrics = terminal.cell_metrics();
        let (cw, ch) = (metrics.width, metrics.height);
        let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
        let pixel = |x: u32, y: u32| {
            let at = ((y * viewport[0] + x) * 4) as usize;
            [pixels[at], pixels[at + 1], pixels[at + 2]]
        };
        let solid = pixel(0, 0);
        assert_ne!(
            solid,
            pixel(4 * cw, 0),
            "the block must differ from empty cells"
        );
        for y in 0..2 * ch {
            for x in 0..3 * cw {
                assert_eq!(pixel(x, y), solid, "gap in █ at {x},{y}, scale {scale}");
            }
        }
        // The two top halves of ▛▜ join across the cell boundary.
        assert_eq!(pixel(cw - 1, 3 * ch), solid, "scale {scale}");
        assert_eq!(pixel(cw, 3 * ch), solid, "scale {scale}");
        // A vertical guide is unbroken across rows.
        let center = cw + (cw - ((cw + 3) / 6).max(1)) / 2;
        for y in 5 * ch..8 * ch {
            assert_eq!(pixel(center, y), solid, "gap in │ at y={y}, scale {scale}");
        }
        if scale == 2.0 {
            write_png(
                &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/blocks-preview.png"),
                viewport,
                &pixels,
                png::ColorType::Rgba,
            );
        }
    }
}

#[test]
#[ignore = "requires a GPU"]
fn editor_scrolls_source_file_with_batched_trackpad_input() {
    use std::time::Instant;
    use teditor::{
        editor::Editor,
        terminal::{Terminal, TerminalConfig},
    };
    use winit::{
        dpi::PhysicalPosition,
        event::{DeviceId, MouseScrollDelta, TouchPhase, WindowEvent},
    };
    let (device, queue) = gpu();
    let workspace = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/renderer_gpu.rs");
    let mut editor = Editor::new(workspace.path()).unwrap();
    editor.open(&source).unwrap();
    let viewport = [1760, 1300];
    let mut terminal = Terminal::new(
        &device,
        &queue,
        FORMAT,
        TerminalConfig {
            viewport,
            ..Default::default()
        },
    )
    .unwrap();
    editor.compose(terminal.back_mut());
    let metrics = terminal.cell_metrics();
    for event in terminal.handle_window_event(
        &WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(
                80.0 * f64::from(metrics.width),
                10.0 * f64::from(metrics.height),
            ),
        },
        Instant::now(),
    ) {
        editor.handle(event);
    }
    editor.compose(terminal.back_mut());
    check_incremental(&device, &queue, &mut terminal);
    let texture = target(&device, viewport);
    let view = texture.create_view(&Default::default());
    let maximum =
        editor.active_document().unwrap().line_count() - (terminal.back().size()[1] as usize - 3);
    let mut expected = 0usize;
    let mut samples = Vec::new();
    // Cover both mapped pages, the EOF clamp, and reverse scrolling. Several
    // wheel messages can arrive between native redraws on a trackpad.
    for step in 0..120 {
        let direction = if step < 60 { 7.0 } else { -7.0 };
        let now = Instant::now();
        for _ in 0..4 {
            let wheel = WindowEvent::MouseWheel {
                device_id: DeviceId::dummy(),
                delta: MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                    0.0,
                    -direction * f64::from(metrics.height),
                )),
                phase: TouchPhase::Moved,
            };
            for event in terminal.handle_window_event(&wheel, Instant::now()) {
                editor.handle(event);
            }
            expected = expected
                .saturating_add_signed(direction as isize)
                .min(maximum);
        }
        editor.compose(terminal.back_mut());
        terminal.render(&view).unwrap();
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(10)),
            })
            .unwrap();
        samples.push(now.elapsed());
        assert_eq!(editor.active_document().unwrap().scroll[1], expected);
        assert_eq!(editor.active_document().unwrap().cursor, 0);
        if matches!(step, 0 | 35 | 59 | 60 | 119) {
            wait_editor_background(&mut editor, &mut terminal);
            check_incremental(&device, &queue, &mut terminal);
        }
    }
    assert_eq!(expected, 0);
    samples.sort();
    eprintln!(
        "source scrolling, 4 wheel events + compose + GPU completion: median {:?}, p95 {:?}, max {:?}",
        samples[60], samples[114], samples[119]
    );
}

#[test]
#[ignore = "requires a GPU"]
fn scrollbar_keeps_its_geometry_when_clipped_by_its_canvas() {
    use teditor::{
        renderer::{Color, Graphic},
        terminal::{Terminal, TerminalConfig},
        ui::Context,
    };
    let (device, queue) = gpu();
    let mut terminal = Terminal::new(
        &device,
        &queue,
        FORMAT,
        TerminalConfig {
            viewport: [440, 520],
            ..Default::default()
        },
    )
    .unwrap();
    let mut ctx = Context::new();
    let columns = terminal.back().size()[0];
    let bar = Graphic::Scrollbar {
        offset: 10,
        visible: 8,
        total: 100,
        thumb: Color::rgb(200, 200, 200),
        track: Color::rgb(40, 40, 40),
    };
    let draw = |ctx: &mut Context, terminal: &mut Terminal, height| {
        ctx.run(terminal.back_mut(), &[], |ui| {
            let mut canvas = ui.canvas("canvas", height, false);
            canvas.graphic(CellRect::new(columns - 1, 0, 1, 8), bar);
        });
    };
    draw(&mut ctx, &mut terminal, 8);
    let (_, original) = terminal_frame(&device, &queue, &mut terminal);
    draw(&mut ctx, &mut terminal, 4);
    check_incremental(&device, &queue, &mut terminal);
    let (_, clipped) = terminal_frame(&device, &queue, &mut terminal);
    let metrics = terminal.cell_metrics();
    for y in 0..4 * metrics.height {
        for x in (columns - 1) * metrics.width..columns * metrics.width {
            let pixel = ((y * 440 + x) * 4) as usize;
            for channel in 0..4 {
                assert!(
                    clipped[pixel + channel].abs_diff(original[pixel + channel]) <= 1,
                    "a clipped scrollbar should be cropped without rescaling"
                );
            }
        }
    }
}

#[test]
#[ignore = "requires a GPU"]
fn tiled_panes_clip_images_and_repaint_shared_edges_after_resize() {
    use teditor::{
        terminal::{Terminal, TerminalConfig},
        ui::{Context, Split},
    };
    use winit::{
        dpi::PhysicalPosition,
        event::{DeviceId, ElementState, MouseButton, WindowEvent},
    };
    let (device, queue) = gpu();
    let mut terminal = Terminal::new(
        &device,
        &queue,
        FORMAT,
        TerminalConfig {
            viewport: [680, 520],
            ..Default::default()
        },
    )
    .unwrap();
    let image = terminal.upload_bitmap(1, 1, &[30, 160, 120, 255]).unwrap();
    let mut ctx = Context::new();
    let build = |ui: &mut teditor::ui::Ui<'_>| {
        ui.split(
            "layout",
            Split::columns(0.4),
            |left| {
                left.pane("image", "Image", |ui| {
                    let height = ui.available_height();
                    let mut canvas = ui.canvas("asset", height, false);
                    let [width, _] = canvas.size();
                    canvas.image(0, 0, [width - 1, 16], image);
                });
            },
            |right| {
                right.pane("tools", "Tools", |ui| {
                    ui.canvas("action", 1, false);
                });
            },
        )
    };
    ctx.run(terminal.back_mut(), &[], build);
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    let metrics = terminal.cell_metrics();
    let (cw, ch) = (metrics.width, metrics.height);
    let [columns, rows] = terminal.back().size();
    let cut = ctx.pane_rect("image").unwrap().columns;
    assert_pixel(
        &pixels,
        terminal.viewport(),
        [0, 0],
        [0x1b, 0x1e, 0x24, 255],
    );
    assert_pixel(
        &pixels,
        terminal.viewport(),
        [679, 519],
        [0x1b, 0x1e, 0x24, 255],
    );
    assert_pixel(
        &pixels,
        terminal.viewport(),
        [2 * cw, 3 * ch],
        [30, 160, 120, 255],
    );
    assert_pixel(
        &pixels,
        terminal.viewport(),
        [(cut + 2) * cw, 3 * ch],
        [0x1b, 0x1e, 0x24, 255],
    );
    assert_pixel(
        &pixels,
        terminal.viewport(),
        [cut * cw - 1, 5 * ch],
        [0x4f, 0x51, 0x57, 255],
    );
    assert_pixel(
        &pixels,
        terminal.viewport(),
        [cut * cw - 1, 519],
        [0x4f, 0x51, 0x57, 255],
    );
    assert_eq!(
        ctx.pane_rect("tools").unwrap(),
        CellRect::new(cut, 0, columns - cut, rows)
    );
    let motion = |x: f64| WindowEvent::CursorMoved {
        device_id: DeviceId::dummy(),
        position: PhysicalPosition::new(x * f64::from(cw), 5.0 * f64::from(ch)),
    };
    let events =
        terminal.handle_window_event(&motion(f64::from(cut) - 0.5), std::time::Instant::now());
    ctx.run(terminal.back_mut(), &events, build);
    assert_eq!(
        check_incremental(&device, &queue, &mut terminal).dirty_cells,
        rows as usize
    );
    let mut events = Vec::new();
    for event in [
        WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
        },
        motion(f64::from(cut) + 7.5),
        WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Released,
            button: MouseButton::Left,
        },
    ] {
        events.extend(terminal.handle_window_event(&event, std::time::Instant::now()));
    }
    ctx.run(terminal.back_mut(), &events, build);
    assert_eq!(ctx.pane_rect("image").unwrap().columns, cut + 8);
    check_incremental(&device, &queue, &mut terminal);
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    assert_pixel(
        &pixels,
        terminal.viewport(),
        [cut * cw - 1, 5 * ch],
        [30, 160, 120, 255],
    );
    assert_pixel(
        &pixels,
        terminal.viewport(),
        [cut * cw - 1, 519],
        [0x1b, 0x1e, 0x24, 255],
    );
    terminal.set_scale_factor(2.0).unwrap();
    ctx.run(terminal.back_mut(), &[], build);
    assert!(check_incremental(&device, &queue, &mut terminal).full_repaint);
    terminal.resize([900, 720]).unwrap();
    ctx.run(terminal.back_mut(), &[], build);
    assert!(check_incremental(&device, &queue, &mut terminal).full_repaint);
    let a = ctx.pane_rect("image").unwrap();
    let b = ctx.pane_rect("tools").unwrap();
    assert_eq!(a.columns, b.column);
    assert_eq!(a.columns + b.columns, terminal.back().size()[0]);
    assert_eq!(
        terminal_frame(&device, &queue, &mut terminal).0.dirty_cells,
        0
    );
}

#[test]
#[ignore = "requires a GPU"]
fn ui_preserves_bitmap_source_coordinates_when_scrolled() {
    use teditor::{
        terminal::{Terminal, TerminalConfig},
        ui::Context,
    };
    use winit::{
        dpi::PhysicalPosition,
        event::{DeviceId, WindowEvent},
    };
    let (device, queue) = gpu();
    let mut terminal = Terminal::new(
        &device,
        &queue,
        FORMAT,
        TerminalConfig {
            viewport: [700, 600],
            ..Default::default()
        },
    )
    .unwrap();
    let metrics = terminal.cell_metrics();
    let (cw, ch) = (metrics.width, metrics.height);
    let bitmap = terminal
        .upload_bitmap(
            1,
            8,
            &[[230, 20, 40, 255]; 4]
                .into_iter()
                .chain([[20, 50, 230, 255]; 4])
                .flatten()
                .collect::<Vec<_>>(),
        )
        .unwrap();
    let mut context = Context::new();
    let build = |ui: &mut teditor::ui::Ui<'_>| {
        let mut canvas = ui.canvas("scroll", 4, false);
        canvas.image(0, -4, [20, 8], bitmap);
    };
    context.run(terminal.back_mut(), &[], build);
    check_incremental(&device, &queue, &mut terminal);
    terminal.handle_window_event(
        &WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: PhysicalPosition::new(f64::from(cw), f64::from(ch)),
        },
        std::time::Instant::now(),
    );
    context.run(terminal.back_mut(), &[], build);
    check_incremental(&device, &queue, &mut terminal);
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    assert_pixel(
        &pixels,
        terminal.viewport(),
        [2 * cw + cw / 2, 2 * ch + ch / 2],
        [20, 50, 230, 255],
    );
    assert_pixel(
        &pixels,
        terminal.viewport(),
        [2 * cw + cw / 2, 5 * ch + ch / 2],
        [0x1b, 0x1e, 0x24, 255],
    );
    assert_eq!(
        terminal_frame(&device, &queue, &mut terminal).0.dirty_cells,
        0
    );
}

#[test]
#[ignore = "requires a GPU; run cargo test --test renderer_gpu -- --ignored"]
fn renders_clipped_layers_glyphs_bitmaps_and_dpi_changes() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&Default::default()))
        .expect("a GPU adapter is required for the renderer integration test");
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).unwrap();
    let mut renderer = Renderer::new(&device, &queue, FORMAT, DEFAULT_FONT, 22.0).unwrap();
    let bitmap = renderer.upload_bitmap(1, 1, &[200, 100, 50, 255]).unwrap();
    // Adjacent atlas entries must not bleed through the filtering gutters.
    let _neighbor = renderer.upload_bitmap(1, 1, &[0, 0, 255, 255]).unwrap();
    assert!(matches!(
        renderer.upload_bitmap(2, 2, &[0; 3]),
        Err(RenderError::InvalidImageData)
    ));

    let metrics = renderer.cell_metrics();
    let (cw, ch) = (metrics.width, metrics.height);
    let size = [cw * 10 + 3, ch * 6 + 5];
    let texture = target(&device, size);
    let view = texture.create_view(&Default::default());
    let clear = Color::rgb(12, 23, 34);
    let mut frame = Frame::new(clear);
    frame.fill(CellRect::new(0, 0, 2, 2), Color::rgb(255, 0, 0));
    frame.fill(CellRect::new(1, 0, 1, 1), Color(0, 0, 255, 128));
    frame.set_clip(Some(CellRect::new(2, 1, 2, 2)));
    frame.fill(CellRect::new(0, 0, 8, 5), Color::rgb(0, 255, 0));
    frame.set_clip(None);
    frame.bitmap(CellRect::new(5, 1, 2, 2), bitmap);
    frame.text(0, 4, "Ag", Color::rgb(255, 255, 255));
    renderer.render(&view, size, &frame).unwrap();
    let pixels = readback(&device, &queue, &texture, size);
    assert_pixel(&pixels, size, [0, 0], [255, 0, 0, 255]);
    assert_pixel(&pixels, size, [cw, 0], [187, 0, 188, 255]);
    assert_pixel(&pixels, size, [cw * 2, ch], [0, 255, 0, 255]);
    assert_pixel(&pixels, size, [cw * 4, ch], [12, 23, 34, 255]);
    assert_pixel(&pixels, size, [cw * 2, 0], [12, 23, 34, 255]);
    assert_pixel(&pixels, size, [cw * 5, ch], [200, 100, 50, 255]);
    assert_pixel(&pixels, size, [cw * 7 - 1, ch * 3 - 1], [200, 100, 50, 255]);
    assert_pixel(&pixels, size, [cw * 10, 0], [12, 23, 34, 255]);
    assert_pixel(&pixels, size, [0, ch * 6], [12, 23, 34, 255]);
    assert_glyph_ink(&pixels, size, [0, ch * 4, cw * 2, ch]);

    // Transparent texels must not tint the filtered edge of an opaque neighbor.
    let transparent = renderer
        .upload_bitmap(2, 1, &[255, 0, 0, 255, 0, 0, 255, 0])
        .unwrap();
    let mut alpha_frame = Frame::new(Color(255, 255, 255, 0));
    alpha_frame.bitmap(CellRect::new(0, 0, 4, 1), transparent);
    alpha_frame.glyph(5, 1, 'W', 2, Color::rgb(255, 255, 255));
    renderer.render(&view, size, &alpha_frame).unwrap();
    let pixels = readback(&device, &queue, &texture, size);
    let midpoint = ((cw * 2) * 4) as usize;
    assert!(pixels[midpoint] > 100 && pixels[midpoint + 3] > 50);
    assert_eq!(
        pixels[midpoint + 2],
        0,
        "transparent blue must not bleed into red"
    );
    assert_pixel(&pixels, size, [cw * 4, 0], [0, 0, 0, 0]);
    // A glyph spanning two cells is centered across the allocation, not the first cell.
    assert_glyph_ink(&pixels, size, [cw * 5, ch, cw * 2, ch]);
    let ink_in_second_cell = (ch..ch * 2)
        .flat_map(|y| (cw * 6..cw * 7).map(move |x| (x, y)))
        .filter(|&(x, y)| pixels[((y * size[0] + x) * 4) as usize] > 100)
        .count();
    assert!(ink_in_second_cell > 10);

    // Reusing a bitmap after a scale change and updating it must preserve its ID.
    renderer.set_font_height(44.0).unwrap();
    renderer
        .update_bitmap(bitmap, &[120, 210, 80, 255])
        .unwrap();
    let mut scaled = Frame::new(clear);
    scaled.bitmap(CellRect::new(0, 0, 1, 1), bitmap);
    scaled.text(1, 0, "Ag", Color::rgb(255, 255, 255));
    renderer.render(&view, size, &scaled).unwrap();
    let pixels = readback(&device, &queue, &texture, size);
    assert_pixel(&pixels, size, [0, 0], [120, 210, 80, 255]);
    let large = renderer.cell_metrics();
    assert_glyph_ink(
        &pixels,
        size,
        [large.width, 0, large.width * 2, large.height],
    );

    // Resetting and repacking a previously used glyph atlas must clear its gutters.
    renderer.set_font_height(22.0).unwrap();
    renderer.render(&view, size, &frame).unwrap();
    assert_glyph_ink(
        &readback(&device, &queue, &texture, size),
        size,
        [0, ch * 4, cw * 2, ch],
    );

    // Empty frames still clear the complete target, including the grid's padding.
    renderer.render(&view, size, &Frame::new(clear)).unwrap();
    let pixels = readback(&device, &queue, &texture, size);
    for pixel in pixels.as_chunks::<4>().0 {
        for (actual, expected) in pixel.iter().zip([12, 23, 34, 255]) {
            assert!(actual.abs_diff(expected) <= 1);
        }
    }

    // Produce a visual artifact using exactly the same scene as the window demo.
    let size = [1060, 700];
    let texture = target(&device, size);
    let bitmap = renderer
        .upload_bitmap(
            demo::BITMAP_WIDTH,
            demo::BITMAP_HEIGHT,
            &demo::bitmap_pixels(),
        )
        .unwrap();
    let frame = demo::frame(renderer.cell_metrics().grid_size(size), bitmap, None);
    renderer
        .render(&texture.create_view(&Default::default()), size, &frame)
        .unwrap();
    let pixels = readback(&device, &queue, &texture, size);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/renderer-preview.png");
    let mut encoder = png::Encoder::new(
        BufWriter::new(File::create(path).unwrap()),
        size[0],
        size[1],
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&pixels)
        .unwrap();
}

fn target(device: &wgpu::Device, [width, height]: [u32; 2]) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("renderer test target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn readback(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    size: [u32; 2],
) -> Vec<u8> {
    let stride = (size[0] * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(stride) * u64::from(size[1]),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: None,
            },
        },
        texture.size(),
    );
    queue.submit([encoder.finish()]);
    let (sender, receiver) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            sender.send(result).unwrap()
        });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(10)),
        })
        .unwrap();
    receiver
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let mapped = buffer.slice(..).get_mapped_range().unwrap();
    let mut pixels = Vec::with_capacity((size[0] * size[1] * 4) as usize);
    for row in mapped.chunks_exact(stride as usize) {
        pixels.extend_from_slice(&row[..size[0] as usize * 4]);
    }
    drop(mapped);
    buffer.unmap();
    pixels
}

fn assert_pixel(pixels: &[u8], size: [u32; 2], position: [u32; 2], expected: [u8; 4]) {
    let offset = ((position[1] * size[0] + position[0]) * 4) as usize;
    let actual = &pixels[offset..offset + 4];
    for (actual, expected) in actual.iter().zip(expected) {
        assert!(
            actual.abs_diff(expected) <= 2,
            "at {position:?}: channel {actual} != {expected}"
        );
    }
}

fn assert_glyph_ink(pixels: &[u8], size: [u32; 2], [x, y, width, height]: [u32; 4]) {
    let mut ink = 0;
    let mut antialiasing = 0;
    for row in y..y + height {
        for column in x..x + width {
            let value = pixels[((row * size[0] + column) * 4) as usize];
            if value > 100 {
                ink += 1;
            }
            if value > 40 && value < 220 {
                antialiasing += 1;
            }
        }
    }
    assert!(ink > 20, "expected visible glyphs");
    assert!(antialiasing > 10, "expected antialiased glyph edges");
}

fn gpu() -> (wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    pollster::block_on(adapter.request_device(&Default::default())).unwrap()
}

fn wait_editor_background(
    editor: &mut teditor::editor::Editor,
    terminal: &mut teditor::terminal::Terminal,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while editor.has_pending_background_work() {
        assert!(
            std::time::Instant::now() < deadline,
            "editor background work timed out"
        );
        std::thread::sleep(Duration::from_millis(1));
        editor.compose(terminal.back_mut());
    }
}

fn terminal_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    terminal: &mut teditor::terminal::Terminal,
) -> (teditor::terminal::FrameStats, Vec<u8>) {
    let size = terminal.viewport();
    let texture = target(device, size);
    let stats = terminal
        .render(&texture.create_view(&Default::default()))
        .unwrap();
    (stats, readback(device, queue, &texture, size))
}

fn check_incremental(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    terminal: &mut teditor::terminal::Terminal,
) -> teditor::terminal::FrameStats {
    let (stats, incremental) = terminal_frame(device, queue, terminal);
    terminal.invalidate();
    let (_, full) = terminal_frame(device, queue, terminal);
    assert_eq!(
        incremental, full,
        "incremental patches must match a complete repaint"
    );
    stats
}

#[test]
#[ignore = "requires a GPU"]
fn terminal_diffs_restore_layers_reuse_assets_and_commit_only_successful_frames() {
    use teditor::terminal::{Cell, Framebuffer, Style, Terminal, TerminalConfig};
    let (device, queue) = gpu();
    let viewport = [163, 143];
    let mut terminal = Terminal::new(
        &device,
        &queue,
        FORMAT,
        TerminalConfig {
            viewport,
            ..Default::default()
        },
    )
    .unwrap();
    let cell = Cell::new(
        'A',
        Style {
            background: Color::rgb(50, 60, 70),
            ..Default::default()
        },
    );
    terminal.back_mut().set(1, 1, cell);
    let (first, pixels) = terminal_frame(&device, &queue, &mut terminal);
    assert!(first.full_repaint);
    assert_eq!(terminal.front(), terminal.back());
    // A different acquired target must receive the full image even with no damage.
    let (unchanged, redisplayed) = terminal_frame(&device, &queue, &mut terminal);
    assert_eq!(unchanged.dirty_cells, 0);
    assert_eq!(pixels, redisplayed);
    terminal
        .back_mut()
        .set(1, 1, Cell::new(' ', Style::default()));
    assert_eq!(
        check_incremental(&device, &queue, &mut terminal).dirty_cells,
        1
    );

    let a = terminal
        .upload_bitmap(2, 1, &[255, 0, 0, 180, 0, 0, 255, 0])
        .unwrap();
    let b = terminal.upload_bitmap(1, 1, &[0, 255, 0, 96]).unwrap();
    terminal
        .back_mut()
        .place_bitmap(a, CellRect::new(2, 1, 4, 2));
    terminal
        .back_mut()
        .place_bitmap(b, CellRect::new(3, 2, 4, 2));
    check_incremental(&device, &queue, &mut terminal);
    terminal.back_mut().set(4, 2, cell);
    assert_eq!(
        check_incremental(&device, &queue, &mut terminal).dirty_cells,
        1
    );
    // Move both overlays and reverse their z order; old bounds must be repaired.
    terminal.back_mut().clear_bitmaps();
    terminal
        .back_mut()
        .place_bitmap(b, CellRect::new(2, 1, 4, 2));
    terminal
        .back_mut()
        .place_bitmap(a, CellRect::new(4, 2, 4, 2));
    check_incremental(&device, &queue, &mut terminal);
    terminal
        .update_bitmap(a, 1, 1, &[80, 90, 230, 128])
        .unwrap();
    assert!(check_incremental(&device, &queue, &mut terminal).dirty_cells > 0);
    terminal.remove_bitmap(a).unwrap();
    check_incremental(&device, &queue, &mut terminal);
    assert_eq!(terminal.bitmap_count(), 1);
    assert!(terminal.bitmap_info(a).is_none());
    // Failed atlas repacking must leave all existing assets and GPU handles intact.
    assert!(
        terminal
            .upload_bitmap(2046, 2046, &vec![0; 2046 * 2046 * 4])
            .is_err()
    );
    assert_eq!(terminal.bitmap_count(), 1);
    terminal.back_mut().set(3, 1, cell);
    check_incremental(&device, &queue, &mut terminal);
    // A removed handle cannot be submitted, and failed rendering does not commit.
    let before = terminal.front().clone();
    terminal
        .back_mut()
        .place_bitmap(a, CellRect::new(0, 0, 1, 1));
    let texture = target(&device, viewport);
    let view = texture.create_view(&Default::default());
    assert!(terminal.render(&view).is_err());
    assert_eq!(&before, terminal.front());
    terminal.back_mut().clear_bitmaps();
    terminal
        .back_mut()
        .place_bitmap(b, CellRect::new(1, 1, 2, 2));
    check_incremental(&device, &queue, &mut terminal);

    terminal.back_mut().set(0, 0, cell);
    terminal.resize([0, 0]).unwrap();
    assert!(terminal.render(&view).unwrap().skipped);
    terminal.resize(viewport).unwrap();
    assert_eq!(*terminal.back().cell(0, 0).unwrap(), cell);
    assert!(check_incremental(&device, &queue, &mut terminal).full_repaint);
    terminal.resize([210, 190]).unwrap();
    assert!(check_incremental(&device, &queue, &mut terminal).full_repaint);
    terminal.set_scale_factor(1.5).unwrap();
    assert!(check_incremental(&device, &queue, &mut terminal).full_repaint);
    // Repeated deletion really reclaims the bounded atlas, not just CPU metadata.
    terminal.remove_bitmap(b).unwrap();
    let large = vec![0; 2046 * 1022 * 4];
    for _ in 0..3 {
        let id = terminal.upload_bitmap(2046, 1022, &large).unwrap();
        terminal.remove_bitmap(id).unwrap();
    }
    assert_eq!(terminal.bitmap_count(), 0);
    let size = terminal.back().size();
    *terminal.back_mut() = Framebuffer::new(size, Color::rgb(0, 0, 0)).unwrap();
    check_incremental(&device, &queue, &mut terminal);
}

#[test]
#[ignore = "requires a GPU"]
fn terminal_demo_selection_scrolling_image_dragging_and_png_drops() {
    use std::time::Instant;
    use teditor::terminal::{Terminal, TerminalConfig};
    use winit::{
        dpi::PhysicalPosition,
        event::{DeviceId, ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent},
    };
    let (device, queue) = gpu();
    let viewport = [1060, 700];
    let mut terminal = Terminal::new(
        &device,
        &queue,
        FORMAT,
        TerminalConfig {
            viewport,
            ..Default::default()
        },
    )
    .unwrap();
    let mut demo = terminal_demo::Demo::new(&mut terminal).unwrap();
    let metrics = terminal.cell_metrics();
    // The demo places its image relative to the right edge; grab it one cell in.
    let image_column = f64::from(terminal.back().size()[0] - 28);
    let motion = |x: f64, y: f64| WindowEvent::CursorMoved {
        device_id: DeviceId::dummy(),
        position: PhysicalPosition::new(
            x * f64::from(metrics.width),
            y * f64::from(metrics.height),
        ),
    };
    let button = |state| WindowEvent::MouseInput {
        device_id: DeviceId::dummy(),
        state,
        button: MouseButton::Left,
    };
    let mut send = |event| {
        for event in terminal.handle_window_event(&event, Instant::now()) {
            demo.handle(event, &mut terminal);
        }
    };
    send(motion(3.2, 8.2));
    send(button(ElementState::Pressed));
    send(motion(10.2, 10.2));
    send(button(ElementState::Released));
    for _ in 0..2 {
        send(WindowEvent::MouseWheel {
            device_id: DeviceId::dummy(),
            delta: MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                0.0,
                -0.6 * f64::from(metrics.height),
            )),
            phase: TouchPhase::Moved,
        });
    }
    send(motion(image_column + 1.2, 10.2));
    send(button(ElementState::Pressed));
    send(motion(60.2, 15.2));
    send(button(ElementState::Released));
    demo.compose(&mut terminal);
    assert_eq!(
        terminal.back().bitmaps().next().unwrap().rect,
        CellRect::new(59, 14, 24, 5)
    );
    assert_eq!(
        (2..5)
            .map(|x| terminal.back().cell(x, 7).unwrap().character)
            .collect::<String>(),
        "002"
    );
    assert_eq!(
        terminal.back().cell(3, 8).unwrap().style.background,
        Color::rgb(45, 76, 87)
    );
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/terminal-preview.png");
    write_png(&path, viewport, &pixels, png::ColorType::Rgba);

    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/terminal-drop-test.png");
    write_png(
        &fixture,
        [2, 1],
        &[230, 20, 50, 20, 210, 80],
        png::ColorType::Rgb,
    );
    let id = terminal.back().bitmaps().next().unwrap().bitmap;
    for event in [
        WindowEvent::HoveredFile(fixture.clone()),
        WindowEvent::DroppedFile(fixture.clone()),
    ] {
        for event in terminal.handle_window_event(&event, Instant::now()) {
            demo.handle(event, &mut terminal);
        }
    }
    assert!(terminal.input().hovered_files().is_empty());
    assert_eq!(terminal.bitmap_info(id).unwrap().width, 2);
    assert_eq!(terminal.bitmap_count(), 1);
    demo.compose(&mut terminal);
    check_incremental(&device, &queue, &mut terminal);
    demo.remove_image(&mut terminal);
    demo.compose(&mut terminal);
    check_incremental(&device, &queue, &mut terminal);
    assert_eq!(terminal.bitmap_count(), 0);
    std::fs::remove_file(fixture).unwrap();
}

fn write_png(path: &std::path::Path, size: [u32; 2], pixels: &[u8], color: png::ColorType) {
    let mut encoder = png::Encoder::new(
        BufWriter::new(File::create(path).unwrap()),
        size[0],
        size[1],
    );
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(pixels)
        .unwrap();
}

#[test]
#[ignore = "requires a GPU"]
fn keyboard_and_ime_drive_the_demo_without_duplicate_text_and_anchor_candidates() {
    use teditor::terminal::{Terminal, TerminalConfig, input::*};
    use winit::{
        event::{Ime, WindowEvent},
        keyboard::ModifiersState,
    };
    let (device, queue) = gpu();
    let viewport = [1060, 700];
    let mut terminal = Terminal::new(
        &device,
        &queue,
        FORMAT,
        TerminalConfig {
            viewport,
            ..Default::default()
        },
    )
    .unwrap();
    let mut demo = terminal_demo::Demo::new(&mut terminal).unwrap();
    assert!(demo.text_input_active());
    let metrics = terminal.cell_metrics();
    let [columns, rows] = terminal.back().size();
    let area = terminal
        .ime_cursor_area(demo.ime_cursor_cells(&terminal))
        .unwrap();
    assert_eq!(
        area.position,
        [9 * metrics.width, (rows - 2) * metrics.height]
    );
    assert_eq!(area.size, [metrics.width, metrics.height]);
    assert_eq!(
        terminal
            .ime_cursor_area(CellRect::new(columns - 1, rows - 1, 5, 5))
            .unwrap()
            .size,
        area.size
    );
    assert!(
        terminal
            .ime_cursor_area(CellRect::new(columns, 0, 1, 1))
            .is_none()
    );

    // Normalized input exercises the same handler used by the native winit adapter.
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::KeyH,
        Key::Character("h".into()),
        Some("hé"),
    );
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::ArrowLeft,
        Key::Named(NamedKey::ArrowLeft),
        None,
    );
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::Digit1,
        Key::Character("!".into()),
        Some("!"),
    );
    assert_eq!(demo_line(&mut terminal, &demo, 3), "h!é");
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::Delete,
        Key::Named(NamedKey::Delete),
        Some("\u{7f}"),
    );
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::Backspace,
        Key::Named(NamedKey::Backspace),
        Some("\u{8}"),
    );
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::Home,
        Key::Named(NamedKey::Home),
        None,
    );
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::KeyO,
        Key::Character("ø".into()),
        Some("ø"),
    );
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::End,
        Key::Named(NamedKey::End),
        None,
    );
    assert_eq!(demo_line(&mut terminal, &demo, 2), "øh");
    assert_eq!(
        terminal.bitmap_count(),
        1,
        "Delete in the input field must not remove the image"
    );
    check_incremental(&device, &queue, &mut terminal);

    demo_window_event(&mut terminal, &mut demo, WindowEvent::Ime(Ime::Enabled));
    demo_window_event(
        &mut terminal,
        &mut demo,
        WindowEvent::Ime(Ime::Preedit("é".into(), Some((2, 2)))),
    );
    assert_eq!(demo_line(&mut terminal, &demo, 3), "øhé");
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::KeyE,
        Key::Character("e".into()),
        Some("e"),
    );
    assert_eq!(
        demo_line(&mut terminal, &demo, 3),
        "øhé",
        "composition keys must not insert extra text"
    );
    demo_window_event(
        &mut terminal,
        &mut demo,
        WindowEvent::Ime(Ime::Preedit(String::new(), None)),
    );
    assert_eq!(
        demo_line(&mut terminal, &demo, 3),
        "øh ",
        "preedit never changed committed text"
    );
    demo_window_event(
        &mut terminal,
        &mut demo,
        WindowEvent::Ime(Ime::Commit("é".into())),
    );
    assert_eq!(demo_line(&mut terminal, &demo, 4), "øhé ");

    let mut repeat = keyboard_input(KeyCode::KeyZ, Key::Character("z".into()), Some("z"));
    for repeating in [false, true] {
        repeat.repeat = repeating;
        for event in terminal.handle_key_input(repeat.clone()) {
            demo.handle(event, &mut terminal);
        }
    }
    repeat.state = ElementState::Released;
    for event in terminal.handle_key_input(repeat) {
        demo.handle(event, &mut terminal);
    }
    assert_eq!(demo_line(&mut terminal, &demo, 5), "øhézz");
    check_incremental(&device, &queue, &mut terminal);

    demo_window_event(
        &mut terminal,
        &mut demo,
        WindowEvent::Ime(Ime::Preedit("ö".into(), Some((0, 2)))),
    );
    demo.compose(&mut terminal);
    let (_, pixels) = terminal_frame(&device, &queue, &mut terminal);
    let preview =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/keyboard-preview.png");
    write_png(&preview, viewport, &pixels, png::ColorType::Rgba);
    terminal.set_scale_factor(1.5).unwrap();
    assert!(terminal.input().is_composing());
    let cells = demo.ime_cursor_cells(&terminal);
    let metrics = terminal.cell_metrics();
    assert_eq!(
        terminal.ime_cursor_area(cells).unwrap().position,
        [cells.column * metrics.width, cells.row * metrics.height]
    );
    demo_window_event(&mut terminal, &mut demo, WindowEvent::Focused(false));
    demo_window_event(
        &mut terminal,
        &mut demo,
        WindowEvent::Ime(Ime::Commit("late".into())),
    );
    assert_eq!(demo_line(&mut terminal, &demo, 6), "øhézz ");
    demo_window_event(&mut terminal, &mut demo, WindowEvent::Focused(true));
    demo_window_event(
        &mut terminal,
        &mut demo,
        WindowEvent::ModifiersChanged(ModifiersState::CONTROL.into()),
    );
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::KeyL,
        Key::Character("l".into()),
        Some("l"),
    );
    demo_window_event(
        &mut terminal,
        &mut demo,
        WindowEvent::ModifiersChanged(ModifiersState::empty().into()),
    );
    assert_eq!(demo.ime_cursor_cells(&terminal).column, 9);
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::KeyA,
        Key::Character("a".into()),
        Some("a"),
    );
    assert_eq!(demo_line(&mut terminal, &demo, 2), "a ");
    // Tab transfers focus; commands travel through terminal key events, not a raw host shortcut.
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::Tab,
        Key::Named(NamedKey::Tab),
        Some("\t"),
    );
    assert!(!demo.text_input_active());
    demo_key(
        &mut terminal,
        &mut demo,
        KeyCode::Delete,
        Key::Named(NamedKey::Delete),
        None,
    );
    assert_eq!(terminal.bitmap_count(), 0);
    demo.compose(&mut terminal);
    check_incremental(&device, &queue, &mut terminal);
}

fn keyboard_input(
    code: teditor::terminal::input::KeyCode,
    logical: teditor::terminal::input::Key,
    text: Option<&str>,
) -> teditor::terminal::input::KeyInput {
    use teditor::terminal::input::*;
    KeyInput {
        physical_key: PhysicalKey::Code(code),
        key_without_modifiers: Some(logical.clone()),
        logical_key: logical,
        location: KeyLocation::Standard,
        state: ElementState::Pressed,
        repeat: false,
        synthetic: false,
        text: text.map(str::to_owned),
    }
}

fn demo_key(
    terminal: &mut teditor::terminal::Terminal,
    demo: &mut terminal_demo::Demo,
    code: teditor::terminal::input::KeyCode,
    logical: teditor::terminal::input::Key,
    text: Option<&str>,
) {
    let mut key = keyboard_input(code, logical, text);
    for event in terminal.handle_key_input(key.clone()) {
        demo.handle(event, terminal);
    }
    key.state = teditor::terminal::input::ElementState::Released;
    for event in terminal.handle_key_input(key) {
        demo.handle(event, terminal);
    }
}

fn demo_window_event(
    terminal: &mut teditor::terminal::Terminal,
    demo: &mut terminal_demo::Demo,
    event: winit::event::WindowEvent,
) {
    for event in terminal.handle_window_event(&event, std::time::Instant::now()) {
        demo.handle(event, terminal);
    }
}

fn demo_line(
    terminal: &mut teditor::terminal::Terminal,
    demo: &terminal_demo::Demo,
    length: u32,
) -> String {
    demo.compose(terminal);
    let row = terminal.back().size()[1] - 2;
    (9..9 + length)
        .map(|column| terminal.back().cell(column, row).unwrap().character)
        .collect()
}
