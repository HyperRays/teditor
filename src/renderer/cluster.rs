//! Grapheme clusters: what a reader sees as one character, however many scalars it takes.
//!
//! A cell holds a single `char`, which keeps cells small and comparisons cheap. A cluster
//! of several scalars gets a *symbol* instead: a `char` from Unicode's last private-use
//! plane that stands for an entry in a process-wide table of cluster texts.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};
use unicode_segmentation::UnicodeSegmentation;

const FIRST_SYMBOL: u32 = 0x10_0000;
const LAST_SYMBOL: u32 = 0x10_FFFD;
const ZERO_WIDTH_JOINER: char = '\u{200D}';

#[derive(Default)]
struct Table {
    texts: Vec<Arc<str>>,
    symbols: HashMap<Arc<str>, char>,
}

static TABLE: LazyLock<Mutex<Table>> = LazyLock::new(Mutex::default);

/// The `char` a cell stores for one grapheme cluster: the scalar itself when there is only
/// one, or a symbol for [`cluster_text`] to look up. Distinct clusters are kept for the life
/// of the process, so this stays small; if its 65,534 symbols run out, new clusters show
/// only their first scalar.
pub fn symbol(cluster: &str) -> char {
    let mut scalars = cluster.chars();
    let first = scalars.next().unwrap_or(' ');
    if scalars.next().is_none() && !is_symbol(first) {
        return first;
    }
    let mut table = TABLE.lock().unwrap();
    if let Some(&symbol) = table.symbols.get(cluster) {
        return symbol;
    }
    let Some(symbol) = char::from_u32(FIRST_SYMBOL + table.texts.len() as u32)
        .filter(|symbol| u32::from(*symbol) <= LAST_SYMBOL)
    else {
        return first;
    };
    let text: Arc<str> = cluster.into();
    table.texts.push(text.clone());
    table.symbols.insert(text, symbol);
    symbol
}

/// Whether a cell's `char` stands for a cluster rather than being the character itself.
pub fn is_symbol(character: char) -> bool {
    (FIRST_SYMBOL..=LAST_SYMBOL).contains(&u32::from(character))
}

/// The text a [`symbol`] stands for.
pub fn cluster_text(symbol: char) -> Option<Arc<str>> {
    let index = u32::from(symbol).checked_sub(FIRST_SYMBOL)?;
    TABLE.lock().unwrap().texts.get(index as usize).cloned()
}

/// Cells a scalar occupies by itself: two for wide characters, none for those that only
/// modify their neighbor, such as combining marks. See [`Columns`] for sequences.
pub fn char_columns(character: char) -> u32 {
    if character.is_ascii() {
        return 1;
    }
    // Skin tones attach to the emoji before them, though they are wide alone.
    if matches!(character, '\u{1F3FB}'..='\u{1F3FF}') {
        return 0;
    }
    match unicode_width::UnicodeWidthChar::width(character) {
        Some(columns) => columns as u32,
        None => 1,
    }
}

/// Measures scalars in sequence. Everything that lays out text must agree on this, so
/// that a document's columns and the cells it is drawn into line up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Columns {
    joined: bool,
}

impl Columns {
    /// Continue measuring after text known to end with a joiner, or not.
    pub fn after(joined: bool) -> Self {
        Self { joined }
    }

    /// Cells the next scalar adds: the second emoji of a joined pair adds none.
    pub fn next(&mut self, character: char) -> u32 {
        let joined = std::mem::replace(&mut self.joined, character == ZERO_WIDTH_JOINER);
        if joined && is_joinable(character) {
            0
        } else {
            char_columns(character)
        }
    }

    /// Whether the last scalar was a joiner, so that the next may be absorbed.
    pub fn joined(self) -> bool {
        self.joined
    }
}

/// Symbols and emoji join with a zero-width joiner into one picture. In other scripts the
/// joiner only selects a letter's form, and the following letter keeps its own width.
pub fn is_joinable(character: char) -> bool {
    u32::from(character) >= 0x2190
}

/// Cells a whole string occupies.
pub fn text_columns(text: &str) -> u32 {
    let mut columns = Columns::default();
    text.chars().map(|c| columns.next(c)).sum()
}

/// Call `visit` with the symbol and width of each grapheme cluster in `text`.
pub fn for_each_cluster(text: &str, mut visit: impl FnMut(char, u32)) {
    if text.is_ascii() {
        for byte in text.bytes() {
            visit(char::from(byte), 1);
        }
        return;
    }
    for cluster in text.graphemes(true) {
        visit(symbol(cluster), text_columns(cluster));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clusters_share_one_symbol_and_single_scalars_stay_themselves() {
        assert_eq!(symbol("a"), 'a');
        assert_eq!(symbol("中"), '中');
        let accented = symbol("e\u{301}");
        assert!(is_symbol(accented));
        assert_eq!(symbol("e\u{301}"), accented);
        assert_eq!(&*cluster_text(accented).unwrap(), "e\u{301}");
        assert_ne!(symbol("a\u{301}"), accented);
        // A real character from the symbols' plane must not be read as a cluster.
        let real = symbol("\u{10FFF0}");
        assert_eq!(&*cluster_text(real).unwrap(), "\u{10FFF0}");
    }

    #[test]
    fn widths_count_what_a_reader_sees() {
        for (text, expected) in [
            ("a", 1),
            ("中", 2),
            ("e\u{301}", 1),
            ("\u{1F469}\u{200D}\u{1F4BB}", 2), // technologist
            ("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}", 2), // family
            ("\u{1F44D}\u{1F3FD}", 2),         // skin tone
            ("\u{1F1E9}\u{1F1EA}", 2),         // flag
            ("\u{1F937}\u{200D}\u{2640}\u{FE0F}", 2),
            ("1\u{FE0F}\u{20E3}", 1), // keycap
            ("한", 2),
            ("\u{1112}\u{1161}\u{11AB}", 2), // the same syllable, in jamo
            ("क\u{94D}\u{200D}ष", 2),        // an Indic joiner keeps the next letter's width
        ] {
            assert_eq!(text_columns(text), expected, "{text:?}");
        }
    }

    #[test]
    fn clusters_are_found_across_mixed_text() {
        let mut seen = Vec::new();
        for_each_cluster("ae\u{301}\u{1F1E9}\u{1F1EA}中", |symbol, columns| {
            seen.push((cluster_text(symbol).map(|t| t.to_string()), symbol, columns));
        });
        let widths: Vec<_> = seen.iter().map(|entry| entry.2).collect();
        assert_eq!(widths, [1, 1, 2, 2]);
        assert_eq!(seen[1].0.as_deref(), Some("e\u{301}"));
        assert_eq!(seen[2].0.as_deref(), Some("\u{1F1E9}\u{1F1EA}"));
        assert_eq!(seen[3].1, '中');
    }
}
