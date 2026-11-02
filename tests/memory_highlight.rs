//! Colors depend on every earlier line, so scrolling far through a source file makes
//! the highlighter scan all of them. That must not pull the file into memory. It takes
//! about 30 seconds: `cargo test --release --test memory_highlight -- --ignored`.
#![cfg(unix)]

#[path = "support/large_file.rs"]
mod large_file;

use large_file::{assert_peak_within_limit, drive_editor, write_fixture};

#[test]
#[ignore = "takes about 30 seconds in a release build"]
fn highlighting_far_into_a_large_file_does_not_load_it_into_memory() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("large.rs");
    write_fixture(&path);
    drive_editor(workspace.path(), &path);
    assert_peak_within_limit();
}
