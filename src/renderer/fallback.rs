//! Finds fonts for characters the main font lacks: the bundled icons, then installed fonts on
//! any platform.
//!
//! Fonts are memory-mapped rather than read, so scanning hundreds of them, some of them
//! tens of megabytes, costs only the few pages actually used.

use memmap2::Mmap;
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::LazyLock,
};

/// One face in an installed font file.
#[derive(Clone, Copy)]
pub(super) struct FontFace {
    pub data: &'static [u8],
    pub index: u32,
}

/// Faces with useless or misleading coverage: placeholder glyphs for every character,
/// or pictures assigned to ordinary letters.
const EXCLUDED: [&str; 4] = ["LastResort", "Wingdings", "Webdings", "Bodoni Ornaments"];

/// Icons that fit the cell grid, bundled so that they look the same everywhere. Nearly all of
/// its characters are in the Private Use Area, so it does not compete with ordinary text.
static ICON_FONT: &[u8] = include_bytes!("../../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

impl FontFace {
    /// Whether this is the bundled icon font, whose symbols sit centered in a square.
    pub fn is_icons(&self) -> bool {
        std::ptr::eq(self.data, ICON_FONT)
    }
}

/// All usable faces, best first, found once on the first character that needs one.
static FACES: LazyLock<Vec<FontFace>> = LazyLock::new(|| {
    let mut files = Vec::new();
    for directory in font_directories() {
        collect_font_files(&directory, 0, &mut files);
    }
    files.sort();
    let mut ranked = Vec::new();
    for path in files {
        let Ok(file) = File::open(&path) else {
            continue;
        };
        // SAFETY: installed fonts are not modified while in use. The mapping is
        // deliberately kept for the life of the process, as faces borrow from it.
        let Ok(map) = (unsafe { Mmap::map(&file) }) else {
            continue;
        };
        let data: &'static [u8] = Box::leak(Box::new(map));
        for index in 0..ttf_parser::fonts_in_collection(data).unwrap_or(1) {
            let Ok(face) = ttf_parser::Face::parse(data, index) else {
                continue;
            };
            let tables = face.tables();
            let has_outlines =
                tables.glyf.is_some() || tables.cff.is_some() || tables.cff2.is_some();
            let excluded = face.names().into_iter().any(|name| {
                name.to_string()
                    .is_some_and(|name| EXCLUDED.iter().any(|bad| name.contains(bad)))
            });
            if has_outlines && !excluded {
                // Prefer faces that match the grid and the main font's upright weight.
                let rank = (!face.is_monospaced(), !face.is_regular(), face.is_italic());
                ranked.push((rank, FontFace { data, index }));
            }
        }
    }
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, face)| face).collect()
});

/// The best installed face for `text`: one with all of its characters if possible,
/// otherwise one with the first.
pub(super) fn find(text: &str) -> Option<FontFace> {
    let has = |face: &ttf_parser::Face<'_>, c: char| face.glyph_index(c).is_some();
    // Joiners and variation selectors are invisible, so a font need not have them.
    let visible = |c: &char| !matches!(*c, '\u{200C}' | '\u{200D}' | '\u{FE00}'..='\u{FE0F}');
    // Checked first, as finding what is installed means opening every font file there is.
    if let Ok(icons) = ttf_parser::Face::parse(ICON_FONT, 0)
        && text.chars().filter(visible).all(|c| has(&icons, c))
    {
        return Some(FontFace {
            data: ICON_FONT,
            index: 0,
        });
    }
    let mut first_only = None;
    for candidate in FACES.iter() {
        let Ok(face) = ttf_parser::Face::parse(candidate.data, candidate.index) else {
            continue;
        };
        if text.chars().filter(visible).all(|c| has(&face, c)) {
            return Some(*candidate);
        }
        if first_only.is_none() && text.chars().next().is_some_and(|c| has(&face, c)) {
            first_only = Some(*candidate);
        }
    }
    first_only
}

fn font_directories() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let under_home = |path: &str| home.as_ref().map(|home| home.join(path));
    if cfg!(target_os = "macos") {
        let mut directories = vec![
            PathBuf::from("/System/Library/Fonts"),
            PathBuf::from("/Library/Fonts"),
        ];
        directories.extend(under_home("Library/Fonts"));
        directories
    } else if cfg!(windows) {
        let variable = |name, path: &str| std::env::var_os(name).map(|v| Path::new(&v).join(path));
        [
            variable("WINDIR", "Fonts"),
            variable("LOCALAPPDATA", r"Microsoft\Windows\Fonts"),
        ]
        .into_iter()
        .flatten()
        .collect()
    } else {
        let mut directories = vec![
            PathBuf::from("/usr/share/fonts"),
            PathBuf::from("/usr/local/share/fonts"),
        ];
        directories.extend(under_home(".fonts"));
        directories.extend(under_home(".local/share/fonts"));
        directories
    }
}

fn collect_font_files(directory: &Path, depth: u32, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth < 8 {
                collect_font_files(&path, depth + 1, files);
            }
        } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            ["ttf", "otf", "ttc", "otc"].contains(&e.to_ascii_lowercase().as_str())
        }) {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_icons_are_found_without_installed_fonts() {
        // The icons the interface uses, and a powerline symbol that prompts commonly print.
        for icon in ["\u{ec0d}", "\u{eb7e}", "\u{e0b0}"] {
            let face = find(icon).expect("a bundled icon");
            assert!(face.is_icons(), "{icon:?}");
        }
        // Part of a cluster it lacks must not stop other fonts being asked.
        assert!(!find("한").is_some_and(|face| face.is_icons()));
    }

    /// Machines without fonts installed, such as CI containers, have nothing to find.
    #[test]
    fn installed_fonts_cover_other_scripts() {
        let started = std::time::Instant::now();
        if FACES.is_empty() {
            return;
        }
        eprintln!("{} faces in {:?}", FACES.len(), started.elapsed());
        for text in ["中", "あ", "한", "א", "ก", "क"] {
            assert!(find(text).is_some(), "no installed font has {text}");
        }
        assert!(find("\u{10FFFF}").is_none());
    }
}
