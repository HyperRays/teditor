// Original renderer scene, retained for GPU regression coverage.
use teditor::renderer::{Bitmap, CellRect, Color, Frame};

pub const BITMAP_WIDTH: u32 = 192;
pub const BITMAP_HEIGHT: u32 = 96;

pub fn bitmap_pixels() -> Vec<u8> {
    let mut pixels = Vec::with_capacity((BITMAP_WIDTH * BITMAP_HEIGHT * 4) as usize);
    for y in 0..BITMAP_HEIGHT {
        for x in 0..BITMAP_WIDTH {
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

pub fn frame(grid: [u32; 2], bitmap: Bitmap, hover: Option<[u32; 2]>) -> Frame {
    let [columns, rows] = grid;
    let background = Color::rgb(17, 23, 33);
    let panel = Color::rgb(27, 36, 49);
    let foreground = Color::rgb(220, 229, 238);
    let muted = Color::rgb(133, 153, 174);
    let accent = Color::rgb(116, 222, 190);
    let mut frame = Frame::new(background);
    if rows == 0 || columns == 0 {
        return frame;
    }
    frame.fill(CellRect::new(0, 0, columns, 3), panel);
    frame.text(2, 1, "teditor / cell renderer", accent);

    // Reserve the bottom row for status; small windows simply clip the sample.
    frame.set_clip(Some(CellRect::new(0, 3, columns, rows.saturating_sub(4))));
    frame.text(2, 4, "01  TEXT", muted);
    frame.text(
        2,
        6,
        "The quick brown fox jumps over the lazy dog.",
        foreground,
    );
    frame.text(2, 7, "ABCDEFGHIJKLMNOPQRSTUVWXYZ  0123456789", foreground);
    frame.text(2, 8, "abcdefghijklmnopqrstuvwxyz  []{}()<> /\\", foreground);
    frame.text(2, 9, "Symbols:  → ← ↑ ↓  ± × ÷  λ π  é ö ñ", accent);

    frame.text(2, 11, "02  CELL BACKGROUNDS / ORDERED OVERLAYS", muted);
    let colors = [
        Color::rgb(91, 148, 209),
        Color::rgb(116, 222, 190),
        Color::rgb(226, 183, 112),
        Color::rgb(203, 127, 151),
        Color::rgb(164, 146, 212),
    ];
    for (index, color) in colors.into_iter().enumerate() {
        let column = 2 + index as u32 * 8;
        frame.fill(CellRect::new(column, 13, 7, 2), color);
        frame.text(column + 1, 13, &format!("{:02}", index + 1), background);
    }

    frame.text(2, 17, "03  BITMAP / 24 x 5 CELLS", muted);
    frame.bitmap(CellRect::new(2, 19, 24, 5), bitmap);
    frame.text(29, 19, "Generated RGBA pixels", foreground);
    frame.text(29, 21, "Whole-cell placement", muted);
    frame.text(29, 22, "Whole-cell clipping", muted);

    frame.set_clip(None);
    frame.fill(CellRect::new(0, rows - 1, columns, 1), panel);
    let status = match hover {
        Some([column, row]) => format!(
            " {} x {} cells   pointer {},{}   Esc: close",
            columns, rows, column, row
        ),
        None => format!(
            " {} x {} cells   Move pointer to inspect cells   Esc: close",
            columns, rows
        ),
    };
    frame.text(1, rows - 1, &status, muted);
    if let Some([column, row]) = hover {
        frame.fill(CellRect::new(column, row, 1, 1), Color(116, 222, 190, 48));
    }
    frame
}
