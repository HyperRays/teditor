use super::input::*;
use super::{InputEvent, InputState};
use crate::renderer::CellMetrics;
use std::time::Instant;
use winit::{
    event::{Ime, WindowEvent},
    keyboard::ModifiersState,
};

fn input() -> InputState {
    InputState::new(
        CellMetrics {
            width: 10,
            height: 20,
            baseline: 15,
        },
        [100, 80],
        1.0,
    )
}

fn key(code: KeyCode, logical: Key, text: Option<&str>) -> KeyInput {
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

fn character(code: KeyCode, text: &str) -> KeyInput {
    key(code, Key::Character(text.into()), Some(text))
}

fn texts(events: &[InputEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| {
            if let InputEvent::Text(text) = event {
                Some(text.text.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn ime(state: &mut InputState, event: Ime) -> Vec<InputEvent> {
    state.handle_window_event(&WindowEvent::Ime(event), Instant::now())
}

fn modifiers(state: &mut InputState, modifiers: ModifiersState) {
    state.handle_window_event(
        &WindowEvent::ModifiersChanged(modifiers.into()),
        Instant::now(),
    );
}

#[test]
fn keyboard_keeps_layout_identity_locations_and_modifier_snapshots() {
    let mut state = input();
    modifiers(&mut state, ModifiersState::SHIFT);
    let mut native = character(KeyCode::KeyQ, "A");
    native.key_without_modifiers = Some(Key::Character("a".into()));
    let events = state.handle_key_input(native);
    let InputEvent::Key(key) = &events[0] else {
        panic!("expected key")
    };
    assert_eq!(key.physical_key, PhysicalKey::Code(KeyCode::KeyQ));
    assert_eq!(key.logical_key, Key::Character("A".into()));
    assert_eq!(key.key_without_modifiers, Some(Key::Character("a".into())));
    assert!(key.modifiers.shift && key.is_actionable());
    assert_eq!(texts(&events), ["A"]);
    let mut keypad = character(KeyCode::Numpad1, "1");
    keypad.location = KeyLocation::Numpad;
    let events = state.handle_key_input(keypad);
    assert!(matches!(&events[0], InputEvent::Key(event) if event.location == KeyLocation::Numpad));
}

#[test]
fn key_repeats_emit_text_but_releases_do_not() {
    let mut state = input();
    let mut native = character(KeyCode::KeyE, "é");
    assert_eq!(texts(&state.handle_key_input(native.clone())), ["é"]);
    assert!(state.is_key_pressed(native.physical_key));
    native.repeat = true;
    let events = state.handle_key_input(native.clone());
    assert!(matches!(&events[0], InputEvent::Key(key) if key.repeat && key.is_actionable()));
    assert_eq!(texts(&events), ["é"]);
    native.state = ElementState::Released;
    let events = state.handle_key_input(native.clone());
    assert!(texts(&events).is_empty());
    assert!(
        matches!(&events[0], InputEvent::Key(key) if key.state == KeyState::Released && !key.repeat)
    );
    assert!(!state.is_key_pressed(native.physical_key));
}

#[test]
fn synthetic_restore_tracks_keys_without_typing_or_triggering_actions() {
    let mut state = input();
    let mut native = character(KeyCode::KeyA, "a");
    native.synthetic = true;
    let events = state.handle_key_input(native.clone());
    assert!(state.is_key_pressed(native.physical_key));
    assert!(texts(&events).is_empty());
    assert!(matches!(&events[0], InputEvent::Key(key) if key.synthetic && !key.is_actionable()));
    native.synthetic = false;
    let events = state.handle_key_input(native);
    assert!(matches!(&events[0], InputEvent::Key(key) if !key.repeat && key.is_actionable()));
}

#[test]
fn shortcut_modifiers_do_not_insert_text_but_option_text_is_preserved() {
    for modifier in [ModifiersState::CONTROL, ModifiersState::SUPER] {
        let mut state = input();
        modifiers(&mut state, modifier);
        let events = state.handle_key_input(character(KeyCode::KeyS, "s"));
        assert!(texts(&events).is_empty());
        assert!(matches!(&events[0], InputEvent::Key(key) if key.is_actionable()));
    }
    let mut state = input();
    modifiers(&mut state, ModifiersState::ALT);
    assert_eq!(
        texts(&state.handle_key_input(character(KeyCode::KeyO, "ø"))),
        ["ø"]
    );
}

#[test]
fn altgraph_is_not_mistaken_for_a_control_shortcut() {
    let mut state = input();
    modifiers(&mut state, ModifiersState::CONTROL | ModifiersState::ALT);
    let mut alt_graph = key(KeyCode::AltRight, Key::Named(NamedKey::AltGraph), None);
    alt_graph.location = KeyLocation::Right;
    state.handle_key_input(alt_graph.clone());
    let events = state.handle_key_input(character(KeyCode::KeyQ, "@"));
    assert_eq!(texts(&events), ["@"]);
    assert!(matches!(&events[0], InputEvent::Key(key) if key.modifiers.alt_graph));
    alt_graph.state = ElementState::Released;
    state.handle_key_input(alt_graph);
    assert!(!state.modifiers().alt_graph);
}

#[test]
fn controls_are_keys_and_dead_keys_wait_for_real_text() {
    let mut state = input();
    for (code, named, text) in [
        (KeyCode::Enter, NamedKey::Enter, "\r"),
        (KeyCode::Tab, NamedKey::Tab, "\t"),
        (KeyCode::Backspace, NamedKey::Backspace, "\u{8}"),
        (KeyCode::Escape, NamedKey::Escape, "\u{1b}"),
    ] {
        assert!(
            texts(&state.handle_key_input(key(code, Key::Named(named), Some(text)))).is_empty()
        );
    }
    assert!(
        texts(&state.handle_key_input(key(KeyCode::Quote, Key::Dead(Some('´')), Some("´"))))
            .is_empty()
    );
    // Some layouts return two characters when the dead key cannot combine.
    assert_eq!(
        texts(&state.handle_key_input(character(KeyCode::KeyX, "´x"))),
        ["´x"]
    );
    assert_eq!(
        texts(&state.handle_key_input(character(KeyCode::KeyE, "e\u{301}👩\u{200d}💻"))),
        ["e\u{301}👩\u{200d}💻"]
    );
}

#[test]
fn named_and_unidentified_keys_are_forwarded_without_guessing_characters() {
    let mut state = input();
    let events = state.handle_key_input(key(KeyCode::F12, Key::Named(NamedKey::F12), None));
    assert!(
        matches!(&events[0], InputEvent::Key(key) if key.logical_key == Key::Named(NamedKey::F12))
    );
    let mut unknown = key(
        KeyCode::KeyA,
        Key::Unidentified(NativeKey::Unidentified),
        None,
    );
    unknown.physical_key = PhysicalKey::Unidentified(NativeKeyCode::Unidentified);
    unknown.key_without_modifiers = None;
    let events = state.handle_key_input(unknown);
    assert!(texts(&events).is_empty());
    assert!(
        matches!(&events[0], InputEvent::Key(key) if matches!(key.physical_key, PhysicalKey::Unidentified(_)))
    );
}

#[test]
fn ime_enable_does_not_disable_ordinary_typing_and_preedit_never_commits() {
    let mut state = input();
    assert_eq!(
        ime(&mut state, Ime::Enabled),
        [InputEvent::Ime(ImeEvent::Enabled)]
    );
    assert_eq!(
        texts(&state.handle_key_input(character(KeyCode::KeyA, "a"))),
        ["a"]
    );
    let events = ime(&mut state, Ime::Preedit("ni".into(), Some((2, 2))));
    assert!(texts(&events).is_empty());
    assert!(state.is_composing());
    assert_eq!(state.preedit().unwrap().text, "ni");
    let events =
        state.handle_key_input(key(KeyCode::Enter, Key::Named(NamedKey::Enter), Some("\r")));
    assert!(matches!(&events[0], InputEvent::Key(key) if key.composing && !key.is_actionable()));
    assert!(texts(&state.handle_key_input(character(KeyCode::KeyX, "x"))).is_empty());
    // Native clear followed by commit inserts the translated text exactly once.
    assert!(texts(&ime(&mut state, Ime::Preedit(String::new(), None))).is_empty());
    let events = ime(&mut state, Ime::Commit("你好".into()));
    assert_eq!(texts(&events), ["你好"]);
    assert!(matches!(&events[0], InputEvent::Text(text) if text.source == TextSource::Ime));
    assert!(!state.is_composing());
    assert_eq!(
        texts(&state.handle_key_input(character(KeyCode::KeyB, "b"))),
        ["b"]
    );
}

#[test]
fn ime_commit_clears_preedit_even_without_a_native_clear_and_keeps_utf8() {
    let mut state = input();
    ime(&mut state, Ime::Enabled);
    ime(&mut state, Ime::Preedit("e".into(), Some((1, 1))));
    modifiers(&mut state, ModifiersState::CONTROL);
    let events = ime(&mut state, Ime::Commit("é👩\u{200d}💻".into()));
    assert!(
        matches!(&events[0], InputEvent::Ime(ImeEvent::Preedit(preedit)) if preedit.text.is_empty())
    );
    assert_eq!(texts(&events), ["é👩\u{200d}💻"]);
    assert!(state.preedit().is_none());
}

#[test]
fn ime_cursor_ranges_are_byte_based_and_invalid_boundaries_hide_the_cursor() {
    let mut state = input();
    ime(&mut state, Ime::Enabled);
    for (cursor, expected) in [
        (Some((0, 3)), Some((0, 3))),
        (Some((4, 3)), Some((4, 3))),
        (Some((1, 3)), None),
        (Some((0, 99)), None),
        (None, None),
    ] {
        ime(&mut state, Ime::Preedit("你a".into(), cursor));
        assert_eq!(state.preedit().unwrap().cursor, expected);
    }
    let events = ime(&mut state, Ime::Disabled);
    assert!(events.contains(&InputEvent::Ime(ImeEvent::Cancelled)));
    assert!(!state.ime_enabled() && !state.is_composing());
    assert!(ime(&mut state, Ime::Commit("late".into())).is_empty());
}

#[test]
fn focus_loss_cancels_each_held_key_and_composition_then_ignores_late_text() {
    let mut state = input();
    for (code, location) in [
        (KeyCode::ShiftLeft, KeyLocation::Left),
        (KeyCode::ShiftRight, KeyLocation::Right),
    ] {
        let mut shift = key(code, Key::Named(NamedKey::Shift), None);
        shift.location = location;
        state.handle_key_input(shift);
    }
    modifiers(&mut state, ModifiersState::SHIFT);
    ime(&mut state, Ime::Enabled);
    ime(&mut state, Ime::Preedit("pending".into(), None));
    let events = state.handle_window_event(&WindowEvent::Focused(false), Instant::now());
    assert_eq!(events.iter().filter(|event| matches!(event, InputEvent::Key(key) if key.state == KeyState::Cancelled && !key.is_actionable())).count(), 2);
    assert!(events.contains(&InputEvent::Ime(ImeEvent::Cancelled)));
    assert_eq!(state.modifiers(), Modifiers::default());
    assert!(!state.is_key_pressed(PhysicalKey::Code(KeyCode::ShiftLeft)));
    assert!(!state.is_key_pressed(PhysicalKey::Code(KeyCode::ShiftRight)));
    assert!(
        state
            .handle_key_input(character(KeyCode::KeyA, "late"))
            .is_empty()
    );
    assert!(ime(&mut state, Ime::Commit("late".into())).is_empty());
    state.handle_window_event(&WindowEvent::Focused(true), Instant::now());
    assert_eq!(
        texts(&state.handle_key_input(character(KeyCode::KeyA, "a"))),
        ["a"]
    );
}

#[test]
fn resize_preserves_keyboard_and_composition_while_explicit_cancel_cleans_up() {
    let mut state = input();
    state.handle_key_input(character(KeyCode::KeyA, "a"));
    ime(&mut state, Ime::Enabled);
    ime(&mut state, Ime::Preedit("é".into(), Some((2, 2))));
    let events = state.set_geometry(
        CellMetrics {
            width: 20,
            height: 40,
            baseline: 30,
        },
        [200, 160],
        2.0,
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, InputEvent::Key(_) | InputEvent::Ime(_)))
    );
    assert!(state.is_key_pressed(PhysicalKey::Code(KeyCode::KeyA)));
    assert_eq!(state.preedit().unwrap().text, "é");
    let events = state.cancel();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, InputEvent::Key(key) if key.state == KeyState::Cancelled))
    );
    assert!(events.contains(&InputEvent::Ime(ImeEvent::Cancelled)));
    assert!(!state.is_composing());
}
