#[path = "terminal_demo_line.rs"]
mod line;
use teditor::{
    renderer::{CellRect, Color},
    terminal::{
        AssetError, BitmapId, Cell, InputEvent, Style, Terminal,
        input::{Key, MouseButton, NamedKey, PointerKind},
        read_png,
    },
};

const BACKGROUND: Color = Color::rgb(17, 23, 33);
const PANEL: Color = Color::rgb(27, 36, 49);
const FOREGROUND: Color = Color::rgb(220, 229, 238);
const MUTED: Color = Color::rgb(133, 153, 174);
const ACCENT: Color = Color::rgb(116, 222, 190);

enum Gesture {
    Selection { anchor: [u32; 2] },
    Bitmap { offset: [u32; 2] },
}

pub struct Demo {
    image: Option<BitmapId>,
    image_position: [u32; 2],
    scroll: [u32; 2],
    scroll_remainder: [f64; 2],
    selection: Option<([u32; 2], [u32; 2])>,
    gesture: Option<Gesture>,
    message: String,
    line: line::LineInput,
    text_focus: bool,
}

impl Demo {
    pub fn new(terminal: &mut Terminal) -> Result<Self, AssetError> {
        let pixels = bitmap_pixels();
        let image = terminal.upload_bitmap(192, 96, &pixels)?;
        let columns = terminal.back().size()[0];
        Ok(Self {
            image: Some(image),
            image_position: [columns.saturating_sub(28), 9],
            scroll: [0, 0],
            scroll_remainder: [0.0; 2],
            selection: None,
            gesture: None,
            message: "Ready. Drop a PNG to replace the image; other files report their path."
                .into(),
            line: line::LineInput::default(),
            text_focus: true,
        })
    }

    pub fn remove_image(&mut self, terminal: &mut Terminal) {
        if let Some(image) = self.image {
            match terminal.remove_bitmap(image) {
                Ok(()) => {
                    self.image = None;
                    self.gesture = None;
                    self.message = "Image removed; atlas storage reclaimed.".into();
                }
                Err(error) => self.message = error.to_string(),
            }
        }
    }

