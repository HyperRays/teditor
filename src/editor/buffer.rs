//! A persistent, balanced piece tree.
//!
//! File contents live in private temporary-file mappings; edits and undo share immutable
//! pieces, and only tree summaries stay in the heap, so the OS's paging handles files larger
//! than physical memory. Each node stores byte, newline and tab-aware column summaries, which
//! makes navigation through a gigantic single line logarithmic rather than a rescan.
//!
//! ```
//! use teditor::editor::buffer::TextBuffer;
//!
//! let mut text = TextBuffer::from_text("one\ntwo\n");
//! let snapshot = text.clone(); // O(1), sharing storage
//! text.replace(4..7, &TextBuffer::from_text("2"));
//! assert_eq!(text.to_text(), "one\n2\n");
//! assert_eq!(snapshot.to_text(), "one\ntwo\n");
//! assert_eq!(text.line_start(1), 4);
//! ```

use super::document::cell_width;
use crate::renderer::{Columns, char_columns, is_joinable, text_columns};
use memmap2::{Mmap, MmapOptions};
use std::{fs::File, io, ops::Range, sync::Arc};

/// The largest piece a buffer loads at once, in bytes.
pub const PAGE_BYTES: usize = 32 * 1024;
const LINE_INDEX_BYTES: usize = 256;
const ZERO_WIDTH_JOINER: char = '\u{200D}';

