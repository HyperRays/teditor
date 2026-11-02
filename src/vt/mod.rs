//! An embedded terminal: libghostty-vt parses output and models the screen, scrollback,
//! modes and input encodings, and this crate's own renderer draws the result.
//!
//! `build.rs` builds the static library with Zig from the `vendor/ghostty` submodule and
//! generates FFI declarations from the same headers with bindgen. The C API is not yet
//! stable, so updating means moving the pinned commit and fixing what no longer compiles.
//!
//! [`Vt`] is the safe wrapper, usable without a process. [`Pty`] runs a program on a
//! pseudoterminal with I/O threads, using ConPTY on Windows. [`Session`] joins the two. All Ghostty handles stay on the UI
//! thread, which parses at most 2 MiB per frame. Output wakes the host through a [`Waker`],
//! so an idle terminal does not poll.

mod ffi;
mod keys;
mod pty;

use crate::{
    renderer::{Color, symbol},
    terminal::input::{
        Key, KeyCode, KeyEvent, KeyLocation, KeyState, Modifiers, MouseButton, NamedKey,
        PhysicalKey,
    },
};
use ffi::*;
pub use pty::{Pty, Waker};
use std::{ffi::c_void, io, path::Path, ptr, time::Instant};

/// Surface units per cell. The grid is the only geometry the UI has, so encoders
/// receive whole-cell positions on this fixed scale rather than real pixels.
const CELL: [u32; 2] = [10, 20];
const MODE_BRACKETED_PASTE: u16 = 2004;
const MODE_ALTERNATE_SCROLL: u16 = 1007;
/// Output parsed per frame, so a flood cannot starve input or drawing.
const FRAME_BUDGET: usize = 2 * 1024 * 1024;

fn sized<T>(init: impl FnOnce(usize) -> T) -> T {
    init(size_of::<T>())
}
fn rgb(color: GhosttyColorRgb) -> Color {
    Color::rgb(color.r, color.g, color.b)
}
fn from_color(Color(r, g, b, _): Color) -> GhosttyColorRgb {
    GhosttyColorRgb { r, g, b }
}
fn succeeded(result: GhosttyResult) -> io::Result<()> {
    if result == GHOSTTY_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::other(format!("libghostty-vt error {result}")))
    }
}

/// One screen cell as Ghostty modeled it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtCell {
    /// What the cell shows: a whole grapheme, as a [`symbol`] if
    /// it has several scalars.
    pub character: char,
    /// Cells this one occupies as Ghostty measured it: 2 for a wide character, and
    /// 0 for the second half of one, which its left neighbor draws.
    pub columns: u8,
    /// Text color.
    pub foreground: Color,
    /// Background color.
    pub background: Color,
    /// Whether the cell is underlined.
    pub underline: bool,
    /// Whether the cell is in the selection.
    pub selected: bool,
}

/// How a program asked for its cursor to look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    /// A filled cell.
    Block,
    /// A thin bar at the cell's left.
    Bar,
    /// A line under the cell.
    Underline,
    /// An outlined cell.
    Hollow,
}

/// The terminal's visible cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtCursor {
    /// Its `[column, row]`.
    pub position: [u16; 2],
    /// How it looks.
    pub shape: CursorShape,
}

/// The scrollback position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scrollbar {
    /// Rows in the whole history.
    pub total: u64,
    /// The first row in view.
    pub offset: u64,
    /// Rows in view.
    pub visible: u64,
}

/// A pointer action to report to a program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    /// A button went down.
    Press,
    /// A button came up.
    Release,
    /// The pointer moved.
    Motion,
}

/// State the C callbacks reach through a stable heap address.
struct Shared {
    output: Vec<u8>,
    size: [u16; 2],
}

unsafe extern "C" fn on_write_pty(
    _: GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    // SAFETY: userdata is the boxed Shared that outlives the terminal, the callback
    // runs on the only thread using it, and Ghostty passes a valid byte range.
    unsafe {
        (*userdata.cast::<Shared>())
            .output
            .extend_from_slice(std::slice::from_raw_parts(data, len));
    }
}
unsafe extern "C" fn on_device_attributes(
    _: GhosttyTerminal,
    _: *mut c_void,
    out: *mut GhosttyDeviceAttributes,
) -> bool {
    // SAFETY: Ghostty passes a valid structure to fill.
    let attributes = unsafe { &mut *out };
    attributes.primary.conformance_level = GHOSTTY_DA_CONFORMANCE_VT220 as u16;
    attributes.primary.features[0] = GHOSTTY_DA_FEATURE_ANSI_COLOR as u16;
    attributes.primary.num_features = 1;
    attributes.secondary.device_type = GHOSTTY_DA_DEVICE_TYPE_VT220 as u16;
    true
}
unsafe extern "C" fn on_size(
    _: GhosttyTerminal,
    userdata: *mut c_void,
    out: *mut GhosttySizeReportSize,
) -> bool {
    // SAFETY: as for on_write_pty; Ghostty passes a valid structure to fill.
    unsafe {
        let [columns, rows] = (*userdata.cast::<Shared>()).size;
        *out = GhosttySizeReportSize {
            rows,
            columns,
            cell_width: CELL[0],
            cell_height: CELL[1],
        };
    }
    true
}

