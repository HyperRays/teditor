//! The CPU framebuffer: cells, overlays, and the diff that finds what to repaint.

use super::BitmapId;
use crate::renderer::{CellRect, Color, Graphic, for_each_cluster};
use std::collections::BTreeSet;

const MAX_CELLS: usize = 1_000_000;

/// The colors of one cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    /// Text color.
    pub foreground: Color,
    /// Background color.
    pub background: Color,
}

impl Style {
    /// A style with these colors.
    pub const fn new(foreground: Color, background: Color) -> Self {
        Self {
            foreground,
            background,
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self {
            foreground: Color::rgb(220, 229, 238),
            background: Color::rgb(17, 23, 33),
        }
    }
}

/// One character position in a [`Framebuffer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The character shown, or [`CONTINUATION`](Self::CONTINUATION).
    pub character: char,
    /// Its colors.
    pub style: Style,
}

impl Cell {
    /// Marks the later cells of a wide character, which the first of them draws.
    /// Overwriting either half simply leaves the other drawing within its own cell.
    pub const CONTINUATION: char = '\0';

    /// A cell with this character and style.
    pub const fn new(character: char, style: Style) -> Self {
        Self { character, style }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::new(' ', Style::default())
    }
}

/// A bitmap positioned on the grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitmapPlacement {
    /// Which image to draw.
    pub bitmap: BitmapId,
    /// The cells the whole image maps onto.
    pub rect: CellRect,
    /// Crop without changing the image's destination rectangle or UV mapping.
    pub clip: CellRect,
    /// Source region measured in an arbitrary integer grid, enabling exact crops.
    pub source: CellRect,
    /// The size of the grid that `source` is measured in.
    pub source_size: [u32; 2],
}

/// A [`Graphic`] positioned on the grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphicPlacement {
    /// What to draw.
    pub graphic: Graphic,
    /// The cells the whole shape maps onto.
    pub rect: CellRect,
    /// The cells it may touch; a clipped shape is cropped, not rescaled.
    pub clip: CellRect,
}

/// Artwork drawn above the cells. Later overlays paint over earlier ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    /// An image.
    Bitmap(BitmapPlacement),
    /// A resolution-independent shape.
    Graphic(GraphicPlacement),
}

impl Overlay {
    /// The cells this overlay may touch.
    pub fn clip(&self) -> CellRect {
        match self {
            Self::Bitmap(placement) => placement.clip,
            Self::Graphic(placement) => placement.clip,
        }
    }

    pub(crate) fn clipped(&self, clip: CellRect) -> Self {
        match *self {
            Self::Bitmap(placement) => Self::Bitmap(BitmapPlacement { clip, ..placement }),
            Self::Graphic(placement) => Self::Graphic(GraphicPlacement { clip, ..placement }),
        }
    }
}

/// A framebuffer was asked to exceed its one-million-cell limit.
#[derive(Debug, thiserror::Error)]
#[error("a framebuffer may contain at most 1,000,000 cells")]
pub struct BufferTooLarge;

/// A complete screen description. Graphics and bitmaps are ordered overlays above the cells.
/// Text consists of grapheme clusters, each stored in the cell where it starts: see
/// [`symbol`](crate::renderer::symbol).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Framebuffer {
    size: [u32; 2],
    cells: Vec<Cell>,
    pub(super) overlays: Vec<Overlay>,
    backdrop: Color,
}

impl Framebuffer {
    /// A blank buffer of `[columns, rows]` over an opaque backdrop.
    pub fn new(size: [u32; 2], backdrop: Color) -> Result<Self, BufferTooLarge> {
        let len = cell_count(size)?;
        let backdrop = opaque(backdrop);
        let cell = Cell::new(
            ' ',
            Style {
                background: backdrop,
                ..Default::default()
            },
        );
        Ok(Self {
            size,
            cells: vec![cell; len],
            overlays: Vec::new(),
            backdrop,
        })
    }

