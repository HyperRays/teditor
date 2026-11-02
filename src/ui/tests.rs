use super::*;
use crate::terminal::input::{
    ImeEvent, Key, KeyCode, KeyEvent, KeyLocation, KeyState, Modifiers, MouseButton, NamedKey,
    PhysicalKey, PointerEvent, PointerKind, PointerPosition, TextInput, TextSource,
};
use controls::Clicks;

fn frame() -> Framebuffer {
    Framebuffer::new([80, 30], Theme::default().desktop).unwrap()
}
fn pointer(kind: PointerKind, x: f64, y: f64) -> InputEvent {
    InputEvent::Pointer(PointerEvent {
        kind,
        position: Some(PointerPosition {
            physical: [x * 10.0, y * 20.0],
            grid: [x, y],
            cell: if x >= 0.0 && y >= 0.0 {
                Some([x as u32, y as u32])
            } else {
                None
            },
        }),
        modifiers: Modifiers::default(),
    })
}
fn click(x: f64, y: f64) -> Vec<InputEvent> {
    vec![
        pointer(PointerKind::Down(MouseButton::Left), x, y),
        pointer(PointerKind::Up(MouseButton::Left), x, y),
    ]
}
fn key(named: NamedKey, shift: bool) -> InputEvent {
    InputEvent::Key(KeyEvent {
        physical_key: PhysicalKey::Code(KeyCode::Space),
        logical_key: Key::Named(named),
        key_without_modifiers: None,
        location: KeyLocation::Standard,
        state: KeyState::Pressed,
        repeat: false,
        synthetic: false,
        modifiers: Modifiers {
            shift,
            ..Default::default()
        },
        composing: false,
    })
}
fn text(value: &str, source: TextSource) -> InputEvent {
    InputEvent::Text(TextInput {
        text: value.into(),
        source,
        modifiers: Modifiers::default(),
    })
}

/// Files on the left; editor above preview on the right, each holding one canvas.
fn tiled(ui: &mut Ui<'_>) {
    ui.split(
        "workspace",
        Split::columns(0.25),
        |left| {
            left.pane("files", "Files", |ui| {
                ui.canvas("action", 1, false);
            });
        },
        |right| {
            right.split(
                "details",
                Split::rows(0.6),
                |top| {
                    top.pane("editor", "Editor", |ui| {
                        ui.canvas("action", 1, false);
                    });
                },
                |bottom| {
                    bottom.pane("preview", "Preview", |ui| {
                        ui.canvas("action", 1, false);
                    });
                },
            );
        },
    );
}
fn canvas_in(pane: &str) -> Id {
    Id::new(pane).with("action")
}

#[test]
fn clicks_require_a_press_and_release_on_the_same_region() {
    let mut ctx = Context::new();
    let mut frame = frame();
    let mut clicks = Clicks::default();
    let regions = [
        (CellRect::new(0, 0, 4, 1), 'a'),
        (CellRect::new(4, 0, 4, 1), 'b'),
    ];
    let mut run = |ctx: &mut Context, events: &[InputEvent]| {
        let mut result = None;
        ctx.run(&mut frame, events, |ui| {
            let canvas = ui.canvas("canvas", 1, false);
            result = clicks.update(&canvas, &regions);
        });
        result
    };
    run(&mut ctx, &[]);
    let release = pointer(PointerKind::Up(MouseButton::Left), 1.0, 0.0);
    assert_eq!(
        run(&mut ctx, &[release]),
        None,
        "a release alone is no click"
    );
    assert_eq!(run(&mut ctx, &click(1.0, 0.0)), Some('a'));
    assert_eq!(run(&mut ctx, &[]), None, "events are consumed only once");
    let dragged = [
        pointer(PointerKind::Down(MouseButton::Left), 1.0, 0.0),
        pointer(PointerKind::Up(MouseButton::Left), 5.0, 0.0),
    ];
    assert_eq!(run(&mut ctx, &dragged), None, "dragging away cancels");
}

#[test]
fn disabled_canvases_ignore_input() {
    let mut ctx = Context::new();
    let mut frame = frame();
    let mut events = 1;
    ctx.run(&mut frame, &[], |ui| {
        ui.disabled(true, |ui| {
            ui.canvas("canvas", 1, false);
        })
    });
    ctx.run(&mut frame, &click(1.0, 0.0), |ui| {
        ui.disabled(true, |ui| {
            events = ui.canvas("canvas", 1, false).events.len();
        })
    });
    assert_eq!(events, 0);
    assert!(ctx.focused().is_none());
}