// The options below pass these as untyped pointers, so check their signatures here.
const _: [GhosttyTerminalWritePtyFn; 1] = [Some(on_write_pty)];
const _: [GhosttyTerminalDeviceAttributesFn; 1] = [Some(on_device_attributes)];
const _: [GhosttyTerminalSizeFn; 1] = [Some(on_size)];

/// A Ghostty terminal and its render snapshot, without any process attached.
pub struct Vt {
    terminal: GhosttyTerminal,
    render: GhosttyRenderState,
    rows_iter: GhosttyRenderStateRowIterator,
    cells: GhosttyRenderStateRowCells,
    key_encoder: GhosttyKeyEncoder,
    key_event: GhosttyKeyEvent,
    mouse_encoder: GhosttyMouseEncoder,
    mouse_event: GhosttyMouseEvent,
    gesture: GhosttySelectionGesture,
    press: GhosttySelectionGestureEvent,
    drag: GhosttySelectionGestureEvent,
    release: GhosttySelectionGestureEvent,
    shared: Box<Shared>,
    created: Instant,
    grid: Vec<Vec<VtCell>>,
    cursor: Option<VtCursor>,
    colors: [Color; 2],
    buttons_held: u32,
}

impl Vt {
    /// A terminal of `[columns, rows]`, with these default colors.
    pub fn new(size: [u16; 2], foreground: Color, background: Color) -> io::Result<Self> {
        let [columns, rows] = size;
        let mut vt = Self {
            terminal: ptr::null_mut(),
            render: ptr::null_mut(),
            rows_iter: ptr::null_mut(),
            cells: ptr::null_mut(),
            key_encoder: ptr::null_mut(),
            key_event: ptr::null_mut(),
            mouse_encoder: ptr::null_mut(),
            mouse_event: ptr::null_mut(),
            gesture: ptr::null_mut(),
            press: ptr::null_mut(),
            drag: ptr::null_mut(),
            release: ptr::null_mut(),
            shared: Box::new(Shared {
                output: Vec::new(),
                size,
            }),
            created: Instant::now(),
            grid: Vec::new(),
            cursor: None,
            colors: [foreground, background],
            buttons_held: 0,
        };
        // SAFETY: every out-pointer is a field that Drop frees; a null allocator
        // selects the default; option values have the documented types, and the
        // terminal copies colors and keeps userdata, which is boxed for its lifetime.
        unsafe {
            let allocator = ptr::null();
            succeeded(ghostty_terminal_new(
                allocator,
                &mut vt.terminal,
                columns,
                rows,
            ))?;
            succeeded(ghostty_render_state_new(allocator, &mut vt.render))?;
            succeeded(ghostty_render_state_row_iterator_new(
                allocator,
                &mut vt.rows_iter,
            ))?;
            succeeded(ghostty_render_state_row_cells_new(allocator, &mut vt.cells))?;
            succeeded(ghostty_key_encoder_new(allocator, &mut vt.key_encoder))?;
            succeeded(ghostty_key_event_new(allocator, &mut vt.key_event))?;
            succeeded(ghostty_mouse_encoder_new(allocator, &mut vt.mouse_encoder))?;
            succeeded(ghostty_mouse_event_new(allocator, &mut vt.mouse_event))?;
            succeeded(ghostty_selection_gesture_new(allocator, &mut vt.gesture))?;
            for (event, kind) in [
                (&mut vt.press, GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS),
                (&mut vt.drag, GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG),
                (
                    &mut vt.release,
                    GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE,
                ),
            ] {
                succeeded(ghostty_selection_gesture_event_new(allocator, event, kind))?;
            }
            let userdata = ptr::from_mut(&mut *vt.shared).cast::<c_void>();
            let (foreground, background) = (from_color(foreground), from_color(background));
            for (option, value) in [
                (GHOSTTY_TERMINAL_OPT_USERDATA, userdata.cast_const()),
                (
                    GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                    on_write_pty as *const c_void,
                ),
                (
                    GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
                    on_device_attributes as *const c_void,
                ),
                (GHOSTTY_TERMINAL_OPT_SIZE, on_size as *const c_void),
                (
                    GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                    ptr::from_ref(&foreground).cast(),
                ),
                (
                    GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                    ptr::from_ref(&background).cast(),
                ),
            ] {
                succeeded(ghostty_terminal_set(vt.terminal, option, value))?;
            }
        }
        vt.resize(size)?;
        // Measure and store whole graphemes, as the rest of the application does.
        vt.write(b"\x1b[?2027h");
        Ok(vt)
    }

