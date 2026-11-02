//! Normalized pointer, scroll and file events, and the state that produces them.

use crate::renderer::CellMetrics;
mod keyboard;
pub use keyboard::{
    ElementState, ImeCursorArea, ImeEvent, Key, KeyCode, KeyEvent, KeyInput, KeyLocation, KeyState,
    NamedKey, NativeKey, NativeKeyCode, PhysicalKey, Preedit, TextInput, TextSource,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use winit::event::WindowEvent;
pub use winit::event::{MouseButton, MouseScrollDelta, TouchPhase};

/// Which modifier keys are held.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Control.
    pub control: bool,
    /// Alt or Option.
    pub alt: bool,
    /// Command or the Windows key.
    pub super_key: bool,
    /// Layout AltGraph, or the desktop Right-Alt Ctrl+Alt representation.
    pub alt_graph: bool,
}

/// Where the pointer is, in both pixel and cell coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerPosition {
    /// Physical pixels from the window's top left.
    pub physical: [f64; 2],
    /// Fractional, possibly negative coordinates, useful for dragging/autoscroll.
    pub grid: [f64; 2],
    /// None outside the grid, including unused window edge pixels.
    pub cell: Option<[u32; 2]>,
}

/// What a pointer event reports.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerKind {
    /// The pointer entered the window.
    Enter,
    /// The pointer left the window.
    Leave,
    /// The pointer moved.
    Move,
    /// A button went down.
    Down(MouseButton),
    /// A button came up.
    Up(MouseButton),
    /// A press and release without a drag; repeated clicks count upward.
    Click {
        /// The button clicked.
        button: MouseButton,
        /// How many clicks in a row: 2 for a double-click.
        count: u8,
    },
    /// A held button moved past the drag threshold.
    DragStart {
        /// The button held.
        button: MouseButton,
        /// Where the press happened.
        origin: PointerPosition,
    },
    /// A drag continued.
    DragMove {
        /// The button held.
        button: MouseButton,
        /// Where the press happened.
        origin: PointerPosition,
    },
    /// A drag finished.
    DragEnd {
        /// The button held.
        button: MouseButton,
        /// Where the press happened.
        origin: PointerPosition,
    },
    /// A press ended without a release, such as on focus loss.
    Cancel {
        /// The button cancelled.
        button: MouseButton,
    },
}

/// A pointer event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerEvent {
    /// What happened.
    pub kind: PointerKind,
    /// Where; `None` if the position is unknown.
    pub position: Option<PointerPosition>,
    /// Modifiers held at the time.
    pub modifiers: Modifiers,
}

/// A wheel or trackpad scroll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollEvent {
    /// Preserve the platform units and sign; positive values move content right/down.
    pub delta: MouseScrollDelta,
    /// Line deltas map to cells, physical pixel deltas divide by cell dimensions.
    /// Fractional values are preserved; consumers decide how to accumulate them.
    pub cells: [f64; 2],
    /// The stage of a trackpad gesture.
    pub phase: TouchPhase,
    /// Where the pointer was.
    pub position: Option<PointerPosition>,
    /// Modifiers held at the time.
    pub modifiers: Modifiers,
}

/// A normalized input event, from [`InputState`].
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// A key changed state.
    Key(KeyEvent),
    /// Text to insert.
    Text(TextInput),
    /// An input method changed state.
    Ime(ImeEvent),
    /// The pointer did something.
    Pointer(PointerEvent),
    /// The wheel or trackpad scrolled.
    Scroll(ScrollEvent),
    /// The window gained or lost focus.
    Focused(bool),
    /// The set of held modifiers changed.
    ModifiersChanged(Modifiers),
    /// A file is being dragged over the window.
    FileHovered(PathBuf),
    /// The drag left without dropping.
    FileHoverCancelled,
    /// A file was dropped on the window.
    FileDropped {
        /// The dropped file.
        path: PathBuf,
        /// The latest cursor position, if available. Native file-drop events do
        /// not themselves carry coordinates, so this is not a guaranteed drop point.
        position: Option<PointerPosition>,
    },
}

struct Press {
    button: MouseButton,
    origin: Option<PointerPosition>,
    dragging: bool,
}

struct Click {
    button: MouseButton,
    position: PointerPosition,
    time: Instant,
    count: u8,
}

/// Keyboard, text/IME, and mouse interpretation. Focus loss and cancellation
/// release held keys/buttons and discard temporary composition. Geometry changes
/// cancel only pointer gestures; resizing must not interrupt typing or IME.
pub struct InputState {
    metrics: CellMetrics,
    viewport: [u32; 2],
    scale_factor: f64,
    physical: Option<[f64; 2]>,
    inside: bool,
    modifiers: Modifiers,
    presses: Vec<Press>,
    last_click: Option<Click>,
    hovered_files: Vec<PathBuf>,
    keyboard: keyboard::KeyboardState,
}