#[test]
fn tiles_cover_the_entire_grid_and_titles_do_not_move_panes() {
    let mut ctx = Context::new();
    let mut frame = frame();
    ctx.run(&mut frame, &[], tiled);
    assert_eq!(ctx.pane_rect("files"), Some(CellRect::new(0, 0, 20, 30)));
    assert_eq!(ctx.pane_rect("editor"), Some(CellRect::new(20, 0, 60, 18)));
    assert_eq!(
        ctx.pane_rect("preview"),
        Some(CellRect::new(20, 18, 60, 12))
    );
    for y in 0..30 {
        for x in 0..80 {
            assert_eq!(
                ctx.panes
                    .iter()
                    .filter(|pane| pane.rect.contains([f64::from(x), f64::from(y)]))
                    .count(),
                1
            );
        }
    }
    let before: Vec<_> = ctx.panes.iter().map(|pane| pane.rect).collect();
    ctx.run(
        &mut frame,
        &[
            pointer(PointerKind::Down(MouseButton::Left), 24.0, 0.0),
            pointer(PointerKind::Move, 40.0, 8.0),
            pointer(PointerKind::Up(MouseButton::Left), 40.0, 8.0),
        ],
        tiled,
    );
    assert_eq!(
        before,
        ctx.panes.iter().map(|pane| pane.rect).collect::<Vec<_>>()
    );
    ctx.run(&mut frame, &click(22.0, 1.0), tiled);
    assert_eq!(ctx.focused(), Some(canvas_in("editor")));
}

#[test]
fn shared_dividers_resize_both_neighbors_and_preserve_proportions() {
    let mut ctx = Context::new();
    let mut frame = frame();
    ctx.run(&mut frame, &[], tiled);
    let output = ctx.run(
        &mut frame,
        &[
            pointer(PointerKind::Down(MouseButton::Left), 19.2, 8.0),
            pointer(PointerKind::Move, 29.8, 8.0),
        ],
        tiled,
    );
    assert_eq!(output.cursor, Cursor::ResizeColumns);
    assert_eq!(ctx.pane_rect("files").unwrap().columns, 30);
    assert_eq!(
        ctx.pane_rect("editor").unwrap(),
        CellRect::new(30, 0, 50, 18)
    );
    ctx.run(
        &mut frame,
        &[pointer(PointerKind::Up(MouseButton::Left), 29.8, 8.0)],
        tiled,
    );
    ctx.run(
        &mut frame,
        &[
            pointer(PointerKind::Down(MouseButton::Left), 50.0, 17.0),
            pointer(PointerKind::Move, 50.0, 20.0),
            pointer(PointerKind::Up(MouseButton::Left), 50.0, 20.0),
        ],
        tiled,
    );
    assert_eq!(ctx.pane_rect("editor").unwrap().rows, 21);
    assert_eq!(
        ctx.pane_rect("preview").unwrap(),
        CellRect::new(30, 21, 50, 9)
    );
    frame = Framebuffer::new([160, 60], ctx.theme.desktop).unwrap();
    ctx.run(&mut frame, &[], tiled);
    assert_eq!(ctx.pane_rect("files").unwrap(), CellRect::new(0, 0, 60, 60));
    assert_eq!(
        ctx.pane_rect("editor").unwrap(),
        CellRect::new(60, 0, 100, 42)
    );
    assert_eq!(
        ctx.pane_rect("preview").unwrap(),
        CellRect::new(60, 42, 100, 18)
    );
}

#[test]
fn divider_capture_clamps_cancels_and_ignores_stale_positions() {
    let mut ctx = Context::new();
    let mut frame = frame();
    ctx.run(&mut frame, &[], tiled);
    ctx.run(
        &mut frame,
        &[
            pointer(PointerKind::Down(MouseButton::Left), 19.0, 8.0),
            pointer(PointerKind::Move, -500.0, 8.0),
            pointer(PointerKind::Up(MouseButton::Left), -500.0, 8.0),
            pointer(PointerKind::Down(MouseButton::Left), 22.0, 1.0),
            pointer(PointerKind::Up(MouseButton::Left), 22.0, 1.0),
        ],
        tiled,
    );
    assert_eq!(ctx.pane_rect("files").unwrap().columns, 12);
    assert!(
        ctx.focused().is_none(),
        "a click after a geometry change must not use the stale layout"
    );
    ctx.run(
        &mut frame,
        &[pointer(PointerKind::Down(MouseButton::Left), 11.0, 8.0)],
        tiled,
    );
    frame = Framebuffer::new([60, 20], ctx.theme.desktop).unwrap();
    ctx.run(&mut frame, &[pointer(PointerKind::Move, 40.0, 8.0)], tiled);
    assert!(ctx.capture.is_none());
    assert_eq!(ctx.pane_rect("files").unwrap().columns, 12);
    ctx.run(
        &mut frame,
        &[
            pointer(PointerKind::Down(MouseButton::Left), 11.0, 8.0),
            InputEvent::Focused(false),
        ],
        tiled,
    );
    assert!(ctx.capture.is_none());
}

