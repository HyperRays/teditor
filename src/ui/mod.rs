//! Immediate-mode, cell-aligned UI. Declare tiled panes and canvases every frame;
//! application values stay with the caller, while [`Context`] retains interaction state.
//!
//! This layer is independent of the GPU and native window system. Keep one `Context`,
//! collect normalized [`InputEvent`]s, and call [`Context::run`] to rebuild a framebuffer.
//!
//! ```
//! use teditor::{
//!     renderer::Color,
//!     terminal::{Framebuffer, Style},
//!     ui::{Context, Split},
//! };
//!
//! let mut context = Context::new(); // retain across frames
//! let mut frame = Framebuffer::new([80, 30], Color::rgb(27, 36, 49)).unwrap();
//! let output = context.run(&mut frame, &[], |ui| {
//!     ui.split(
//!         "workspace",
//!         Split::columns(0.25),
//!         |left| {
//!             left.pane("files", "Documents", |ui| {
//!                 let height = ui.available_height();
//!                 let mut canvas = ui.canvas("list", height, false);
//!                 canvas.text(0, 0, "main.rs", Style::default());
//!             });
//!         },
//!         |right| {
//!             right.pane("editor", "Editor", |ui| {
//!                 let height = ui.available_height();
//!                 ui.canvas("text", height, true);
//!             });
//!         },
//!     );
//! });
//! // Apply output.cursor and output.ime_cursor to the native window, then render.
//! # let _ = output;
//! ```
//!
//! # Layout
//!
//! All dimensions are cells. [`Ui::split`] partitions all remaining space into two adjacent
//! regions; nest splits, then call [`Ui::pane`] in each leaf. There are no exterior margins,
//! floating positions or shadows. Shared dividers are thin lines with a one-cell hit strip
//! inside the first tile, so they reserve no gutter. [`Ui::footer`] reserves bottom rows
//! with no divider. Split fractions survive window resizing.
//!
//! # Canvases and controls
//!
//! Everything inside a pane is application-drawn on a [`Canvas`]: a clipped cell surface
//! that receives the input routed to it. [`controls`] has what canvases commonly need:
//! [`Clicks`](controls::Clicks), [`Chip`](controls::Chip) buttons and a
//! [`Scroll`](controls::Scroll) bar.
//!
//! # Input rules
//!
//! Events are consumed once against the previous completed layout, so newly declared
//! canvases start receiving input on the following frame. Pointer capture survives leaving
//! a canvas or divider; resizing, focus loss and disappearance cancel it. After a divider
//! moves, new presses wait for the rebuilt layout rather than act on old positions.
//!
//! Declare visible UI every frame, with a stable key per canvas within its container:
//! omitted canvases and splits lose their state, and duplicate IDs in a frame panic.
//! On [`Output::reset_ime`], turn native IME off before enabling it for the new target,
//! so late commits cannot cross between inputs.

mod canvas;
pub mod controls;
mod geometry;
mod input;
mod paint;
mod tiles;

use crate::{
    renderer::{CellRect, Color},
    terminal::{
        Cell, Framebuffer, InputEvent, Style,
        input::{Modifiers, Preedit},
    },
};
pub use canvas::Canvas;
use geometry::Rect;
use paint::Paint;
use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
};
pub use tiles::{Axis, Split};

/// A stable identity for a pane or canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id(u64);
impl Id {
    /// The identity for a key.
    pub fn new(value: impl Hash) -> Self {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hash);
        Self(hash.finish())
    }
    /// A child identity, scoped under this one.
    pub fn with(self, value: impl Hash) -> Self {
        Self::new((self.0, value))
    }
}

