//! Generates block and box-drawing characters from the cell's own dimensions.

/// Coverage for a `width` × `height` cell, for characters drawn without the font.
pub(super) fn sprite(character: char, width: u32, height: u32) -> Option<Vec<u8>> {
    let code = character as u32;
    if !(0x2500..=0x259F).contains(&code) || width == 0 || height == 0 {
        return None;
    }
    let mut cell = Cell {
        width,
        height,
        pixels: vec![0; (width * height) as usize],
    };
    if code >= 0x2580 {
        cell.block(code);
    } else {
        cell.line(code);
    }
    Some(cell.pixels)
}

struct Cell {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Cell {
    fn fill(&mut self, [x0, y0, x1, y1]: [u32; 4], coverage: u8) {
        for y in y0..y1.min(self.height) {
            for x in x0..x1.min(self.width) {
                self.pixels[(y * self.width + x) as usize] = coverage;
            }
        }
    }
    fn rect(&mut self, rect: [u32; 4]) {
        self.fill(rect, 255);
    }

    fn block(&mut self, code: u32) {
        let [w, h] = [self.width, self.height];
        // Splits use the same rounding, so complementary blocks meet exactly.
        let (mid_x, mid_y) = (w / 2, h / 2);
        match code {
            0x2580 => self.rect([0, 0, w, mid_y]),
            0x2581..=0x2588 => {
                let eighths = code - 0x2580;
                let top = (h * (8 - eighths) / 8).min(h - 1);
                self.rect([0, top, w, h]);
            }
            0x2589..=0x258F => {
                let eighths = 0x2590 - code;
                self.rect([0, 0, (w * eighths / 8).max(1), h]);
            }
            0x2590 => self.rect([mid_x, 0, w, h]),
            0x2591 => self.fill([0, 0, w, h], 64),
            0x2592 => self.fill([0, 0, w, h], 128),
            0x2593 => self.fill([0, 0, w, h], 191),
            0x2594 => self.rect([0, 0, w, (h / 8).max(1)]),
            0x2595 => self.rect([w - (w / 8).max(1), 0, w, h]),
            _ => {
                const UL: u8 = 1;
                const UR: u8 = 2;
                const LL: u8 = 4;
                const LR: u8 = 8;
                let quadrants = match code {
                    0x2596 => LL,
                    0x2597 => LR,
                    0x2598 => UL,
                    0x2599 => UL | LL | LR,
                    0x259A => UL | LR,
                    0x259B => UL | UR | LL,
                    0x259C => UL | UR | LR,
                    0x259D => UR,
                    0x259E => UR | LL,
                    _ => UR | LL | LR,
                };
                for (bit, rect) in [
                    (UL, [0, 0, mid_x, mid_y]),
                    (UR, [mid_x, 0, w, mid_y]),
                    (LL, [0, mid_y, mid_x, h]),
                    (LR, [mid_x, mid_y, w, h]),
                ] {
                    if quadrants & bit != 0 {
                        self.rect(rect);
                    }
                }
            }
        }
    }

    fn line(&mut self, code: u32) {
        match code {
            0x2504..=0x250B | 0x254C..=0x254F => self.dashes(code),
            0x256D..=0x2570 => self.arc(code),
            0x2571..=0x2573 => self.diagonal(code),
            _ => {
                let arms = match code {
                    0x2500..=0x2503 => LINES[(code - 0x2500) as usize],
                    0x250C..=0x254B => LINES[(code - 0x250C + 4) as usize],
                    0x2550..=0x256C => DOUBLE[(code - 0x2550) as usize],
                    _ => HALF[(code - 0x2574) as usize],
                };
                let digit = |index: usize| arms.as_bytes()[index] - b'0';
                self.arms([digit(0), digit(1), digit(2), digit(3)]);
            }
        }
    }

    fn thickness(&self) -> [u32; 2] {
        let light = ((self.width + 3) / 6).max(1);
        [light, light * 2]
    }

