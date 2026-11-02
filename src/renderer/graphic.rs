//! Resolution-independent shapes drawn by the shader.

use super::Color;

/// Resolution-independent artwork inside a whole-cell rectangle. These shapes
/// share the renderer's batching, clipping and premultiplied-alpha pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Graphic {
    /// A thin line on the right (vertical) or bottom (horizontal) cell edge.
    Divider {
        /// Whether the line runs top to bottom.
        vertical: bool,
        /// Line color.
        color: Color,
    },
    /// A proportional vertical scrollbar in a one-cell-wide track.
    Scrollbar {
        /// The first visible row.
        offset: u32,
        /// Rows in view.
        visible: u32,
        /// Rows in the content.
        total: u32,
        /// Thumb color.
        thumb: Color,
        /// Track color.
        track: Color,
    },
    /// Slim leading accent for a selected list row.
    Selection(Color),
}

impl Graphic {
    pub(super) fn parameters(self) -> [f32; 4] {
        match self {
            Self::Scrollbar {
                offset,
                visible,
                total,
                ..
            } => [offset as f32, visible as f32, total as f32, 0.0],
            _ => [0.0; 4],
        }
    }
    pub(super) fn instance(self) -> (u32, Color, Color) {
        let transparent = Color(0, 0, 0, 0);
        match self {
            Self::Divider { vertical, color } => {
                (if vertical { 12 } else { 13 }, color, transparent)
            }
            Self::Scrollbar { thumb, track, .. } => (10, thumb, track),
            Self::Selection(color) => (11, color, transparent),
        }
    }
}
