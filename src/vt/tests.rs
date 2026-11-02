use super::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const FG: Color = Color::rgb(200, 200, 200);
const BG: Color = Color::rgb(10, 20, 30);

fn vt(size: [u16; 2]) -> Vt {
    Vt::new(size, FG, BG).unwrap()
}
fn line(vt: &Vt, y: usize) -> String {
    vt.grid()[y].iter().map(|cell| cell.character).collect()
}
fn press(code: KeyCode, logical: Key, plain: Option<&str>, modifiers: Modifiers) -> KeyEvent {
    KeyEvent {
        physical_key: PhysicalKey::Code(code),
        key_without_modifiers: plain.map(|text| Key::Character(text.into())),
        logical_key: logical,
        location: KeyLocation::Standard,
        state: KeyState::Pressed,
        repeat: false,
        synthetic: false,
        modifiers,
        composing: false,
    }
}

#[test]
fn line_editing_keys_send_what_shells_understand() {
    let mut vt = vt([20, 4]);
    let sent = |vt: &mut Vt, code, named, modifiers| {
        vt.encode_key(&press(code, Key::Named(named), None, modifiers))
            .0
    };
    let left =
        |vt: &mut Vt, modifiers| sent(vt, KeyCode::ArrowLeft, NamedKey::ArrowLeft, modifiers);
    let shift = Modifiers {
        shift: true,
        ..Default::default()
    };
    let option = Modifiers {
        alt: true,
        ..Default::default()
    };
    let command = Modifiers {
        super_key: true,
        ..Default::default()
    };
    if cfg!(target_os = "macos") {
        assert_eq!(left(&mut vt, option), b"\x1bb");
        assert_eq!(
            sent(&mut vt, KeyCode::ArrowRight, NamedKey::ArrowRight, option),
            b"\x1bf"
        );
        assert_eq!(left(&mut vt, command), b"\x01");
        assert_eq!(
            sent(&mut vt, KeyCode::ArrowRight, NamedKey::ArrowRight, command),
            b"\x05"
        );
        assert_eq!(
            sent(&mut vt, KeyCode::Backspace, NamedKey::Backspace, command),
            b"\x15"
        );
    }
    if cfg!(not(windows)) {
        // At a prompt Shift+arrow is an arrow, also in application cursor mode.
        assert_eq!(left(&mut vt, shift), b"\x1b[D");
        assert_eq!(
            sent(&mut vt, KeyCode::ArrowUp, NamedKey::ArrowUp, shift),
            b"\x1b[A"
        );
        vt.write(b"\x1b[?1h");
        assert_eq!(left(&mut vt, shift), b"\x1bOD");
        vt.write(b"\x1b[?1l");
    }
    // A program that reads every key gets the real thing.
    vt.write(b"\x1b[>1u");
    assert_eq!(left(&mut vt, shift), b"\x1b[1;2D");
    vt.write(b"\x1b[<u");
    // So does a full-screen program.
    vt.write(b"\x1b[?1049h");
    assert_eq!(left(&mut vt, shift), b"\x1b[1;2D");
    assert_eq!(
        left(
            &mut vt,
            Modifiers {
                control: true,
                ..Default::default()
            }
        ),
        b"\x1b[1;5D"
    );
}

#[test]
fn renders_text_colors_cursor_and_only_dirty_rows() {
    let mut vt = vt([20, 4]);
    vt.write(b"hello \x1b[31;44mred\x1b[0m \x1b[4mline\x1b[0m\r\n\x1b[7minv\x1b[0m");
    vt.sync().unwrap();
    assert!(line(&vt, 0).starts_with("hello red line"));
    let hello = vt.grid()[0][0];
    assert_eq!((hello.foreground, hello.background), (FG, BG));
    let red = vt.grid()[0][6];
    assert_ne!(red.foreground, FG);
    assert_ne!(red.background, BG);
    assert!(vt.grid()[0][10].underline);
    let inverse = vt.grid()[1][0];
    assert_eq!((inverse.foreground, inverse.background), (BG, FG));
    assert_eq!(vt.cursor().unwrap().position, [3, 1]);
    // Overwriting one row leaves the others correct.
    vt.write(b"\x1b[1;1HHELLO");
    vt.sync().unwrap();
    assert!(line(&vt, 0).starts_with("HELLO red"));
    assert!(line(&vt, 1).starts_with("inv"));
}