    pub fn handle(&mut self, event: InputEvent, terminal: &mut Terminal) {
        let bounds = list_bounds(terminal.back().size());
        match event {
            InputEvent::Text(text) if self.text_focus => self.line.insert(&text.text),
            InputEvent::Key(key) if key.is_actionable() => {
                let shortcut =
                    key.modifiers.super_key || (key.modifiers.control && !key.modifiers.alt_graph);
                let logical = key
                    .key_without_modifiers
                    .as_ref()
                    .unwrap_or(&key.logical_key);
                if self.text_focus
                    && shortcut
                    && matches!(logical, Key::Character(ch) if ch.eq_ignore_ascii_case("l"))
                {
                    self.line.clear();
                } else if !shortcut {
                    match key.logical_key {
                        Key::Named(NamedKey::Tab) if !key.repeat => {
                            self.text_focus = !self.text_focus
                        }
                        Key::Named(NamedKey::Enter) if self.text_focus => {
                            self.message = format!("Entered: {}", self.line.text)
                        }
                        Key::Named(named) if self.text_focus => self.line.key(named),
                        Key::Named(NamedKey::Delete) if !key.repeat => self.remove_image(terminal),
                        _ => {}
                    }
                }
            }
            InputEvent::Pointer(pointer) => match pointer.kind {
                PointerKind::Down(MouseButton::Left) => {
                    if let Some(cell) = pointer.position.and_then(|position| position.cell) {
                        self.text_focus = cell[1] == terminal.back().size()[1].saturating_sub(2);
                        if self.text_focus && !terminal.input().is_composing() {
                            self.line.click(cell[0], terminal.back().size());
                        }
                        if self.image.is_some() && contains(self.image_rect(), cell) {
                            self.gesture = Some(Gesture::Bitmap {
                                offset: [
                                    cell[0] - self.image_position[0],
                                    cell[1] - self.image_position[1],
                                ],
                            });
                        } else if contains(bounds, cell) {
                            let anchor = self.document_position(cell, bounds);
                            self.selection = Some((anchor, anchor));
                            self.gesture = Some(Gesture::Selection { anchor });
                        }
                    }
                }
                PointerKind::DragMove {
                    button: MouseButton::Left,
                    ..
                } => {
                    if let Some(position) = pointer.position {
                        match self.gesture {
                            Some(Gesture::Bitmap { offset }) => {
                                let size = terminal.back().size();
                                self.image_position =
                                    [
                                        ((position.grid[0].floor() as i64 - i64::from(offset[0]))
                                            .max(0)
                                            as u32)
                                            .min(size[0].saturating_sub(24)),
                                        ((position.grid[1].floor() as i64 - i64::from(offset[1]))
                                            .max(0)
                                            as u32)
                                            .min(size[1].saturating_sub(5)),
                                    ];
                            }
                            Some(Gesture::Selection { anchor })
                                if bounds.columns > 0 && bounds.rows > 0 =>
                            {
                                let cell = [
                                    position.grid[0]
                                        .max(f64::from(bounds.column))
                                        .min(f64::from(bounds.column + bounds.columns - 1))
                                        as u32,
                                    position.grid[1]
                                        .max(f64::from(bounds.row))
                                        .min(f64::from(bounds.row + bounds.rows - 1))
                                        as u32,
                                ];
                                self.selection =
                                    Some((anchor, self.document_position(cell, bounds)));
                            }
                            _ => {}
                        }
                    }
                }
                PointerKind::Click { button, count } => {
                    self.message = format!("{button:?} click ({count})");
                }
                PointerKind::Up(MouseButton::Left)
                | PointerKind::Cancel {
                    button: MouseButton::Left,
                } => self.gesture = None,
                _ => {}
            },
            InputEvent::Scroll(scroll) => {
                if scroll
                    .position
                    .and_then(|position| position.cell)
                    .is_some_and(|cell| contains(bounds, cell))
                {
                    let cells = if scroll.modifiers.shift && scroll.cells[0] == 0.0 {
                        [scroll.cells[1], 0.0]
                    } else {
                        scroll.cells
                    };
                    for (axis, delta) in cells.into_iter().enumerate() {
                        self.scroll_remainder[axis] -= delta;
                        let steps = self.scroll_remainder[axis].trunc() as i64;
                        self.scroll_remainder[axis] -= steps as f64;
                        let max = if axis == 0 {
                            100
                        } else {
                            200_u32.saturating_sub(bounds.rows)
                        };
                        self.scroll[axis] =
                            (i64::from(self.scroll[axis]) + steps).clamp(0, i64::from(max)) as u32;
                    }
                    self.message = format!(
                        "Scrolled to column {}, line {}",
                        self.scroll[0] + 1,
                        self.scroll[1] + 1
                    );
                }
            }
            InputEvent::FileHovered(path) => {
                self.message = format!("Drop to load: {}", path.display())
            }
            InputEvent::FileHoverCancelled => self.message = "File drop cancelled.".into(),
            InputEvent::FileDropped { path, .. } => {
                if path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
                {
                    let result = read_png(&path).and_then(|(size, pixels)| {
                        if let Some(image) = self.image {
                            terminal.update_bitmap(image, size[0], size[1], &pixels)
                        } else {
                            terminal
                                .upload_bitmap(size[0], size[1], &pixels)
                                .map(|image| self.image = Some(image))
                        }
                    });
                    self.message = match result {
                        Ok(()) => format!("Loaded {}", path.display()),
                        Err(error) => format!("Could not load image: {error}"),
                    };
                } else {
                    self.message = format!("Dropped file: {}", path.display());
                }
            }
            _ => {}
        }
    }

