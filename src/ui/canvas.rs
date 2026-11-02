//! The application-drawn surface, and the footer layout that places canvases.

use super::*;
use crate::{
    renderer::Graphic,
    terminal::{BitmapId, input::Modifiers},
};

/// A clipped cell surface for application-owned widgets. Coordinates passed to
/// drawing methods are local; pointer events retain their absolute grid position.
/// Text canvases receive committed text and IME preedit. All canvases receive
/// keys (including Tab/Escape), captured pointer drags and wheel events.
pub struct Canvas<'a> {
    /// This canvas's interaction state.
    pub response: Response,
    /// Input routed here this frame, in order.
    pub events: Vec<InputEvent>,
    /// The IME composition in progress, while focused.
    pub preedit: Option<Preedit>,
    /// The context's colors.
    pub theme: Theme,
    /// Whether the window has focus.
    pub window_focused: bool,
    /// The modifier keys currently held.
    pub modifiers: Modifiers,
    ctx: &'a mut Context,
    rect: Rect,
    clip: Rect,
}

impl Ui<'_> {
    /// A canvas filling the layout's width, `height` rows tall.
    pub fn canvas(&mut self, key: impl Hash, height: u32, text_input: bool) -> Canvas<'_> {
        let rect = self.layout.allocate(height.min(self.available_height()));
        self.canvas_in(key, rect, text_input)
    }

    /// Draw artwork at explicit cell bounds, outside the layout and taking no input.
    pub fn graphic_at(&mut self, rect: CellRect, graphic: Graphic) {
        self.graphic(rect.into(), graphic);
    }

    /// A canvas at explicit cell bounds, outside the layout. Declared after other
    /// content, it paints above it and receives pointer input first.
    pub fn canvas_at(&mut self, key: impl Hash, rect: CellRect, text_input: bool) -> Canvas<'_> {
        self.canvas_in(key, rect.into(), text_input)
    }

    fn canvas_in(&mut self, key: impl Hash, rect: Rect, text_input: bool) -> Canvas<'_> {
        let id = self.id(key);
        assert!(
            self.ctx.seen.insert(id),
            "duplicate canvas ID; use a distinct key or container scope"
        );
        self.hit(id, self.pane, rect, Kind::Canvas(text_input));
        let clip = rect.intersect(self.clip);
        let focused = self.enabled && self.ctx.focus == Some(id);
        let response = Response {
            id,
            rect: clip.cells(),
            hovered: self.enabled && self.ctx.pointer.is_some_and(|point| clip.contains(point)),
            focused,
            active: self.enabled && self.captured(id),
        };
        Canvas {
            response,
            events: self.ctx.inbox.remove(&id).unwrap_or_default(),
            preedit: self.ctx.preedit.clone().filter(|_| focused),
            theme: self.ctx.theme,
            window_focused: self.ctx.window_focused,
            modifiers: self.ctx.modifiers,
            ctx: self.ctx,
            rect,
            clip,
        }
    }

    /// Reserve a fixed number of bottom rows. Both children cover the remaining
    /// region exactly; unlike a split, this adds no draggable divider or spacing.
    pub fn footer(
        &mut self,
        key: impl Hash,
        rows: u32,
        body: impl FnOnce(&mut Ui<'_>),
        footer: impl FnOnce(&mut Ui<'_>),
    ) {
        let rect = self.layout.allocate(self.available_height());
        let bottom = rows.min(rect.h);
        let scope = self.id(key);
        let (above, below) = (
            Rect::new(rect.x, rect.y, rect.w, rect.h - bottom),
            Rect::new(rect.x, rect.bottom() - bottom as i32, rect.w, bottom),
        );
        body(&mut self.child(scope.with("body"), self.pane, above));
        footer(&mut self.child(scope.with("footer"), self.pane, below));
    }
}

impl Canvas<'_> {
    /// The canvas's `[columns, rows]`.
    pub fn size(&self) -> [u32; 2] {
        [self.rect.w, self.rect.h]
    }
    /// Convert an absolute grid position, as events carry, to local cells.
    pub fn local(&self, point: [f64; 2]) -> [f64; 2] {
        [
            point[0] - f64::from(self.rect.x),
            point[1] - f64::from(self.rect.y),
        ]
    }
    /// The hovering pointer in local cells; absent while another widget has captured it.
    pub fn pointer(&self) -> Option<[f64; 2]> {
        let captured_elsewhere = self
            .ctx
            .capture
            .as_ref()
            .is_some_and(|capture| capture.hit.id != self.response.id);
        if !self.response.hovered || captured_elsewhere {
            return None;
        }
        self.ctx.pointer.map(|point| self.local(point))
    }
    /// Override this frame's native cursor for an application-drawn control.
    pub fn set_cursor(&mut self, cursor: Cursor) {
        self.ctx.cursor_override = Some(cursor);
    }
    /// Take keyboard focus and activate the containing pane.
    pub fn request_focus(&mut self) {
        if !self.clip.empty() {
            self.ctx.set_focus(Some(self.response.id));
            if let Some(pane) = self
                .ctx
                .hits
                .iter()
                .find(|hit| hit.id == self.response.id)
                .and_then(|hit| hit.pane)
            {
                self.ctx.active_pane = Some(pane);
            }
            self.response.focused = true;
        }
    }
    /// Fill a local rectangle with blanks of this style.
    pub fn fill(&mut self, rect: CellRect, style: Style) {
        let rect = self.region(rect).intersect(self.clip);
        if !rect.empty() {
            self.ctx
                .commands
                .push(Paint::Fill(rect, Cell::new(' ', style)));
        }
    }
    /// Draw a line of text, each character measured by [`char_columns`](crate::renderer::char_columns).
    pub fn text(&mut self, x: i32, y: i32, text: &str, style: Style) {
        self.ctx.commands.push(Paint::Text {
            origin: [self.rect.x.saturating_add(x), self.rect.y.saturating_add(y)],
            text: text.into(),
            style,
            clip: self.clip,
        });
    }
    /// One character across an explicit number of cells, for callers such as the
    /// terminal panel whose program has already decided how wide it is.
    pub fn glyph(&mut self, x: i32, y: i32, character: char, columns: u32, style: Style) {
        self.ctx.commands.push(Paint::Glyph {
            origin: [self.rect.x.saturating_add(x), self.rect.y.saturating_add(y)],
            character,
            columns,
            style,
            clip: self.clip,
        });
    }
    /// Stretch a bitmap over `size` cells at a local position. A negative position
    /// shows the part of the image that remains inside the canvas.
    pub fn image(&mut self, x: i32, y: i32, size: [u32; 2], bitmap: BitmapId) {
        self.ctx.commands.push(Paint::Image {
            bitmap,
            rect: Rect::new(
                self.rect.x.saturating_add(x),
                self.rect.y.saturating_add(y),
                size[0],
                size[1],
            ),
            clip: self.clip,
        });
    }
    /// Draw a shape above this canvas's cells. Later text and fills mask it.
    pub fn graphic(&mut self, rect: CellRect, graphic: Graphic) {
        self.ctx.commands.push(Paint::Graphic {
            rect: self.region(rect),
            graphic,
            clip: self.clip,
        });
    }
    /// Position the native IME candidate window at the application-drawn caret.
    pub fn caret(&mut self, x: u32, y: u32) {
        let rect = self.region(CellRect::new(x, y, 1, 1)).intersect(self.clip);
        if self.response.focused && self.window_focused && !rect.empty() {
            self.ctx.output.ime_cursor = Some(rect.cells());
        }
    }
    fn region(&self, rect: CellRect) -> Rect {
        Rect::new(
            self.rect.x.saturating_add(rect.column as i32),
            self.rect.y.saturating_add(rect.row as i32),
            rect.columns,
            rect.rows,
        )
    }
}