    /// Draw lines from each edge to the middle. Weights are 0 none, 1 light,
    /// 2 heavy, 3 double, in the order up, right, down, left.
    fn arms(&mut self, [up, right, down, left]: [u8; 4]) {
        let [w, h] = [self.width, self.height];
        let [light, heavy] = self.thickness();
        let thick = |weight: u8| match weight {
            1 => light,
            2 => heavy,
            _ => 0,
        };
        // Double rails, each `light` wide and one `light` apart.
        let x1 = w.saturating_sub(3 * light) / 2;
        let x2 = x1 + 2 * light;
        let y1 = h.saturating_sub(3 * light) / 2;
        let y2 = y1 + 2 * light;
        let center_x = w.saturating_sub(light) / 2;
        let center_y = h.saturating_sub(light) / 2;
        // Single lines passing straight through double ones must not break at them.
        let vertical_through = up == 1 && down == 1;
        let horizontal_through = left == 1 && right == 1;
        let single_vertical = up == 1 || down == 1;
        let single_horizontal = left == 1 || right == 1;
        let width_across = thick(up).max(thick(down));
        let height_across = thick(left).max(thick(right));

        if right == 3 {
            let start = |double: bool| match (single_vertical, double) {
                (true, _) => center_x,
                (_, true) => x2,
                _ => x1,
            };
            self.rect([start(up == 3), y1, w, y1 + light]);
            self.rect([start(down == 3), y2, w, y2 + light]);
        } else if right != 0 {
            let start = if horizontal_through {
                w / 2
            } else if up == 3 && down == 3 {
                x2
            } else if up == 3 || down == 3 {
                x1
            } else if width_across > 0 {
                w.saturating_sub(width_across) / 2
            } else {
                w / 2
            };
            let top = h.saturating_sub(thick(right)) / 2;
            self.rect([start, top, w, top + thick(right)]);
        }
        if left == 3 {
            let end = |double: bool| match (single_vertical, double) {
                (true, _) => center_x + light,
                (_, true) => x1 + light,
                _ => x2 + light,
            };
            self.rect([0, y1, end(up == 3), y1 + light]);
            self.rect([0, y2, end(down == 3), y2 + light]);
        } else if left != 0 {
            let end = if horizontal_through {
                w / 2
            } else if up == 3 && down == 3 {
                x1 + light
            } else if up == 3 || down == 3 {
                x2 + light
            } else if width_across > 0 {
                w.saturating_sub(width_across) / 2 + width_across
            } else {
                w / 2
            };
            let top = h.saturating_sub(thick(left)) / 2;
            self.rect([0, top, end, top + thick(left)]);
        }
        if up == 3 {
            let end = |double: bool| match (single_horizontal, double) {
                (true, _) => center_y + light,
                (_, true) => y1 + light,
                _ => y2 + light,
            };
            self.rect([x1, 0, x1 + light, end(left == 3)]);
            self.rect([x2, 0, x2 + light, end(right == 3)]);
        } else if up != 0 {
            let end = if vertical_through {
                h / 2
            } else if left == 3 && right == 3 {
                y1 + light
            } else if left == 3 || right == 3 {
                y2 + light
            } else if height_across > 0 {
                h.saturating_sub(height_across) / 2 + height_across
            } else {
                h / 2
            };
            let start = w.saturating_sub(thick(up)) / 2;
            self.rect([start, 0, start + thick(up), end]);
        }
        if down == 3 {
            let start = |double: bool| match (single_horizontal, double) {
                (true, _) => center_y,
                (_, true) => y2,
                _ => y1,
            };
            self.rect([x1, start(left == 3), x1 + light, h]);
            self.rect([x2, start(right == 3), x2 + light, h]);
        } else if down != 0 {
            let top = if vertical_through {
                h / 2
            } else if left == 3 && right == 3 {
                y2
            } else if left == 3 || right == 3 {
                y1
            } else if height_across > 0 {
                h.saturating_sub(height_across) / 2
            } else {
                h / 2
            };
            let start = w.saturating_sub(thick(down)) / 2;
            self.rect([start, top, start + thick(down), h]);
        }
    }

    fn dashes(&mut self, code: u32) {
        let [light, heavy] = self.thickness();
        let (segments, vertical, is_heavy) = match code {
            0x2504 => (3, false, false),
            0x2505 => (3, false, true),
            0x2506 => (3, true, false),
            0x2507 => (3, true, true),
            0x2508 => (4, false, false),
            0x2509 => (4, false, true),
            0x250A => (4, true, false),
            0x250B => (4, true, true),
            0x254C => (2, false, false),
            0x254D => (2, false, true),
            0x254E => (2, true, false),
            _ => (2, true, true),
        };
        let thick = if is_heavy { heavy } else { light };
        let [w, h] = [self.width, self.height];
        let length = if vertical { h } else { w };
        for segment in 0..segments {
            let start = segment * length / segments;
            let end = (segment + 1) * length / segments;
            let gap = (end - start) / 3;
            let (start, end) = (start + gap / 2, end - (gap - gap / 2));
            self.rect(if vertical {
                let x = w.saturating_sub(thick) / 2;
                [x, start, x + thick, end]
            } else {
                let y = h.saturating_sub(thick) / 2;
                [start, y, end, y + thick]
            });
        }
    }

