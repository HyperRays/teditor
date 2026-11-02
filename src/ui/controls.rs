//! Reusable controls drawn on a [`Canvas`]: click tracking, key/label chips and
//! a scrollbar. They keep their own state, so an application stores one per control.
use super::{Canvas, Cursor};
use crate::{
    renderer::{CellRect, Color, Graphic, text_columns},
    terminal::{
        InputEvent, Style,
        input::{MouseButton, PointerKind},
    },
};

/// The target of the first region containing a point, in local cell coordinates.
pub fn hit<T: Copy>(regions: &[(CellRect, T)], [x, y]: [f64; 2]) -> Option<T> {
    regions
        .iter()
        .find(|(rect, _)| {
            x >= f64::from(rect.column)
                && x < f64::from(rect.column + rect.columns)
                && y >= f64::from(rect.row)
                && y < f64::from(rect.row + rect.rows)
        })
        .map(|(_, target)| *target)
}

/// Press-and-release tracking: a click lands only when the pointer is released
/// over the region it pressed, so dragging away cancels it.
pub struct Clicks<T> {
    pressed: Option<T>,
}

impl<T> Default for Clicks<T> {
    fn default() -> Self {
        Self { pressed: None }
    }
}

impl<T: Copy + PartialEq> Clicks<T> {
    /// The region currently held down, for drawing its pressed state.
    pub fn pressed(&self) -> Option<T> {
        self.pressed
    }

    /// Process this frame's pointer events, returning the region that was clicked.
    pub fn update(&mut self, canvas: &Canvas<'_>, regions: &[(CellRect, T)]) -> Option<T> {
        let mut clicked = None;
        for event in &canvas.events {
            let InputEvent::Pointer(pointer) = event else {
                continue;
            };
            let target = pointer
                .position
                .and_then(|position| hit(regions, canvas.local(position.grid)));
            match pointer.kind {
                PointerKind::Down(MouseButton::Left) => self.pressed = target,
                PointerKind::Up(MouseButton::Left)
                    if self.pressed.take().is_some_and(|p| Some(p) == target) =>
                {
                    clicked = target;
                }
                _ => {}
            }
        }
        if !canvas.response.active {
            self.pressed = None;
        }
        clicked
    }
}

/// The emphasis of a [`Chip`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// An ordinary action.
    Plain,
    /// The action the current state suggests.
    Primary,
    /// An action that loses data.
    Danger,
}

/// A one-row button: an optional key cap followed by an optional label, so the
/// keyboard shortcut stays visible next to the click target.
#[derive(Clone, Copy)]
pub struct Chip<'a, T> {
    /// The shortcut drawn as a key cap; empty for none.
    pub keys: &'a str,
    /// The text after the key cap; empty for none.
    pub label: &'a str,
    /// How strongly the chip is drawn.
    pub tone: Tone,
    /// What a click on this chip reports.
    pub action: T,
}

impl<T> Chip<'_, T> {
    fn width(&self) -> u32 {
        let part = |text: &str| {
            if text.is_empty() {
                0
            } else {
                text.chars().count() as u32 + 2
            }
        };
        part(self.keys) + part(self.label)
    }
}

/// What an [`IconButton`] shows.
#[derive(Clone, Copy)]
pub enum Icon<'a> {
    /// Ordinary characters, such as `↑` or `Aa`, each in its own cells.
    Text(&'a str),
    /// One detailed symbol drawn [`ICON_COLUMNS`] wide, so that the detail survives.
    Wide(char),
}

/// Cells a [`Icon::Wide`] takes: one is too narrow to show much at usual sizes.
pub const ICON_COLUMNS: u32 = 2;

impl Icon<'_> {
    /// Cells it takes.
    pub fn columns(self) -> u32 {
        match self {
            Self::Text(text) => text_columns(text),
            Self::Wide(_) => ICON_COLUMNS,
        }
    }

    /// Draw it with its first cell at `[x, y]`.
    pub fn draw(self, canvas: &mut Canvas<'_>, x: i32, y: i32, style: Style) {
        match self {
            Self::Text(text) => canvas.text(x, y, text, style),
            Self::Wide(icon) => canvas.glyph(x, y, icon, ICON_COLUMNS, style),
        }
    }
}

/// A small button showing an icon, for bars where text labels would not fit.
#[derive(Clone, Copy)]
pub struct IconButton<'a, T> {
    /// What it shows.
    pub icon: Icon<'a>,
    /// What it does, for a caller to show while it is hovered.
    pub name: &'a str,
    /// A toggle that is on stays filled.
    pub on: bool,
    /// Empty cells before it, which group the buttons around it.
    pub gap: u32,
    /// How it reacts to the pointer: [`Tone::Danger`] warns of closing or losing something.
    pub tone: Tone,
    /// What a click reports.
    pub action: T,
}