    /// The `[columns, rows]` of the buffer.
    pub fn size(&self) -> [u32; 2] {
        self.size
    }
    /// The color behind every cell.
    pub fn backdrop(&self) -> Color {
        self.backdrop
    }
    /// The base of a terminal is opaque. Cell backgrounds can blend over this color.
    pub fn set_backdrop(&mut self, color: Color) {
        self.backdrop = opaque(color);
    }
    /// The whole buffer as a rectangle.
    pub fn bounds(&self) -> CellRect {
        CellRect::new(0, 0, self.size[0], self.size[1])
    }
    /// Everything drawn above the cells, in painting order.
    pub fn overlays(&self) -> &[Overlay] {
        &self.overlays
    }

    /// The placed bitmaps, in painting order.
    pub fn bitmaps(&self) -> impl Iterator<Item = &BitmapPlacement> {
        self.overlays.iter().filter_map(|overlay| match overlay {
            Overlay::Bitmap(placement) => Some(placement),
            Overlay::Graphic(_) => None,
        })
    }

    /// The placed graphics, in painting order.
    pub fn graphics(&self) -> impl Iterator<Item = &GraphicPlacement> {
        self.overlays.iter().filter_map(|overlay| match overlay {
            Overlay::Graphic(placement) => Some(placement),
            Overlay::Bitmap(_) => None,
        })
    }

    /// Add a graphic above everything placed so far.
    pub fn place_graphic(&mut self, graphic: Graphic, rect: CellRect, clip: CellRect) {
        let clip = clip.intersection(rect);
        if clip.columns > 0 && clip.rows > 0 {
            self.overlays.push(Overlay::Graphic(GraphicPlacement {
                graphic,
                rect,
                clip,
            }));
        }
    }

    /// Remove all graphics, keeping bitmaps.
    pub fn clear_graphics(&mut self) {
        self.overlays
            .retain(|overlay| matches!(overlay, Overlay::Bitmap(_)));
    }

    /// One row of cells, which must be within the buffer.
    pub(super) fn row(&self, row: u32) -> &[Cell] {
        let width = self.size[0] as usize;
        let start = row as usize * width;
        &self.cells[start..start + width]
    }

    /// The cell at a position, if it is inside the buffer.
    pub fn cell(&self, column: u32, row: u32) -> Option<&Cell> {
        self.index(column, row).map(|index| &self.cells[index])
    }

    /// Out-of-bounds writes are clipped and return false.
    pub fn set(&mut self, column: u32, row: u32, cell: Cell) -> bool {
        let Some(index) = self.index(column, row) else {
            return false;
        };
        self.cells[index] = cell;
        true
    }

    /// Clear the writable buffer, including its bitmap and graphic placements. This does not
    /// touch the front buffer or display anything until Terminal::render succeeds.
    pub fn clear(&mut self, cell: Cell) {
        self.cells.fill(cell);
        self.overlays.clear();
    }

    /// Set every cell in a rectangle, clipped to the buffer.
    pub fn fill(&mut self, rect: CellRect, cell: Cell) {
        let rect = rect.intersection(self.bounds());
        if rect.columns == 0 || rect.rows == 0 {
            return;
        }
        for row in rect.row..rect.row + rect.rows {
            let start = (row * self.size[0] + rect.column) as usize;
            self.cells[start..start + rect.columns as usize].fill(cell);
        }
    }

    /// Change colors without changing text, useful for selections and hover states.
    pub fn style(&mut self, rect: CellRect, style: Style) {
        let rect = rect.intersection(self.bounds());
        for row in rect.row..rect.row + rect.rows {
            for column in rect.column..rect.column + rect.columns {
                self.cells[(row * self.size[0] + column) as usize].style = style;
            }
        }
    }