    /// The center of a light line, so curves meet the straight lines beside them.
    fn line_center(&self) -> [f32; 2] {
        let [light, _] = self.thickness();
        let half = light as f32 / 2.0;
        [
            (self.width.saturating_sub(light) / 2) as f32 + half,
            (self.height.saturating_sub(light) / 2) as f32 + half,
        ]
    }

    /// Fill from a distance to the line's middle, antialiased over one pixel.
    fn shade(&mut self, distance: impl Fn(f32, f32) -> f32) {
        let half = self.thickness()[0] as f32 / 2.0;
        for y in 0..self.height {
            for x in 0..self.width {
                let d = distance(x as f32 + 0.5, y as f32 + 0.5);
                let coverage = (half - d + 0.5).clamp(0.0, 1.0);
                self.pixels[(y * self.width + x) as usize] = (coverage * 255.0).round() as u8;
            }
        }
    }

    fn arc(&mut self, code: u32) {
        let (w, h) = (self.width as f32, self.height as f32);
        let [cx, cy] = self.line_center();
        // Which edges the arc joins: toward +1 (right, down) or -1 (left, up).
        let (sx, sy) = match code {
            0x256D => (1.0, 1.0),
            0x256E => (-1.0, 1.0),
            0x256F => (-1.0, -1.0),
            _ => (1.0, -1.0),
        };
        let reach_x = if sx > 0.0 { w - cx } else { cx };
        let reach_y = if sy > 0.0 { h - cy } else { cy };
        // Short of the edges, so the curve is straight again where the next cell joins.
        let radius = reach_x.min(reach_y) * 0.75;
        let (ax, ay) = (cx + sx * radius, cy + sy * radius);
        self.shade(|x, y| {
            let (dx, dy) = ((x - ax) * sx, (y - ay) * sy);
            if dx <= 0.0 && dy <= 0.0 {
                (dx.hypot(dy) - radius).abs()
            } else if dy > 0.0 {
                (x - cx).abs()
            } else {
                (y - cy).abs()
            }
        });
    }

    fn diagonal(&mut self, code: u32) {
        let (w, h) = (self.width as f32, self.height as f32);
        let length = w.hypot(h);
        let rising = move |x: f32, y: f32| ((x * h + y * w) - w * h).abs() / length;
        let falling = move |x: f32, y: f32| (x * h - y * w).abs() / length;
        match code {
            0x2571 => self.shade(rising),
            0x2572 => self.shade(falling),
            _ => self.shade(|x, y| rising(x, y).min(falling(x, y))),
        }
    }
}

/// Arm weights as "up right down left": U+2500–2503, then U+250C–254B.
const LINES: [&str; 68] = [
    "0101", "0202", "1010", "2020", "0110", "0210", "0120", "0220", "0011", "0012", "0021", "0022",
    "1100", "1200", "2100", "2200", "1001", "1002", "2001", "2002", "1110", "1210", "2110", "1120",
    "2120", "2210", "1220", "2220", "1011", "1012", "2011", "1021", "2021", "2012", "1022", "2022",
    "0111", "0112", "0211", "0212", "0121", "0122", "0221", "0222", "1101", "1102", "1201", "1202",
    "2101", "2102", "2201", "2202", "1111", "1112", "1211", "1212", "2111", "1121", "2121", "2112",
    "2211", "1122", "1221", "2212", "1222", "2122", "2221", "2222",
];
/// U+2550–256C.
const DOUBLE: [&str; 29] = [
    "0303", "3030", "0310", "0130", "0330", "0013", "0031", "0033", "1300", "3100", "3300", "1003",
    "3001", "3003", "1310", "3130", "3330", "1013", "3031", "3033", "0313", "0131", "0333", "1303",
    "3101", "3303", "1313", "3131", "3333",
];
/// U+2574–257F.
const HALF: [&str; 12] = [
    "0001", "1000", "0100", "0010", "0002", "2000", "0200", "0020", "0201", "1020", "0102", "2010",
];

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [[u32; 2]; 5] = [[10, 26], [20, 51], [30, 76], [13, 32], [3, 5]];

    fn at(pixels: &[u8], width: u32, x: u32, y: u32) -> u8 {
        pixels[(y * width + x) as usize]
    }

    #[test]
    fn covers_exactly_the_block_and_box_ranges_at_any_size() {
        for size in SIZES {
            for code in 0x2500..=0x259F {
                let character = char::from_u32(code).unwrap();
                let pixels = sprite(character, size[0], size[1]).unwrap();
                assert_eq!(pixels.len(), (size[0] * size[1]) as usize);
                assert!(pixels.iter().any(|p| *p > 0), "U+{code:04X} is empty");
            }
        }
        for outside in ['A', ' ', '\u{24FF}', '\u{25A0}', '─'] {
            assert_eq!(sprite(outside, 10, 20).is_some(), outside == '─');
        }
        assert!(sprite('█', 0, 20).is_none());
    }

