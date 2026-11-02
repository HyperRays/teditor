//! The terminal panel: draws a Ghostty session's cells and forwards input to it.
use super::clipboard;
use crate::{
    renderer::{CellRect, Graphic, char_columns, is_symbol},
    terminal::{
        InputEvent, Style,
        input::{Key, KeyEvent, MouseButton, PointerKind, TextSource},
    },
    ui::{Canvas, Cursor, Theme},
    vt::{CursorShape, MouseAction, Session, VtCell, Waker, line_editing_bytes},
};
use std::{io, path::Path};

pub struct Console {
    session: Session,
    size: [u16; 2],
    /// Text the last key already sent, so its matching text event is not repeated.
    sent_text: Option<String>,
    wheel: f64,
    selecting: bool,
    scrollbar_drag: bool,
    /// A press the program received, so its release and drags follow even if
    /// the program stops tracking in between.
    reporting: bool,
    last_pointer: Option<[u16; 2]>,
    shell_name: String,
}

impl Console {
    pub fn spawn(command: &[String], cwd: &Path, theme: Theme, waker: Waker) -> io::Result<Self> {
        let size = [80, 24];
        let session = Session::spawn(command, cwd, size, [theme.text, theme.desktop], waker)?;
        let shell_name = command
            .first()
            .and_then(|program| Path::new(program).file_name())
            .map_or_else(|| "terminal".into(), |n| n.to_string_lossy().into_owned());
        Ok(Self {
            session,
            size,
            sent_text: None,
            wheel: 0.0,
            selecting: false,
            scrollbar_drag: false,
            reporting: false,
            last_pointer: None,
            shell_name,
        })
    }
    /// Parse output, including while hidden. Returns whether more is ready.
    pub fn pump(&mut self) -> bool {
        self.session.pump()
    }
    pub fn exited(&self) -> bool {
        self.session.exited()
    }
    pub fn title(&self) -> String {
        let title = self.session.vt.title();
        if title.is_empty() {
            self.shell_name.clone()
        } else {
            title
        }
    }

    fn key(&mut self, key: &KeyEvent) -> Result<(), String> {
        // Cmd (or Ctrl+Shift elsewhere) belongs to the application, never the program.
        let shortcut = key.modifiers.super_key
            || (cfg!(not(target_os = "macos")) && key.modifiers.control && key.modifiers.shift);
        let moves_in_line = line_editing_bytes(key.physical_key, key.modifiers).is_some();
        if shortcut && !moves_in_line {
            let letter = match key
                .key_without_modifiers
                .as_ref()
                .unwrap_or(&key.logical_key)
            {
                Key::Character(text) => text.to_lowercase(),
                _ => String::new(),
            };
            match letter.as_str() {
                "c" => {
                    if let Some(text) = self.session.vt.selection_text() {
                        clipboard::write_with(|out| out.write_all(text.as_bytes()))
                            .map_err(|e| e.to_string())?;
                    }
                }
                "v" => {
                    let text = clipboard::read().map_err(|e| e.to_string())?;
                    let bytes = self.session.vt.encode_paste(&text);
                    self.session.vt.scroll_to_bottom();
                    self.session.send(bytes);
                }
                _ => {}
            }
            return Ok(());
        }
        let (bytes, text) = self.session.vt.encode_key(key);
        self.sent_text = text;
        if !bytes.is_empty() {
            self.session.vt.clear_selection();
            self.session.vt.scroll_to_bottom();
            self.session.send(bytes);
        }
        Ok(())
    }