#[test]
fn cursor_changes_that_touch_no_cells_are_still_reported() {
    let mut vt = vt([20, 3]);
    vt.write(b"abc");
    vt.sync().unwrap();
    assert_eq!(vt.cursor().unwrap().position, [3, 0]);
    // Moves, as a program sends for a trailing space or an arrow key.
    vt.write(b"\x1b[10G");
    vt.sync().unwrap();
    assert_eq!(vt.cursor().unwrap().position, [9, 0]);
    vt.write(b"\x08");
    vt.sync().unwrap();
    assert_eq!(vt.cursor().unwrap().position, [8, 0]);
    vt.write(b"\x1b[2;4H");
    vt.sync().unwrap();
    assert_eq!(vt.cursor().unwrap().position, [3, 1]);
    // Shape and visibility.
    vt.write(b"\x1b[6 q");
    vt.sync().unwrap();
    assert_eq!(vt.cursor().unwrap().shape, CursorShape::Bar);
    vt.write(b"\x1b[?25l");
    vt.sync().unwrap();
    assert!(vt.cursor().is_none());
    vt.write(b"\x1b[?25h");
    vt.sync().unwrap();
    assert_eq!(vt.cursor().unwrap().position, [3, 1]);
    // The cells themselves are undisturbed.
    assert!(line(&vt, 0).starts_with("abc"));
}

#[test]
fn wide_characters_report_both_halves_from_ghostty() {
    let mut vt = vt([20, 3]);
    vt.write("a\u{1F389}b\r\n\u{4E2D}".as_bytes());
    vt.sync().unwrap();
    let row = &vt.grid()[0];
    assert_eq!((row[0].character, row[0].columns), ('a', 1));
    assert_eq!((row[1].character, row[1].columns), ('\u{1F389}', 2));
    assert_eq!(row[2].columns, 0);
    assert_eq!((row[3].character, row[3].columns), ('b', 1));
    assert_eq!(vt.grid()[1][0].columns, 2);
    assert_eq!(vt.cursor().unwrap().position, [2, 1]);
}

#[test]
fn resizing_rewraps_and_clearing_and_hiding_the_cursor_are_reflected() {
    let mut vt = vt([10, 3]);
    vt.write(b"0123456789abc");
    vt.sync().unwrap();
    assert_eq!(line(&vt, 1).trim_end(), "abc");
    vt.resize([20, 3]).unwrap();
    vt.sync().unwrap();
    assert_eq!(vt.grid().len(), 3);
    assert_eq!(vt.grid()[0].len(), 20);
    assert!(line(&vt, 0).starts_with("0123456789abc"));
    vt.write(b"\x1b[?25l\x1b[2J");
    vt.sync().unwrap();
    assert!(vt.cursor().is_none());
    assert_eq!(line(&vt, 0).trim(), "");
}

#[test]
fn answers_device_and_size_queries_for_the_program() {
    let mut vt = vt([80, 24]);
    vt.write(b"\x1b[c");
    assert_eq!(vt.take_output(), b"\x1b[?62;22c");
    vt.write(b"\x1b[18t");
    assert_eq!(vt.take_output(), b"\x1b[8;24;80t");
}

#[test]
fn encodes_keys_for_shells_including_layout_text_and_control() {
    let mut vt = vt([80, 24]);
    let none = Modifiers::default();
    let control = Modifiers {
        control: true,
        ..none
    };
    let (bytes, text) = vt.encode_key(&press(
        KeyCode::KeyA,
        Key::Character("a".into()),
        Some("a"),
        none,
    ));
    assert_eq!((bytes.as_slice(), text.as_deref()), (&b"a"[..], Some("a")));
    let (bytes, text) = vt.encode_key(&press(
        KeyCode::KeyC,
        Key::Character("c".into()),
        Some("c"),
        control,
    ));
    assert_eq!((bytes.as_slice(), text), (&b"\x03"[..], None));
    let named = |vt: &mut Vt, code, key| vt.encode_key(&press(code, Key::Named(key), None, none)).0;
    assert_eq!(named(&mut vt, KeyCode::Enter, NamedKey::Enter), b"\r");
    assert_eq!(
        named(&mut vt, KeyCode::Backspace, NamedKey::Backspace),
        b"\x7f"
    );
    assert_eq!(
        named(&mut vt, KeyCode::ArrowUp, NamedKey::ArrowUp),
        b"\x1b[A"
    );
    // Application cursor mode, requested by a program, changes the encoding.
    vt.write(b"\x1b[?1h");
    assert_eq!(
        named(&mut vt, KeyCode::ArrowUp, NamedKey::ArrowUp),
        b"\x1bOA"
    );
    // Option+L on a German Mac layout is the text "@", not Alt+L.
    let option = Modifiers { alt: true, ..none };
    let (bytes, text) = vt.encode_key(&press(
        KeyCode::KeyL,
        Key::Character("@".into()),
        Some("l"),
        option,
    ));
    assert_eq!(
        (bytes.as_slice(), text.as_deref()),
        ("@".as_bytes(), Some("@"))
    );
}

