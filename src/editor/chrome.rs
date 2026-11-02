//! The application's own rows: status bar, title bar, terminal bar, tab row and
//! the document pane's placeholder screens.
use std::ops::Range;

use super::{Command, IoJob, Tab, document::columns, view::primary_name};
use crate::{
    renderer::{CellRect, Color, Graphic, char_columns},
    terminal::{
        InputEvent, Style,
        input::{Key, MouseButton, NamedKey, PointerKind},
    },
    ui::{
        Canvas, Cursor, Ui,
        controls::{Chip, Clicks, Tone, chips_width, compact, draw_chips, hit, place},
    },
};

/// What a click or key on the tab row asks for.
pub(super) enum TabAction {
    Activate(usize),
    Close(u64),
    New,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TabButton {
    Close(u64),
    New,
}

/// The tab row's scroll position and button state.
#[derive(Default)]
pub(super) struct TabBar {
    start: usize,
    buttons: Clicks<TabButton>,
    /// The pointer is on a tab or button, where pressing clicks rather than drags.
    over_control: bool,
}

/// The status bar's chips.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusButton {
    Palette,
    Save,
    Terminal,
    Encoding,
}

/// Everything the bottom row shows, gathered before the interface borrows the editor.
pub(super) struct StatusBar {
    location: String,
    encoding: &'static str,
    dirty: bool,
    message: String,
}

impl StatusBar {
    pub fn new(editor: &super::Editor) -> Self {
        let document = editor.active_document();
        let progress = editor
            .tabs
            .get(editor.active)
            .and_then(|tab| tab.io.as_ref())
            .map(IoJob::label);
        let status = if editor.status.is_empty() {
            "F6 switch pane"
        } else {
            &editor.status
        };
        let message = status_message(status, progress.as_deref());
        Self {
            location: document.map_or_else(String::new, |doc| {
                let [column, row] = doc.position();
                format!("Ln {}, Col {}", row + 1, column + 1)
            }),
            encoding: document.map_or("", |doc| doc.encoding().name()),
            dirty: document.is_some_and(super::Document::dirty),
            message,
        }
    }

    /// Draw the row, returning the command of a clicked chip.
    pub fn draw(
        &self,
        canvas: &mut Canvas<'_>,
        clicks: &mut Clicks<StatusButton>,
    ) -> Option<Command> {
        let [width, _] = canvas.size();
        let theme = canvas.theme;
        let muted = Style::new(theme.muted, theme.title);
        canvas.fill(CellRect::new(0, 0, width, 1), muted);
        let palette_keys = format!("{}+P", primary_name());
        let save_keys = format!("{}+S", primary_name());
        let terminal_keys = format!("{}+Shift+J", primary_name());
        let chip = |keys, label, tone, action| Chip {
            keys,
            label,
            tone,
            action,
        };
        let save_tone = if self.dirty {
            Tone::Primary
        } else {
            Tone::Plain
        };
        let left = compact(
            &[
                chip(
                    &palette_keys,
                    "Commands",
                    Tone::Plain,
                    StatusButton::Palette,
                ),
                chip(&save_keys, "Save", save_tone, StatusButton::Save),
                chip(
                    &terminal_keys,
                    "Terminal",
                    Tone::Plain,
                    StatusButton::Terminal,
                ),
            ],
            width * 2 / 3,
        );
        let right = [chip("", self.encoding, Tone::Plain, StatusButton::Encoding)];
        let right_x = width.saturating_sub(chips_width(&right));
        let mut placed = place(&left, 0);
        placed.extend(place(&right, right_x));
        let clicked = draw_chips(canvas, clicks, &placed);
        let text_x = chips_width(&left) + 2;
        let location_x = right_x.saturating_sub(self.location.chars().count() as u32 + 2);
        if location_x > text_x {
            canvas.text(location_x as i32, 0, &self.location, muted);
        }
        let room = location_x.saturating_sub(text_x + 1) as usize;
        let message: String = self.message.chars().take(room).collect();
        canvas.text(text_x as i32, 0, &message, muted);
        clicked.map(|button| match button {
            StatusButton::Palette => Command::Palette,
            StatusButton::Save => Command::Save,
            StatusButton::Terminal => Command::Terminal,
            StatusButton::Encoding => Command::ReopenEncoding,
        })
    }
}