    /// Parse output from the program.
    pub fn write(&mut self, bytes: &[u8]) {
        // SAFETY: a live terminal and a valid byte range.
        unsafe { ghostty_terminal_vt_write(self.terminal, bytes.as_ptr(), bytes.len()) }
    }
    /// Replies the terminal generated for the program, such as device attributes.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.shared.output)
    }
    /// Change the grid to `[columns, rows]`.
    pub fn resize(&mut self, size: [u16; 2]) -> io::Result<()> {
        self.shared.size = size;
        // SAFETY: a live terminal, and encoder options of the documented type.
        unsafe {
            succeeded(ghostty_terminal_resize(
                self.terminal,
                size[0],
                size[1],
                CELL[0],
                CELL[1],
            ))?;
            let geometry = sized(|size_| GhosttyMouseEncoderSize {
                size: size_,
                screen_width: u32::from(size[0]) * CELL[0],
                screen_height: u32::from(size[1]) * CELL[1],
                cell_width: CELL[0],
                cell_height: CELL[1],
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            });
            ghostty_mouse_encoder_setopt(
                self.mouse_encoder,
                GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                ptr::from_ref(&geometry).cast(),
            );
        }
        self.grid.clear();
        Ok(())
    }

    /// Bring the cell grid up to date, copying only rows Ghostty reports dirty.
    pub fn sync(&mut self) -> io::Result<()> {
        // SAFETY: live handles; each `get` receives the documented output type, and
        // cell data is read before the next render-state update invalidates it.
        unsafe {
            succeeded(ghostty_render_state_update(self.render, self.terminal))?;
            // Dirty state only tracks cells, and a program can move or restyle the
            // cursor without touching any, so this cannot wait for the check below.
            self.read_cursor()?;
            let mut dirty = GHOSTTY_RENDER_STATE_DIRTY_FALSE;
            self.get(GHOSTTY_RENDER_STATE_DATA_DIRTY, &mut dirty)?;
            let [columns, rows] = self.shared.size;
            let rebuild = self.grid.len() != usize::from(rows);
            if dirty == GHOSTTY_RENDER_STATE_DIRTY_FALSE && !rebuild {
                return Ok(());
            }
            let mut foreground = from_color(self.colors[0]);
            let mut background = from_color(self.colors[1]);
            self.get(GHOSTTY_RENDER_STATE_DATA_COLOR_FOREGROUND, &mut foreground)?;
            self.get(GHOSTTY_RENDER_STATE_DATA_COLOR_BACKGROUND, &mut background)?;
            let defaults = (rgb(foreground), rgb(background));
            let blank = VtCell {
                character: ' ',
                columns: 1,
                foreground: defaults.0,
                background: defaults.1,
                underline: false,
                selected: false,
            };
            let full = rebuild || dirty == GHOSTTY_RENDER_STATE_DIRTY_FULL;
            if full {
                self.grid = vec![vec![blank; usize::from(columns)]; usize::from(rows)];
            }
            let mut iterator = self.rows_iter;
            self.get(GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &mut iterator)?;
            let mut next_row = 0;
            loop {
                let mut y = next_row;
                let more = if full {
                    ghostty_render_state_row_iterator_next(iterator)
                } else {
                    ghostty_render_state_row_iterator_next_dirty(iterator, &mut y)
                };
                if !more {
                    break;
                }
                next_row = y + 1;
                self.copy_row(iterator, usize::from(y), defaults, blank)?;
            }
            succeeded(ghostty_render_state_clean(self.render))?;
        }
        Ok(())
    }

    unsafe fn get<T>(&self, data: GhosttyRenderStateData, out: &mut T) -> io::Result<()> {
        // SAFETY: the caller pairs `data` with its documented output type.
        succeeded(unsafe { ghostty_render_state_get(self.render, data, ptr::from_mut(out).cast()) })
    }

    unsafe fn copy_row(
        &mut self,
        iterator: GhosttyRenderStateRowIterator,
        y: usize,
        defaults: (Color, Color),
        blank: VtCell,
    ) -> io::Result<()> {
        let Some(row) = self.grid.get_mut(y) else {
            return Ok(());
        };
        let mut cells = self.cells;
        // SAFETY: the iterator is positioned on a row, and each output has its documented type.
        unsafe {
            let mut selection = sized(|size| GhosttyRenderStateRowSelection {
                size,
                start_x: 0,
                end_x: 0,
            });
            let selected = (ghostty_render_state_row_get(
                iterator,
                GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION,
                ptr::from_mut(&mut selection).cast(),
            ) == GHOSTTY_SUCCESS)
                .then_some(selection.start_x..=selection.end_x);
            succeeded(ghostty_render_state_row_get(
                iterator,
                GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                ptr::from_mut(&mut cells).cast(),
            ))?;
            for (x, slot) in row.iter_mut().enumerate() {
                if !ghostty_render_state_row_cells_next(cells) {
                    *slot = blank;
                    continue;
                }
                *slot = read_cell(cells, defaults);
                slot.selected = selected
                    .as_ref()
                    .is_some_and(|range| range.contains(&(x as u16)));
            }
        }
        Ok(())
    }

    unsafe fn read_cursor(&mut self) -> io::Result<()> {
        // SAFETY: a sized cursor structure, as the C API requires.
        let cursor = unsafe {
            let mut cursor: GhosttyRenderStateCursor = std::mem::zeroed();
            cursor.size = size_of::<GhosttyRenderStateCursor>();
            self.get(GHOSTTY_RENDER_STATE_DATA_CURSOR, &mut cursor)?;
            cursor
        };
        self.cursor = (cursor.visible && cursor.viewport_has_value).then_some(VtCursor {
            position: [cursor.viewport_x, cursor.viewport_y],
            shape: match cursor.visual_style {
                GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CursorShape::Bar,
                GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => CursorShape::Underline,
                GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => CursorShape::Hollow,
                _ => CursorShape::Block,
            },
        });
        Ok(())
    }

    /// The cell grid from the last [`sync`](Self::sync), by row.
    pub fn grid(&self) -> &[Vec<VtCell>] {
        &self.grid
    }
    /// The cursor from the last [`sync`](Self::sync); `None` when hidden.
    pub fn cursor(&self) -> Option<VtCursor> {
        self.cursor
    }
    /// The default `[foreground, background]` colors.
    pub fn colors(&self) -> [Color; 2] {
        self.colors
    }
    /// The program-set title, or empty when it has not set one.
    pub fn title(&self) -> String {
        let mut title = GhosttyString {
            ptr: ptr::null(),
            len: 0,
        };
        // SAFETY: the string borrows terminal memory and is copied before the next call.
        unsafe {
            let result = ghostty_terminal_get(
                self.terminal,
                GHOSTTY_TERMINAL_DATA_TITLE,
                ptr::from_mut(&mut title).cast(),
            );
            if result != GHOSTTY_SUCCESS || title.ptr.is_null() {
                return String::new();
            }
            String::from_utf8_lossy(std::slice::from_raw_parts(title.ptr, title.len)).into_owned()
        }
    }
    /// The scrollback position.
    pub fn scrollbar(&self) -> Scrollbar {
        let mut bar = GhosttyTerminalScrollbar {
            total: 0,
            offset: 0,
            len: 0,
        };
        // SAFETY: the documented output type for this query.
        unsafe {
            ghostty_terminal_get(
                self.terminal,
                GHOSTTY_TERMINAL_DATA_SCROLLBAR,
                ptr::from_mut(&mut bar).cast(),
            );
        }
        Scrollbar {
            total: bar.total,
            offset: bar.offset,
            visible: bar.len,
        }
    }
    fn scroll(
        &mut self,
        tag: GhosttyTerminalScrollViewportTag,
        value: GhosttyTerminalScrollViewportValue,
    ) {
        // SAFETY: a live terminal; the tag selects the union member the caller set.
        unsafe {
            ghostty_terminal_scroll_viewport(
                self.terminal,
                GhosttyTerminalScrollViewport { tag, value },
            )
        }
    }
    /// Move the viewport through scrollback; negative moves toward older output.
    pub fn scroll_lines(&mut self, delta: isize) {
        self.scroll(
            GHOSTTY_SCROLL_VIEWPORT_DELTA,
            GhosttyTerminalScrollViewportValue { delta },
        );
    }
    /// Scroll so `row` is the first in view.
    pub fn scroll_to_row(&mut self, row: u64) {
        self.scroll(
            GHOSTTY_SCROLL_VIEWPORT_ROW,
            GhosttyTerminalScrollViewportValue { row: row as usize },
        );
    }
    /// Return to the live screen.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll(
            GHOSTTY_SCROLL_VIEWPORT_BOTTOM,
            GhosttyTerminalScrollViewportValue { delta: 0 },
        );
    }
    /// Whether the program wants pointer events.
    pub fn mouse_tracking(&self) -> bool {
        let mut tracking = false;
        // SAFETY: the documented output type for this query.
        unsafe {
            ghostty_terminal_get(
                self.terminal,
                GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING,
                ptr::from_mut(&mut tracking).cast(),
            );
        }
        tracking
    }
    fn mode(&self, number: u16) -> bool {
        let mut config = GhosttyTerminalModeConfig {
            mode: number,
            value: false,
        };
        // SAFETY: the documented input/output type; DEC modes have the ANSI bit clear.
        unsafe {
            ghostty_terminal_get(
                self.terminal,
                GHOSTTY_TERMINAL_DATA_MODE,
                ptr::from_mut(&mut config).cast(),
            );
        }
        config.value
    }
    /// Whether a full-screen program has switched to the alternate screen.
    fn on_alternate_screen(&self) -> bool {
        let mut screen = GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        // SAFETY: the documented output type for this query.
        unsafe {
            ghostty_terminal_get(
                self.terminal,
                GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                ptr::from_mut(&mut screen).cast(),
            );
        }
        screen == GHOSTTY_TERMINAL_SCREEN_ALTERNATE
    }
    /// Whether the wheel should send arrow keys, as full-screen programs expect.
    pub fn wheel_sends_arrows(&self) -> bool {
        self.on_alternate_screen() && self.mode(MODE_ALTERNATE_SCROLL)
    }
    /// Whether a program has asked for the Kitty keyboard protocol, so it reads every key.
    fn uses_kitty_keys(&self) -> bool {
        let mut flags: GhosttyKittyKeyFlags = 0;
        // SAFETY: the documented output type for this query.
        unsafe {
            ghostty_terminal_get(
                self.terminal,
                GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS,
                ptr::from_mut(&mut flags).cast(),
            );
        }
        flags != 0
    }
    /// The modifiers to report for a key. Line editors such as zsh do not know Shift+arrow and
    /// type the tail of its sequence, so at a prompt it is a plain arrow. Full-screen programs
    /// and those using Kitty keys understand it, and PowerShell selects text with it.
    fn reported_modifiers(&self, key: &KeyEvent) -> Modifiers {
        let arrow = matches!(
            key.physical_key,
            PhysicalKey::Code(
                KeyCode::ArrowLeft | KeyCode::ArrowRight | KeyCode::ArrowUp | KeyCode::ArrowDown
            )
        );
        let mut modifiers = key.modifiers;
        if modifiers.shift
            && arrow
            && cfg!(not(windows))
            && !self.on_alternate_screen()
            && !self.uses_kitty_keys()
        {
            modifiers.shift = false;
        }
        modifiers
    }

    /// Encode a key press for the program, honoring the modes it has requested.
    /// The returned text, if any, is what the key produced; the host's matching
    /// text event must not be written again.
    pub fn encode_key(&mut self, key: &KeyEvent) -> (Vec<u8>, Option<String>) {
        let modifiers = self.reported_modifiers(key);
        if let Some(bytes) = line_editing_bytes(key.physical_key, modifiers) {
            return (bytes.to_vec(), None);
        }
        let utf8 = key_text(key);
        let code = match key.physical_key {
            PhysicalKey::Code(code) => keys::ghostty_key(code),
            PhysicalKey::Unidentified(_) => GHOSTTY_KEY_UNIDENTIFIED,
        };
        let plain = match key.key_without_modifiers.as_ref() {
            Some(Key::Character(text)) => text.chars().next(),
            _ => None,
        };
        let mut consumed = 0;
        if utf8.is_some() {
            if key.modifiers.shift {
                consumed |= GHOSTTY_MODS_SHIFT;
            }
            // Option produced a different character instead of acting as Alt.
            if key.modifiers.alt && utf8.as_deref().and_then(|t| t.chars().next()) != plain {
                consumed |= GHOSTTY_MODS_ALT;
            }
        }
        let mut buffer = [0u8; 128];
        let mut written = 0;
        // SAFETY: live handles; utf8 outlives the encode call that reads it.
        let result = unsafe {
            ghostty_key_encoder_setopt_from_terminal(self.key_encoder, self.terminal);
            ghostty_key_event_set_action(
                self.key_event,
                if key.repeat {
                    GHOSTTY_KEY_ACTION_REPEAT
                } else {
                    GHOSTTY_KEY_ACTION_PRESS
                },
            );
            ghostty_key_event_set_key(self.key_event, code);
            ghostty_key_event_set_mods(self.key_event, mods(modifiers));
            ghostty_key_event_set_consumed_mods(self.key_event, consumed as GhosttyMods);
            ghostty_key_event_set_unshifted_codepoint(self.key_event, plain.map_or(0, u32::from));
            match &utf8 {
                Some(text) => {
                    ghostty_key_event_set_utf8(self.key_event, text.as_ptr().cast(), text.len())
                }
                None => ghostty_key_event_set_utf8(self.key_event, ptr::null(), 0),
            }
            ghostty_key_encoder_encode(
                self.key_encoder,
                self.key_event,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut written,
            )
        };
        if result != GHOSTTY_SUCCESS || written == 0 {
            return (Vec::new(), None);
        }
        (buffer[..written].to_vec(), utf8)
    }

    /// Encode a pointer event at local cell coordinates; empty if the program does
    /// not want this event under its current tracking mode.
    pub fn encode_mouse(
        &mut self,
        action: MouseAction,
        button: Option<MouseButton>,
        modifiers: Modifiers,
        position: [f64; 2],
    ) -> Vec<u8> {
        match action {
            MouseAction::Press => self.buttons_held += 1,
            MouseAction::Release => self.buttons_held = self.buttons_held.saturating_sub(1),
            MouseAction::Motion => {}
        }
        let mut buffer = [0u8; 128];
        let mut written = 0;
        let held = self.buttons_held > 0;
        // SAFETY: live handles and option values of the documented types.
        let result = unsafe {
            ghostty_mouse_encoder_setopt_from_terminal(self.mouse_encoder, self.terminal);
            ghostty_mouse_encoder_setopt(
                self.mouse_encoder,
                GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                ptr::from_ref(&held).cast(),
            );
            ghostty_mouse_event_set_action(
                self.mouse_event,
                match action {
                    MouseAction::Press => GHOSTTY_MOUSE_ACTION_PRESS,
                    MouseAction::Release => GHOSTTY_MOUSE_ACTION_RELEASE,
                    MouseAction::Motion => GHOSTTY_MOUSE_ACTION_MOTION,
                },
            );
            match button.map(mouse_button) {
                Some(button) => ghostty_mouse_event_set_button(self.mouse_event, button),
                None => ghostty_mouse_event_clear_button(self.mouse_event),
            }
            ghostty_mouse_event_set_mods(self.mouse_event, mods(modifiers));
            ghostty_mouse_event_set_position(
                self.mouse_event,
                GhosttyMousePosition {
                    x: (position[0] * f64::from(CELL[0])) as f32,
                    y: (position[1] * f64::from(CELL[1])) as f32,
                },
            );
            ghostty_mouse_encoder_encode(
                self.mouse_encoder,
                self.mouse_event,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut written,
            )
        };
        if result == GHOSTTY_SUCCESS {
            buffer[..written].to_vec()
        } else {
            Vec::new()
        }
    }
    /// Wheel lines for a tracking program: one press per line, no release.
    pub fn encode_wheel(
        &mut self,
        lines: isize,
        modifiers: Modifiers,
        position: [f64; 2],
    ) -> Vec<u8> {
        let button = if lines < 0 {
            MouseButton::Back
        } else {
            MouseButton::Forward
        };
        let mut bytes = Vec::new();
        for _ in 0..lines.unsigned_abs() {
            bytes.extend(self.encode_mouse(MouseAction::Press, Some(button), modifiers, position));
            self.buttons_held = self.buttons_held.saturating_sub(1);
        }
        bytes
    }

    /// Wheel lines as arrow keys, for full-screen programs without mouse tracking.
    pub fn encode_arrows(&mut self, lines: isize) -> Vec<u8> {
        let (code, key) = if lines < 0 {
            (KeyCode::ArrowUp, NamedKey::ArrowUp)
        } else {
            (KeyCode::ArrowDown, NamedKey::ArrowDown)
        };
        let event = KeyEvent {
            physical_key: PhysicalKey::Code(code),
            logical_key: Key::Named(key),
            key_without_modifiers: None,
            location: KeyLocation::Standard,
            state: KeyState::Pressed,
            repeat: false,
            synthetic: true,
            modifiers: Modifiers::default(),
            composing: false,
        };
        let (bytes, _) = self.encode_key(&event);
        bytes.repeat(lines.unsigned_abs())
    }

    /// Prepare clipboard text for the program, stripping unsafe control bytes and
    /// bracketing it when the program asked for that.
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        let mut data = text.as_bytes().to_vec();
        let mut buffer = vec![0u8; data.len() + 32];
        let mut written = 0;
        // SAFETY: both buffers are valid for their lengths; Ghostty may scrub `data`.
        let result = unsafe {
            ghostty_paste_encode(
                data.as_mut_ptr().cast(),
                data.len(),
                self.mode(MODE_BRACKETED_PASTE),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut written,
            )
        };
        if result == GHOSTTY_SUCCESS {
            buffer.truncate(written);
            buffer
        } else {
            Vec::new()
        }
    }

    fn grid_ref(&self, [x, y]: [u16; 2]) -> Option<GhosttyGridRef> {
        let mut grid_ref = sized(|size| GhosttyGridRef {
            size,
            node: ptr::null_mut(),
            x: 0,
            y: 0,
        });
        let point = GhosttyPoint {
            tag: GHOSTTY_POINT_TAG_VIEWPORT,
            value: GhosttyPointValue {
                coordinate: GhosttyPointCoordinate { x, y: u32::from(y) },
            },
        };
        // SAFETY: a live terminal and a sized output structure.
        let result = unsafe { ghostty_terminal_grid_ref(self.terminal, point, &mut grid_ref) };
        (result == GHOSTTY_SUCCESS).then_some(grid_ref)
    }
    /// Begin a selection; repeated presses select words and then lines.
    pub fn selection_press(&mut self, point: [f64; 2]) {
        let Some(grid_ref) = self.grid_ref(point.map(|v| v as u16)) else {
            return;
        };
        let now = self.created.elapsed().as_nanos() as u64;
        let interval: u64 = 500_000_000;
        let distance: f64 = f64::from(CELL[0]);
        let position = surface(point);
        // SAFETY: live handles, option values of the documented types (copied by the
        // event), and a null selection output, which the API permits.
        unsafe {
            for (option, value) in [
                (
                    GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                    ptr::from_ref(&grid_ref).cast::<c_void>(),
                ),
                (
                    GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                    ptr::from_ref(&position).cast(),
                ),
                (
                    GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS,
                    ptr::from_ref(&now).cast(),
                ),
                (
                    GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS,
                    ptr::from_ref(&interval).cast(),
                ),
                (
                    GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE,
                    ptr::from_ref(&distance).cast(),
                ),
            ] {
                ghostty_selection_gesture_event_set(self.press, option, value);
            }
            let mut selection = self.empty_selection();
            let result = ghostty_selection_gesture_event(
                self.gesture,
                self.terminal,
                self.press,
                &mut selection,
            );
            // A single press clears; a repeated press produces a word or line.
            self.set_selection((result == GHOSTTY_SUCCESS).then_some(&selection));
        }
    }
    /// Extend the selection to a point.
    pub fn selection_drag(&mut self, point: [f64; 2]) {
        let Some(grid_ref) = self.grid_ref(point.map(|v| v as u16)) else {
            return;
        };
        let position = surface(point);
        let [columns, rows] = self.shared.size;
        let geometry = GhosttySelectionGestureGeometry {
            columns: u32::from(columns),
            cell_width: CELL[0],
            padding_left: 0,
            screen_height: u32::from(rows) * CELL[1],
        };
        // SAFETY: as for selection_press.
        unsafe {
            for (option, value) in [
                (
                    GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                    ptr::from_ref(&grid_ref).cast::<c_void>(),
                ),
                (
                    GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                    ptr::from_ref(&position).cast(),
                ),
                (
                    GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
                    ptr::from_ref(&geometry).cast(),
                ),
            ] {
                ghostty_selection_gesture_event_set(self.drag, option, value);
            }
            let mut selection = self.empty_selection();
            if ghostty_selection_gesture_event(
                self.gesture,
                self.terminal,
                self.drag,
                &mut selection,
            ) == GHOSTTY_SUCCESS
            {
                self.set_selection(Some(&selection));
            }
        }
    }
    /// Finish the selection gesture.
    pub fn selection_release(&mut self) {
        // SAFETY: the release event needs no options, and a null output is permitted.
        unsafe {
            ghostty_selection_gesture_event(
                self.gesture,
                self.terminal,
                self.release,
                ptr::null_mut(),
            );
        }
    }
    fn empty_selection(&self) -> GhosttySelection {
        let empty = sized(|size| GhosttyGridRef {
            size,
            node: ptr::null_mut(),
            x: 0,
            y: 0,
        });
        sized(|size| GhosttySelection {
            size,
            start: empty,
            end: empty,
            rectangle: false,
        })
    }
    fn set_selection(&mut self, selection: Option<&GhosttySelection>) {
        let value = selection.map_or(ptr::null(), |s| ptr::from_ref(s).cast());
        // SAFETY: null clears; otherwise Ghostty copies a selection it just produced.
        unsafe {
            ghostty_terminal_set(self.terminal, GHOSTTY_TERMINAL_OPT_SELECTION, value);
        }
    }
    /// Deselect everything.
    pub fn clear_selection(&mut self) {
        self.set_selection(None);
    }
    /// The selected text, with soft wraps joined as a clipboard copy expects.
    pub fn selection_text(&self) -> Option<String> {
        let options = sized(|size| GhosttyTerminalSelectionFormatOptions {
            size,
            emit: GHOSTTY_FORMATTER_FORMAT_PLAIN,
            unwrap: true,
            trim: true,
            selection: ptr::null(),
        });
        let mut data = ptr::null_mut();
        let mut len = 0;
        // SAFETY: on success Ghostty hands over an allocation, freed here after copying.
        unsafe {
            if ghostty_terminal_selection_format_alloc(
                self.terminal,
                ptr::null(),
                options,
                &mut data,
                &mut len,
            ) != GHOSTTY_SUCCESS
            {
                return None;
            }
            let text = String::from_utf8_lossy(std::slice::from_raw_parts(data, len)).into_owned();
            ghostty_free(ptr::null(), data, len);
            Some(text)
        }
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        // SAFETY: each handle is null or live, the free functions accept null, and the
        // gesture is released while the terminal it tracks still exists.
        unsafe {
            ghostty_selection_gesture_event_free(self.press);
            ghostty_selection_gesture_event_free(self.drag);
            ghostty_selection_gesture_event_free(self.release);
            ghostty_selection_gesture_free(self.gesture, self.terminal);
            ghostty_mouse_event_free(self.mouse_event);
            ghostty_mouse_encoder_free(self.mouse_encoder);
            ghostty_key_event_free(self.key_event);
            ghostty_key_encoder_free(self.key_encoder);
            ghostty_render_state_row_cells_free(self.cells);
            ghostty_render_state_row_iterator_free(self.rows_iter);
            ghostty_render_state_free(self.render);
            ghostty_terminal_free(self.terminal);
        }
    }
}