#[test]
fn mouse_events_follow_the_programs_tracking_mode() {
    let mut vt = vt([80, 24]);
    let none = Modifiers::default();
    assert!(!vt.mouse_tracking());
    vt.write(b"\x1b[?1000h\x1b[?1006h");
    assert!(vt.mouse_tracking());
    let down = vt.encode_mouse(
        MouseAction::Press,
        Some(MouseButton::Left),
        none,
        [4.5, 2.5],
    );
    assert_eq!(down, b"\x1b[<0;5;3M");
    let up = vt.encode_mouse(
        MouseAction::Release,
        Some(MouseButton::Left),
        none,
        [4.5, 2.5],
    );
    assert_eq!(up, b"\x1b[<0;5;3m");
    assert_eq!(vt.encode_wheel(-1, none, [0.5, 0.5]), b"\x1b[<64;1;1M");
    assert_eq!(
        vt.encode_wheel(2, none, [0.5, 0.5]),
        b"\x1b[<65;1;1M\x1b[<65;1;1M"
    );
}

#[test]
fn drag_and_double_click_select_text_for_copying() {
    let mut vt = vt([20, 3]);
    vt.write(b"hello world\r\nsecond");
    vt.sync().unwrap();
    assert_eq!(vt.selection_text(), None);
    vt.selection_press([0.1, 0.5]);
    vt.selection_drag([4.9, 0.5]);
    vt.selection_release();
    assert_eq!(vt.selection_text().as_deref(), Some("hello"));
    vt.sync().unwrap();
    assert!(vt.grid()[0][4].selected);
    assert!(!vt.grid()[0][5].selected);
    vt.clear_selection();
    assert_eq!(vt.selection_text(), None);
    vt.selection_press([7.5, 0.5]);
    vt.selection_release();
    vt.selection_press([7.5, 0.5]);
    vt.selection_release();
    assert_eq!(vt.selection_text().as_deref(), Some("world"));
}

#[test]
fn paste_strips_control_bytes_and_brackets_on_request() {
    let mut vt = vt([20, 3]);
    assert_eq!(vt.encode_paste("a\x1bb"), b"a b");
    vt.write(b"\x1b[?2004h");
    assert_eq!(vt.encode_paste("x"), b"\x1b[200~x\x1b[201~");
}

#[test]
fn scrollback_moves_the_viewport_and_returns_to_the_bottom() {
    let mut vt = vt([20, 3]);
    for n in 0..20 {
        vt.write(format!("line {n}\r\n").as_bytes());
    }
    vt.sync().unwrap();
    let bar = vt.scrollbar();
    assert!(bar.total > bar.visible);
    assert_eq!(bar.offset + bar.visible, bar.total);
    assert_eq!(line(&vt, 1).trim_end(), "line 19");
    vt.scroll_lines(-1);
    vt.sync().unwrap();
    assert_eq!(line(&vt, 2).trim_end(), "line 19");
    assert!(vt.cursor().is_none(), "the cursor is below the viewport");
    vt.scroll_to_row(0);
    vt.sync().unwrap();
    assert_eq!(line(&vt, 0).trim_end(), "line 0");
    vt.scroll_to_bottom();
    vt.sync().unwrap();
    assert_eq!(line(&vt, 1).trim_end(), "line 19");
}

#[test]
fn session_runs_a_real_program_through_the_pty_and_reports_exit() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    let (shell, input, sizes): (&str, &[u8], &[&str]) = if cfg!(windows) {
        // Clearing first leaves everything within the ten rows.
        (
            "cmd.exe",
            b"cls & set /a N=40+2 >nul & call echo sum-%N% & mode con | findstr \"Lines Columns\"\r\nexit\r\n",
            &[" 10", " 60"],
        )
    } else {
        (
            "/bin/sh",
            b"stty size; echo sum-$((40+2))\nexit\n",
            &["10 60"],
        )
    };
    let mut session = Session::spawn(
        &[shell.into()],
        &std::env::temp_dir(),
        [60, 10],
        [FG, BG],
        Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }),
    )
    .unwrap();
    session.send(input.to_vec());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !session.exited() {
        assert!(Instant::now() < deadline, "the shell did not exit");
        session.pump();
        std::thread::sleep(Duration::from_millis(2));
    }
    while session.pump() {}
    session.vt.sync().unwrap();
    let screen: String = (0..10).map(|y| line(&session.vt, y) + "\n").collect();
    for size in sizes {
        assert!(screen.contains(size), "pty size not applied: {screen}");
    }
    assert!(screen.contains("sum-42"), "{screen}");
    assert!(wakes.load(Ordering::Relaxed) > 0);
}
