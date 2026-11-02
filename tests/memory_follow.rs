//! Following a growing log must not hold it in memory, however much it grows.
#![cfg(unix)]

// Only the memory limit is shared with the other memory tests.
#[allow(dead_code)]
#[path = "support/large_file.rs"]
mod large_file;

use large_file::{FILE_MIB, assert_peak_within_limit};
use std::{io::Write, time::Duration};
use teditor::{editor::Editor, terminal::Framebuffer, ui::Theme};

#[test]
fn following_a_growing_log_does_not_fill_memory() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("growing.log");
    std::fs::write(&path, "start\n").unwrap();
    let mut frame = Framebuffer::new([160, 50], Theme::default().panel).unwrap();
    let mut editor = Editor::new(workspace.path()).unwrap();
    editor.open(&path).unwrap();
    editor.compose(&mut frame);
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    let line = "2026-10-30 12:00:00 INFO something happened in the service\n";
    let block = line.repeat(1024 * 1024 / line.len());
    let mut written = 6;
    for _ in 0..FILE_MIB {
        log.write_all(block.as_bytes()).unwrap();
        written += block.len();
    }
    log.flush().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while editor.active_document().unwrap().text.len() < written {
        assert!(std::time::Instant::now() < deadline, "never caught up");
        if !editor.needs_repaint() {
            std::thread::sleep(Duration::from_millis(5));
        }
        editor.compose(&mut frame);
    }
    assert!(!editor.active_document().unwrap().dirty());
    assert_peak_within_limit();
}
