//! The system clipboard, through each platform's command-line helpers.

use std::{
    io::{self, Write},
    process::{Command, Stdio},
};

pub fn read() -> io::Result<String> {
    let (program, args) = command(false)?;
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::other("Clipboard is unavailable"));
    }
    String::from_utf8(output.stdout).map_err(io::Error::other)
}
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
            "System clipboard is supported on macOS and Linux",
        ))
    }
}
