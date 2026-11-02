//! The display list handed to the renderer, and its color and rectangle types.

use std::sync::LazyLock;

/// An sRGB color with straight (unpremultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8, pub u8);

impl Color {
    /// An opaque color.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self(r, g, b, 255)
    }

    pub(super) fn linear(self) -> [f32; 4] {
        // Every drawn command needs this, so the sRGB curve is tabulated once.
        static LINEAR: LazyLock<[f32; 256]> = LazyLock::new(|| {
            std::array::from_fn(|value| {
                let value = value as f32 / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            })
        });
        [
            LINEAR[usize::from(self.0)],
            LINEAR[usize::from(self.1)],
            LINEAR[usize::from(self.2)],
            f32::from(self.3) / 255.0,
        ]
    }
}

/// A rectangle in whole cells, with an exclusive right and bottom edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRect {
    /// The left column.
    pub column: u32,
    /// The top row.
    pub row: u32,
    /// Width in cells.
    pub columns: u32,
    /// Height in cells.
    pub rows: u32,
}

impl CellRect {
    /// A rectangle at `(column, row)` of `columns` by `rows` cells.
    pub const fn new(column: u32, row: u32, columns: u32, rows: u32) -> Self {
        Self {
            column,
            row,
            columns,
            rows,
        }
    }

    /// The overlap of two rectangles, which may be empty.
    pub fn intersection(self, other: Self) -> Self {
        let column = self.column.max(other.column);
        let row = self.row.max(other.row);
        let right = self
            .column
            .saturating_add(self.columns)
            .min(other.column.saturating_add(other.columns));
        let bottom = self
            .row
            .saturating_add(self.rows)
            .min(other.row.saturating_add(other.rows));
        Self::new(
            column,
            row,
            right.saturating_sub(column),
            bottom.saturating_sub(row),
        )
    }
}

/// A raw bitmap uploaded to one renderer. Rebuilding the bitmap atlas invalidates
/// these handles; the terminal layer provides stable application asset IDs.
#[derive(Clone, Copy, Debug)]
pub struct Bitmap {
    pub(super) owner: u64,
    pub(super) generation: u64,
    pub(super) region: super::atlas::Region,
}

#[derive(Clone, Copy)]
pub(super) enum Content {
    Fill(Color),
    FillToEdge(Color),
    Glyph(char, Color),
    Bitmap(Bitmap, [f32; 4]),
    Graphic(super::Graphic),
}

pub(super) struct Command {
    pub rect: CellRect,
    pub clip: Option<CellRect>,
    pub content: Content,
}

/// An ordered display list. Later commands paint over earlier commands.
///
/// Coordinates are cells, never pixels. The renderer clips all commands to the
/// whole cells that fit in the target; any remaining pixels keep the clear color.
pub struct Frame {
    /// The color the frame starts from.
    pub clear: Color,
    pub(super) commands: Vec<Command>,
    clip: Option<CellRect>,
}

impl Frame {
    /// An empty frame.
    pub fn new(clear: Color) -> Self {
        Self {
            clear,
            commands: Vec::new(),
            clip: None,
        }
    }

    /// Set the cell-aligned clip for subsequent commands, or remove it with None.
    pub fn set_clip(&mut self, clip: Option<CellRect>) {
        self.clip = clip;
    }

    /// Fill a region with a color, which may be translucent.
    pub fn fill(&mut self, rect: CellRect, color: Color) {
        self.push(rect, Content::Fill(color));
    }

    /// Restore retained framebuffer padding along with the final cell row/column.
    pub(crate) fn fill_to_edge(&mut self, rect: CellRect, color: Color) {
        self.push(rect, Content::FillToEdge(color));
    }

    /// Draw antialiased artwork, scaled to the current physical cell metrics.
    pub fn graphic(&mut self, rect: CellRect, graphic: super::Graphic) {
        self.push(rect, Content::Graphic(graphic));
    }

    /// Draw a character at the font baseline, centered and clipped within its cells.
    /// The caller supplies the display width; this layer does not shape text.
    pub fn glyph(&mut self, column: u32, row: u32, character: char, columns: u32, color: Color) {
        self.push(
            CellRect::new(column, row, columns, 1),
            Content::Glyph(character, color),
        );
    }

    /// Convenience for a single line of single-cell characters. Control characters
    /// are skipped. Use `glyph` for explicit widths or preprocessed document text.
    pub fn text(&mut self, column: u32, row: u32, text: &str, color: Color) {
        for (offset, character) in text.chars().filter(|ch| !ch.is_control()).enumerate() {
            let Ok(offset) = u32::try_from(offset) else {
                break;
            };
            let Some(column) = column.checked_add(offset) else {
                break;
            };
            self.glyph(column, row, character, 1, color);
        }
    }

    /// Stretch an uploaded RGBA bitmap to a whole-cell region.
    pub fn bitmap(&mut self, rect: CellRect, bitmap: Bitmap) {
        self.bitmap_region(rect, bitmap, [0.0, 0.0, 1.0, 1.0]);
    }

    /// Draw a normalized source rectangle [x, y, width, height] from an image.
    /// Invalid or empty source rectangles are ignored.
    pub fn bitmap_region(&mut self, rect: CellRect, bitmap: Bitmap, source: [f32; 4]) {
        if source
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
            || source[2] == 0.0
            || source[3] == 0.0
            || source[0] + source[2] > 1.000001
            || source[1] + source[3] > 1.000001
        {
            return;
        }
        self.push(rect, Content::Bitmap(bitmap, source));
    }

    fn push(&mut self, rect: CellRect, content: Content) {
        if rect.columns > 0 && rect.rows > 0 {
            self.commands.push(Command {
                rect,
                clip: self.clip,
                content,
            });
        }
    }
}
