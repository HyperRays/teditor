//! Keeps untouched documents up to date as their files change, as when watching a log, and
//! the file tree up to date as its folders change.
//!
//! One shared thread checks each watched path's size and time ten times a second, which
//! costs a few microseconds and wakes nothing unless one changed. Polling works the same
//! on every platform and on network drives. New bytes are appended to the document rather
//! than reloading it, and reach disk in batches, so a growing log cannot fill memory.

use super::buffer::{Snapshot, TextBuffer};
use crate::vt::Waker;
use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Most bytes taken in one frame, so a burst of output cannot stall the interface.
const READ_LIMIT: usize = 1024 * 1024;
/// Appended text moves from memory to the private log file in batches of this size.
const SPILL_BYTES: usize = 1024 * 1024;
/// How much of the document's end must still match the file, to tell growth from a rewrite.
const CHECK_BYTES: usize = 4096;

/// What following a file found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Followed {
    /// Text was added, or there was nothing new.
    Current,
    /// Text was added, and more is waiting.
    More,
    /// The file no longer starts with what was loaded, so it must be loaded again.
    Rewritten,
}

/// How far a document has followed its file.
pub(crate) struct Tail {
    hash: blake3::Hasher,
    disk_length: u64,
    modified: Option<SystemTime>,
    /// Where text not yet in the log file starts.
    in_memory_from: usize,
    /// One private file for all appended text, so following needs a single descriptor.
    log: Option<Arc<File>>,
    log_length: u64,
}

impl Tail {
    pub fn new(
        hash: blake3::Hasher,
        disk_length: u64,
        modified: Option<SystemTime>,
        text: &TextBuffer,
    ) -> Self {
        Self {
            hash,
            disk_length,
            modified,
            in_memory_from: text.len(),
            log: None,
            log_length: 0,
        }
    }

    /// The hash of everything taken from the file so far.
    pub fn hash(&self) -> blake3::Hash {
        self.hash.finalize()
    }

    /// Whether the file's size or time differs from what was last taken from it.
    pub fn file_changed(&self, path: &Path) -> io::Result<bool> {
        let metadata = fs::metadata(path)?;
        Ok(metadata.len() != self.disk_length || metadata.modified().ok() != self.modified)
    }

    /// Append what the file has gained to `text`, up to one frame's limit.
    pub fn read_new(&mut self, path: &Path, text: &mut TextBuffer) -> io::Result<Followed> {
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        let (length, modified) = (metadata.len(), metadata.modified().ok());
        if length < self.disk_length || (length == self.disk_length && modified != self.modified) {
            return Ok(Followed::Rewritten);
        }
        if length == self.disk_length {
            return Ok(Followed::Current);
        }
        if !self.end_matches(&mut file, text)? {
            return Ok(Followed::Rewritten);
        }
        let mut block = vec![0; READ_LIMIT.min((length - self.disk_length) as usize)];
        file.seek(SeekFrom::Start(self.disk_length))?;
        file.read_exact(&mut block)?;
        let valid = match std::str::from_utf8(&block) {
            Ok(_) => block.len(),
            // A character cut off at the end finishes in a later read.
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Ok(Followed::Rewritten),
        };
        let added = std::str::from_utf8(&block[..valid]).expect("validated above");
        text.append(&TextBuffer::from_text(added));
        self.hash.update(added.as_bytes());
        self.disk_length += valid as u64;
        self.modified = modified;
        if text.len() - self.in_memory_from >= SPILL_BYTES {
            self.spill(text)?;
        }
        let more = valid == READ_LIMIT;
        Ok(if more {
            Followed::More
        } else {
            Followed::Current
        })
    }

    /// Whether the file still holds what the document ends with, just before the new bytes.
    fn end_matches(&self, file: &mut File, text: &TextBuffer) -> io::Result<bool> {
        let start = text.boundary(text.len().saturating_sub(CHECK_BYTES));
        let expected = text.read(start..text.len());
        let Some(position) = self.disk_length.checked_sub(expected.len() as u64) else {
            return Ok(false);
        };
        let mut actual = vec![0; expected.len()];
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut actual)?;
        Ok(actual == expected.as_bytes())
    }

    /// Move the text held in memory to the log file, keeping memory use flat.
    fn spill(&mut self, text: &mut TextBuffer) -> io::Result<()> {
        let log = match &self.log {
            Some(log) => log.clone(),
            None => self.log.insert(Arc::new(tempfile::tempfile()?)).clone(),
        };
        let range = self.in_memory_from..text.len();
        let mut writer = &*log;
        writer.seek(SeekFrom::End(0))?;
        text.read_chunks(range.clone(), |_, chunk| writer.write_all(chunk.as_bytes()))?;
        let segment = Snapshot::segment(&log, self.log_length, range.len())?;
        text.replace(
            range.clone(),
            &TextBuffer::from_snapshot(segment, 0, |_| Ok(()))?,
        );
        self.log_length += range.len() as u64;
        self.in_memory_from = text.len();
        Ok(())
    }
}

/// A path's size and time, which change along with it.
type Stamp = (u64, Option<SystemTime>);

/// How long a folder's time can stay the same across changes inside it. Times are coarse, by
/// up to two seconds depending on the file system, so a change just after a recent time may
/// not alter it.
const TIME_GRANULARITY: Duration = Duration::from_secs(2);

/// When to look at a folder once more, if its time is too recent to rule out later changes.
fn recheck_after(stamp: Stamp) -> Option<Instant> {
    let age = SystemTime::now()
        .duration_since(stamp.1?)
        .unwrap_or(Duration::ZERO);
    Some(Instant::now() + TIME_GRANULARITY.checked_sub(age)?)
}