#[test]
fn small_and_odd_viewports_have_no_gaps_or_overlaps() {
    let mut ctx = Context::new();
    for size in [[81, 31], [13, 5], [2, 2], [1, 1], [0, 0], [80, 30]] {
        let mut frame = Framebuffer::new(size, ctx.theme.desktop).unwrap();
        ctx.run(&mut frame, &[], tiled);
        for y in 0..size[1] {
            for x in 0..size[0] {
                assert_eq!(
                    ctx.panes
                        .iter()
                        .filter(|pane| pane.rect.contains([f64::from(x), f64::from(y)]))
                        .count(),
                    1,
                    "{size:?} at {x},{y}"
                );
            }
        }
    }
    assert_eq!(
        ctx.pane_rect("files").unwrap().columns,
        20,
        "shrinking must preserve the preferred proportion for restoration"
    );
}

#[test]
fn tab_focuses_a_canvas_in_the_active_pane_and_then_belongs_to_it() {
    let mut ctx = Context::new();
    let mut frame = frame();
    ctx.run(&mut frame, &[], tiled);
    // A press on a pane header activates that pane without focusing any canvas.
    ctx.run(
        &mut frame,
        &[pointer(PointerKind::Down(MouseButton::Left), 40.0, 18.0)],
        tiled,
    );
    assert_eq!(ctx.active_pane, Some(Id::new("preview")));
    ctx.run(&mut frame, &[key(NamedKey::Tab, false)], tiled);
    assert_eq!(ctx.focused(), Some(canvas_in("preview")));
    ctx.run(&mut frame, &[key(NamedKey::Tab, false)], tiled);
    assert_eq!(
        ctx.focused(),
        Some(canvas_in("preview")),
        "a focused canvas receives Tab itself"
    );
}

#[test]
fn focus_transfer_rejects_a_late_ime_commit_until_ime_is_enabled_again() {
    let mut ctx = Context::new();
    let mut frame = frame();
    let mut received = [Vec::new(), Vec::new()];
    let mut build = |ctx: &mut Context, frame: &mut Framebuffer, events: &[InputEvent]| {
        ctx.run(frame, events, |ui| {
            for (index, key) in ["first", "second"].into_iter().enumerate() {
                let canvas = ui.canvas(key, 1, true);
                for event in &canvas.events {
                    if let InputEvent::Text(text) = event {
                        received[index].push(text.text.clone());
                    }
                }
            }
        })
    };
    build(&mut ctx, &mut frame, &[]);
    let mut events = click(1.0, 0.0);
    events.push(text("λ", TextSource::Keyboard));
    build(&mut ctx, &mut frame, &events);
    let commit = text("候補", TextSource::Ime);
    let mut transfer = click(1.0, 2.0);
    transfer.push(commit.clone());
    let output = build(&mut ctx, &mut frame, &transfer);
    assert!(output.reset_ime);
    build(
        &mut ctx,
        &mut frame,
        &[InputEvent::Ime(ImeEvent::Enabled), commit],
    );
    assert_eq!(received[0], ["λ"]);
    assert_eq!(
        received[1],
        ["候補"],
        "the late commit from the old target must be discarded"
    );
}

#[test]
fn stale_capture_and_focus_do_not_leak_events() {
    let mut ctx = Context::new();
    let mut frame = frame();
    let build = |ctx: &mut Context, frame: &mut Framebuffer, input: &[InputEvent]| {
        let mut events = 0;
        ctx.run(frame, input, |ui| {
            events = ui.canvas("gone", 1, false).events.len();
        });
        events
    };
    build(&mut ctx, &mut frame, &[]);
    ctx.run(
        &mut frame,
        &[pointer(PointerKind::Down(MouseButton::Left), 1.0, 0.0)],
        |_| {},
    );
    assert!(ctx.capture.is_none() && ctx.focused().is_none());
    let release = pointer(PointerKind::Up(MouseButton::Left), 1.0, 0.0);
    assert_eq!(build(&mut ctx, &mut frame, &[release]), 0);
    build(&mut ctx, &mut frame, &click(1.0, 0.0));
    let unfocused = [InputEvent::Focused(false), key(NamedKey::Enter, false)];
    assert_eq!(
        build(&mut ctx, &mut frame, &unfocused),
        0,
        "keys do not reach an unfocused window's canvas"
    );
}

#[test]
fn subtracting_occlusion_preserves_only_visible_image_cells() {
    let pieces = geometry::subtract(CellRect::new(2, 2, 8, 6), CellRect::new(4, 3, 3, 3));
    assert_eq!(
        pieces.iter().map(|p| p.columns * p.rows).sum::<u32>(),
        48 - 9
    );
    for piece in pieces {
        assert_eq!(
            piece.intersection(CellRect::new(4, 3, 3, 3)).columns
                * piece.intersection(CellRect::new(4, 3, 3, 3)).rows,
            0
        );
    }
}