    #[test]
    fn full_block_reaches_every_edge_and_halves_tile_without_overlap() {
        for [w, h] in SIZES {
            assert!(sprite('█', w, h).unwrap().iter().all(|p| *p == 255));
            for pair in [
                ('▀', '▄'),
                ('▌', '▐'),
                ('▘', '▟'),
                ('▝', '▙'),
                ('▖', '▜'),
                ('▗', '▛'),
            ] {
                let a = sprite(pair.0, w, h).unwrap();
                let b = sprite(pair.1, w, h).unwrap();
                for (a, b) in a.iter().zip(&b) {
                    assert_eq!(u16::from(*a) + u16::from(*b), 255, "{pair:?} at {w}x{h}");
                }
            }
        }
    }

    #[test]
    fn lines_reach_the_edges_they_name_and_only_those() {
        for [w, h] in [[10, 26], [20, 51], [30, 76]] {
            let touches = |character| {
                let p = sprite(character, w, h).unwrap();
                [
                    (0..w).any(|x| at(&p, w, x, 0) > 0),
                    (0..h).any(|y| at(&p, w, w - 1, y) > 0),
                    (0..w).any(|x| at(&p, w, x, h - 1) > 0),
                    (0..h).any(|y| at(&p, w, 0, y) > 0),
                ]
            };
            for (character, expected) in [
                ('─', [false, true, false, true]),
                ('│', [true, false, true, false]),
                ('├', [true, true, true, false]),
                ('└', [true, true, false, false]),
                ('┌', [false, true, true, false]),
                ('┐', [false, false, true, true]),
                ('┘', [true, false, false, true]),
                ('┼', [true, true, true, true]),
                ('┃', [true, false, true, false]),
                ('╔', [false, true, true, false]),
                ('╬', [true, true, true, true]),
                ('╭', [false, true, true, false]),
                ('╮', [false, false, true, true]),
                ('╯', [true, false, false, true]),
                ('╰', [true, true, false, false]),
                ('╴', [false, false, false, true]),
                ('╷', [false, false, true, false]),
            ] {
                assert_eq!(touches(character), expected, "{character} at {w}x{h}");
            }
        }
    }

    #[test]
    fn neighboring_lines_align_so_guides_join_across_cells() {
        for [w, h] in [[10, 26], [20, 51], [21, 51], [30, 76]] {
            let row = |character, y: u32| {
                let p = sprite(character, w, h).unwrap();
                (0..w).map(|x| at(&p, w, x, y)).collect::<Vec<_>>()
            };
            let column = |character, x: u32| {
                let p = sprite(character, w, h).unwrap();
                (0..h).map(|y| at(&p, w, x, y)).collect::<Vec<_>>()
            };
            // Bottom of one row meets the top of the next.
            for (above, below) in [('│', '│'), ('│', '├'), ('├', '└'), ('┌', '│'), ('╭', '╰')]
            {
                assert_eq!(row(above, h - 1), row(below, 0), "{above}{below}");
            }
            // Right edge of one column meets the left of the next.
            for (before, after) in [('─', '─'), ('├', '─'), ('└', '─'), ('╰', '─'), ('─', '╮')]
            {
                assert_eq!(column(before, w - 1), column(after, 0), "{before}{after}");
            }
        }
    }

    #[test]
    fn double_line_junctions_keep_their_inner_gap() {
        let [w, h] = [20, 51];
        let p = sprite('╔', w, h).unwrap();
        let light = (w + 3) / 6;
        let (x1, y1) = ((w - 3 * light) / 2, (h - 3 * light) / 2);
        let (x2, y2) = (x1 + 2 * light, y1 + 2 * light);
        assert_eq!(at(&p, w, x1, y1), 255, "outer corner");
        assert_eq!(at(&p, w, x2, y2), 255, "inner corner");
        assert_eq!(at(&p, w, x1, y2), 255, "outer rail continues down");
        assert_eq!(at(&p, w, x2, y1), 255, "outer rail continues right");
        assert_eq!(at(&p, w, x1 + light, y1 + light), 0, "gap between rails");
        // Straight double lines join themselves.
        let line = sprite('═', w, h).unwrap();
        assert!((0..w).all(|x| at(&line, w, x, y1) == 255));
        // A single line crossing a double one stays continuous.
        let cross = sprite('╪', w, h).unwrap();
        let center = (w - light) / 2;
        assert!((0..h).all(|y| at(&cross, w, center, y) == 255));
    }
}
