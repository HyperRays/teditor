use super::input::{MouseButton, MouseScrollDelta, PointerKind, TouchPhase};
use super::*;
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
use winit::{
    dpi::PhysicalPosition,
    event::{DeviceId, ElementState, WindowEvent},
    keyboard::ModifiersState,
};

fn cell(character: char) -> Cell {
    Cell::new(character, Style::default())
}
fn buffer() -> Framebuffer {
    Framebuffer::new([8, 4], Style::default().background).unwrap()
}

#[test]
fn buffers_are_independent_and_diff_only_changed_runs() {
    let front = buffer();
    let mut back = front.clone();
    back.set(1, 1, cell('A'));
    back.set(2, 1, cell('B'));
    back.set(6, 1, cell('C'));
    assert_eq!(front.cell(1, 1).unwrap().character, ' ');
    assert_eq!(
        back.damage(&front, &BTreeSet::new(), false),
        vec![CellRect::new(1, 1, 2, 1), CellRect::new(6, 1, 1, 1)]
    );
    back.clear(cell(' '));
    assert!(back.damage(&front, &BTreeSet::new(), false).is_empty());
    assert_eq!(
        back.damage(&front, &BTreeSet::new(), true),
        vec![back.bounds()]
    );
}

#[test]
fn styles_and_backdrop_changes_invalidate_content() {
    let front = buffer();
    let mut back = front.clone();
    back.style(
        CellRect::new(2, 2, 1, 1),
        Style {
            background: Color::rgb(255, 0, 0),
            ..Default::default()
        },
    );
    assert_eq!(
        back.damage(&front, &BTreeSet::new(), false),
        vec![CellRect::new(2, 2, 1, 1)]
    );
    back.set_backdrop(Color(10, 20, 30, 0));
    assert_eq!(back.backdrop(), Color::rgb(10, 20, 30));
    assert_eq!(
        back.damage(&front, &BTreeSet::new(), false),
        vec![back.bounds()]
    );
}

#[test]
fn writes_clip_and_expand_tabs_and_newlines() {
    let mut frame = buffer();
    frame.write(1, 0, "a\tb\nc", Style::default());
    assert_eq!(frame.cell(1, 0).unwrap().character, 'a');
    assert_eq!(frame.cell(4, 0).unwrap().character, 'b');
    assert_eq!(frame.cell(1, 1).unwrap().character, 'c');
    assert!(!frame.set(8, 0, cell('X')));
    let before = frame.clone();
    frame.fill(CellRect::new(u32::MAX, 0, 5, 2), cell('X'));
    frame.fill(CellRect::new(0, u32::MAX, 5, 2), cell('X'));
    assert_eq!(frame, before);
    frame.fill(CellRect::new(6, 3, u32::MAX, u32::MAX), cell('Z'));
    assert_eq!(frame.cell(7, 3).unwrap().character, 'Z');
    assert!(Framebuffer::new([u32::MAX, 2], Color::rgb(0, 0, 0)).is_err());
}

#[test]
fn scrolling_moves_cells_and_styles_without_touching_neighbors() {
    let mut frame = buffer();
    frame.write(1, 1, "ABC\nDEF", Style::default());
    frame.set(0, 0, cell('!'));
    frame.scroll(CellRect::new(1, 1, 3, 2), -1, 0, cell('.'));
    assert_eq!(
        (1..4)
            .map(|x| frame.cell(x, 1).unwrap().character)
            .collect::<String>(),
        "BC."
    );
    assert_eq!(
        (1..4)
            .map(|x| frame.cell(x, 2).unwrap().character)
            .collect::<String>(),
        "EF."
    );
    assert_eq!(frame.cell(0, 0).unwrap().character, '!');
    frame.scroll(CellRect::new(1, 1, 3, 2), 0, -1, cell(' '));
    assert_eq!(frame.cell(1, 1).unwrap().character, 'E');
    assert_eq!(frame.cell(1, 2).unwrap().character, ' ');
    frame.scroll(frame.bounds(), i32::MIN, i32::MAX, cell(' '));
    assert_eq!(frame, buffer());
}

