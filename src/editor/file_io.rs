//! Background file loading and saving, with progress and cancellation.

use super::{
    buffer::{Snapshot, TextBuffer},
    encoding::{self, Encoding},
    follow::Tail,
};
use std::{
    fs::{self, File},
    io::{self, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};

#[derive(Default)]
pub struct Progress {
    cancelled: AtomicBool,
    done: AtomicU64,
    total: AtomicU64,
    phase: AtomicU8,
}
impl Progress {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
    pub fn check(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Operation cancelled",
            ))
        } else {
            Ok(())
        }
    }
    pub fn advance(&self, done: u64) -> io::Result<()> {
        self.check()?;
        self.done.store(done, Ordering::Relaxed);
        Ok(())
    }
    fn begin(&self, phase: u8, total: usize) {
        self.done.store(0, Ordering::Relaxed);
        self.total.store(total as u64, Ordering::Relaxed);
        self.phase.store(phase, Ordering::Relaxed);
    }
    pub fn label(&self) -> String {
        let phase = match self.phase.load(Ordering::Relaxed) {
            1 => "Decoding",
            2 => "Indexing",
            3 => "Saving",
            4 => "Checking disk changes",
            _ => "Reading",
        };
        let total = self.total.load(Ordering::Relaxed);
        let done = self.done.load(Ordering::Relaxed).min(total);
        format!(
            "{phase}… {:.0}%",
            if total == 0 {
                0.0
            } else {
                100.0 * done as f64 / total as f64
            }
        )
    }
}

pub(crate) struct Loaded {
    pub text: TextBuffer,
    pub original: Arc<Snapshot>,
    pub tail: Tail,
    pub encoding: Encoding,
    pub bom: bool,
    pub notice: Option<String>,
}

