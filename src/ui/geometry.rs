//! Signed cell rectangles, for content partly outside its container.

use crate::renderer::CellRect;

/// Signed cell coordinates allow a scrolled child's origin to be offscreen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(self) -> i32 {
        self.x.saturating_add(self.w.min(i32::MAX as u32) as i32)
    }
    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.h.min(i32::MAX as u32) as i32)
    }
    pub fn intersect(self, other: Self) -> Self {
        let (x, y) = (self.x.max(other.x), self.y.max(other.y));
        Self::new(
            x,
            y,
            self.right().min(other.right()).saturating_sub(x).max(0) as u32,
            self.bottom().min(other.bottom()).saturating_sub(y).max(0) as u32,
        )
    }
    pub fn contains(self, [x, y]: [f64; 2]) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.right())
            && y < f64::from(self.bottom())
    }
    pub fn cells(self) -> CellRect {
        let rect = self.intersect(Self::new(0, 0, i32::MAX as u32, i32::MAX as u32));
        CellRect::new(rect.x.max(0) as u32, rect.y.max(0) as u32, rect.w, rect.h)
    }
    pub fn empty(self) -> bool {
        self.w == 0 || self.h == 0
    }
}

impl From<CellRect> for Rect {
    fn from(rect: CellRect) -> Self {
        Self::new(
            rect.column.min(i32::MAX as u32) as i32,
            rect.row.min(i32::MAX as u32) as i32,
            rect.columns,
            rect.rows,
        )
    }
}

pub(super) fn subtract(rect: CellRect, cover: CellRect) -> Vec<CellRect> {
    let overlap = rect.intersection(cover);
    if overlap.columns == 0 || overlap.rows == 0 {
        return vec![rect];
    }
    let mut result = Vec::with_capacity(4);
    let right = rect.column + rect.columns;
    let bottom = rect.row + rect.rows;
    for part in [
        CellRect::new(rect.column, rect.row, rect.columns, overlap.row - rect.row),
        CellRect::new(
            rect.column,
            overlap.row + overlap.rows,
            rect.columns,
            bottom - overlap.row - overlap.rows,
        ),
        CellRect::new(
            rect.column,
            overlap.row,
            overlap.column - rect.column,
            overlap.rows,
        ),
        CellRect::new(
            overlap.column + overlap.columns,
            overlap.row,
            right - overlap.column - overlap.columns,
            overlap.rows,
        ),
    ] {
        if part.columns > 0 && part.rows > 0 {
            result.push(part);
        }
    }
    result
}
