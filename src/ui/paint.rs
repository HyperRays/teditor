//! Replays a frame's paint commands into the framebuffer, masking overlays beneath text.

use super::{
    Id,
    geometry::{Rect, subtract},
};
use crate::{
    renderer::{CellRect, Graphic, for_each_cluster, text_columns},
    terminal::{BitmapId, BitmapPlacement, Cell, Framebuffer, GraphicPlacement, Overlay, Style},
};

pub(super) enum Paint {
    Fill(Rect, Cell),
    Text {
        origin: [i32; 2],
        text: String,
        style: Style,
        clip: Rect,
    },
    /// One character with an explicit width, for callers that measure their own text.
    Glyph {
        origin: [i32; 2],
        character: char,
        columns: u32,
        style: Style,
        clip: Rect,
    },
    Image {
        bitmap: BitmapId,
        rect: Rect,
        clip: Rect,
    },
    Graphic {
        graphic: Graphic,
        rect: Rect,
        clip: Rect,
    },
    Header {
        pane: Id,
        rect: Rect,
        title: String,
        clip: Rect,
    },
}

pub(super) fn compose(
    frame: &mut Framebuffer,
    commands: &[Paint],
    active_pane: Option<Id>,
    theme: super::Theme,
) {
    frame.set_backdrop(theme.panel);
    frame.clear(Cell::new(
        ' ',
        Style {
            background: theme.panel,
            ..Default::default()
        },
    ));
    let mut overlays = Vec::<Overlay>::new();
    for command in commands {
        match command {
            Paint::Fill(rect, cell) => {
                let rect = rect.cells().intersection(frame.bounds());
                mask(&mut overlays, rect);
                frame.fill(rect, *cell);
            }
            Paint::Text {
                origin,
                text,
                style,
                clip,
            } => {
                for (y, line) in text.split('\n').enumerate() {
                    let row = origin[1].saturating_add(y as i32);
                    let width = text_columns(line);
                    let bounds = Rect::new(origin[0], row, width, 1)
                        .intersect(*clip)
                        .cells()
                        .intersection(frame.bounds());
                    mask(&mut overlays, bounds);
                    let mut x = origin[0];
                    for_each_cluster(line, |character, columns| {
                        if columns > 0 {
                            put(frame, [x, row], character, columns, *style, *clip);
                        }
                        x = x.saturating_add(columns as i32);
                    });
                }
            }
            Paint::Glyph {
                origin,
                character,
                columns,
                style,
                clip,
            } => {
                let bounds = Rect::new(origin[0], origin[1], *columns, 1)
                    .intersect(*clip)
                    .cells()
                    .intersection(frame.bounds());
                mask(&mut overlays, bounds);
                put(frame, *origin, *character, *columns, *style, *clip);
            }
            Paint::Image { bitmap, rect, clip } => {
                let visible = rect.intersect(*clip).cells().intersection(frame.bounds());
                if visible.columns > 0 && visible.rows > 0 {
                    let destination = rect.cells();
                    overlays.push(Overlay::Bitmap(BitmapPlacement {
                        bitmap: *bitmap,
                        rect: destination,
                        clip: visible,
                        source: crate::renderer::CellRect::new(
                            rect.x.saturating_neg().max(0) as u32,
                            rect.y.saturating_neg().max(0) as u32,
                            destination.columns,
                            destination.rows,
                        ),
                        source_size: [rect.w, rect.h],
                    }));
                }
            }
            Paint::Graphic {
                graphic,
                rect,
                clip,
            } => {
                let clip = rect.intersect(*clip).cells().intersection(frame.bounds());
                if clip.columns > 0 && clip.rows > 0 {
                    overlays.push(Overlay::Graphic(GraphicPlacement {
                        graphic: *graphic,
                        rect: rect.cells(),
                        clip,
                    }));
                }
            }
            Paint::Header {
                pane,
                rect,
                title,
                clip,
            } => {
                let visible = rect.intersect(*clip).cells().intersection(frame.bounds());
                if visible.columns == 0 || visible.rows == 0 {
                    continue;
                }
                let active = active_pane == Some(*pane);
                let style = Style {
                    foreground: if active { theme.text } else { theme.muted },
                    background: theme.title,
                };
                mask(&mut overlays, visible);
                frame.fill(visible, Cell::new(' ', style));
                let room = rect.w.saturating_sub(2);
                let mut x = 0;
                for_each_cluster(title, |character, columns| {
                    if columns > 0 && x + columns <= room {
                        let position = [rect.x + 1 + x as i32, rect.y];
                        put(
                            frame,
                            position,
                            character,
                            columns,
                            style,
                            rect.intersect(*clip),
                        );
                    }
                    x = x.saturating_add(columns);
                });
                overlays.push(Overlay::Graphic(GraphicPlacement {
                    graphic: Graphic::Divider {
                        vertical: false,
                        color: if active {
                            theme.active_border
                        } else {
                            theme.border
                        },
                    },
                    rect: rect.cells(),
                    clip: visible,
                }));
            }
        }
    }
    for overlay in overlays {
        match overlay {
            Overlay::Bitmap(image) => frame.place_bitmap_region(
                image.bitmap,
                image.rect,
                image.clip,
                image.source,
                image.source_size,
            ),
            Overlay::Graphic(p) => frame.place_graphic(p.graphic, p.rect, p.clip),
        }
    }
}

/// Write a character if it is visible. A wide character that is partly clipped
/// leaves blanks in the part that remains, rather than half a glyph.
fn put(
    frame: &mut Framebuffer,
    [x, y]: [i32; 2],
    character: char,
    columns: u32,
    style: Style,
    clip: Rect,
) {
    let visible = |x: i32| {
        let rect = Rect::new(x, y, 1, 1).intersect(clip);
        !rect.empty()
            && x >= 0
            && y >= 0
            && (x as u32) < frame.size()[0]
            && (y as u32) < frame.size()[1]
    };
    let cells = || (0..columns as i32).map(|offset| x.saturating_add(offset));
    if cells().all(visible) {
        frame.put(x as u32, y as u32, character, columns, style);
        return;
    }
    let shown: Vec<_> = cells().filter(|&x| visible(x)).collect();
    for x in shown {
        frame.set(x as u32, y as u32, Cell::new(' ', style));
    }
}

fn mask(overlays: &mut Vec<Overlay>, cover: CellRect) {
    let covered = |overlay: &Overlay| {
        let overlap = overlay.clip().intersection(cover);
        overlap.columns > 0 && overlap.rows > 0
    };
    // Most text and fills touch no overlay; only rebuild the list for those that do.
    if !overlays.iter().any(covered) {
        return;
    }
    *overlays = overlays
        .drain(..)
        .flat_map(|overlay| {
            subtract(overlay.clip(), cover)
                .into_iter()
                .map(move |clip| overlay.clipped(clip))
        })
        .collect();
}