impl<T> IconButton<'_, T> {
    fn width(&self) -> u32 {
        self.icon.columns() + 2
    }
}

/// Total width of a row of icon buttons, including their gaps.
pub fn icons_width<T>(icons: &[IconButton<'_, T>]) -> u32 {
    icons.iter().map(|icon| icon.gap + icon.width()).sum()
}

/// Draw a row of icon buttons from column `x` of the canvas's first row, over `background`.
/// Returns what was clicked, and the name of the one under the pointer.
pub fn draw_icons<'a, T: Copy + PartialEq>(
    canvas: &mut Canvas<'_>,
    clicks: &mut Clicks<T>,
    background: Color,
    mut x: u32,
    icons: &[IconButton<'a, T>],
) -> (Option<T>, Option<&'a str>) {
    let mut regions = Vec::with_capacity(icons.len());
    let mut columns = Vec::with_capacity(icons.len());
    for icon in icons {
        x += icon.gap;
        regions.push((CellRect::new(x, 0, icon.width(), 1), icon.action));
        columns.push(x);
        x += icon.width();
    }
    let clicked = clicks.update(canvas, &regions);
    let hovered = canvas.pointer().and_then(|point| hit(&regions, point));
    if hovered.is_some() {
        canvas.set_cursor(Cursor::Pointer);
    }
    let theme = canvas.theme;
    let mut name = None;
    for (icon, column) in icons.iter().zip(columns) {
        let over = hovered == Some(icon.action);
        let pressed = clicks.pressed() == Some(icon.action);
        if over {
            name = Some(icon.name);
        }
        let style = match icon.tone {
            Tone::Danger if over || pressed => Style::new(theme.desktop, theme.danger),
            _ if pressed || icon.on => Style::new(theme.desktop, theme.accent),
            _ if over => Style::new(theme.text, theme.hover),
            _ => Style::new(theme.muted, background),
        };
        canvas.fill(CellRect::new(column, 0, icon.width(), 1), style);
        icon.icon.draw(canvas, column as i32 + 1, 0, style);
    }
    (clicked, name)
}

/// Total width of a chip group, including the one-cell gaps between chips.
pub fn chips_width<T>(chips: &[Chip<'_, T>]) -> u32 {
    chips.iter().map(Chip::width).sum::<u32>() + chips.len().saturating_sub(1) as u32
}

/// Drop labels from keyed chips when a group does not fit; the keys stay clickable.
pub fn compact<'a, T: Copy>(chips: &[Chip<'a, T>], available: u32) -> Vec<Chip<'a, T>> {
    let mut chips = chips.to_vec();
    if chips_width(&chips) > available {
        for chip in chips.iter_mut().filter(|chip| !chip.keys.is_empty()) {
            chip.label = "";
        }
    }
    chips
}

/// Assign left edges to a group starting at `x`.
pub fn place<'a, T: Copy>(chips: &[Chip<'a, T>], mut x: u32) -> Vec<(u32, Chip<'a, T>)> {
    chips
        .iter()
        .map(|chip| {
            let placed = (x, *chip);
            x += chip.width() + 1;
            placed
        })
        .collect()
}

