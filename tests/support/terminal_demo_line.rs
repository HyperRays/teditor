use super::{ACCENT, FOREGROUND, MUTED, PANEL};
use teditor::{
    renderer::{CellRect, Color},
    terminal::{
        Cell, Framebuffer, Style,
        input::{NamedKey, Preedit},
    },
};

/// Small single-line input for exercising terminal events. Like the framebuffer,
/// it navigates Unicode scalars; document/grapheme editing is a later editor layer.
#[derive(Default)]
pub(super) struct LineInput {
    pub text: String,
    cursor: usize,
}

impl LineInput {
    pub fn insert(&mut self, text: &str) {
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn key(&mut self, key: NamedKey) {
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        let next = self.cursor
            + self.text[self.cursor..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
        match key {
            NamedKey::ArrowLeft => self.cursor = previous,
            NamedKey::ArrowRight => self.cursor = next,
            NamedKey::Home => self.cursor = 0,
            NamedKey::End => self.cursor = self.text.len(),
            NamedKey::Backspace => {
                self.text.replace_range(previous..self.cursor, "");
                self.cursor = previous;
            }
            NamedKey::Delete => {
                self.text.replace_range(self.cursor..next, "");
            }
            _ => {}
        }
    }

    fn cursor_column(&self, preedit: Option<&Preedit>) -> usize {
        self.text[..self.cursor].chars().count()
            + preedit
                .and_then(|p| {
                    p.cursor
                        .map(|(start, _)| p.text.get(..start).unwrap_or("").chars().count())
                })
                .unwrap_or(0)
    }

    pub fn caret_rect(&self, size: [u32; 2], preedit: Option<&Preedit>) -> CellRect {
        let width = size[0].saturating_sub(11);
        if width == 0 || size[1] < 2 {
            return CellRect::new(0, 0, 0, 0);
        }
        CellRect::new(
            9 + self.cursor_column(preedit).min(width as usize - 1) as u32,
            size[1] - 2,
            1,
            1,
        )
    }

    pub fn click(&mut self, column: u32, size: [u32; 2]) {
        let width = size[0].saturating_sub(11) as usize;
        let start = self
            .cursor_column(None)
            .saturating_sub(width.saturating_sub(1));
        let character = start + column.saturating_sub(9) as usize;
        self.cursor = self
            .text
            .char_indices()
            .nth(character)
            .map_or(self.text.len(), |(index, _)| index);
    }

    pub fn paint(&self, frame: &mut Framebuffer, focused: bool, preedit: Option<&Preedit>) {
        let [columns, rows] = frame.size();
        if columns <= 11 || rows < 2 {
            return;
        }
        let style = Style {
            foreground: FOREGROUND,
            background: PANEL,
        };
        frame.fill(
            CellRect::new(2, rows - 2, columns - 4, 1),
            Cell::new(' ', style),
        );
        frame.write(
            2,
            rows - 2,
            "Input: ",
            Style {
                foreground: if focused { ACCENT } else { MUTED },
                ..style
            },
        );
        let width = columns as usize - 11;
        let cursor = self.cursor_column(preedit);
        let start = cursor.saturating_sub(width - 1);
        let before = self.text[..self.cursor].chars().count();
        let composing = preedit.map_or("", |p| p.text.as_str());
        let composition_end = before + composing.chars().count();
        let selection = preedit.and_then(|p| p.cursor).map(|(a, b)| {
            (
                before + composing.get(..a.min(b)).unwrap_or("").chars().count(),
                before + composing.get(..a.max(b)).unwrap_or("").chars().count(),
            )
        });
        let display = format!(
            "{}{composing}{}",
            &self.text[..self.cursor],
            &self.text[self.cursor..]
        );
        for (offset, character) in display.chars().enumerate().skip(start).take(width) {
            let selected = selection.is_some_and(|(a, b)| offset >= a && offset < b);
            frame.set(
                9 + (offset - start) as u32,
                rows - 2,
                Cell::new(
                    character,
                    Style {
                        foreground: if offset >= before && offset < composition_end {
                            Color::rgb(226, 183, 112)
                        } else {
                            FOREGROUND
                        },
                        background: if selected {
                            Color::rgb(80, 65, 35)
                        } else {
                            PANEL
                        },
                    },
                ),
            );
        }
        if display.is_empty() {
            frame.write(
                9,
                rows - 2,
                &"Type here…".chars().take(width).collect::<String>(),
                Style {
                    foreground: MUTED,
                    ..style
                },
            );
        }
        if focused && preedit.is_none_or(|p| p.cursor.is_some()) {
            frame.style(
                self.caret_rect(frame.size(), preedit),
                Style {
                    foreground: PANEL,
                    background: ACCENT,
                },
            );
        }
    }
}
