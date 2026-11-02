//! Canonical paths that read the same on every platform.

use std::{
    io,
    path::{Path, PathBuf},
};

/// [`Path::canonicalize`], without the `\\?\` prefix it adds on Windows wherever the plain path
/// means the same. Prefixed paths compare unequal to plain ones, such as those from ctags, and
/// many programs reject them.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    #[cfg(windows)]
    if let Some(plain) = canonical.to_str().and_then(without_verbatim_prefix) {
        return Ok(PathBuf::from(plain));
    }
    Ok(canonical)
}

/// `path` without its prefix, if it is on a drive and Windows would read it the same way
/// unprefixed. Network paths and those plain paths cannot express keep the prefix.
#[cfg(any(windows, test))]
fn without_verbatim_prefix(path: &str) -> Option<&str> {
    /// The longest plain path, including its final NUL.
    const MAX_PATH: usize = 260;
    let plain = path.strip_prefix(r"\\?\")?;
    let (drive, rest) = plain.split_at_checked(3)?;
    let &[letter, b':', b'\\'] = drive.as_bytes() else {
        return None;
    };
    (letter.is_ascii_alphabetic()
        && plain.encode_utf16().count() < MAX_PATH
        && rest.split('\\').all(plain_name_is_literal))
    .then_some(plain)
}

/// Whether Windows takes this file name literally in a plain path, rather than trimming its end
/// or treating it as a device.
#[cfg(any(windows, test))]
fn plain_name_is_literal(name: &str) -> bool {
    if name.ends_with(['.', ' ']) || name.contains(['<', '>', ':', '"', '/', '|', '?', '*']) {
        return false;
    }
    // A device name stays one with an extension: `nul.txt` is still NUL.
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let stem = stem.to_ascii_uppercase();
    let numbered = |prefix| {
        stem.strip_prefix(prefix).is_some_and(|number| {
            matches!(
                number,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    };
    !(matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT"))
}

#[cfg(test)]
mod tests {
    use super::without_verbatim_prefix as plain;

    #[test]
    fn ordinary_drive_paths_lose_the_prefix() {
        assert_eq!(plain(r"\\?\C:\Users\me\a.rs"), Some(r"C:\Users\me\a.rs"));
        assert_eq!(plain(r"\\?\d:\"), Some(r"d:\"));
        assert_eq!(plain(r"\\?\C:\日本語\.git"), Some(r"C:\日本語\.git"));
        assert_eq!(
            plain(r"\\?\C:\console\nullable.txt\com10"),
            Some(r"C:\console\nullable.txt\com10")
        );
    }

    #[test]
    fn paths_that_need_the_prefix_keep_it() {
        for path in [
            r"C:\already\plain",
            r"\\?\UNC\server\share\a.rs",
            r"\\?\Volume{1234}\a.rs",
            r"\\?\C:\dir\nul",
            r"\\?\C:\NUL.txt",
            r"\\?\C:\Aux .tar.gz",
            r"\\?\C:\dir\com1",
            r"\\?\C:\lpt9.log",
            r"\\?\C:\com².txt",
            r"\\?\C:\dir\trailing.",
            r"\\?\C:\trailing \a.rs",
            r"\\?\C:\a:stream",
        ] {
            assert_eq!(plain(path), None, "{path}");
        }
    }

    #[test]
    fn the_length_limit_counts_utf16_units() {
        let at = |length: usize, fill: &str| format!(r"\\?\C:\{}", fill.repeat(length - 3));
        assert!(plain(&at(259, "a")).is_some());
        assert!(plain(&at(260, "a")).is_none());
        // Each is three UTF-8 bytes but one unit, so this still fits.
        assert!(plain(&at(259, "日")).is_some());
    }
}