/// The status text, with any file progress after it so the message stays in place.
fn status_message(status: &str, progress: Option<&str>) -> String {
    match progress {
        Some(progress) => format!("{status}  {progress}"),
        None => status.to_owned(),
    }
}

/// Draw the document pane's content: the active document, its loading message or,
/// with no tabs open, a welcome screen. Returns any editing error.
pub(super) fn draw_document(
    ui: &mut Ui<'_>,
    tab: Option<&mut Tab>,
    focus: bool,
    word_wrap: bool,
    matches: &[Range<usize>],
) -> Option<String> {
    let height = ui.available_height();
    let loading = tab
        .as_ref()
        .is_some_and(|tab| matches!(tab.io, Some(IoJob::Load { .. })));
    let mut canvas = ui.canvas("document", height, tab.is_some() && !loading);
    if focus {
        canvas.request_focus();
    }
    let theme = canvas.theme;
    let accent = Style::new(theme.accent, theme.panel);
    let muted = Style::new(theme.muted, theme.panel);
    match tab {
        Some(tab) if !loading => {
            return tab
                .view
                .draw(&mut canvas, &mut tab.document, word_wrap, matches);
        }
        Some(tab) => {
            let message = tab.io.as_ref().map_or_else(String::new, IoJob::label);
            canvas.text(3, 2, &message, accent);
            canvas.text(3, 4, "Close this tab to cancel", muted);
        }
        None => {
            canvas.text(3, 2, "teditor", accent);
            let text = Style::new(theme.text, theme.panel);
            canvas.text(3, 4, "Open a file from the tree", text);
            canvas.text(3, 5, "Cmd+P  Find a file or command", muted);
            canvas.text(3, 6, "Cmd+N  New file", muted);
        }
    }
    None
}

/// Overlay the editor pane's header with a click target that edits the path.
pub(super) fn draw_title(canvas: &mut Canvas<'_>, title: &str, clicks: &mut Clicks<()>) -> bool {
    let [width, _] = canvas.size();
    let whole = CellRect::new(0, 0, width, 1);
    let clicked = clicks.update(canvas, &[(whole, ())]).is_some();
    if canvas.pointer().is_some() {
        let theme = canvas.theme;
        canvas.set_cursor(Cursor::Text);
        canvas.fill(whole, Style::new(theme.text, theme.control));
        canvas.text(1, 0, title, Style::new(theme.accent, theme.control));
        let hint = format!("Click to edit  {}+L", primary_name());
        let hint_x = width.saturating_sub(hint.chars().count() as u32 + 1);
        if hint_x > title.chars().count() as u32 + 2 {
            canvas.text(
                hint_x as i32,
                0,
                &hint,
                Style::new(theme.muted, theme.control),
            );
        }
        canvas.graphic(
            whole,
            Graphic::Divider {
                vertical: false,
                color: theme.active_border,
            },
        );
    }
    clicked
}

/// What happened on the terminal panel's header this frame.
pub(super) struct TerminalBar {
    pub tab: Option<TabAction>,
    /// The panel height a drag asked for.
    pub rows: Option<u32>,
    /// The color of the line above the panel: lit like a split's divider when a press here
    /// would resize it or a drag is under way, and otherwise showing focus like a pane header.
    pub edge: Color,
}