    /// Positioned text with newlines, carriage returns, and four-column tab stops.
    /// Newlines return to the supplied starting column. Other controls are ignored.
    pub fn write(&mut self, column: u32, row: u32, text: &str, style: Style) {
        let (mut x, mut y) = (column, row);
        for line in text.split_inclusive(['\n', '\r', '\t']) {
            if y >= self.size[1] {
                break;
            }
            let (content, ending) = match line.chars().next_back() {
                Some(end @ ('\n' | '\r' | '\t')) => (&line[..line.len() - 1], Some(end)),
                _ => (line, None),
            };
            for_each_cluster(content, |character, columns| {
                if !character.is_control() && columns > 0 {
                    x = self.put(x, y, character, columns, style);
                }
            });
            match ending {
                Some('\n') => {
                    x = column;
                    y = y.saturating_add(1);
                }
                Some('\r') => x = column,
                Some('\t') => {
                    let end = x.saturating_add(4 - x % 4);
                    while x < end {
                        self.set(x, y, Cell::new(' ', style));
                        x += 1;
                    }
                }
                _ => {}
            }
        }
    }

    /// Write one character across `columns` cells, returning the next column. A wide
    /// character cut off by the edge leaves blanks rather than part of a glyph.
    pub fn put(
        &mut self,
        column: u32,
        row: u32,
        character: char,
        columns: u32,
        style: Style,
    ) -> u32 {
        let columns = columns.max(1);
        let next = column.saturating_add(columns);
        if next > self.size[0] && columns > 1 {
            for x in column..self.size[0] {
                self.set(x, row, Cell::new(' ', style));
            }
            return next;
        }
        self.set(column, row, Cell::new(character, style));
        for x in column.saturating_add(1)..next {
            self.set(x, row, Cell::new(Cell::CONTINUATION, style));
        }
        next
    }

    /// Add a whole bitmap above everything placed so far.
    pub fn place_bitmap(&mut self, bitmap: BitmapId, rect: CellRect) {
        self.place_bitmap_clipped(bitmap, rect, rect);
    }

    /// Add a whole bitmap that only shows inside `clip`, keeping its mapping onto `rect`.
    pub fn place_bitmap_clipped(&mut self, bitmap: BitmapId, rect: CellRect, clip: CellRect) {
        self.place_bitmap_region(
            bitmap,
            rect,
            clip,
            CellRect::new(0, 0, rect.columns, rect.rows),
            [rect.columns, rect.rows],
        );
    }

    /// Add part of a bitmap: `source` selects a region of a `source_size` grid over the image.
    pub fn place_bitmap_region(
        &mut self,
        bitmap: BitmapId,
        rect: CellRect,
        clip: CellRect,
        source: CellRect,
        source_size: [u32; 2],
    ) {
        if source_size.contains(&0)
            || source.columns == 0
            || source.rows == 0
            || u64::from(source.column) + u64::from(source.columns) > u64::from(source_size[0])
            || u64::from(source.row) + u64::from(source.rows) > u64::from(source_size[1])
        {
            return;
        }
        let clip = clip.intersection(rect);
        if rect.columns > 0 && rect.rows > 0 {
            self.overlays.push(Overlay::Bitmap(BitmapPlacement {
                bitmap,
                rect,
                clip,
                source,
                source_size,
            }));
        }
    }

    /// Remove all bitmaps, keeping graphics.
    pub fn clear_bitmaps(&mut self) {
        self.overlays
            .retain(|overlay| matches!(overlay, Overlay::Graphic(_)));
    }

    /// Scroll cells in a clipped region. Positive offsets move content right/down.
    /// Vacated cells use `fill`. Bitmap placements are managed separately.
    pub fn scroll(&mut self, rect: CellRect, columns: i32, rows: i32, fill: Cell) {
        let rect = rect.intersection(self.bounds());
        if rect.columns == 0 || rect.rows == 0 {
            return;
        }
        let mut original = Vec::with_capacity((rect.columns * rect.rows) as usize);
        for y in rect.row..rect.row + rect.rows {
            let start = (y * self.size[0] + rect.column) as usize;
            original.extend_from_slice(&self.cells[start..start + rect.columns as usize]);
        }
        for y in 0..rect.rows {
            for x in 0..rect.columns {
                let (sx, sy) = (
                    i64::from(x) - i64::from(columns),
                    i64::from(y) - i64::from(rows),
                );
                let cell = if sx >= 0
                    && sy >= 0
                    && sx < i64::from(rect.columns)
                    && sy < i64::from(rect.rows)
                {
                    original[(sy as u32 * rect.columns + sx as u32) as usize]
                } else {
                    fill
                };
                self.set(rect.column + x, rect.row + y, cell);
            }
        }
    }

