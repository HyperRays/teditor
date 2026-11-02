//! The system clipboard: the Win32 API on Windows, and command-line helpers elsewhere.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{read, write_with};

#[cfg(not(windows))]
use std::{
    io::{self, Write},
    process::{Command, Stdio},
};

#[cfg(not(windows))]
pub fn read() -> io::Result<String> {
    let (program, args) = command(false)?;
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        if holds_no_text(&output.stderr) {
            return Ok(String::new());
        }
        return Err(io::Error::other("Clipboard is unavailable"));
    }
    String::from_utf8(output.stdout).map_err(io::Error::other)
}
/// Whether a failed helper only found no text, which `xclip` and `wl-paste` report as an error
/// when the clipboard is empty or holds something else, such as an image.
#[cfg(not(windows))]
fn holds_no_text(stderr: &[u8]) -> bool {
    let message = String::from_utf8_lossy(stderr);
    [
        "not available",
        "Nothing is copied",
        "No selection",
        "No suitable type",
    ]
    .iter()
    .any(|report| message.contains(report))
}
#[cfg(not(windows))]
pub fn write_with(write: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> io::Result<()> {
    let (program, args) = command(true)?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let result = write(
        &mut child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("Clipboard input unavailable"))?,
    );
    let status = child.wait()?;
    result?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("Clipboard is unavailable"))
    }
}
#[cfg(not(windows))]
fn command(write: bool) -> io::Result<(&'static str, Vec<&'static str>)> {
    if cfg!(target_os = "macos") {
        Ok((
            if write {
                "/usr/bin/pbcopy"
            } else {
                "/usr/bin/pbpaste"
            },
            vec![],
        ))
    } else if cfg!(target_os = "linux") {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            Ok((
                if write { "wl-copy" } else { "wl-paste" },
                if write { vec![] } else { vec!["--no-newline"] },
            ))
        } else {
            Ok((
                "xclip",
                vec!["-selection", "clipboard", if write { "-i" } else { "-o" }],
            ))
        }
    } else {
        Err(io::Error::other(
            "System clipboard is supported on macOS, Linux and Windows",
        ))
    }
}

/// Text as the Windows clipboard holds it: UTF-16 with CRLF line endings and a final NUL.
#[cfg(any(windows, test))]
fn to_clipboard_text(text: &str) -> Vec<u16> {
    let mut units = Vec::with_capacity(text.len() + 1);
    let mut previous = '\0';
    let mut buffer = [0; 2];
    for character in text.chars() {
        if character == '\n' && previous != '\r' {
            units.push(u16::from(b'\r'));
        }
        units.extend_from_slice(character.encode_utf16(&mut buffer));
        previous = character;
    }
    units.push(0);
    units
}

/// The text in the Windows clipboard's data, which may have space after its NUL. Line endings
/// become `\n`, as paste targets expect; a document converts them to its own when inserting.
#[cfg(any(windows, test))]
fn from_clipboard_text(units: &[u16]) -> String {
    let end = units.iter().position(|&unit| unit == 0);
    String::from_utf16_lossy(&units[..end.unwrap_or(units.len())]).replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    #[cfg(not(windows))]
    fn an_empty_clipboard_is_told_apart_from_an_unusable_one() {
        for empty in [
            "Error: target STRING not available\n",
            "Nothing is copied\n",
            "No selection\n",
            "No suitable type of content copied\n",
        ] {
            assert!(holds_no_text(empty.as_bytes()), "{empty}");
        }
        for broken in [
            "Error: Can't open display: :99\n",
            "Failed to connect to a Wayland server\n",
            "",
        ] {
            assert!(!holds_no_text(broken.as_bytes()), "{broken}");
        }
    }

    #[test]
    fn windows_text_gets_crlf_endings_and_a_final_nul() {
        assert_eq!(to_clipboard_text("a\nb\r\nc"), units("a\r\nb\r\nc\0"));
        assert_eq!(to_clipboard_text(""), [0]);
    }

    #[test]
    fn windows_text_keeps_characters_outside_the_basic_plane() {
        let text = "é 👩‍💻\n日本語";
        assert_eq!(from_clipboard_text(&to_clipboard_text(text)), text);
    }

    #[test]
    fn windows_text_stops_at_its_nul_and_survives_bad_data() {
        assert_eq!(from_clipboard_text(&units("one\r\ntwo\0junk")), "one\ntwo");
        assert_eq!(
            from_clipboard_text(&units("no terminator")),
            "no terminator"
        );
        assert_eq!(from_clipboard_text(&[0xd800, u16::from(b'x')]), "\u{fffd}x");
    }
}