/// Overlay the terminal panel's header with its tabs and resize handle.
/// Dragging anywhere on the header resizes the panel, except from a button.
pub(super) fn draw_terminal_bar(
    canvas: &mut Canvas<'_>,
    labels: &[(u64, String)],
    active: usize,
    tab_bar: &mut TabBar,
    resizing: &mut bool,
    screen_height: u32,
    focused: bool,
) -> TerminalBar {
    let [width, _] = canvas.size();
    if canvas.pointer().is_some() {
        canvas.set_cursor(Cursor::ResizeRows);
    }
    let theme = canvas.theme;
    let tab = tabs(canvas, width, labels, active, tab_bar);
    let on_button = tab_bar.buttons.pressed().is_some();
    let mut rows = None;
    for event in &canvas.events {
        let InputEvent::Pointer(pointer) = event else {
            continue;
        };
        match pointer.kind {
            PointerKind::Down(MouseButton::Left) => *resizing = !on_button,
            PointerKind::Move if *resizing => {
                if let Some(position) = pointer.position {
                    let y = position.grid[1].max(0.0) as u32;
                    rows = Some(screen_height.saturating_sub(1).saturating_sub(y));
                }
            }
            PointerKind::Up(MouseButton::Left) => *resizing = false,
            _ => {}
        }
    }
    if !canvas.response.active {
        *resizing = false;
    }
    let resizable = *resizing || (canvas.pointer().is_some() && !tab_bar.over_control);
    TerminalBar {
        tab,
        rows,
        edge: if resizable {
            theme.accent
        } else if focused && canvas.window_focused {
            theme.active_border
        } else {
            theme.border
        },
    }
}