/// What a watch last saw.
struct Seen {
    /// The size and time; `None` until first checked.
    stamp: Option<Stamp>,
    /// When to report a change even if the stamp stays the same.
    recheck: Option<Instant>,
}

fn snapshot(path: &Path) -> Option<Stamp> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()))
}

/// A file or folder being watched for changes. A folder's time changes when something inside
/// it is added, removed or renamed. Dropping this stops the watching.
pub(super) struct Watch {
    path: PathBuf,
    seen: Mutex<Seen>,
    /// Whether this is a folder, where a stamp cannot rule out a later change. A file's size
    /// and contents are checked when it reads again, and a folder's are not.
    is_folder: bool,
    changed: AtomicBool,
    waker: Waker,
}

impl Watch {
    /// Start watching. The first check always reports a change, which closes any gap
    /// between the file being read and the watch starting.
    pub fn start(path: &Path, waker: Waker) -> Arc<Self> {
        Self::register(path, None, false, waker)
    }

    /// Start watching, reporting only changes from now on. The caller reads the path afterward,
    /// so that nothing can change unseen in between.
    pub fn start_from_now(path: &Path, waker: Waker) -> Arc<Self> {
        Self::register(path, snapshot(path), true, waker)
    }

    fn register(path: &Path, stamp: Option<Stamp>, is_folder: bool, waker: Waker) -> Arc<Self> {
        let watch = Arc::new(Self {
            path: path.to_owned(),
            seen: Mutex::new(Seen {
                stamp,
                recheck: stamp.filter(|_| is_folder).and_then(recheck_after),
            }),
            is_folder,
            changed: AtomicBool::new(false),
            waker,
        });
        let registry = REGISTRY.get_or_init(|| {
            std::thread::Builder::new()
                .name("teditor-follow".into())
                .spawn(check_watches)
                .expect("starting the file watcher");
            Registry::default()
        });
        registry
            .watches
            .lock()
            .unwrap()
            .push(Arc::downgrade(&watch));
        registry.added.notify_one();
        watch
    }

    /// Whether it changed since this was last called.
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }

    /// Ask to read again on the next frame, because a read stopped at its limit.
    pub fn mark_changed(&self) {
        self.changed.store(true, Ordering::Release);
    }

    fn check(&self) {
        // A deleted file keeps its text on screen until it comes back.
        let Some(now) = snapshot(&self.path) else {
            return;
        };
        let mut seen = self.seen.lock().unwrap();
        if seen.stamp != Some(now) {
            seen.stamp = Some(now);
            seen.recheck = recheck_after(now).filter(|_| self.is_folder);
        } else if seen.recheck.is_some_and(|at| Instant::now() >= at) {
            seen.recheck = None;
        } else {
            return;
        }
        drop(seen);
        self.mark_changed();
        (self.waker)();
    }
}

#[derive(Default)]
struct Registry {
    watches: Mutex<Vec<Weak<Watch>>>,
    added: Condvar,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

/// The watcher thread: sleeps indefinitely with nothing to watch.
fn check_watches() {
    let registry = loop {
        if let Some(registry) = REGISTRY.get() {
            break registry;
        }
        std::thread::yield_now();
    };
    loop {
        let live: Vec<_> = {
            let mut watches = registry.watches.lock().unwrap();
            watches.retain(|watch| watch.strong_count() > 0);
            while watches.is_empty() {
                watches = registry.added.wait(watches).unwrap();
            }
            watches.iter().filter_map(Weak::upgrade).collect()
        };
        for watch in &live {
            watch.check();
        }
        drop(live);
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn only_a_recent_time_needs_another_look() {
        let recently = SystemTime::now();
        assert!(recheck_after((0, Some(recently))).is_some());
        let from_the_future = recently + Duration::from_secs(60);
        assert!(recheck_after((0, Some(from_the_future))).is_some());
        let long_ago = recently - TIME_GRANULARITY * 2;
        assert!(recheck_after((0, Some(long_ago))).is_none());
        assert!(recheck_after((0, None)).is_none());
    }

    #[test]
    fn a_folder_with_a_recent_time_is_read_once_more_in_case_it_hid_a_change() {
        let folder = tempfile::tempdir().unwrap();
        let watch = Watch::start_from_now(folder.path(), Arc::new(|| {}));
        // The stamp never changes here, like a change landing within the same tick.
        assert!(!watch.take_changed());
        assert!(wait_for(|| watch.take_changed()), "looked again");
        // Once is enough: by then, any later change alters the time.
        std::thread::sleep(POLL_INTERVAL * 3);
        assert!(!watch.take_changed());
    }

    #[test]
    fn a_folder_watch_reports_changes_and_wakes_the_interface() {
        let folder = tempfile::tempdir().unwrap();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = wakes.clone();
        let watch = Watch::start_from_now(
            folder.path(),
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        );
        // Nothing changed, so nothing is reported, unlike a watch on a file being read.
        std::thread::sleep(POLL_INTERVAL * 3);
        assert!(!watch.take_changed());
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        // Each kind of change shows, with no frame needed to notice it.
        let file = folder.path().join("a.txt");
        fs::write(&file, "a").unwrap();
        assert!(wait_for(|| watch.take_changed()), "added");
        assert!(wakes.load(Ordering::Relaxed) > 0);
        let renamed = folder.path().join("b.txt");
        fs::rename(&file, &renamed).unwrap();
        assert!(wait_for(|| watch.take_changed()), "renamed");
        fs::remove_file(&renamed).unwrap();
        assert!(wait_for(|| watch.take_changed()), "removed");
    }
}