    fn pointer(&mut self, canvas: &Canvas<'_>, event: &crate::terminal::input::PointerEvent) {
        let Some(position) = event.position else {
            return;
        };
        let [width, height] = canvas.size();
        let local = canvas.local(position.grid);
        let columns = width.saturating_sub(1);
        // The last column is the scrollback bar.
        match event.kind {
            PointerKind::Down(MouseButton::Left) if local[0] >= f64::from(columns) => {
                self.scrollbar_drag = true;
                self.scroll_to(local[1], height);
                return;
            }
            PointerKind::Move if self.scrollbar_drag => {
                self.scroll_to(local[1], height);
                return;
            }
            PointerKind::Up(MouseButton::Left) if self.scrollbar_drag => {
                self.scrollbar_drag = false;
                return;
            }
            _ => {}
        }
        let clamped = [
            local[0].clamp(0.0, f64::from(columns.max(1)) - 0.001),
            local[1].clamp(0.0, f64::from(height.max(1)) - 0.001),
        ];
        // Shift always selects, the usual escape hatch from a tracking program.
        let tracking = self.session.vt.mouse_tracking() && !event.modifiers.shift;
        let vt = &mut self.session.vt;
        let report = match event.kind {
            PointerKind::Down(button) if tracking => {
                self.reporting = button == MouseButton::Left;
                Some((MouseAction::Press, Some(button)))
            }
            PointerKind::Up(button) if tracking || self.reporting => {
                self.reporting = false;
                Some((MouseAction::Release, Some(button)))
            }
            PointerKind::Move if self.reporting => {
                Some((MouseAction::Motion, Some(MouseButton::Left)))
            }
            _ => None,
        };
        if let Some((action, button)) = report {
            let bytes = vt.encode_mouse(action, button, event.modifiers, clamped);
            self.session.send(bytes);
            return;
        }
        match event.kind {
            PointerKind::Down(MouseButton::Left) => {
                self.selecting = true;
                vt.selection_press(clamped);
            }
            PointerKind::Move if self.selecting => vt.selection_drag(clamped),
            PointerKind::Up(MouseButton::Left) if self.selecting => {
                self.selecting = false;
                vt.selection_release();
            }
            _ => {}
        }
    }

    fn scroll_to(&mut self, y: f64, height: u32) {
        let bar = self.session.vt.scrollbar();
        let fraction = (y / f64::from(height.max(1))).clamp(0.0, 1.0);
        let row = fraction * bar.total.saturating_sub(bar.visible) as f64;
        self.session.vt.scroll_to_row(row.round() as u64);
    }

    fn wheel(&mut self, canvas: &Canvas<'_>, event: &crate::terminal::input::ScrollEvent) {
        self.wheel -= event.cells[1];
        let lines = self.wheel.trunc() as isize;
        self.wheel -= lines as f64;
        if lines == 0 {
            return;
        }
        let position = event
            .position
            .map_or([0.0, 0.0], |position| canvas.local(position.grid));
        let vt = &mut self.session.vt;
        if vt.mouse_tracking() && !event.modifiers.shift {
            let bytes = vt.encode_wheel(lines, event.modifiers, position);
            self.session.send(bytes);
        } else if vt.wheel_sends_arrows() {
            let bytes = vt.encode_arrows(lines);
            self.session.send(bytes);
        } else {
            vt.scroll_lines(lines);
        }
    }