/// The interface's colors.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Behind drawers and lists.
    pub desktop: Color,
    /// Behind content.
    pub panel: Color,
    /// Behind bars and lists that open over content, darker so that their edges show.
    pub popup: Color,
    /// Ordinary text.
    pub text: Color,
    /// Secondary text.
    pub muted: Color,
    /// Text that should recede, such as code comments.
    pub faint: Color,
    /// Dividers.
    pub border: Color,
    /// Input fields and buttons.
    pub control: Color,
    /// Hovered and selected items.
    pub hover: Color,
    /// Emphasis.
    pub accent: Color,
    /// Headers and bars.
    pub title: Color,
    /// The active pane's edge.
    pub active_border: Color,
    /// Destructive actions.
    pub danger: Color,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            desktop: Color::rgb(0x1b, 0x1e, 0x24),
            panel: Color::rgb(0x1b, 0x1e, 0x24),
            popup: Color::rgb(0x08, 0x06, 0x17),
            text: Color::rgb(0xfc, 0xff, 0xfd),
            muted: Color::rgb(0xb5, 0xb4, 0xcf),
            faint: Color::rgb(0x7b, 0x8d, 0xab),
            border: Color::rgb(0x4f, 0x51, 0x57),
            control: Color::rgb(0x2c, 0x31, 0x3b),
            hover: Color::rgb(0x4f, 0x51, 0x57),
            accent: Color::rgb(0xbd, 0xcc, 0xe4),
            title: Color::rgb(0x1b, 0x1e, 0x24),
            active_border: Color::rgb(0xbd, 0xcc, 0xe4),
            danger: Color::rgb(0xff, 0x99, 0xb6),
        }
    }
}

/// The interaction state of a canvas this frame.
#[derive(Clone, Debug)]
pub struct Response {
    /// The canvas's stable identity.
    pub id: Id,
    /// Visible interaction bounds, after window and container clipping.
    pub rect: CellRect,
    /// The pointer is over the canvas.
    pub hovered: bool,
    /// The canvas receives keyboard input.
    pub focused: bool,
    /// The canvas has captured the pointer, from a press until its release.
    pub active: bool,
}

/// A native mouse cursor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cursor {
    /// The arrow.
    #[default]
    Default,
    /// A clickable control.
    Pointer,
    /// Editable text.
    Text,
    /// A divider between columns.
    ResizeColumns,
    /// A divider between rows.
    ResizeRows,
}

/// What the host should apply to its window after a frame.
#[derive(Default)]
pub struct Output {
    /// The native cursor to show.
    pub cursor: Cursor,
    /// Where the IME candidate window belongs; `None` when no text input is focused.
    pub ime_cursor: Option<CellRect>,
    /// Toggle native IME off before re-enabling for the new text target.
    pub reset_ime: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Application-drawn content; the flag says whether it accepts text input.
    Canvas(bool),
    Pane,
    Divider(Axis),
}

#[derive(Clone)]
struct Hit {
    id: Id,
    pane: Option<Id>,
    rect: Rect,
    clip: Rect,
    kind: Kind,
    enabled: bool,
}
impl Hit {
    fn visible(&self) -> Rect {
        self.rect.intersect(self.clip)
    }
}

struct PaneMemory {
    id: Id,
    rect: Rect,
}

#[derive(Clone)]
struct Capture {
    hit: Hit,
    origin: [f64; 2],
    split_cut: Option<u32>,
}

/// Keep one Context per UI. Explicit IDs are scoped by their pane and containers;
/// labels may change without resetting focus, text cursors, or scroll positions.
#[derive(Default)]
pub struct Context {
    /// The colors drawn with.
    pub theme: Theme,
    panes: Vec<PaneMemory>,
    splits: HashMap<Id, tiles::SplitMemory>,
    active_pane: Option<Id>,
    previous: Vec<Hit>,
    hits: Vec<Hit>,
    commands: Vec<Paint>,
    seen: HashSet<Id>,
    layout_changed: bool,
    /// Events routed to each canvas, waiting for it to be declared this frame.
    inbox: HashMap<Id, Vec<InputEvent>>,
    focus: Option<Id>,
    capture: Option<Capture>,
    pointer: Option<[f64; 2]>,
    preedit: Option<Preedit>,
    modifiers: Modifiers,
    ime_blocked: bool,
    window_focused: bool,
    initialized: bool,
    reset_text_input: bool,
    screen: Rect,
    output: Output,
    cursor_override: Option<Cursor>,
}