pub(crate) struct Snapshot {
    map: Option<Mmap>,
    file: Arc<File>,
    /// Where this snapshot starts in the file.
    start: u64,
}
impl Snapshot {
    /// The file must be an exclusively owned, anonymous temporary file. No
    /// writable descriptor or mapping may survive this transfer of ownership.
    pub fn seal(file: File) -> io::Result<Arc<Self>> {
        let length = file.metadata()?.len();
        usize::try_from(length)
            .map_err(|_| io::Error::other("File exceeds this platform's address space"))?;
        // SAFETY: callers transfer the sole handle to a private, unlinked temp
        // file, after completing all writes. It is never modified or truncated.
        let map = if length == 0 {
            None
        } else {
            Some(unsafe { Mmap::map(&file)? })
        };
        Ok(Arc::new(Self {
            map,
            file: Arc::new(file),
            start: 0,
        }))
    }
    /// A snapshot of one finished part of an append-only private file. The file may keep
    /// growing, but the bytes in this range must never change.
    pub fn segment(file: &Arc<File>, start: u64, length: usize) -> io::Result<Arc<Self>> {
        // SAFETY: as for `seal`, and the caller keeps this range of the file unchanged.
        let map = unsafe { MmapOptions::new().offset(start).len(length).map(&**file)? };
        Ok(Arc::new(Self {
            map: Some(map),
            file: file.clone(),
            start,
        }))
    }
    pub fn bytes(&self) -> &[u8] {
        self.map.as_deref().unwrap_or(&[])
    }
    pub fn len(&self) -> usize {
        self.map.as_ref().map_or(0, |map| map.len())
    }
    pub fn read(&self, offset: usize, bytes: &mut [u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(bytes, self.start + offset as u64)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0;
            while done < bytes.len() {
                let n = self
                    .file
                    .seek_read(&mut bytes[done..], self.start + (offset + done) as u64)?;
                if n == 0 {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                done += n;
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (offset, bytes);
            Err(io::Error::other(
                "Positional file reads are unsupported on this platform",
            ))
        }
    }
    pub fn read_chunks(
        &self,
        skip: usize,
        mut read: impl FnMut(usize, &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut bytes = [0; 32 * 1024];
        let mut offset = skip;
        while offset < self.len() {
            let n = bytes.len().min(self.len() - offset);
            self.read(offset, &mut bytes[..n])?;
            read(offset - skip, &bytes[..n])?;
            offset += n;
        }
        Ok(())
    }
}

enum Source {
    Memory(Box<str>),
    Disk(Arc<Snapshot>),
}
impl Source {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Memory(s) => s.as_bytes(),
            Self::Disk(s) => s.bytes(),
        }
    }
}
#[derive(Clone)]
struct Piece {
    source: Arc<Source>,
    range: Range<usize>,
    lines: Arc<LineIndex>,
}
impl Piece {
    fn text(&self) -> &str {
        self.text_range(0..self.range.len())
    }
    fn text_range(&self, range: Range<usize>) -> &str {
        assert!(range.start <= range.end && range.end <= self.range.len());
        // Validate only the requested slice, not the entire 32 KiB piece on
        // every viewport query. Invalid scalar boundaries still fail safely.
        std::str::from_utf8(
            &self.source.bytes()[self.range.start + range.start..self.range.start + range.end],
        )
        .expect("validated text piece")
    }
    /// The text of `slice`, copied into `block` when it lives on disk.
    fn copy<'a>(&'a self, slice: Range<usize>, block: &'a mut [u8]) -> io::Result<&'a str> {
        match self.source.as_ref() {
            Source::Memory(_) => Ok(self.text_range(slice)),
            Source::Disk(snapshot) => {
                let bytes = &mut block[..slice.len()];
                snapshot.read(self.range.start + slice.start, bytes)?;
                Ok(std::str::from_utf8(bytes).expect("validated immutable text"))
            }
        }
    }
    fn lines_before(&self, offset: usize) -> usize {
        self.lines
            .before(self.source.bytes(), self.range.start + offset)
            - self.lines.before(self.source.bytes(), self.range.start)
    }
    fn line_start(&self, nth: usize) -> usize {
        let bytes = self.source.bytes();
        let target = self.lines.before(bytes, self.range.start) + nth;
        let block = self
            .lines
            .counts
            .partition_point(|&n| usize::from(n) < target)
            - 1;
        let remaining = target - usize::from(self.lines.counts[block]);
        let start = self.lines.start + block * LINE_INDEX_BYTES;
        let end = (start + LINE_INDEX_BYTES).min(self.range.end);
        start
            + bytes[start..end]
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .nth(remaining - 1)
                .expect("indexed newline")
                .0
            + 1
            - self.range.start
    }
}

/// Cumulative newline counts at fixed byte intervals. Each local scan reads at
/// most 256 bytes, even in a piece containing thousands of short lines. Counts use
/// at most ~0.8% of the text size and are shared by pieces split during edits.
/// Newline-free pages need no checkpoint allocation.
struct LineIndex {
    start: usize,
    counts: Box<[u16]>,
}
impl LineIndex {
    fn new(start: usize, text: &str, lines: usize) -> Arc<Self> {
        assert!(text.len() <= PAGE_BYTES && PAGE_BYTES <= u16::MAX as usize);
        let mut counts = Vec::new();
        if lines > 0 {
            counts.reserve(text.len() / LINE_INDEX_BYTES + 1);
            counts.push(0);
            let mut count = 0;
            for block in text.as_bytes().as_chunks::<LINE_INDEX_BYTES>().0 {
                count += block.iter().filter(|&&b| b == b'\n').count() as u16;
                counts.push(count);
            }
        }
        Arc::new(Self {
            start,
            counts: counts.into_boxed_slice(),
        })
    }
    fn before(&self, bytes: &[u8], offset: usize) -> usize {
        if self.counts.is_empty() {
            return 0;
        }
        let block = (offset - self.start) / LINE_INDEX_BYTES;
        usize::from(self.counts[block])
            + bytes[self.start + block * LINE_INDEX_BYTES..offset]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Summary {
    bytes: usize,
    lines: usize,
    /// Width for each possible initial tab alignment, or the final line's width
    /// after a newline. This makes navigation through gigantic lines logarithmic.
    columns: [usize; 4],
    /// Columns the first scalar adds, if a joiner before it would absorb them instead.
    absorbable: u8,
    ends_joined: bool,
}
impl Summary {
    fn text(text: &str) -> Self {
        let lines = text.bytes().filter(|&b| b == b'\n').count();
        let tail = text.rsplit('\n').next().unwrap_or("");
        let columns = if tail.is_ascii() && !tail.contains('\t') {
            [tail.len(); 4]
        } else if !tail.contains('\t') {
            [text_columns(tail) as usize; 4]
        } else {
            std::array::from_fn(|initial| {
                let initial = if lines > 0 { 0 } else { initial };
                let mut measure = Columns::default();
                tail.chars()
                    .fold(initial, |x, c| x + cell_width(c, x, &mut measure))
                    - initial
            })
        };
        let first = text.chars().next().filter(|&c| is_joinable(c));
        Self {
            bytes: text.len(),
            lines,
            columns,
            absorbable: first.map_or(0, char_columns) as u8,
            ends_joined: text.ends_with(ZERO_WIDTH_JOINER),
        }
    }
    /// The column after this text, starting at `column`. `joined` says the text before it
    /// ended with a joiner, which absorbs a first scalar that would otherwise add width.
    fn advance(self, column: usize, joined: bool) -> usize {
        if self.lines > 0 {
            self.columns[0]
        } else if joined && self.absorbable > 0 {
            // Start early by the absorbed width, so later tabs align from the true column.
            let start = column + 4 - usize::from(self.absorbable);
            start + self.columns[start % 4] - 4
        } else {
            column + self.columns[column % 4]
        }
    }
    fn then(self, right: Self) -> Self {
        if self.bytes == 0 || right.bytes == 0 {
            return if self.bytes == 0 { right } else { self };
        }
        let joined = self.ends_joined;
        let columns = if right.lines > 0 {
            right.columns
        } else if self.lines > 0 {
            [right.advance(self.columns[0], joined); 4]
        } else {
            std::array::from_fn(|i| right.advance(i + self.columns[i], joined) - i)
        };
        Self {
            bytes: self.bytes + right.bytes,
            lines: self.lines + right.lines,
            columns,
            absorbable: self.absorbable,
            ends_joined: right.ends_joined,
        }
    }
}

type Link = Option<Arc<Node>>;
struct Node {
    summary: Summary,
    height: u16,
    kind: Kind,
}
enum Kind {
    Leaf(Piece),
    Branch(Arc<Node>, Arc<Node>),
}
fn height(node: &Link) -> u16 {
    node.as_ref().map_or(0, |n| n.height)
}
fn summary(node: &Link) -> Summary {
    node.as_ref().map_or(Summary::default(), |n| n.summary)
}
fn leaf(piece: Piece) -> Link {
    (!piece.range.is_empty()).then(|| {
        Arc::new(Node {
            summary: Summary::text(piece.text()),
            height: 1,
            kind: Kind::Leaf(piece),
        })
    })
}
/// A leaf for a page of validated text found at `start` in `source`.
fn page(source: Arc<Source>, start: usize, text: &str) -> Arc<Node> {
    let summary = Summary::text(text);
    Arc::new(Node {
        summary,
        height: 1,
        kind: Kind::Leaf(Piece {
            source,
            range: start..start + text.len(),
            lines: LineIndex::new(start, text, summary.lines),
        }),
    })
}
fn branch(left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
    Arc::new(Node {
        summary: left.summary.then(right.summary),
        height: 1 + left.height.max(right.height),
        kind: Kind::Branch(left, right),
    })
}
fn balance(left: Arc<Node>, right: Arc<Node>) -> Arc<Node> {
    if left.height > right.height + 1 {
        let Kind::Branch(a, b) = &left.kind else {
            unreachable!()
        };
        if a.height >= b.height {
            return branch(a.clone(), branch(b.clone(), right));
        }
        let Kind::Branch(c, d) = &b.kind else {
            unreachable!()
        };
        return branch(branch(a.clone(), c.clone()), branch(d.clone(), right));
    }
    if right.height > left.height + 1 {
        let Kind::Branch(a, b) = &right.kind else {
            unreachable!()
        };
        if b.height >= a.height {
            return branch(branch(left, a.clone()), b.clone());
        }
        let Kind::Branch(c, d) = &a.kind else {
            unreachable!()
        };
        return branch(branch(left, c.clone()), branch(d.clone(), b.clone()));
    }
    branch(left, right)
}
fn join(left: Link, right: Link) -> Link {
    match (left, right) {
        (None, other) | (other, None) => other,
        (Some(left), Some(right)) => {
            if left.height > right.height + 1 {
                let Kind::Branch(a, b) = &left.kind else {
                    unreachable!()
                };
                Some(balance(
                    a.clone(),
                    join(Some(b.clone()), Some(right)).unwrap(),
                ))
            } else if right.height > left.height + 1 {
                let Kind::Branch(a, b) = &right.kind else {
                    unreachable!()
                };
                Some(balance(
                    join(Some(left), Some(a.clone())).unwrap(),
                    b.clone(),
                ))
            } else {
                Some(branch(left, right))
            }
        }
    }
}
fn split(node: Link, offset: usize) -> (Link, Link) {
    let Some(node) = node else {
        return (None, None);
    };
    if offset == 0 {
        return (None, Some(node));
    }
    if offset >= node.summary.bytes {
        return (Some(node), None);
    }
    match &node.kind {
        Kind::Leaf(piece) => {
            let mid = piece.range.start + offset;
            (
                leaf(Piece {
                    source: piece.source.clone(),
                    range: piece.range.start..mid,
                    lines: piece.lines.clone(),
                }),
                leaf(Piece {
                    source: piece.source.clone(),
                    range: mid..piece.range.end,
                    lines: piece.lines.clone(),
                }),
            )
        }
        Kind::Branch(left, right) => {
            if offset < left.summary.bytes {
                let (a, b) = split(Some(left.clone()), offset);
                (a, join(b, Some(right.clone())))
            } else {
                let (a, b) = split(Some(right.clone()), offset - left.summary.bytes);
                (join(Some(left.clone()), a), b)
            }
        }
    }
}

/// Immutable-by-sharing UTF-8 text: a balanced tree of pieces. Cloning is `O(1)`, and
/// clones share storage, which is what makes undo snapshots cheap. Offsets are bytes.
#[derive(Clone, Default)]
pub struct TextBuffer {
    root: Link,
}

/// How a buffer's storage is laid out, for tests and diagnostics.
#[derive(Debug, Default)]
pub struct BufferStats {
    /// Leaves in the tree.
    pub pieces: usize,
    /// Tree height.
    pub height: u16,
    /// Bytes held on the heap by typed or pasted text.
    pub memory_bytes: usize,
    /// Bytes held in temporary-file mappings.
    pub mapped_bytes: usize,
}
impl TextBuffer {
    /// Measure the tree and its distinct backing stores.
    pub fn stats(&self) -> BufferStats {
        let mut stats = BufferStats {
            height: self.height(),
            ..Default::default()
        };
        let mut seen = std::collections::HashSet::new();
        let mut nodes: Vec<_> = self.root.as_deref().into_iter().collect();
        while let Some(node) = nodes.pop() {
            match &node.kind {
                Kind::Branch(a, b) => {
                    nodes.push(a);
                    nodes.push(b);
                }
                Kind::Leaf(piece) => {
                    stats.pieces += 1;
                    if seen.insert(Arc::as_ptr(&piece.source)) {
                        match piece.source.as_ref() {
                            Source::Memory(text) => stats.memory_bytes += text.len(),
                            Source::Disk(snapshot) => stats.mapped_bytes += snapshot.bytes().len(),
                        }
                    }
                }
            }
        }
        stats
    }
    /// An empty buffer.
    pub fn new() -> Self {
        Self::default()
    }
    /// A buffer holding a copy of `text`, split into pages.
    pub fn from_text(text: &str) -> Self {
        let mut nodes = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + PAGE_BYTES).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let part = &text[start..end];
            nodes.push(page(Arc::new(Source::Memory(part.into())), 0, part));
            start = end;
        }
        Self::from_nodes(nodes)
    }
    /// A buffer holding `text` in a private temporary file rather than in memory.
    pub(crate) fn from_text_on_disk(text: &str) -> io::Result<Self> {
        use std::io::Write;
        let mut file = tempfile::tempfile()?;
        file.write_all(text.as_bytes())?;
        Self::from_snapshot(Snapshot::seal(file)?, 0, |_| Ok(()))
    }
    pub(crate) fn from_snapshot(
        snapshot: Arc<Snapshot>,
        skip: usize,
        mut check: impl FnMut(usize) -> io::Result<()>,
    ) -> io::Result<Self> {
        let source = Arc::new(Source::Disk(snapshot.clone()));
        let mut nodes = Vec::new();
        let mut start = skip;
        let mut bytes = [0u8; PAGE_BYTES + 1];
        while start < snapshot.len() {
            check(start)?;
            let available = (snapshot.len() - start).min(bytes.len());
            snapshot.read(start, &mut bytes[..available])?;
            let mut length = available.min(PAGE_BYTES);
            while length > 0 && length < available && bytes[length] & 0xc0 == 0x80 {
                length -= 1;
            }
            let text = std::str::from_utf8(&bytes[..length]).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Invalid UTF-8 near byte {}", start + e.valid_up_to()),
                )
            })?;
            if text.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid UTF-8 sequence",
                ));
            }
            nodes.push(page(source.clone(), start, text));
            start += length;
        }
        check(snapshot.len())?;
        Ok(Self::from_nodes(nodes))
    }
    /// Build a balanced tree over consecutive leaves in linear time.
    fn from_nodes(nodes: Vec<Arc<Node>>) -> Self {
        fn build(nodes: &[Arc<Node>]) -> Link {
            match nodes {
                [] => None,
                [node] => Some(node.clone()),
                _ => {
                    let (a, b) = nodes.split_at(nodes.len() / 2);
                    Some(branch(build(a)?, build(b)?))
                }
            }
        }
        Self {
            root: build(&nodes),
        }
    }
    /// Length in bytes.
    pub fn len(&self) -> usize {
        summary(&self.root).bytes
    }
    /// Whether the buffer holds no text.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }
    pub(crate) fn same_snapshot(&self, other: &Self) -> bool {
        match (&self.root, &other.root) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
    /// Whether two buffers hold the same text. Cheap whatever their size, because they
    /// usually differ in length, or share nearly everything from an earlier version.
    pub(crate) fn same_text(&self, other: &Self) -> bool {
        self.same_snapshot(other)
            || (self.len() == other.len() && self.common_prefix_len(other) == self.len())
    }
    /// Find the first changed byte, skipping shared subtrees and source ranges.
    pub(crate) fn common_prefix_len(&self, other: &Self) -> usize {
        self.common_edge_len(other, false)
    }
    pub(crate) fn common_suffix_len(&self, other: &Self) -> usize {
        self.common_edge_len(other, true)
    }
    fn common_edge_len(&self, other: &Self, reverse: bool) -> usize {
        let mut a: Vec<_> = self.root.as_deref().map(|n| (n, 0)).into_iter().collect();
        let mut b: Vec<_> = other.root.as_deref().map(|n| (n, 0)).into_iter().collect();
        let mut prefix = 0;
        while let (Some(&(left, x)), Some(&(right, y))) = (a.last(), b.last()) {
            if std::ptr::eq(left, right) && x == y {
                prefix += left.summary.bytes - x;
                a.pop();
                b.pop();
                continue;
            }
            if let Kind::Branch(l, r) = &left.kind
                && (left.summary.bytes >= right.summary.bytes
                    || matches!(right.kind, Kind::Leaf(_)))
            {
                a.pop();
                if reverse {
                    a.extend([(l.as_ref(), 0), (r.as_ref(), 0)]);
                } else {
                    a.extend([(r.as_ref(), 0), (l.as_ref(), 0)]);
                }
                continue;
            }
            if let Kind::Branch(l, r) = &right.kind {
                b.pop();
                if reverse {
                    b.extend([(l.as_ref(), 0), (r.as_ref(), 0)]);
                } else {
                    b.extend([(r.as_ref(), 0), (l.as_ref(), 0)]);
                }
                continue;
            }
            let (Kind::Leaf(l), Kind::Leaf(r)) = (&left.kind, &right.kind) else {
                unreachable!();
            };
            let n = (l.range.len() - x).min(r.range.len() - y);
            let lx = if reverse {
                l.range.end - x - n
            } else {
                l.range.start + x
            };
            let ry = if reverse {
                r.range.end - y - n
            } else {
                r.range.start + y
            };
            let left = &l.source.bytes()[lx..lx + n];
            let right = &r.source.bytes()[ry..ry + n];
            if !(Arc::ptr_eq(&l.source, &r.source) && lx == ry)
                && let Some(i) = if reverse {
                    left.iter()
                        .rev()
                        .zip(right.iter().rev())
                        .position(|(a, b)| a != b)
                } else {
                    left.iter().zip(right).position(|(a, b)| a != b)
                }
            {
                return prefix + i;
            }
            prefix += n;
            if x + n == l.range.len() {
                a.pop();
            } else {
                a.last_mut().unwrap().1 += n;
            }
            if y + n == r.range.len() {
                b.pop();
            } else {
                b.last_mut().unwrap().1 += n;
            }
        }
        prefix
    }
    /// Number of lines; text without a newline is one line, and an empty buffer has one.
    pub fn line_count(&self) -> usize {
        summary(&self.root).lines + 1
    }
    /// Height of the tree, which stays logarithmic in the number of pieces.
    pub fn height(&self) -> u16 {
        height(&self.root)
    }
    /// The text in `range`, sharing storage with this buffer.
    pub fn slice(&self, range: Range<usize>) -> Self {
        let (_, tail) = split(self.root.clone(), range.start);
        let (body, _) = split(tail, range.end - range.start);
        Self { root: body }
    }
    /// Replace `range` with `inserted`, sharing storage with both. Only tree paths are copied.
    pub fn replace(&mut self, range: Range<usize>, inserted: &Self) {
        assert!(range.start <= range.end && range.end <= self.len());
        let (left, tail) = split(self.root.clone(), range.start);
        let (_, right) = split(tail, range.end - range.start);
        self.root = join(join(left, inserted.root.clone()), right);
    }
    /// Add `other` to the end.
    pub fn append(&mut self, other: &Self) {
        self.root = join(self.root.clone(), other.root.clone());
    }
    /// Copy `range` into a new `String`.
    pub fn read(&self, range: Range<usize>) -> String {
        let mut text = String::with_capacity(range.len());
        for (_, chunk) in self.chunks(range) {
            text.push_str(chunk);
        }
        text
    }
    /// Copy the whole buffer into a `String`. Prefer [`chunks`](Self::chunks) for large text.
    pub fn to_text(&self) -> String {
        self.read(0..self.len())
    }
    /// Whether `range` holds exactly `text`, without copying it.
    pub fn equals(&self, range: Range<usize>, text: &str) -> bool {
        if range.len() != text.len() {
            return false;
        }
        let mut offset = 0;
        for (_, chunk) in self.chunks(range) {
            if !text[offset..].starts_with(chunk) {
                return false;
            }
            offset += chunk.len();
        }
        true
    }
    /// Read a large range without faulting the entire file into its mapping.
    pub fn read_chunks(
        &self,
        range: Range<usize>,
        mut read: impl FnMut(usize, &str) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut block = [0; PAGE_BYTES];
        for (offset, piece, slice) in self.chunks(range).leaves() {
            read(offset, piece.copy(slice, &mut block)?)?;
        }
        Ok(())
    }
    /// Read whole lines from `offset` onward, for long sequential scans. Like
    /// [`read_chunks`](Self::read_chunks), this does not fault the scanned text into
    /// the process's mapping, so memory use stays flat however far it goes.
    pub(crate) fn lines_from(&self, offset: usize) -> LineReader<'_> {
        LineReader {
            leaves: self.chunks(offset..self.len()),
            block: Box::new([0; PAGE_BYTES]),
            page: String::new(),
            position: 0,
        }
    }
    /// Read the whole text as bytes, without faulting it into the process's mapping.
    pub(crate) fn reader(&self) -> TextReader<'_> {
        TextReader {
            leaves: self.chunks(0..self.len()),
            block: Box::new([0; PAGE_BYTES]),
            page: String::new(),
            position: 0,
        }
    }
    /// Iterate over `range` as `(offset, text)` chunks, in order.
    pub fn chunks(&self, range: Range<usize>) -> Chunks<'_> {
        self.iter_chunks(range, false)
    }
    /// Iterate over `range` as `(offset, text)` chunks, last chunk first.
    pub fn chunks_rev(&self, range: Range<usize>) -> Chunks<'_> {
        self.iter_chunks(range, true)
    }
    fn iter_chunks(&self, range: Range<usize>, reverse: bool) -> Chunks<'_> {
        assert!(range.start <= range.end && range.end <= self.len());
        Chunks {
            stack: self.root.as_deref().map(|n| (n, 0)).into_iter().collect(),
            range,
            reverse,
        }
    }
    /// The character starting at `offset`, which must be a character boundary.
    pub fn char_at(&self, offset: usize) -> Option<char> {
        if offset >= self.len() {
            return None;
        }
        self.chunks(offset..self.len()).next()?.1.chars().next()
    }
    /// The byte at `offset`, if it is in the buffer.
    pub fn byte(&self, mut offset: usize) -> Option<u8> {
        let mut node = self.root.as_deref()?;
        loop {
            match &node.kind {
                Kind::Leaf(piece) => {
                    return piece
                        .source
                        .bytes()
                        .get(piece.range.start + offset)
                        .copied()
                        .filter(|_| offset < piece.range.len());
                }
                Kind::Branch(left, right) => {
                    if offset < left.summary.bytes {
                        node = left;
                    } else {
                        offset -= left.summary.bytes;
                        node = right;
                    }
                }
            }
        }
    }
    /// The nearest character boundary at or before `offset`, clamped to the buffer.
    pub fn boundary(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.len());
        while self.byte(offset).is_some_and(|b| b & 0xc0 == 0x80) {
            offset -= 1;
        }
        offset
    }
    /// The start of the character before `offset`.
    pub fn previous(&self, offset: usize) -> usize {
        self.boundary(offset.saturating_sub(1))
    }
    /// The start of the character after the one at `offset`.
    pub fn next(&self, offset: usize) -> usize {
        offset + self.char_at(offset).map_or(0, char::len_utf8)
    }
    fn range_summary(&self, range: Range<usize>) -> Summary {
        fn visit(node: &Node, range: Range<usize>) -> Summary {
            if range.is_empty() {
                return Summary::default();
            }
            if range.start == 0 && range.end == node.summary.bytes {
                return node.summary;
            }
            match &node.kind {
                Kind::Leaf(p) => Summary::text(p.text_range(range)),
                Kind::Branch(a, b) => {
                    let cut = a.summary.bytes;
                    if range.end <= cut {
                        visit(a, range)
                    } else if range.start >= cut {
                        visit(b, range.start - cut..range.end - cut)
                    } else {
                        visit(a, range.start..cut).then(visit(b, 0..range.end - cut))
                    }
                }
            }
        }
        self.root
            .as_ref()
            .map_or(Summary::default(), |n| visit(n, range))
    }
    /// The zero-based line containing `offset`.
    pub fn row_at(&self, mut offset: usize) -> usize {
        assert!(offset <= self.len());
        let Some(mut node) = self.root.as_deref() else {
            return 0;
        };
        let mut lines = 0;
        loop {
            match &node.kind {
                Kind::Leaf(piece) => return lines + piece.lines_before(offset),
                Kind::Branch(left, right) => {
                    if offset < left.summary.bytes {
                        node = left;
                    } else {
                        offset -= left.summary.bytes;
                        lines += left.summary.lines;
                        node = right;
                    }
                }
            }
        }
    }
    /// The display width of `range`, which starts a line, counting tabs and wide characters.
    pub fn columns(&self, range: Range<usize>) -> usize {
        self.range_summary(range).advance(0, false)
    }
    /// The offset where line `row` starts; the buffer's length when `row` is past the end.
    pub fn line_start(&self, row: usize) -> usize {
        if row == 0 {
            return 0;
        }
        if row >= self.line_count() {
            return self.len();
        }
        let mut node = self.root.as_deref().unwrap();
        let mut offset = 0;
        let mut nth = row;
        loop {
            match &node.kind {
                Kind::Leaf(p) => {
                    return offset + p.line_start(nth);
                }
                Kind::Branch(a, b) => {
                    if nth <= a.summary.lines {
                        node = a;
                    } else {
                        nth -= a.summary.lines;
                        offset += a.summary.bytes;
                        node = b;
                    }
                }
            }
        }
    }
    /// Find the scalar covering a visual column, skipping complete subtree
    /// summaries. Ranges must not include a newline.
    pub fn offset_at_column(&self, range: Range<usize>, target: usize) -> usize {
        /// Where the walk has reached along the line.
        struct Walk {
            column: usize,
            measure: Columns,
        }
        fn visit(
            node: &Node,
            base: usize,
            range: &Range<usize>,
            target: usize,
            walk: &mut Walk,
        ) -> Option<usize> {
            let end = base + node.summary.bytes;
            if range.end <= base || range.start >= end {
                return None;
            }
            if range.start <= base && end <= range.end {
                let after = node.summary.advance(walk.column, walk.measure.joined());
                if after <= target {
                    walk.column = after;
                    walk.measure = Columns::after(node.summary.ends_joined);
                    return None;
                }
            }
            match &node.kind {
                Kind::Leaf(p) => {
                    let a = range.start.saturating_sub(base);
                    let b = (range.end - base).min(node.summary.bytes);
                    for (i, c) in p.text_range(a..b).char_indices() {
                        let width = cell_width(c, walk.column, &mut walk.measure);
                        if walk.column + width > target {
                            return Some(base + a + i);
                        }
                        walk.column += width;
                    }
                    None
                }
                Kind::Branch(a, b) => visit(a, base, range, target, walk)
                    .or_else(|| visit(b, base + a.summary.bytes, range, target, walk)),
            }
        }
        let mut walk = Walk {
            column: 0,
            measure: Columns::default(),
        };
        self.root
            .as_ref()
            .and_then(|n| visit(n, 0, &range, target, &mut walk))
            .unwrap_or(range.end)
    }
}