    pub(super) fn resized(&self, size: [u32; 2]) -> Result<Self, BufferTooLarge> {
        let mut next = Self::new(size, self.backdrop)?;
        for row in 0..self.size[1].min(size[1]) {
            let width = self.size[0].min(size[0]) as usize;
            let old = (row * self.size[0]) as usize;
            let new = (row * size[0]) as usize;
            next.cells[new..new + width].copy_from_slice(&self.cells[old..old + width]);
        }
        next.overlays.clone_from(&self.overlays);
        Ok(next)
    }

    fn index(&self, column: u32, row: u32) -> Option<usize> {
        (column < self.size[0] && row < self.size[1])
            .then(|| (row * self.size[0] + column) as usize)
    }

    pub(super) fn damage(
        &self,
        previous: &Self,
        changed: &BTreeSet<BitmapId>,
        full: bool,
    ) -> Vec<CellRect> {
        if self.size.contains(&0) {
            return Vec::new();
        }
        if full || self.size != previous.size || self.backdrop != previous.backdrop {
            return vec![self.bounds()];
        }
        if self.cells == previous.cells && self.overlays == previous.overlays && changed.is_empty()
        {
            return Vec::new();
        }
        let mut dirty: Vec<bool> = self
            .cells
            .iter()
            .zip(&previous.cells)
            .map(|(a, b)| a != b)
            .collect();
        // A wide character repaints as one piece, whichever half changed.
        let width = self.size[0] as usize;
        let continues = |index: usize| {
            self.cells[index].character == Cell::CONTINUATION
                || previous.cells[index].character == Cell::CONTINUATION
        };
        let mut spread = |index: usize| {
            if !index.is_multiple_of(width)
                && continues(index)
                && (dirty[index] || dirty[index - 1])
            {
                dirty[index] = true;
                dirty[index - 1] = true;
            }
        };
        // Both directions, as a character can span more than two cells.
        (1..self.cells.len()).for_each(&mut spread);
        (1..self.cells.len()).rev().for_each(&mut spread);
        // Overlays paint in list order, so a difference at any index repairs both versions.
        for index in 0..self.overlays.len().max(previous.overlays.len()) {
            let (current, old) = (self.overlays.get(index), previous.overlays.get(index));
            for overlay in current.into_iter().chain(old) {
                let asset_changed = matches!(overlay, Overlay::Bitmap(placement)
                    if changed.contains(&placement.bitmap));
                if current != old || asset_changed {
                    let rect = overlay.clip().intersection(self.bounds());
                    for row in rect.row..rect.row + rect.rows {
                        let start = row as usize * width + rect.column as usize;
                        dirty[start..start + rect.columns as usize].fill(true);
                    }
                }
            }
        }
        let mut regions = Vec::new();
        for (row, flags) in dirty.chunks(width).enumerate() {
            let mut column = 0;
            for run in flags.chunk_by(|a, b| a == b) {
                if run[0] {
                    regions.push(CellRect::new(column, row as u32, run.len() as u32, 1));
                }
                column += run.len() as u32;
            }
        }
        regions
    }
}

fn opaque(color: Color) -> Color {
    Color::rgb(color.0, color.1, color.2)
}

fn cell_count([columns, rows]: [u32; 2]) -> Result<usize, BufferTooLarge> {
    let count = u64::from(columns) * u64::from(rows);
    if count > MAX_CELLS as u64 || columns > MAX_CELLS as u32 || rows > MAX_CELLS as u32 {
        return Err(BufferTooLarge);
    }
    Ok(count as usize)
}