impl Context {
    /// A context with default colors.
    pub fn new() -> Self {
        Self::default()
    }
    /// Cancel composition when the application changes the document behind a widget.
    pub fn reset_text_input(&mut self) {
        self.reset_text_input = true;
    }
    /// The canvas with keyboard focus.
    pub fn focused(&self) -> Option<Id> {
        self.focus
    }
    /// Bounds from the most recently completed layout.
    pub fn pane_rect(&self, key: impl Hash) -> Option<CellRect> {
        let id = Id::new(key);
        self.panes
            .iter()
            .find(|pane| pane.id == id)
            .map(|pane| pane.rect.cells())
    }

    /// Consume events once against the previous completed layout, then build a
    /// complete frame. The closure runs once. Splits fill their available region.
    pub fn run(
        &mut self,
        framebuffer: &mut Framebuffer,
        events: &[InputEvent],
        build: impl FnOnce(&mut Ui<'_>),
    ) -> Output {
        let [width, height] = framebuffer.size();
        let screen = Rect::new(0, 0, width, height);
        self.layout_changed = self.screen != screen;
        self.screen = screen;
        if !self.initialized {
            self.window_focused = true;
            self.initialized = true;
        }
        self.output = Output::default();
        if std::mem::take(&mut self.reset_text_input) {
            self.output.reset_ime = true;
            self.ime_blocked = true;
            self.preedit = None;
        }
        self.inbox.clear();
        if self.layout_changed {
            self.capture = None;
        }
        self.route(events);
        self.hits.clear();
        self.commands.clear();
        self.panes.clear();
        self.seen.clear();
        build(&mut Ui {
            ctx: self,
            scope: Id(0),
            pane: None,
            clip: screen,
            layout: Layout::new(screen),
            enabled: true,
        });
        let valid = |id| {
            self.hits
                .iter()
                .any(|hit| hit.id == id && hit.enabled && !hit.visible().empty())
        };
        let invalid_focus = self.focus.is_some_and(|id| !valid(id));
        let invalid_capture = self
            .capture
            .as_ref()
            .is_some_and(|capture| !valid(capture.hit.id));
        if invalid_focus {
            self.set_focus(None);
        }
        if invalid_capture {
            self.capture = None;
        }
        if !self
            .panes
            .iter()
            .any(|pane| Some(pane.id) == self.active_pane && !pane.rect.empty())
        {
            self.active_pane = self
                .panes
                .iter()
                .find(|pane| !pane.rect.empty())
                .map(|pane| pane.id);
        }
        self.splits.retain(|id, _| self.seen.contains(id));
        let cursor_hit = self
            .capture
            .as_ref()
            .map(|capture| &capture.hit)
            .or_else(|| {
                self.pointer
                    .and_then(|point| self.hit_at(&self.hits, point))
            });
        if let Some(hit) = cursor_hit.filter(|hit| hit.enabled) {
            self.output.cursor = match hit.kind {
                Kind::Canvas(true) => Cursor::Text,
                Kind::Divider(Axis::Columns) => Cursor::ResizeColumns,
                Kind::Divider(Axis::Rows) => Cursor::ResizeRows,
                Kind::Pane | Kind::Canvas(false) => Cursor::Default,
            };
        }
        if let Some(cursor) = self.cursor_override.take() {
            self.output.cursor = cursor;
        }
        if self.focus.is_none() || !self.window_focused {
            self.output.ime_cursor = None;
        }
        paint::compose(
            framebuffer,
            &self.commands,
            self.active_pane.filter(|_| self.window_focused),
            self.theme,
        );
        self.previous = std::mem::take(&mut self.hits);
        std::mem::take(&mut self.output)
    }

    fn set_focus(&mut self, focus: Option<Id>) {
        if self.focus != focus {
            let was_text = self.focus.is_some_and(|id| {
                self.previous
                    .iter()
                    .any(|hit| hit.id == id && hit.kind == Kind::Canvas(true))
            });
            self.output.reset_ime |= was_text;
            self.ime_blocked |= was_text;
            self.preedit = None;
            self.output.ime_cursor = None;
            self.focus = focus;
        }
        if let Some(pane) = focus
            .and_then(|id| self.previous.iter().find(|hit| hit.id == id))
            .and_then(|hit| hit.pane)
        {
            self.active_pane = Some(pane);
        }
    }

    fn pane_at(&self, point: [f64; 2]) -> Option<Id> {
        self.panes
            .iter()
            .find(|pane| pane.rect.contains(point))
            .map(|pane| pane.id)
    }

    fn hit_at<'a>(&self, hits: &'a [Hit], point: [f64; 2]) -> Option<&'a Hit> {
        hits.iter().rev().find(|hit| hit.visible().contains(point))
    }
}