/// Draw a row of tabs in the canvas's first `width` columns, with overflow arrows and a
/// new-tab button, returning what a click, wheel or arrow key asked for.
pub(super) fn tabs(
    canvas: &mut Canvas<'_>,
    width: u32,
    labels: &[(u64, String)],
    active: usize,
    bar: &mut TabBar,
) -> Option<TabAction> {
    let theme = canvas.theme;
    // A fixed new-tab button sits at the right edge, past the overflow arrow.
    let new_width = if width >= 12 { 3 } else { 0 };
    let arrow = width.saturating_sub(1 + new_width);
    let start = &mut bar.start;
    let tab_width =
        |index: usize| (columns(&labels[index].1) as u32 + 4).min(arrow.saturating_sub(1));
    if active < *start {
        *start = active;
    }
    while *start < active && (*start..=active).map(tab_width).sum::<u32>() > arrow.saturating_sub(1)
    {
        *start += 1;
    }
    *start = (*start).min(labels.len().saturating_sub(1));
    canvas.fill(
        CellRect::new(0, 0, width, 1),
        Style::new(theme.muted, theme.title),
    );
    let mut x = 1;
    let mut regions = Vec::new();
    for (index, (id, _)) in labels.iter().enumerate().skip(*start) {
        let n = tab_width(index);
        if n == 0 || x + n > arrow {
            break;
        }
        let close = x + n - if n >= 3 { 2 } else { 1 };
        regions.push((x, x + n, close, index, *id));
        x += n;
    }
    let mut buttons: Vec<_> = regions
        .iter()
        .map(|(_, _, close, _, id)| (CellRect::new(*close, 0, 1, 1), TabButton::Close(*id)))
        .collect();
    if new_width > 0 {
        buttons.push((
            CellRect::new(width - new_width, 0, new_width, 1),
            TabButton::New,
        ));
    }
    let clicked = bar.buttons.update(canvas, &buttons);
    let pressed = bar.buttons.pressed();
    let pointer = canvas.pointer();
    let hovered_button = pointer.and_then(|point| hit(&buttons, point));
    let hovered_tab = pointer.and_then(|point| {
        regions
            .iter()
            .find(|(a, b, _, _, _)| point[0] >= f64::from(*a) && point[0] < f64::from(*b))
            .map(|(_, _, _, index, _)| *index)
    });
    let more_left = *start > 0;
    let more_right = regions
        .last()
        .is_some_and(|(_, _, _, index, _)| index + 1 < labels.len());
    bar.over_control = hovered_button.is_some()
        || hovered_tab.is_some()
        || pointer.is_some_and(|point| {
            let column = point[0] as u32;
            (column == 0 && more_left) || (column == arrow && more_right)
        });
    if bar.over_control {
        canvas.set_cursor(Cursor::Pointer);
    }
    for (x, end, close, index, id) in &regions {
        let n = end - x;
        let background = if *index == active {
            theme.control
        } else if hovered_tab == Some(*index) {
            theme.hover
        } else {
            theme.title
        };
        canvas.fill(
            CellRect::new(*x, 0, n, 1),
            Style::new(theme.text, background),
        );
        let mut room = n.saturating_sub(4);
        let label: String = labels[*index]
            .1
            .chars()
            .take_while(|ch| {
                let fits = char_columns(*ch) <= room;
                room = room.saturating_sub(char_columns(*ch));
                fits
            })
            .collect();
        canvas.text(
            (*x + 1) as i32,
            0,
            &label,
            Style::new(
                if *index == active {
                    theme.accent
                } else if hovered_tab == Some(*index) {
                    theme.text
                } else {
                    theme.muted
                },
                background,
            ),
        );
        let button = TabButton::Close(*id);
        canvas.text(
            *close as i32,
            0,
            "×",
            if pressed == Some(button) {
                Style::new(theme.desktop, theme.danger)
            } else if hovered_button == Some(button) {
                Style::new(theme.danger, theme.hover)
            } else {
                Style::new(theme.muted, background)
            },
        );
    }
    if more_left {
        canvas.text(0, 0, "‹", Style::new(theme.accent, theme.title));
    }
    if more_right {
        canvas.text(arrow as i32, 0, "›", Style::new(theme.accent, theme.title));
    }
    if new_width > 0 {
        canvas.text(
            (width - new_width) as i32,
            0,
            " + ",
            if pressed == Some(TabButton::New) {
                Style::new(theme.desktop, theme.accent)
            } else if hovered_button == Some(TabButton::New) {
                Style::new(theme.accent, theme.hover)
            } else {
                Style::new(theme.muted, theme.title)
            },
        );
    }
    let mut action = clicked.map(|button| match button {
        TabButton::Close(id) => TabAction::Close(id),
        TabButton::New => TabAction::New,
    });
    for event in &canvas.events {
        match event {
            InputEvent::Pointer(p) if matches!(p.kind, PointerKind::Down(MouseButton::Left)) => {
                if pressed.is_some() {
                    continue;
                }
                if let Some(point) = p.position.map(|p| canvas.local(p.grid)) {
                    let x = point[0] as u32;
                    action = if x == 0 && more_left {
                        Some(TabAction::Activate(start.saturating_sub(1)))
                    } else if x == arrow {
                        regions.last().and_then(|(_, _, _, i, _)| {
                            (i + 1 < labels.len()).then_some(TabAction::Activate(i + 1))
                        })
                    } else {
                        regions
                            .iter()
                            .find(|(a, b, _, _, _)| x >= *a && x < *b)
                            .map(|(_, _, _, i, _)| TabAction::Activate(*i))
                    };
                }
            }
            InputEvent::Scroll(scroll) if !labels.is_empty() => {
                action = Some(TabAction::Activate(
                    if scroll.cells[1] + scroll.cells[0] < 0.0 {
                        (active + 1).min(labels.len() - 1)
                    } else {
                        active.saturating_sub(1)
                    },
                ));
            }
            InputEvent::Key(key) => match key.logical_key {
                Key::Named(NamedKey::ArrowLeft) => {
                    action = Some(TabAction::Activate(active.saturating_sub(1)))
                }
                Key::Named(NamedKey::ArrowRight) if !labels.is_empty() => {
                    action = Some(TabAction::Activate((active + 1).min(labels.len() - 1)))
                }
                _ => {}
            },
            _ => {}
        }
    }
    action
}

#[cfg(test)]
mod tests {
    use super::status_message;

    #[test]
    fn progress_follows_the_status_instead_of_replacing_it() {
        assert_eq!(status_message("F6 switch pane", None), "F6 switch pane");
        assert_eq!(
            status_message("F6 switch pane", Some("Reading… 0%")),
            "F6 switch pane  Reading… 0%"
        );
    }
}