impl InputState {
    /// Track input for a grid of this geometry.
    pub fn new(metrics: CellMetrics, viewport: [u32; 2], scale_factor: f64) -> Self {
        assert!(
            metrics.width > 0
                && metrics.height > 0
                && scale_factor.is_finite()
                && scale_factor > 0.0
        );
        Self {
            metrics,
            viewport,
            scale_factor,
            physical: None,
            inside: false,
            modifiers: Modifiers::default(),
            presses: Vec::new(),
            last_click: None,
            hovered_files: Vec::new(),
            keyboard: keyboard::KeyboardState::default(),
        }
    }

    /// The pointer's last known position.
    pub fn position(&self) -> Option<PointerPosition> {
        self.physical.map(|physical| PointerPosition {
            physical,
            grid: [
                physical[0] / f64::from(self.metrics.width),
                physical[1] / f64::from(self.metrics.height),
            ],
            cell: if self.inside {
                self.metrics.cell_at(self.viewport, physical)
            } else {
                None
            },
        })
    }

    /// The modifiers currently held.
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }
    /// Whether a pointer button is held.
    pub fn is_pressed(&self, button: MouseButton) -> bool {
        self.presses.iter().any(|press| press.button == button)
    }
    /// Files currently being dragged over the window.
    pub fn hovered_files(&self) -> &[PathBuf] {
        &self.hovered_files
    }

    /// Whether a physical key is held.
    pub fn is_key_pressed(&self, key: PhysicalKey) -> bool {
        self.keyboard.is_pressed(key)
    }
    /// Whether the window has focus.
    pub fn focused(&self) -> bool {
        self.keyboard.focused
    }
    /// Whether an input method is active.
    pub fn ime_enabled(&self) -> bool {
        self.keyboard.ime_enabled
    }
    /// The composition in progress, if any.
    pub fn preedit(&self) -> Option<&Preedit> {
        self.keyboard.preedit.as_ref()
    }
    /// Whether a non-empty composition is in progress.
    pub fn is_composing(&self) -> bool {
        self.keyboard.is_composing()
    }

    /// Alternate-host/test entry point; native windows normally use handle_window_event.
    pub fn handle_key_input(&mut self, key: KeyInput) -> Vec<InputEvent> {
        self.keyboard.handle(key, &mut self.modifiers)
    }

    pub(super) fn set_geometry(
        &mut self,
        metrics: CellMetrics,
        viewport: [u32; 2],
        scale: f64,
    ) -> Vec<InputEvent> {
        let events = self.cancel_pointer();
        self.metrics = metrics;
        self.viewport = viewport;
        self.scale_factor = scale;
        events
    }

    /// Convert a native event. Supplying the timestamp makes click handling testable.
    pub fn handle_window_event(&mut self, event: &WindowEvent, now: Instant) -> Vec<InputEvent> {
        match event {
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => self.handle_key_input(KeyInput::from_winit(event, *is_synthetic)),
            WindowEvent::Ime(ime) => self.keyboard.ime(ime, self.modifiers),
            WindowEvent::CursorMoved { position, .. } => {
                self.move_pointer([position.x, position.y])
            }
            WindowEvent::CursorEntered { .. } => {
                self.inside = true;
                if self.presses.is_empty() {
                    self.physical = None;
                }
                vec![self.pointer(PointerKind::Enter)]
            }
            WindowEvent::CursorLeft { .. } => {
                self.inside = false;
                self.last_click = None;
                vec![self.pointer(PointerKind::Leave)]
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.button(*button, *state == ElementState::Pressed, now)
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                let cells = match delta {
                    MouseScrollDelta::LineDelta(x, y) => [f64::from(*x), f64::from(*y)],
                    MouseScrollDelta::PixelDelta(p) => [
                        p.x / f64::from(self.metrics.width),
                        p.y / f64::from(self.metrics.height),
                    ],
                };
                if cells.iter().any(|value| !value.is_finite()) {
                    return Vec::new();
                }
                vec![InputEvent::Scroll(ScrollEvent {
                    delta: *delta,
                    cells,
                    phase: *phase,
                    position: self.position(),
                    modifiers: self.modifiers,
                })]
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                self.modifiers = Modifiers {
                    shift: state.shift_key(),
                    control: state.control_key(),
                    alt: state.alt_key(),
                    super_key: state.super_key(),
                    alt_graph: false,
                };
                self.modifiers.alt_graph = self.keyboard.alt_graph(self.modifiers);
                vec![InputEvent::ModifiersChanged(self.modifiers)]
            }
            WindowEvent::Focused(focused) => {
                self.keyboard.focused = *focused;
                let mut events = if !focused {
                    let mut events = self.cancel();
                    self.physical = None;
                    self.inside = false;
                    if !self.hovered_files.is_empty() {
                        self.hovered_files.clear();
                        events.push(InputEvent::FileHoverCancelled);
                    }
                    events
                } else {
                    Vec::new()
                };
                events.push(InputEvent::Focused(*focused));
                events
            }
            WindowEvent::HoveredFile(path) => {
                if !self.hovered_files.contains(path) {
                    self.hovered_files.push(path.clone());
                }
                vec![InputEvent::FileHovered(path.clone())]
            }
            WindowEvent::HoveredFileCancelled => {
                self.hovered_files.clear();
                vec![InputEvent::FileHoverCancelled]
            }
            WindowEvent::DroppedFile(path) => {
                self.hovered_files.retain(|hover| hover != path);
                vec![InputEvent::FileDropped {
                    path: path.clone(),
                    position: self.position(),
                }]
            }
            WindowEvent::Destroyed => self.cancel(),
            _ => Vec::new(),
        }
    }

    /// Call on suspension, focus transfer to another input target, or destruction.
    /// Use cancel_pointer for loss of mouse capture alone.
    pub fn cancel(&mut self) -> Vec<InputEvent> {
        let mut events = self.cancel_pointer();
        events.extend(self.keyboard.cancel(self.modifiers));
        self.modifiers = Modifiers::default();
        events.push(InputEvent::ModifiersChanged(self.modifiers));
        events
    }

    /// Cancel pointer gestures only, leaving keyboard and IME state alone.
    pub fn cancel_pointer(&mut self) -> Vec<InputEvent> {
        let buttons: Vec<_> = self.presses.drain(..).map(|press| press.button).collect();
        self.last_click = None;
        buttons
            .into_iter()
            .map(|button| self.pointer(PointerKind::Cancel { button }))
            .collect()
    }

    fn move_pointer(&mut self, physical: [f64; 2]) -> Vec<InputEvent> {
        if physical.iter().any(|value| !value.is_finite()) {
            return Vec::new();
        }
        self.physical = Some(physical);
        self.inside = physical[0] >= 0.0
            && physical[1] >= 0.0
            && physical[0] < f64::from(self.viewport[0])
            && physical[1] < f64::from(self.viewport[1]);
        let mut kinds = vec![PointerKind::Move];
        for press in &mut self.presses {
            let Some(origin) = press.origin.filter(|origin| origin.cell.is_some()) else {
                continue;
            };
            if !press.dragging && distance(physical, origin.physical) >= 4.0 * self.scale_factor {
                press.dragging = true;
                self.last_click = None;
                kinds.push(PointerKind::DragStart {
                    button: press.button,
                    origin,
                });
            }
            if press.dragging {
                kinds.push(PointerKind::DragMove {
                    button: press.button,
                    origin,
                });
            }
        }
        kinds.into_iter().map(|kind| self.pointer(kind)).collect()
    }

    fn button(&mut self, button: MouseButton, down: bool, now: Instant) -> Vec<InputEvent> {
        let position = self.position();
        if down {
            if self.is_pressed(button) {
                return Vec::new();
            }
            self.presses.push(Press {
                button,
                origin: position,
                dragging: false,
            });
            return vec![self.pointer(PointerKind::Down(button))];
        }
        let mut events = vec![self.pointer(PointerKind::Up(button))];
        let Some(index) = self.presses.iter().position(|press| press.button == button) else {
            return events;
        };
        let press = self.presses.remove(index);
        if press.dragging {
            if let Some(origin) = press.origin {
                events.push(self.pointer(PointerKind::DragEnd { button, origin }));
            }
        } else if let (Some(origin), Some(position)) = (press.origin, position)
            && origin.cell.is_some()
            && origin.cell == position.cell
            && distance(origin.physical, position.physical) < 4.0 * self.scale_factor
        {
            let count = self
                .last_click
                .as_ref()
                .filter(|click| {
                    click.button == button
                        && click.position.cell == position.cell
                        && now.saturating_duration_since(click.time) <= Duration::from_millis(500)
                        && distance(click.position.physical, position.physical)
                            < 4.0 * self.scale_factor
                })
                .map_or(1, |click| click.count % 3 + 1);
            self.last_click = Some(Click {
                button,
                position,
                time: now,
                count,
            });
            events.push(self.pointer(PointerKind::Click { button, count }));
        } else {
            self.last_click = None;
        }
        events
    }

    fn pointer(&self, kind: PointerKind) -> InputEvent {
        InputEvent::Pointer(PointerEvent {
            kind,
            position: self.position(),
            modifiers: self.modifiers,
        })
    }
}

fn distance(a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}