/// Text a key produces. Ctrl sequences come from the key itself, so they carry none.
/// What macOS keys for moving around a line send, the same defaults as the Ghostty app. The
/// standard sequences for these reach most shells as stray text such as `;3D`.
pub fn line_editing_bytes(key: PhysicalKey, modifiers: Modifiers) -> Option<&'static [u8]> {
    if !cfg!(target_os = "macos") || modifiers.shift || modifiers.control {
        return None;
    }
    let PhysicalKey::Code(code) = key else {
        return None;
    };
    match (modifiers.super_key, modifiers.alt, code) {
        (false, true, KeyCode::ArrowLeft) => Some(b"\x1bb"),
        (false, true, KeyCode::ArrowRight) => Some(b"\x1bf"),
        (true, false, KeyCode::ArrowLeft) => Some(b"\x01"),
        (true, false, KeyCode::ArrowRight) => Some(b"\x05"),
        (true, false, KeyCode::Backspace) => Some(b"\x15"),
        _ => None,
    }
}
fn key_text(key: &KeyEvent) -> Option<String> {
    if key.modifiers.control || key.modifiers.super_key {
        return None;
    }
    match &key.logical_key {
        Key::Character(text) => Some(text.to_string()),
        Key::Named(NamedKey::Space) => Some(" ".into()),
        _ => None,
    }
}
fn mods(modifiers: Modifiers) -> GhosttyMods {
    let mut mods = 0;
    for (held, bit) in [
        (modifiers.shift, GHOSTTY_MODS_SHIFT),
        (modifiers.control, GHOSTTY_MODS_CTRL),
        (modifiers.alt, GHOSTTY_MODS_ALT),
        (modifiers.super_key, GHOSTTY_MODS_SUPER),
    ] {
        if held {
            mods |= bit;
        }
    }
    mods as GhosttyMods
}
fn mouse_button(button: MouseButton) -> GhosttyMouseButton {
    match button {
        MouseButton::Left => GHOSTTY_MOUSE_BUTTON_LEFT,
        MouseButton::Right => GHOSTTY_MOUSE_BUTTON_RIGHT,
        MouseButton::Middle => GHOSTTY_MOUSE_BUTTON_MIDDLE,
        // Wheel directions use the X11 button numbers tracking programs expect.
        MouseButton::Back => GHOSTTY_MOUSE_BUTTON_FOUR,
        MouseButton::Forward => GHOSTTY_MOUSE_BUTTON_FIVE,
        MouseButton::Other(_) => GHOSTTY_MOUSE_BUTTON_UNKNOWN,
    }
}
fn surface([x, y]: [f64; 2]) -> GhosttySurfacePosition {
    GhosttySurfacePosition {
        x: x * f64::from(CELL[0]),
        y: y * f64::from(CELL[1]),
    }
}