    pub fn draw(&mut self, canvas: &mut Canvas<'_>) -> Option<String> {
        let [width, height] = canvas.size();
        if width < 2 || height == 0 {
            return None;
        }
        let mut error = None;
        let size = [
            (width - 1).min(u32::from(u16::MAX)) as u16,
            height.min(u32::from(u16::MAX)) as u16,
        ];
        if size != self.size {
            self.size = size;
            if let Err(e) = self.session.resize(size) {
                error = Some(e.to_string());
            }
        }
        for event in std::mem::take(&mut canvas.events) {
            match &event {
                InputEvent::Key(key) => {
                    if let Err(e) = self.key(key) {
                        error = Some(e);
                    }
                }
                InputEvent::Text(text) => {
                    // A key press that produced this text has already sent it.
                    let repeated = text.source == TextSource::Keyboard
                        && self.sent_text.take().as_deref() == Some(text.text.as_str());
                    if !repeated {
                        self.session.vt.clear_selection();
                        self.session.vt.scroll_to_bottom();
                        self.session.send(text.text.clone().into_bytes());
                    }
                }
                InputEvent::Pointer(pointer) => self.pointer(canvas, pointer),
                InputEvent::Scroll(scroll) => self.wheel(canvas, scroll),
                _ => {}
            }
        }
        if !canvas.response.active {
            self.selecting = false;
            self.scrollbar_drag = false;
        }
        // Hover motion for programs tracking every movement.
        let hover = canvas.pointer();
        let tracking = self.session.vt.mouse_tracking();
        if let Some(point) = hover.filter(|_| tracking && !canvas.response.active) {
            let cell = [point[0] as u16, point[1] as u16];
            if self.last_pointer != Some(cell) && point[0] < f64::from(size[0]) {
                self.last_pointer = Some(cell);
                let bytes = self.session.vt.encode_mouse(
                    MouseAction::Motion,
                    None,
                    Default::default(),
                    point,
                );
                self.session.send(bytes);
            }
        }
        if hover.is_some() {
            canvas.set_cursor(
                if tracking || hover.is_some_and(|p| p[0] >= f64::from(size[0])) {
                    Cursor::Default
                } else {
                    Cursor::Text
                },
            );
        }
        if let Err(e) = self.session.vt.sync() {
            error = Some(e.to_string());
        }
        self.paint(canvas);
        error
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let [width, height] = canvas.size();
        let theme = canvas.theme;
        let vt = &self.session.vt;
        let [foreground, background] = vt.colors();
        canvas.fill(
            CellRect::new(0, 0, width, height),
            Style::new(foreground, background),
        );
        let focused = canvas.response.focused && canvas.window_focused;
        let cursor = vt.cursor();
        let selected = |cell: &VtCell| {
            if cell.selected {
                (theme.text, theme.hover)
            } else {
                (cell.foreground, cell.background)
            }
        };
        let mut underlines = Vec::new();
        let mut run = String::new();
        for (y, row) in vt.grid().iter().enumerate().take(height as usize) {
            let mut start = 0;
            let mut colors = None;
            run.clear();
            for (x, cell) in row.iter().enumerate() {
                let cell_colors = selected(cell);
                // Ghostty decides widths here, so anything not plainly one cell by both
                // measures is drawn by itself, outside the self-measuring text runs.
                if cell.columns != 1
                    || char_columns(cell.character) != 1
                    || is_symbol(cell.character)
                {
                    if let Some((fg, bg)) = colors.take() {
                        canvas.text(start as i32, y as i32, &run, Style::new(fg, bg));
                    }
                    run.clear();
                    if cell.columns > 0 {
                        canvas.glyph(
                            x as i32,
                            y as i32,
                            cell.character,
                            u32::from(cell.columns),
                            Style::new(cell_colors.0, cell_colors.1),
                        );
                    }
                } else {
                    if colors != Some(cell_colors) {
                        if let Some((fg, bg)) = colors {
                            canvas.text(start as i32, y as i32, &run, Style::new(fg, bg));
                        }
                        run.clear();
                        start = x;
                        colors = Some(cell_colors);
                    }
                    run.push(cell.character);
                }
                if cell.underline {
                    underlines.push((x as u32, y as u32, cell_colors.0));
                }
            }
            if let Some((fg, bg)) = colors {
                canvas.text(start as i32, y as i32, &run, Style::new(fg, bg));
            }
        }
        let mut cursor_graphic: Option<(CellRect, Graphic)> = None;
        if let Some(cursor) = cursor {
            let [x, y] = cursor.position.map(u32::from);
            let cell = vt
                .grid()
                .get(y as usize)
                .and_then(|row| row.get(x as usize))
                .copied();
            let rect = CellRect::new(x, y, 1, 1);
            let character = cell.map_or(' ', |cell| cell.character);
            let columns = cell.map_or(1, |cell| u32::from(cell.columns.max(1)));
            match cursor.shape {
                CursorShape::Block if focused => canvas.glyph(
                    x as i32,
                    y as i32,
                    character,
                    columns,
                    Style::new(theme.desktop, theme.accent),
                ),
                CursorShape::Bar if focused => {
                    cursor_graphic = Some((rect, Graphic::Selection(theme.accent)));
                }
                CursorShape::Underline if focused => {
                    cursor_graphic = Some((
                        rect,
                        Graphic::Divider {
                            vertical: false,
                            color: theme.accent,
                        },
                    ));
                }
                // Unfocused or hollow: mark the position without implying input goes here.
                _ => canvas.glyph(
                    x as i32,
                    y as i32,
                    character,
                    columns,
                    Style::new(cell.map_or(foreground, |c| c.foreground), theme.control),
                ),
            }
            canvas.caret(x, y);
            if let Some(preedit) = canvas.preedit.clone() {
                canvas.text(
                    x as i32,
                    y as i32,
                    &preedit.text,
                    Style::new(theme.accent, theme.control),
                );
            }
        }
        // Graphics last, since later text masks them.
        for (x, y, color) in underlines {
            canvas.graphic(
                CellRect::new(x, y, 1, 1),
                Graphic::Divider {
                    vertical: false,
                    color,
                },
            );
        }
        if let Some((rect, graphic)) = cursor_graphic {
            canvas.graphic(rect, graphic);
        }
        let bar = vt.scrollbar();
        if bar.total > bar.visible {
            let track = |value: u64| value.min(u64::from(u32::MAX)) as u32;
            canvas.graphic(
                CellRect::new(width - 1, 0, 1, height),
                Graphic::Scrollbar {
                    offset: track(bar.offset),
                    visible: track(bar.visible),
                    total: track(bar.total),
                    thumb: if self.scrollbar_drag {
                        theme.accent
                    } else {
                        theme.muted
                    },
                    track: theme.control,
                },
            );
        }
    }
}