    pub fn compose(&self, terminal: &mut Terminal) {
        let preedit = terminal.input().preedit().cloned();
        let focused = terminal.input().focused();
        let pointer = terminal
            .input()
            .position()
            .and_then(|position| position.cell);
        let files_hovered = !terminal.input().hovered_files().is_empty();
        let assets = terminal.bitmap_count();
        let buffer = terminal.back_mut();
        let [columns, rows] = buffer.size();
        let normal = Style {
            foreground: FOREGROUND,
            background: BACKGROUND,
        };
        let muted = Style {
            foreground: MUTED,
            ..normal
        };
        buffer.clear(Cell::new(' ', normal));
        buffer.fill(
            CellRect::new(0, 0, columns, 3),
            Cell::new(
                ' ',
                Style {
                    background: PANEL,
                    ..normal
                },
            ),
        );
        buffer.write(
            2,
            1,
            "teditor / visual terminal",
            Style {
                foreground: ACCENT,
                background: PANEL,
            },
        );
        buffer.write(
            2,
            4,
            "Drag text to select. Drag the image to move it. Wheel: scroll.",
            muted,
        );
        buffer.write(
            2,
            5,
            "Tab: focus input. Ctrl/Cmd+L: clear input. Drop PNG: load image.",
            muted,
        );
        let bounds = list_bounds([columns, rows]);
        for y in 0..bounds.rows {
            let line = self.scroll[1] + y;
            let text = format!(
                "{:03}  All text and images share a fixed grid. Select this row with the mouse.",
                line + 1
            );
            let visible: String = text
                .chars()
                .skip(self.scroll[0] as usize)
                .take(bounds.columns as usize)
                .collect();
            buffer.write(bounds.column, bounds.row + y, &visible, normal);
            if let Some((a, b)) = self.selection {
                let (start, end) = if (a[1], a[0]) <= (b[1], b[0]) {
                    (a, b)
                } else {
                    (b, a)
                };
                for x in 0..bounds.columns {
                    let document = [self.scroll[0] + x, line];
                    if (document[1], document[0]) >= (start[1], start[0])
                        && (document[1], document[0]) <= (end[1], end[0])
                    {
                        buffer.style(
                            CellRect::new(bounds.column + x, bounds.row + y, 1, 1),
                            Style {
                                background: Color::rgb(45, 76, 87),
                                ..normal
                            },
                        );
                    }
                }
            }
        }
        if let Some(image) = self.image {
            buffer.place_bitmap(image, self.image_rect());
        }
        if rows >= 3 {
            buffer.write(
                2,
                rows - 3,
                &self
                    .message
                    .chars()
                    .take(columns.saturating_sub(4) as usize)
                    .collect::<String>(),
                Style {
                    foreground: if files_hovered { ACCENT } else { MUTED },
                    ..normal
                },
            );
            buffer.fill(
                CellRect::new(0, rows - 1, columns, 1),
                Cell::new(
                    ' ',
                    Style {
                        background: PANEL,
                        ..normal
                    },
                ),
            );
            let location =
                pointer.map_or_else(|| "outside grid".into(), |[x, y]| format!("cell {x},{y}"));
            buffer.write(
                2,
                rows - 1,
                &format!(
                    "{columns} x {rows}  |  {location}  |  {assets} bitmap asset(s)  |  Esc: close"
                ),
                Style {
                    foreground: MUTED,
                    background: PANEL,
                },
            );
        }
        self.line
            .paint(buffer, self.text_focus && focused, preedit.as_ref());
    }

    pub fn text_input_active(&self) -> bool {
        self.text_focus
    }

    pub fn ime_cursor_cells(&self, terminal: &Terminal) -> CellRect {
        self.line
            .caret_rect(terminal.back().size(), terminal.input().preedit())
    }

    fn image_rect(&self) -> CellRect {
        CellRect::new(self.image_position[0], self.image_position[1], 24, 5)
    }
    fn document_position(&self, cell: [u32; 2], bounds: CellRect) -> [u32; 2] {
        [
            cell[0] - bounds.column + self.scroll[0],
            cell[1] - bounds.row + self.scroll[1],
        ]
    }
}

fn list_bounds([columns, rows]: [u32; 2]) -> CellRect {
    CellRect::new(2, 7, columns.saturating_sub(4), rows.saturating_sub(11))
}

fn contains(rect: CellRect, [column, row]: [u32; 2]) -> bool {
    column >= rect.column
        && row >= rect.row
        && column < rect.column.saturating_add(rect.columns)
        && row < rect.row.saturating_add(rect.rows)
}

fn bitmap_pixels() -> Vec<u8> {
    let mut pixels = Vec::with_capacity(192 * 96 * 4);
    for y in 0..96 {
        for x in 0..192 {
            let wave = 48.0 + (x as f32 / 18.0).sin() * 26.0;
            let color = if (y as f32 - wave).abs() < 2.0 {
                [116, 222, 190, 255]
            } else if x % 24 == 0 || y % 24 == 0 {
                [47, 64, 80, 255]
            } else {
                [25, 34, 47, 255]
            };
            pixels.extend_from_slice(&color);
        }
    }
    pixels
}