/// Route clicks to every chip on the canvas's first row and paint their states.
pub fn draw_chips<T: Copy + PartialEq>(
    canvas: &mut Canvas<'_>,
    clicks: &mut Clicks<T>,
    chips: &[(u32, Chip<'_, T>)],
) -> Option<T> {
    let regions: Vec<_> = chips
        .iter()
        .map(|(x, chip)| (CellRect::new(*x, 0, chip.width(), 1), chip.action))
        .collect();
    let clicked = clicks.update(canvas, &regions);
    let hovered = canvas.pointer().and_then(|point| hit(&regions, point));
    if hovered.is_some() {
        canvas.set_cursor(Cursor::Pointer);
    }
    let theme = canvas.theme;
    for (x, chip) in chips {
        let strong = match chip.tone {
            Tone::Plain | Tone::Primary => theme.accent,
            Tone::Danger => theme.danger,
        };
        let pressed = clicks.pressed() == Some(chip.action);
        let hover = hovered == Some(chip.action);
        let label = if pressed {
            Style::new(theme.desktop, strong)
        } else if hover {
            Style::new(theme.text, theme.hover)
        } else {
            Style::new(theme.text, theme.control)
        };
        let cap = match chip.tone {
            _ if pressed => Style::new(theme.desktop, strong),
            Tone::Plain if hover => Style::new(theme.accent, theme.hover),
            Tone::Plain => Style::new(theme.accent, theme.control),
            Tone::Primary | Tone::Danger => Style::new(theme.desktop, strong),
        };
        let mut column = *x;
        for (text, part) in [(chip.keys, cap), (chip.label, label)] {
            if !text.is_empty() {
                canvas.text(column as i32, 0, &format!(" {text} "), part);
                column += text.chars().count() as u32 + 2;
            }
        }
    }
    clicked
}

/// A vertical scroll position with a draggable scrollbar in the canvas's last column.
#[derive(Default)]
pub struct Scroll {
    /// The first visible row.
    pub offset: usize,
    /// The largest valid [`offset`](Self::offset), updated by [`event`](Self::event).
    pub maximum: usize,
    /// Columns between the scrollbar and the canvas's right edge.
    pub inset: u32,
    fraction: f64,
    grab: Option<f64>,
    bar_active: bool,
}

impl Scroll {
    /// Scroll just far enough to bring `row` into a viewport of `height` rows.
    pub fn reveal(&mut self, row: usize, height: usize) {
        if row < self.offset {
            self.offset = row;
        }
        if row >= self.offset + height.max(1) {
            self.offset = row.saturating_sub(height.saturating_sub(1));
        }
    }

    fn thumb(&self, height: u32, total: usize) -> (f64, f64) {
        let track = (f64::from(height) - 0.2).max(0.0);
        let length = (track * f64::from(height) / total.max(1) as f64)
            .max(0.75)
            .min(track);
        (
            0.1 + (track - length) * self.offset as f64 / self.maximum.max(1) as f64,
            length,
        )
    }

    /// Apply a wheel or scrollbar event over `total` rows. Returns whether it was consumed.
    pub fn event(&mut self, event: &InputEvent, canvas: &Canvas<'_>, total: usize) -> bool {
        let [width, height] = canvas.size();
        self.maximum = total.saturating_sub(height as usize);
        if !canvas.response.active {
            self.grab = None;
            self.bar_active = false;
        }
        match event {
            InputEvent::Scroll(scroll) => {
                self.fraction -= scroll.cells[1];
                let steps = self.fraction.trunc() as isize;
                self.fraction -= steps as f64;
                self.offset = self.offset.saturating_add_signed(steps).min(self.maximum);
                true
            }
            InputEvent::Pointer(p) => {
                let Some(point) = p.position.map(|p| canvas.local(p.grid)) else {
                    return false;
                };
                if matches!(p.kind, PointerKind::Down(MouseButton::Left))
                    && point[0] as u32 == width.saturating_sub(1 + self.inset)
                    && self.maximum > 0
                {
                    let (top, length) = self.thumb(height, total);
                    self.bar_active = true;
                    if point[1] >= top && point[1] < top + length {
                        self.grab = Some(point[1] - top);
                    } else if point[1] < top {
                        self.offset = self.offset.saturating_sub(height as usize);
                    } else {
                        self.offset = (self.offset + height as usize).min(self.maximum);
                    }
                    return true;
                }
                if self.bar_active {
                    if matches!(p.kind, PointerKind::Move)
                        && let Some(grab) = self.grab
                    {
                        let (_, length) = self.thumb(height, total);
                        let travel = (f64::from(height) - 0.2 - length).max(0.001);
                        self.offset = (((point[1] - grab - 0.1) / travel).clamp(0.0, 1.0)
                            * self.maximum as f64)
                            .round() as usize;
                    }
                    if matches!(p.kind, PointerKind::Up(MouseButton::Left)) {
                        self.bar_active = false;
                        self.grab = None;
                    }
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// Draw the scrollbar when the content does not fit.
    pub fn draw(&mut self, canvas: &mut Canvas<'_>, total: usize) {
        let [width, height] = canvas.size();
        if self.maximum > 0 && width > self.inset {
            // The model uses platform-sized offsets. Scale only the GPU's
            // scrollbar parameters instead of truncating files with >2^32 rows.
            let graphic_total = total.min(u32::MAX as usize) as u32;
            let graphic_offset = if total > u32::MAX as usize {
                (self.offset as f64 / self.maximum as f64
                    * f64::from(graphic_total.saturating_sub(height)))
                .round() as u32
            } else {
                self.offset as u32
            };
            canvas.graphic(
                CellRect::new(width - 1 - self.inset, 0, 1, height),
                Graphic::Scrollbar {
                    offset: graphic_offset,
                    visible: height,
                    total: graphic_total,
                    thumb: if self.bar_active {
                        canvas.theme.accent
                    } else {
                        canvas.theme.muted
                    },
                    track: canvas.theme.control,
                },
            );
        }
    }
}