/// Read one cell, including every scalar of its grapheme.
unsafe fn read_cell(cells: GhosttyRenderStateRowCells, (fg, bg): (Color, Color)) -> VtCell {
    // SAFETY: the iterator is on a cell, and each output has its documented type.
    unsafe {
        let mut length = 0u32;
        ghostty_render_state_row_cells_get(
            cells,
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN,
            ptr::from_mut(&mut length).cast(),
        );
        let mut character = ' ';
        if length > 0 {
            let mut codepoints = vec![0u32; length as usize];
            ghostty_render_state_row_cells_get(
                cells,
                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF,
                codepoints.as_mut_ptr().cast(),
            );
            character = if let [single] = codepoints[..] {
                char::from_u32(single).unwrap_or(' ')
            } else {
                let text: String = codepoints
                    .iter()
                    .filter_map(|&c| char::from_u32(c))
                    .collect();
                symbol(&text)
            };
        }
        let color = |data| {
            let mut value = from_color(Color::rgb(0, 0, 0));
            (ghostty_render_state_row_cells_get(cells, data, ptr::from_mut(&mut value).cast())
                == GHOSTTY_SUCCESS)
                .then(|| rgb(value))
        };
        let mut foreground = color(GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR).unwrap_or(fg);
        let mut background = color(GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR).unwrap_or(bg);
        let mut styled = false;
        ghostty_render_state_row_cells_get(
            cells,
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING,
            ptr::from_mut(&mut styled).cast(),
        );
        let mut underline = false;
        if styled {
            let mut style: GhosttyStyle = std::mem::zeroed();
            style.size = size_of::<GhosttyStyle>();
            ghostty_render_state_row_cells_get(
                cells,
                GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
                ptr::from_mut(&mut style).cast(),
            );
            if style.inverse {
                std::mem::swap(&mut foreground, &mut background);
            }
            if style.faint {
                foreground = blend(foreground, background);
            }
            if style.invisible {
                foreground = background;
            }
            underline = style.underline != 0;
        }
        let mut raw: GhosttyCell = 0;
        let mut wide = GHOSTTY_CELL_WIDE_NARROW;
        ghostty_render_state_row_cells_get(
            cells,
            GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
            ptr::from_mut(&mut raw).cast(),
        );
        ghostty_cell_get(raw, GHOSTTY_CELL_DATA_WIDE, ptr::from_mut(&mut wide).cast());
        VtCell {
            character,
            columns: match wide {
                GHOSTTY_CELL_WIDE_WIDE => 2,
                GHOSTTY_CELL_WIDE_SPACER_TAIL => 0,
                _ => 1,
            },
            foreground,
            background,
            underline,
            selected: false,
        }
    }
}
fn blend(Color(r, g, b, _): Color, Color(x, y, z, _): Color) -> Color {
    let mix = |a: u8, c: u8| ((u16::from(a) + u16::from(c)) / 2) as u8;
    Color::rgb(mix(r, x), mix(g, y), mix(b, z))
}