/// A top-to-bottom layout within fixed bounds, with a one-cell gap between items.
#[derive(Clone, Copy)]
struct Layout {
    bounds: Rect,
    next_row: i32,
}

impl Layout {
    fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            next_row: bounds.y,
        }
    }

    fn allocate(&mut self, height: u32) -> Rect {
        let rect = Rect::new(self.bounds.x, self.next_row, self.bounds.w, height);
        self.next_row = rect.bottom().saturating_add(1);
        rect
    }
}

/// A temporary builder borrowed from [`Context::run`] or a container closure.
pub struct Ui<'a> {
    ctx: &'a mut Context,
    scope: Id,
    pane: Option<Id>,
    clip: Rect,
    layout: Layout,
    enabled: bool,
}

impl Ui<'_> {
    /// Remaining vertical space in the current layout, measured in whole cells.
    pub fn available_height(&self) -> u32 {
        self.layout
            .bounds
            .bottom()
            .saturating_sub(self.layout.next_row)
            .max(0) as u32
    }

    /// The identity of a canvas declared here with this key.
    pub fn id(&self, key: impl Hash) -> Id {
        self.scope.with(key)
    }

    /// Declare content that ignores input while `disabled` is true.
    pub fn disabled(&mut self, disabled: bool, build: impl FnOnce(&mut Ui<'_>)) {
        let enabled = self.enabled;
        self.enabled &= !disabled;
        build(self);
        self.enabled = enabled;
    }

    /// A builder for a sub-region, clipped to it, with its own ID scope.
    fn child(&mut self, scope: Id, pane: Option<Id>, region: Rect) -> Ui<'_> {
        Ui {
            ctx: self.ctx,
            scope,
            pane,
            clip: self.clip.intersect(region),
            layout: Layout::new(region),
            enabled: self.enabled,
        }
    }

    fn captured(&self, id: Id) -> bool {
        let capture = self.ctx.capture.as_ref();
        capture.is_some_and(|capture| capture.hit.id == id)
    }

    fn hit(&mut self, id: Id, pane: Option<Id>, rect: Rect, kind: Kind) {
        self.ctx.hits.push(Hit {
            id,
            pane,
            rect,
            clip: self.clip,
            kind,
            enabled: self.enabled,
        });
    }

    fn graphic(&mut self, rect: Rect, graphic: crate::renderer::Graphic) {
        self.ctx.commands.push(Paint::Graphic {
            graphic,
            rect,
            clip: self.clip,
        });
    }

    fn fill(&mut self, rect: Rect, style: Style) {
        let rect = rect.intersect(self.clip);
        if !rect.empty() {
            self.ctx
                .commands
                .push(Paint::Fill(rect, Cell::new(' ', style)));
        }
    }
}

#[cfg(test)]
mod tests;
