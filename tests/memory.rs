//! Viewing, editing and saving a large file must cost memory for what is shown, not
//! for the file's size. `memory_highlight.rs` covers syntax highlighting.
#![cfg(unix)]

#[path = "support/large_file.rs"]
mod large_file;

use large_file::{assert_peak_within_limit, drive_editor, write_fixture};
use teditor::editor::{document::Document, search};

#[test]
fn large_files_cost_memory_for_what_is_shown_not_their_size() {
    let workspace = tempfile::tempdir().unwrap();
    // Not a recognized language, so this stays fast enough to always run.
    let path = workspace.path().join("large.txt");
    write_fixture(&path);

    // Saving reads the whole buffer, both untouched and edited.
    let mut document = Document::open(&path).unwrap();
    document.save(None).unwrap();
    document.set_cursor(document.text.len() / 2, false);
    document.insert("edit");
    document.save(None).unwrap();
    // Searching streams the text, however many lines match.
    let matcher = search::compile("compute", search::Options::default()).unwrap();
    let mut count = 0;
    search::scan(
        &document.text,
        &matcher,
        &std::sync::atomic::AtomicBool::new(false),
        |_, _, _| {
            count += 1;
            true
        },
    )
    .unwrap();
    assert!(count > 1_000_000);
    drop(document);

    drive_editor(workspace.path(), &path);
    assert_peak_within_limit();
}