/// An iterator over a range of a buffer's text. See [`TextBuffer::chunks`].
pub struct Chunks<'a> {
    stack: Vec<(&'a Node, usize)>,
    range: Range<usize>,
    reverse: bool,
}

/// Reads a buffer's text in order, holding at most one page at a time.
pub(crate) struct TextReader<'a> {
    leaves: Chunks<'a>,
    block: Box<[u8; PAGE_BYTES]>,
    page: String,
    position: usize,
}
impl io::Read for TextReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position == self.page.len() {
            let Some((_, piece, slice)) = self.leaves.next_leaf() else {
                return Ok(0);
            };
            let text = piece.copy(slice, &mut self.block[..])?;
            self.page.clear();
            self.page.push_str(text);
            self.position = 0;
        }
        let rest = &self.page.as_bytes()[self.position..];
        let taken = rest.len().min(buffer.len());
        buffer[..taken].copy_from_slice(&rest[..taken]);
        self.position += taken;
        Ok(taken)
    }
}

/// One result of [`LineReader::next_line`].
pub(crate) enum Line {
    /// A line including its ending, if it has one.
    Text(String),
    /// The line is over the limit, or could not be read.
    Unavailable,
    /// There is no more text.
    End,
}
/// Reads a buffer's lines in order, holding at most one page of text at a time.
pub(crate) struct LineReader<'a> {
    leaves: Chunks<'a>,
    block: Box<[u8; PAGE_BYTES]>,
    page: String,
    position: usize,
}
impl LineReader<'_> {
    /// The next line, refusing to gather one longer than `limit` bytes.
    pub fn next_line(&mut self, limit: usize) -> Line {
        let mut line = String::new();
        loop {
            if self.position == self.page.len() {
                let Some((_, piece, slice)) = self.leaves.next_leaf() else {
                    return if line.is_empty() {
                        Line::End
                    } else {
                        Line::Text(line)
                    };
                };
                let Ok(text) = piece.copy(slice, &mut self.block[..]) else {
                    return Line::Unavailable;
                };
                self.page.clear();
                self.page.push_str(text);
                self.position = 0;
            }
            let rest = &self.page[self.position..];
            let taken = rest.find('\n').map_or(rest.len(), |at| at + 1);
            if line.len() + taken > limit {
                return Line::Unavailable;
            }
            line.push_str(&rest[..taken]);
            self.position += taken;
            if line.ends_with('\n') {
                return Line::Text(line);
            }
        }
    }
}
impl<'a> Chunks<'a> {
    /// The next piece overlapping the range: its offset in the buffer, and the
    /// overlapping bytes relative to the piece.
    fn next_leaf(&mut self) -> Option<(usize, &'a Piece, Range<usize>)> {
        while let Some((node, base)) = self.stack.pop() {
            if self.range.end <= base || self.range.start >= base + node.summary.bytes {
                continue;
            }
            match &node.kind {
                Kind::Leaf(piece) => {
                    let a = self.range.start.saturating_sub(base);
                    let b = (self.range.end - base).min(node.summary.bytes);
                    return Some((base + a, piece, a..b));
                }
                Kind::Branch(a, b) => {
                    let left = (a.as_ref(), base);
                    let right = (b.as_ref(), base + a.summary.bytes);
                    if self.reverse {
                        self.stack.push(left);
                        self.stack.push(right);
                    } else {
                        self.stack.push(right);
                        self.stack.push(left);
                    }
                }
            }
        }
        None
    }
    fn leaves(mut self) -> impl Iterator<Item = (usize, &'a Piece, Range<usize>)> {
        std::iter::from_fn(move || self.next_leaf())
    }
}
impl<'a> Iterator for Chunks<'a> {
    type Item = (usize, &'a str);
    fn next(&mut self) -> Option<Self::Item> {
        let (offset, piece, slice) = self.next_leaf()?;
        Some((offset, piece.text_range(slice)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn verify(node: &Node) -> Summary {
        match &node.kind {
            Kind::Leaf(p) => {
                assert!(p.range.len() <= PAGE_BYTES);
                assert_eq!(node.summary, Summary::text(p.text()));
            }
            Kind::Branch(a, b) => {
                assert!(a.height.abs_diff(b.height) <= 1);
                assert_eq!(node.height, 1 + a.height.max(b.height));
                assert_eq!(node.summary, verify(a).then(verify(b)));
            }
        }
        node.summary
    }
    #[test]
    fn randomized_edits_match_string_and_keep_snapshots_balanced() {
        let mut expected = "α\t🙂\nabc\r\n".repeat(8000);
        let mut buffer = TextBuffer::from_text(&expected);
        let original = buffer.clone();
        let original_text = expected.clone();
        let mut random = 12345u64;
        for step in 0..1200 {
            let previous = (step % 50 == 0).then(|| (buffer.clone(), expected.clone()));
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let a = buffer.boundary(random as usize % (buffer.len() + 1));
            let b = buffer.boundary((a + (random >> 32) as usize % 100).min(buffer.len()));
            let inserted = ["", "🙂", "\n", "é\txyz", "hello"][step % 5];
            buffer.replace(a..b, &TextBuffer::from_text(inserted));
            expected.replace_range(a..b, inserted);
            if step % 50 == 0 {
                let (previous, before) = previous.unwrap();
                let prefix = before
                    .bytes()
                    .zip(expected.bytes())
                    .position(|(a, b)| a != b)
                    .unwrap_or(before.len().min(expected.len()));
                assert_eq!(previous.common_prefix_len(&buffer), prefix);
                assert_eq!(buffer.common_prefix_len(&previous), prefix);
                let suffix = before
                    .bytes()
                    .rev()
                    .zip(expected.bytes().rev())
                    .position(|(a, b)| a != b)
                    .unwrap_or(before.len().min(expected.len()));
                assert_eq!(previous.common_suffix_len(&buffer), suffix);
                assert_eq!(buffer.common_suffix_len(&previous), suffix);
                assert!(buffer.same_snapshot(&buffer.clone()));
                assert_eq!(
                    buffer.common_prefix_len(&TextBuffer::from_text(&expected)),
                    expected.len()
                );
                assert_eq!(
                    buffer.common_suffix_len(&TextBuffer::from_text(&expected)),
                    expected.len()
                );
                assert_eq!(buffer.to_text(), expected);
                verify(buffer.root.as_ref().unwrap());
                for row in [0, buffer.line_count() / 2, buffer.line_count() - 1] {
                    let start = if row == 0 {
                        0
                    } else {
                        expected.match_indices('\n').nth(row - 1).unwrap().0 + 1
                    };
                    assert_eq!(buffer.line_start(row), start);
                    assert_eq!(buffer.row_at(start), row);
                }
            }
        }
        assert_eq!(original.to_text(), original_text);
    }
    #[test]
    fn indexed_lines_survive_page_boundaries_slices_and_edits() {
        use std::io::Write;
        fn check(buffer: &TextBuffer, text: &str) {
            let starts: Vec<_> = std::iter::once(0)
                .chain(text.match_indices('\n').map(|(i, _)| i + 1))
                .collect();
            assert_eq!(buffer.line_count(), starts.len());
            for (row, &start) in starts.iter().enumerate() {
                assert_eq!(buffer.line_start(row), start);
                assert_eq!(buffer.row_at(start), row);
                if start > 0 {
                    assert_eq!(buffer.row_at(start - 1), row - 1);
                }
            }
            assert_eq!(buffer.line_start(starts.len()), text.len());
            assert_eq!(buffer.row_at(text.len()), starts.len() - 1);
            assert_eq!(buffer.to_text(), text);
            let mut reader = buffer.lines_from(0);
            let mut gathered = String::new();
            while let Line::Text(line) = reader.next_line(usize::MAX) {
                assert!(line.ends_with('\n') || gathered.len() + line.len() == text.len());
                gathered.push_str(&line);
            }
            assert_eq!(gathered, text);
        }
        for text in [
            "\n".repeat(PAGE_BYTES + 1),
            format!("{}\n{}", "x".repeat(255), "é\t🙂\r\nabc\n".repeat(3000)),
            format!(
                "{}\n{}\n",
                "x".repeat(PAGE_BYTES - 1),
                "y".repeat(PAGE_BYTES + 5)
            ),
        ] {
            // Exercise both owned and mapped pages, including a nonzero BOM skip.
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(b"\xef\xbb\xbf").unwrap();
            file.write_all(text.as_bytes()).unwrap();
            let mapped =
                TextBuffer::from_snapshot(Snapshot::seal(file).unwrap(), 3, |_| Ok(())).unwrap();
            for mut buffer in [TextBuffer::from_text(&text), mapped] {
                check(&buffer, &text);
                let snapshot = buffer.clone();
                let a = buffer.boundary(257);
                let b = buffer.boundary(PAGE_BYTES + 1);
                check(&buffer.slice(a..b), &text[a..b]);
                let mut edited = text.clone();
                buffer.replace(a..b, &TextBuffer::from_text("\nα\t\r\n🙂"));
                edited.replace_range(a..b, "\nα\t\r\n🙂");
                check(&buffer, &edited);
                check(&snapshot, &text);
            }
        }
    }
    #[test]
    fn joined_emoji_measure_the_same_however_the_text_is_split() {
        let text =
            "a\t\u{1F469}\u{200D}\u{1F4BB}\te\u{301}\u{1F44D}\u{1F3FD}\u{200D}x".repeat(3000);
        let expected = super::super::document::columns(&text);
        let whole = TextBuffer::from_text(&text);
        assert_eq!(whole.columns(0..whole.len()), expected);
        // Split at every scalar near the joiners, so a piece can begin right after one.
        for split in text.char_indices().map(|(i, _)| i).take(40) {
            let mut pieces = TextBuffer::from_text(&text[..split]);
            pieces.append(&TextBuffer::from_text(&text[split..]));
            assert_eq!(
                pieces.columns(0..pieces.len()),
                expected,
                "split at {split}"
            );
            for column in [0, 1, 4, 6, 8, 9, 10, 13, 1000] {
                let found = pieces.offset_at_column(0..pieces.len(), column);
                let actual = super::super::document::columns(&text[..found]);
                assert!(actual <= column, "split {split}, column {column}");
            }
        }
    }
    #[test]
    fn long_line_column_queries_cross_piece_boundaries_and_tabs() {
        let text = "α\t🙂z".repeat(100_000);
        let buffer = TextBuffer::from_text(&text);
        let end = buffer.len();
        // α, a tab to the next stop, a two-cell 🙂 and z repeat every eight columns.
        assert_eq!(buffer.columns(0..end), 799_999);
        for column in [0, 1, 2, 4, 5, 32_767, 220_001, 799_998, 799_999] {
            let offset = buffer.offset_at_column(0..end, column);
            let actual = super::super::document::columns(&text[..offset]);
            assert!(actual <= column);
            if offset < end {
                let character = buffer.char_at(offset).unwrap();
                let width =
                    super::super::document::cell_width(character, actual, &mut Columns::default());
                assert!(actual + width > column);
            }
        }
        assert!(buffer.height() < 20);
    }
}
