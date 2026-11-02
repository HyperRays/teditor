//! Routes each input event to the canvas it belongs to, using the previous frame's layout.
use super::*;
use crate::terminal::input::{
    ImeEvent, Key, Modifiers, MouseButton, NamedKey, PointerEvent, PointerKind, TextSource,
};

impl Context {
    pub(super) fn route(&mut self, events: &[InputEvent]) {
        for event in events {
            match event {
                InputEvent::Pointer(pointer) => self.route_pointer(pointer, event),
                InputEvent::Scroll(scroll) if !self.layout_changed => {
                    let point = scroll.position.filter(|p| p.cell.is_some());
                    if let Some(position) = point {
                        self.deliver_at(position.grid, event);
                    }
                }
                InputEvent::Key(key) if key.is_actionable() && self.window_focused => {
                    if let Some(id) = self.focus {
                        self.deliver(id, event);
                    } else if key.logical_key == Key::Named(NamedKey::Tab) && !key.repeat {
                        self.focus_first(key.modifiers.shift);
                    } else if key.logical_key == Key::Named(NamedKey::Escape) {
                        self.capture = None;
                    }
                }
                InputEvent::Text(text) if self.window_focused => {
                    let id = self.focus.filter(|id| {
                        self.previous
                            .iter()
                            .any(|hit| hit.id == *id && hit.kind == Kind::Canvas(true))
                    });
                    if let Some(id) = id
                        && (text.source != TextSource::Ime || !self.ime_blocked)
                    {
                        self.deliver(id, event);
                        if text.source == TextSource::Ime {
                            self.preedit = None;
                        }
                    }
                }
                InputEvent::Ime(ime) => match ime {
                    ImeEvent::Enabled => self.ime_blocked = false,
                    ImeEvent::Preedit(preedit) if !self.ime_blocked && self.window_focused => {
                        self.preedit = (!preedit.text.is_empty()).then(|| preedit.clone());
                    }
                    ImeEvent::Disabled | ImeEvent::Cancelled => self.preedit = None,
                    _ => {}
                },
                InputEvent::ModifiersChanged(modifiers) => self.modifiers = *modifiers,
                InputEvent::Focused(focused) => {
                    self.window_focused = *focused;
                    if !focused {
                        self.capture = None;
                        self.preedit = None;
                        self.pointer = None;
                        self.modifiers = Modifiers::default();
                    }
                }
                _ => {}
            }
        }
    }

    fn route_pointer(&mut self, pointer: &PointerEvent, event: &InputEvent) {
        let point = pointer.position.map(|position| position.grid);
        let inside = pointer
            .position
            .filter(|position| position.cell.is_some())
            .map(|position| position.grid);
        match pointer.kind {
            PointerKind::Move => {
                self.pointer = inside;
                if let Some(capture) = &self.capture
                    && matches!(capture.hit.kind, Kind::Canvas(_))
                {
                    self.deliver(capture.hit.id, event);
                }
                if let Some(point) = point {
                    self.drag(point);
                }
            }
            PointerKind::Leave => self.pointer = None,
            PointerKind::Down(MouseButton::Left) => {
                let Some(point) = inside else {
                    return;
                };
                self.pointer = Some(point);
                if !self.layout_changed {
                    self.press(point, event);
                }
            }
            PointerKind::Up(MouseButton::Left) => {
                if let Some(capture) = self.capture.take()
                    && matches!(capture.hit.kind, Kind::Canvas(_))
                {
                    self.deliver(capture.hit.id, event);
                }
            }
            PointerKind::Cancel {
                button: MouseButton::Left,
            } => self.capture = None,
            // Other buttons never capture, but canvases may forward them.
            PointerKind::Click {
                button: MouseButton::Left,
                count: 2..,
            }
            | PointerKind::Down(_)
            | PointerKind::Up(_) => {
                if let Some(point) = point {
                    self.deliver_at(point, event);
                }
            }
            _ => {} // Click/Drag events accompany Down/Move/Up; do not apply twice.
        }
    }

    /// A left press focuses the canvas under it and captures the pointer until release.
    fn press(&mut self, point: [f64; 2], event: &InputEvent) {
        let hit = self
            .hit_at(&self.previous, point)
            .cloned()
            .filter(|hit| hit.enabled);
        let on_divider = hit
            .as_ref()
            .is_some_and(|hit| matches!(hit.kind, Kind::Divider(_)));
        if !on_divider {
            self.active_pane = self.pane_at(point);
            let canvas = hit
                .as_ref()
                .filter(|hit| matches!(hit.kind, Kind::Canvas(_)));
            self.set_focus(canvas.map(|hit| hit.id));
        }
        self.capture = hit.filter(|hit| hit.kind != Kind::Pane).map(|hit| Capture {
            split_cut: self.splits.get(&hit.id).map(|split| split.cut()),
            origin: point,
            hit,
        });
        if let Some(capture) = &self.capture
            && matches!(capture.hit.kind, Kind::Canvas(_))
        {
            self.deliver(capture.hit.id, event);
        }
    }

    fn deliver(&mut self, id: Id, event: &InputEvent) {
        self.inbox.entry(id).or_default().push(event.clone());
    }

    /// Give an event to the enabled canvas under a point, if there is one.
    fn deliver_at(&mut self, point: [f64; 2], event: &InputEvent) {
        if let Some(hit) = self
            .hit_at(&self.previous, point)
            .filter(|hit| hit.enabled && matches!(hit.kind, Kind::Canvas(_)))
        {
            self.deliver(hit.id, event);
        }
    }

    /// With nothing focused, Tab focuses a canvas, preferring the active pane.
    fn focus_first(&mut self, backwards: bool) {
        let canvases = || {
            self.previous.iter().filter(|hit| {
                hit.enabled && matches!(hit.kind, Kind::Canvas(_)) && !hit.visible().empty()
            })
        };
        let in_pane = || canvases().filter(|hit| hit.pane == self.active_pane);
        let chosen = if backwards {
            in_pane().next_back().or_else(|| canvases().next_back())
        } else {
            in_pane().next().or_else(|| canvases().next())
        };
        let id = chosen.map(|hit| hit.id);
        self.set_focus(id);
    }

    /// Move the captured divider along with the pointer.
    fn drag(&mut self, point: [f64; 2]) {
        let Some(capture) = &self.capture else {
            return;
        };
        let Kind::Divider(axis) = capture.hit.kind else {
            return;
        };
        if let Some(split) = self.splits.get_mut(&capture.hit.id)
            && let Some(original) = capture.split_cut
        {
            let index = if axis == Axis::Columns { 0 } else { 1 };
            let delta = (point[index].floor() - capture.origin[index].floor())
                .clamp(-1_000_000.0, 1_000_000.0) as i64;
            let cut = split.clamp((i64::from(original) + delta).max(0) as u32);
            if cut != split.cut() && split.span() > 0 {
                split.fraction = f64::from(cut) / f64::from(split.span());
                self.layout_changed = true;
            }
        }
    }
}
