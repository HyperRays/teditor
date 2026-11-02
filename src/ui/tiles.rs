//! Tiled panes and the draggable splits between them.

use super::*;
use crate::renderer::Graphic;

/// The direction a [`Split`] divides space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Side by side, with a vertical divider.
    Columns,
    /// Above and below, with a horizontal divider.
    Rows,
}

/// A persistent split of all available space. Its initial fraction applies only
/// when first declared; resizing the native window preserves the saved proportion.
#[derive(Clone, Copy, Debug)]
pub struct Split {
    /// Side by side or stacked.
    pub axis: Axis,
    /// The first child's share of the space when first declared.
    pub initial_fraction: f64,
    /// Minimum sizes along the split axis, in cells. In a smaller viewport these
    /// shrink proportionally so the children still cover the parent exactly.
    pub minimum: [u32; 2],
}
impl Split {
    /// Two children side by side, the first taking `initial_fraction` of the width.
    pub fn columns(initial_fraction: f64) -> Self {
        Self {
            axis: Axis::Columns,
            initial_fraction,
            minimum: [12, 12],
        }
    }
    /// Two children stacked, the first taking `initial_fraction` of the height.
    pub fn rows(initial_fraction: f64) -> Self {
        Self {
            axis: Axis::Rows,
            initial_fraction,
            minimum: [4, 4],
        }
    }
}

pub(super) struct SplitMemory {
    pub axis: Axis,
    pub fraction: f64,
    pub rect: Rect,
    pub minimum: [u32; 2],
}
impl SplitMemory {
    pub fn span(&self) -> u32 {
        match self.axis {
            Axis::Columns => self.rect.w,
            Axis::Rows => self.rect.h,
        }
    }
    pub fn clamp(&self, cut: u32) -> u32 {
        let span = self.span();
        if span < 2 {
            return span;
        }
        let [a, b] = self.minimum.map(|n| n.max(1));
        let total = u64::from(a) + u64::from(b);
        if total > u64::from(span) {
            return ((u64::from(span) * u64::from(a) / total) as u32).clamp(1, span - 1);
        }
        cut.clamp(a, span - b)
    }
    pub fn cut(&self) -> u32 {
        self.clamp((f64::from(self.span()) * self.fraction).round() as u32)
    }
}

impl Ui<'_> {
    /// Partition the remaining region without gaps. A divider is drawn on the
    /// shared edge, inside the first tile; it never consumes a gutter cell.
    pub fn split(
        &mut self,
        key: impl Hash,
        options: Split,
        first: impl FnOnce(&mut Ui<'_>),
        second: impl FnOnce(&mut Ui<'_>),
    ) {
        let id = self.id(key);
        assert!(self.ctx.seen.insert(id), "duplicate UI split ID");
        let rect = self.layout.allocate(self.available_height());
        let initial = if options.initial_fraction.is_finite() {
            options.initial_fraction.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let memory = self.ctx.splits.entry(id).or_insert(SplitMemory {
            axis: options.axis,
            fraction: initial,
            rect,
            minimum: options.minimum,
        });
        if memory.axis != options.axis {
            memory.axis = options.axis;
            memory.fraction = initial;
            self.ctx.capture.take_if(|capture| capture.hit.id == id);
        }
        memory.rect = rect;
        memory.minimum = options.minimum;
        let cut = memory.cut();
        let (a, b, divider) = match options.axis {
            Axis::Columns => (
                Rect::new(rect.x, rect.y, cut, rect.h),
                Rect::new(rect.x + cut as i32, rect.y, rect.w - cut, rect.h),
                Rect::new(
                    rect.x + cut.saturating_sub(1) as i32,
                    rect.y,
                    u32::from(cut > 0 && cut < rect.w),
                    rect.h,
                ),
            ),
            Axis::Rows => (
                Rect::new(rect.x, rect.y, rect.w, cut),
                Rect::new(rect.x, rect.y + cut as i32, rect.w, rect.h - cut),
                Rect::new(
                    rect.x,
                    rect.y + cut.saturating_sub(1) as i32,
                    rect.w,
                    u32::from(cut > 0 && cut < rect.h),
                ),
            ),
        };
        first(&mut self.child(id.with("first"), self.pane, a));
        second(&mut self.child(id.with("second"), self.pane, b));
        // The split's ID also identifies its divider; register after children so
        // the edge wins hit testing without raising or reordering any pane.
        let visible = divider.intersect(self.clip);
        self.hit(id, self.pane, divider, Kind::Divider(options.axis));
        let hovered = self
            .ctx
            .pointer
            .is_some_and(|point| visible.contains(point));
        if !visible.empty() {
            let highlighted = self.enabled && (self.captured(id) || hovered);
            self.graphic(
                divider,
                Graphic::Divider {
                    vertical: options.axis == Axis::Columns,
                    color: if highlighted {
                        self.ctx.theme.accent
                    } else {
                        self.ctx.theme.border
                    },
                },
            );
        }
    }

    /// Fill the remaining tile with a rectangular pane: a header above content that
    /// reaches its edges, with no exterior margin, position or stacking state. Split
    /// before declaring panes, since they cannot nest. Returns the pane's bounds.
    pub fn pane(
        &mut self,
        key: impl Hash,
        title: &str,
        build: impl FnOnce(&mut Ui<'_>),
    ) -> CellRect {
        assert!(
            self.pane.is_none(),
            "split tiles before declaring panes; panes cannot nest"
        );
        let id = Id::new(key);
        assert!(self.ctx.seen.insert(id), "duplicate UI pane ID");
        let rect = self.layout.allocate(self.available_height());
        self.ctx.panes.push(PaneMemory {
            id,
            rect: rect.intersect(self.clip),
        });
        let theme = self.ctx.theme;
        self.fill(rect, Style::new(theme.text, theme.panel));
        self.ctx.commands.push(Paint::Header {
            pane: id,
            rect: Rect::new(rect.x, rect.y, rect.w, rect.h.min(1)),
            title: title.into(),
            clip: self.clip,
        });
        self.hit(id, Some(id), rect, Kind::Pane);
        let content = Rect::new(rect.x, rect.y + 1, rect.w, rect.h.saturating_sub(1));
        build(&mut self.child(id, Some(id), content));
        rect.cells()
    }
}