/// A running program attached to a terminal.
pub struct Session {
    /// The terminal; hold no reference across [`pump`](Self::pump).
    pub vt: Vt,
    pty: Pty,
}

impl Session {
    /// Start `command` in `cwd` on a new pseudoterminal.
    pub fn spawn(
        command: &[String],
        cwd: &Path,
        size: [u16; 2],
        colors: [Color; 2],
        waker: Waker,
    ) -> io::Result<Self> {
        Ok(Self {
            vt: Vt::new(size, colors[0], colors[1])?,
            pty: Pty::spawn(command, cwd, size, waker)?,
        })
    }
    /// Parse pending output, sending any replies. Returns whether more remains.
    pub fn pump(&mut self) -> bool {
        let Self { vt, pty } = self;
        let more = pty.read(FRAME_BUDGET, |bytes| vt.write(bytes));
        self.flush();
        more
    }
    /// Send bytes to the program.
    pub fn send(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.pty.write(bytes);
        }
    }
    fn flush(&mut self) {
        let replies = self.vt.take_output();
        self.send(replies);
    }
    /// Resize both the terminal and the pseudoterminal.
    pub fn resize(&mut self, size: [u16; 2]) -> io::Result<()> {
        self.vt.resize(size)?;
        self.pty.resize(size)
    }
    /// Whether the program has ended.
    pub fn exited(&self) -> bool {
        self.pty.exited()
    }
}

#[cfg(test)]
mod tests;
