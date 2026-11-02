//! Shared by the memory tests, each of which needs a process of its own because
//! peak memory is per process.
use std::{
    io::Write,
    path::Path,
    time::{Duration, Instant},
};
use teditor::{
    editor::Editor,
    terminal::{
        Framebuffer, InputEvent,
        input::{
            Modifiers, MouseScrollDelta, PointerPosition, ScrollEvent, TextInput, TextSource,
            TouchPhase,
        },
    },
    ui::Theme,
};

pub const FILE_MIB: usize = 128;
/// Far below the file's size, so reading it into memory anywhere fails these tests.
pub const PEAK_LIMIT_MIB: usize = 64;

/// The process's peak resident memory so far, in MiB.
#[cfg(target_os = "linux")]
pub fn peak_mib() -> usize {
    // Not `getrusage`, whose peak survives `exec` here and so can be the parent's, such as Cargo's.
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|value| value.trim().strip_suffix("kB"))
        .and_then(|kib| kib.trim().parse::<usize>().ok())
        .expect("the status file reports peak memory");
    kib / 1024
}

/// The process's peak resident memory so far, in MiB.
#[cfg(not(target_os = "linux"))]
pub fn peak_mib() -> usize {
    // SAFETY: getrusage fills the zeroed structure it is given.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut usage), 0);
        usage
    };
    // macOS reports bytes; other Unix systems report KiB.
    let unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
    usage.ru_maxrss as usize * unit / (1024 * 1024)
}

/// Write source-like lines, so highlighting and line indexing do their real work.
pub fn write_fixture(path: &Path) {
    let line = format!("let value = compute(input); // {}\n", "x".repeat(47));
    let block = line.repeat(1024 * 1024 / line.len() + 1);
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    for _ in 0..FILE_MIB {
        file.write_all(&block.as_bytes()[..1024 * 1024]).unwrap();
    }
    file.flush().unwrap();
}

fn settle(editor: &mut Editor, frame: &mut Framebuffer) {
    let deadline = Instant::now() + Duration::from_secs(600);
    editor.compose(frame);
    while editor.has_pending_background_work() {
        assert!(Instant::now() < deadline, "background work timed out");
        std::thread::sleep(Duration::from_millis(1));
        editor.compose(frame);
    }
}

fn scroll(rows: f64) -> InputEvent {
    InputEvent::Scroll(ScrollEvent {
        delta: MouseScrollDelta::LineDelta(0.0, rows as f32),
        cells: [0.0, rows],
        phase: TouchPhase::Moved,
        position: Some(PointerPosition {
            physical: [880.0, 300.0],
            grid: [80.0, 12.0],
            cell: Some([80, 12]),
        }),
        modifiers: Modifiers::default(),
    })
}

/// Run the whole application over a file: open it, scroll far, then type.
pub fn drive_editor(workspace: &Path, path: &Path) {
    let mut frame = Framebuffer::new([160, 50], Theme::default().panel).unwrap();
    let mut editor = Editor::new(workspace).unwrap();
    editor.request_open(path).unwrap();
    settle(&mut editor, &mut frame);
    let lines = editor.active_document().unwrap().line_count();
    assert!(lines > 1_000_000);
    for rows in [lines as f64 / 2.0, 200.0, lines as f64 / 4.0] {
        editor.handle(scroll(-rows));
        settle(&mut editor, &mut frame);
    }
    assert!(editor.active_document().unwrap().scroll[1] > lines / 2);
    editor.handle(InputEvent::Text(TextInput {
        text: "typed".into(),
        source: TextSource::Keyboard,
        modifiers: Modifiers::default(),
    }));
    settle(&mut editor, &mut frame);
    assert!(editor.active_document().unwrap().dirty());
}

/// Fail if the process has ever used more memory than the limit.
pub fn assert_peak_within_limit() {
    let peak = peak_mib();
    eprintln!("peak memory with a {FILE_MIB} MiB file: {peak} MiB");
    assert!(
        peak < PEAK_LIMIT_MIB,
        "peak memory {peak} MiB exceeds {PEAK_LIMIT_MIB} MiB"
    );
}