pub(crate) fn load(
    path: &Path,
    forced: Option<Encoding>,
    progress: &Progress,
) -> io::Result<Loaded> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::other("Choose a regular file"));
    }
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(io::Error::other("Choose a regular file"));
    }
    let length = usize::try_from(before.len())
        .map_err(|_| io::Error::other("File exceeds this platform's address space"))?;
    progress.begin(0, length);
    let mut private = tempfile::tempfile()?;
    let mut done = 0;
    let hash = hash_stream(&mut file, progress, |bytes| {
        done += bytes.len() as u64;
        private.write_all(bytes)
    })?;
    let after = file.metadata()?;
    // Growing while being read is normal for a log, and following picks up the rest.
    if after.len() < done {
        return Err(io::Error::other("File changed while loading; reopen it"));
    }
    let original = Snapshot::seal(private)?;
    let bytes = original.bytes();
    let sample = &bytes[..bytes.len().min(64 * 1024)];
    let detected = encoding::detect(sample);
    let mut selected = forced.unwrap_or(detected);
    let mut bom = !selected.bom().is_empty() && bytes.starts_with(selected.bom());
    let mut notice = None;
    let text = match decode_buffer(original.clone(), selected, bom, progress) {
        Ok(buffer) => buffer,
        Err(error) if error.kind() == io::ErrorKind::InvalidData && forced.is_none() => {
            notice = Some(format!(
                "{} detection did not decode every byte; opened lossless Bytes view",
                selected.name()
            ));
            selected = Encoding::Bytes;
            bom = false;
            decode_buffer(original.clone(), selected, bom, progress)?
        }
        Err(error) => return Err(error),
    };
    progress.check()?;
    let tail = Tail::new(hash, done, after.modified().ok(), &text);
    Ok(Loaded {
        text,
        original,
        tail,
        encoding: selected,
        bom,
        notice,
    })
}
/// Hash everything `file` still has to give, passing each block to `sink`.
fn hash_stream(
    file: &mut File,
    progress: &Progress,
    mut sink: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<blake3::Hasher> {
    let mut hash = blake3::Hasher::new();
    let mut block = [0u8; 64 * 1024];
    let mut done = 0;
    loop {
        progress.advance(done)?;
        let n = file.read(&mut block)?;
        if n == 0 {
            return Ok(hash);
        }
        sink(&block[..n])?;
        hash.update(&block[..n]);
        done += n as u64;
    }
}
fn decode_buffer(
    original: Arc<Snapshot>,
    encoding: Encoding,
    bom: bool,
    progress: &Progress,
) -> io::Result<TextBuffer> {
    let skip = if bom { encoding.bom().len() } else { 0 };
    let bytes = &original.bytes()[skip..];
    if encoding == Encoding::Utf8 {
        progress.begin(2, original.bytes().len());
        return TextBuffer::from_snapshot(original, skip, |n| progress.advance(n as u64));
    }
    progress.begin(1, bytes.len());
    let mut file = BufWriter::with_capacity(64 * 1024, tempfile::tempfile()?);
    encoding::decode(&original, skip, encoding, &mut file, progress)?;
    let decoded = Snapshot::seal(file.into_inner().map_err(|e| e.into_error())?)?;
    progress.begin(2, decoded.bytes().len());
    TextBuffer::from_snapshot(decoded, 0, |n| progress.advance(n as u64))
}

pub(crate) struct SaveRequest {
    pub destination: PathBuf,
    pub expected: Option<blake3::Hash>,
    pub unchanged: Option<Arc<Snapshot>>,
    pub text: TextBuffer,
    pub encoding: Encoding,
    pub bom: bool,
}
pub(crate) struct Saved {
    pub path: PathBuf,
    pub hash: blake3::Hash,
    pub text: TextBuffer,
    pub encoding: Encoding,
    pub bom: bool,
}
impl SaveRequest {
    pub fn run(self, progress: &Progress) -> io::Result<Saved> {
        let path = &self.destination;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Choose a file path"))?;
        let mut output = tempfile::Builder::new()
            .prefix(".teditor-")
            .suffix(".tmp")
            .tempfile_in(parent)?;
        if self.expected.is_some() {
            output
                .as_file()
                .set_permissions(fs::metadata(path)?.permissions())?;
        } else if path.symlink_metadata().is_ok() {
            return Err(io::Error::other(
                "That path already exists; choose a new file name",
            ));
        }
        progress.begin(
            3,
            self.unchanged
                .as_ref()
                .map_or(self.text.len(), |s| s.bytes().len()),
        );
        let mut writer = HashWriter {
            inner: BufWriter::with_capacity(64 * 1024, output.as_file_mut()),
            hash: blake3::Hasher::new(),
        };
        if let Some(original) = self.unchanged {
            original.read_chunks(0, |offset, bytes| {
                progress.advance(offset as u64)?;
                writer.write_all(bytes)
            })?;
        } else {
            if self.bom {
                writer.write_all(self.encoding.bom())?;
            }
            encoding::encode(&self.text, self.encoding, &mut writer, progress)?;
        }
        writer.flush()?;
        let hash = writer.hash.finalize();
        drop(writer);
        output.as_file().sync_all()?;
        if let Some(expected) = self.expected {
            // One bounded-memory streaming comparison immediately before atomic
            // replacement. Never trust just mtime/size for overwrite detection.
            let mut current = File::open(path)?;
            if !current.metadata()?.is_file() {
                return Err(io::Error::other("Destination is no longer a regular file"));
            }
            progress.begin(
                4,
                current
                    .metadata()?
                    .len()
                    .try_into()
                    .map_err(|_| io::Error::other("File exceeds address space"))?,
            );
            if hash_stream(&mut current, progress, |_| Ok(()))?.finalize() != expected {
                return Err(io::Error::other(
                    "File changed on disk; use Save As to keep your edits",
                ));
            }
            progress.check()?;
            output.persist(path).map_err(|e| e.error)?;
        } else {
            progress.check()?;
            output.persist_noclobber(path).map_err(|e| e.error)?;
        }
        Ok(Saved {
            path: self.destination,
            hash,
            text: self.text,
            encoding: self.encoding,
            bom: self.bom,
        })
    }
}
struct HashWriter<W> {
    inner: W,
    hash: blake3::Hasher,
}
impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