#[test]
fn resize_preserves_overlap_and_initializes_new_cells() {
    let mut frame = buffer();
    frame.set(2, 2, cell('A'));
    let larger = frame.resized([10, 6]).unwrap();
    assert_eq!(larger.cell(2, 2).unwrap().character, 'A');
    assert_eq!(larger.cell(9, 5).unwrap().character, ' ');
    let smaller = larger.resized([3, 3]).unwrap();
    assert_eq!(smaller.cell(2, 2).unwrap().character, 'A');
}

fn input() -> InputState {
    InputState::new(
        CellMetrics {
            width: 10,
            height: 20,
            baseline: 15,
        },
        [103, 83],
        1.0,
    )
}
fn motion(x: f64, y: f64) -> WindowEvent {
    WindowEvent::CursorMoved {
        device_id: DeviceId::dummy(),
        position: PhysicalPosition::new(x, y),
    }
}
fn button(button: MouseButton, down: bool) -> WindowEvent {
    WindowEvent::MouseInput {
        device_id: DeviceId::dummy(),
        state: if down {
            ElementState::Pressed
        } else {
            ElementState::Released
        },
        button,
    }
}
fn kinds(events: &[InputEvent]) -> Vec<PointerKind> {
    events
        .iter()
        .filter_map(|event| {
            if let InputEvent::Pointer(pointer) = event {
                Some(pointer.kind)
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn click_counts_modifiers_and_buttons_are_preserved() {
    let mut state = input();
    let now = Instant::now();
    state.handle_window_event(&motion(12.0, 22.0), now);
    state.handle_window_event(
        &WindowEvent::ModifiersChanged((ModifiersState::SHIFT | ModifiersState::CONTROL).into()),
        now,
    );
    for count in 1..=3 {
        let time = now + Duration::from_millis(count as u64 * 100);
        state.handle_window_event(&button(MouseButton::Left, true), time);
        let events = state.handle_window_event(&button(MouseButton::Left, false), time);
        assert!(kinds(&events).contains(&PointerKind::Click {
            button: MouseButton::Left,
            count
        }));
        assert!(events.iter().any(|event| matches!(event, InputEvent::Pointer(pointer) if pointer.modifiers.shift && pointer.modifiers.control)));
    }
    let time = now + Duration::from_secs(2);
    state.handle_window_event(&button(MouseButton::Right, true), time);
    state.handle_window_event(&button(MouseButton::Middle, true), time);
    assert!(state.is_pressed(MouseButton::Right) && state.is_pressed(MouseButton::Middle));
    let events = state.handle_window_event(&button(MouseButton::Right, false), time);
    assert!(kinds(&events).contains(&PointerKind::Click {
        button: MouseButton::Right,
        count: 1
    }));
    assert!(state.is_pressed(MouseButton::Middle));
}

#[test]
fn dragging_survives_leave_and_never_emits_a_click() {
    let mut state = input();
    let now = Instant::now();
    state.handle_window_event(&motion(12.0, 22.0), now);
    state.handle_window_event(&button(MouseButton::Left, true), now);
    let events = state.handle_window_event(&motion(18.0, 22.0), now);
    assert!(
        kinds(&events)
            .iter()
            .any(|kind| matches!(kind, PointerKind::DragStart { .. }))
    );
    state.handle_window_event(
        &WindowEvent::CursorLeft {
            device_id: DeviceId::dummy(),
        },
        now,
    );
    assert!(state.is_pressed(MouseButton::Left));
    let events = state.handle_window_event(&motion(-20.0, 100.0), now);
    assert!(events.iter().any(|event| matches!(event, InputEvent::Pointer(pointer)
        if matches!(pointer.kind, PointerKind::DragMove { .. }) && pointer.position.unwrap().cell.is_none())));
    let events = state.handle_window_event(&button(MouseButton::Left, false), now);
    assert!(
        kinds(&events)
            .iter()
            .any(|kind| matches!(kind, PointerKind::DragEnd { .. }))
    );
    assert!(
        !kinds(&events)
            .iter()
            .any(|kind| matches!(kind, PointerKind::Click { .. }))
    );
    assert!(!state.is_pressed(MouseButton::Left));
}

#[test]
fn drag_threshold_uses_logical_pixels_and_geometry_cancels_capture() {
    let mut state = InputState::new(
        CellMetrics {
            width: 20,
            height: 40,
            baseline: 30,
        },
        [200, 160],
        2.0,
    );
    let now = Instant::now();
    state.handle_window_event(&motion(22.0, 42.0), now);
    state.handle_window_event(&button(MouseButton::Left, true), now);
    let events = state.handle_window_event(&motion(28.0, 42.0), now);
    assert!(
        !kinds(&events)
            .iter()
            .any(|kind| matches!(kind, PointerKind::DragStart { .. }))
    );
    let events = state.handle_window_event(&motion(30.0, 42.0), now);
    assert!(
        kinds(&events)
            .iter()
            .any(|kind| matches!(kind, PointerKind::DragStart { .. }))
    );
    let events = state.set_geometry(
        CellMetrics {
            width: 10,
            height: 20,
            baseline: 15,
        },
        [100, 80],
        1.0,
    );
    assert!(kinds(&events).contains(&PointerKind::Cancel {
        button: MouseButton::Left
    }));
    assert!(!state.is_pressed(MouseButton::Left));
}

#[test]
fn focus_loss_resets_capture_and_modifiers_and_late_release_is_not_click() {
    let mut state = input();
    let now = Instant::now();
    state.handle_window_event(&motion(10.0, 20.0), now);
    state.handle_window_event(&button(MouseButton::Left, true), now);
    state.handle_window_event(
        &WindowEvent::ModifiersChanged(ModifiersState::ALT.into()),
        now,
    );
    let events = state.handle_window_event(&WindowEvent::Focused(false), now);
    assert!(kinds(&events).contains(&PointerKind::Cancel {
        button: MouseButton::Left
    }));
    assert!(!state.modifiers().alt);
    assert!(state.position().is_none());
    let events = state.handle_window_event(&button(MouseButton::Left, false), now);
    assert_eq!(kinds(&events), vec![PointerKind::Up(MouseButton::Left)]);
}

#[test]
fn padding_and_unknown_positions_do_not_click() {
    let mut state = input();
    let now = Instant::now();
    for position in [None, Some([101.0, 12.0]), Some([12.0, 81.0])] {
        if let Some([x, y]) = position {
            state.handle_window_event(&motion(x, y), now);
        }
        state.handle_window_event(&button(MouseButton::Left, true), now);
        let events = state.handle_window_event(&button(MouseButton::Left, false), now);
        assert!(
            !kinds(&events)
                .iter()
                .any(|kind| matches!(kind, PointerKind::Click { .. }))
        );
    }
}

#[test]
fn scroll_keeps_fractional_axes_units_and_phases() {
    let mut state = input();
    let now = Instant::now();
    for (delta, phase, expected) in [
        (
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(2.5, -5.0)),
            TouchPhase::Moved,
            [0.25, -0.25],
        ),
        (
            MouseScrollDelta::LineDelta(-0.5, 2.0),
            TouchPhase::Started,
            [-0.5, 2.0],
        ),
        (
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.0)),
            TouchPhase::Ended,
            [0.0, 0.0],
        ),
    ] {
        let events = state.handle_window_event(
            &WindowEvent::MouseWheel {
                device_id: DeviceId::dummy(),
                delta,
                phase,
            },
            now,
        );
        let InputEvent::Scroll(scroll) = events[0] else {
            panic!("expected scroll")
        };
        assert_eq!(scroll.cells, expected);
        assert_eq!(scroll.delta, delta);
        assert_eq!(scroll.phase, phase);
    }
}

#[test]
fn multiple_file_hover_drop_and_cancel_events_preserve_paths() {
    let mut state = input();
    let now = Instant::now();
    let first = std::path::PathBuf::from("/tmp/one image.png");
    let second = std::path::PathBuf::from("/tmp/two.txt");
    state.handle_window_event(&WindowEvent::HoveredFile(first.clone()), now);
    state.handle_window_event(&WindowEvent::HoveredFile(second.clone()), now);
    assert_eq!(state.hovered_files(), &[first.clone(), second.clone()]);
    let events = state.handle_window_event(&WindowEvent::DroppedFile(first.clone()), now);
    assert_eq!(
        events,
        vec![InputEvent::FileDropped {
            path: first,
            position: None
        }]
    );
    assert_eq!(state.hovered_files(), &[second]);
    assert_eq!(
        state.handle_window_event(&WindowEvent::HoveredFileCancelled, now),
        vec![InputEvent::FileHoverCancelled]
    );
    assert!(state.hovered_files().is_empty());
}
