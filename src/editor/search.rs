//! Text search, built on the crates ripgrep is made of. They read through a small rolling
//! buffer, so searching costs memory for the longest line rather than for the text's size.
//!
//! Matching is by line, as in ripgrep, so a pattern cannot span lines.

use super::buffer::TextBuffer;
use crate::vt::Waker;
use grep_matcher::{Captures, LineTerminator, Matcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::WalkBuilder;
use std::{
    fs::File,
    io::{self, Read},
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
};

/// The longest line searched. A longer one stops the search rather than filling memory.
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;
/// Most results a search across files keeps.
const MAX_HITS: usize = 1000;
/// How much of a matched line a result keeps ahead of the match.
const CONTEXT_CHARS: usize = 24;
/// How much of a matched line a result keeps in all.
const LINE_CHARS: usize = 240;

/// How a query is matched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Match upper and lower case exactly.
    pub case_sensitive: bool,
    /// Ignore case unless the query has an upper case letter, overriding `case_sensitive`.
    pub smart_case: bool,
    /// Match only whole words.
    pub whole_word: bool,
    /// Read the query as a regular expression rather than as literal text.
    pub regex: bool,
}

/// Prepare a query for searching.
pub fn compile(query: &str, options: Options) -> Result<RegexMatcher, String> {
    RegexMatcherBuilder::new()
        // Smart case only applies where ignoring case has not already been asked for.
        .case_insensitive(!options.case_sensitive && !options.smart_case)
        .case_smart(options.smart_case)
        .word(options.whole_word)
        .fixed_strings(!options.regex)
        // As ripgrep sets them: `^` and `$` work on each line of the buffer being searched.
        .multi_line(true)
        .line_terminator(Some(b'\n'))
        .crlf(true)
        .build(query)
        .map_err(|error| {
            // The library's message is several lines, with the reason last.
            let message = error.to_string();
            let reason = message.lines().rfind(|line| !line.trim().is_empty());
            reason.unwrap_or("Invalid pattern").trim().to_owned()
        })
}

/// Reads on until asked to stop, so that a long search can be abandoned.
struct Cancellable<'a, R> {
    inner: R,
    cancel: &'a AtomicBool,
}
impl<R: Read> Read for Cancellable<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            // Not `Interrupted`, which readers are expected to simply retry.
            return Err(io::Error::other("cancelled"));
        }
        self.inner.read(buffer)
    }
}

/// Hands each match in a line to a callback, which returns whether to go on.
struct Matches<'a, F> {
    matcher: &'a RegexMatcher,
    found: F,
}
impl<F: FnMut(Range<usize>, &[u8], u64, Option<u64>) -> bool> Sink for Matches<'_, F> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, line: &SinkMatch<'_>) -> io::Result<bool> {
        // Without its ending, so that `$` means the end of the text, as in ripgrep's printer.
        let bytes = line.bytes();
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        let (start, number) = (line.absolute_byte_offset(), line.line_number());
        let found = &mut self.found;
        let mut going = true;
        self.matcher
            .find_iter(bytes, |range| {
                // A pattern such as `x*` matches nothing everywhere, which is no use here.
                if !range.is_empty() {
                    going = found(range.start()..range.end(), bytes, start, number);
                }
                going
            })
            .map_err(io::Error::other)?;
        Ok(going)
    }
}

fn searcher() -> SearcherBuilder {
    let mut builder = SearcherBuilder::new();
    // To match the matcher: lines end at `\n`, and a `\r` before it is not part of the text.
    builder
        .line_terminator(LineTerminator::crlf())
        .heap_limit(Some(MAX_LINE_BYTES));
    builder
}

/// Search a document's text, calling `found` with each match and the line holding it, until
/// it returns false. Nothing is reported as an error after `cancel` is set.
pub fn scan(
    text: &TextBuffer,
    matcher: &RegexMatcher,
    cancel: &AtomicBool,
    mut found: impl FnMut(Range<usize>, &[u8], usize) -> bool,
) -> Result<(), String> {
    let reader = Cancellable {
        inner: text.reader(),
        cancel,
    };
    let sink = Matches {
        matcher,
        found: |range: Range<usize>, line: &[u8], start: u64, _: Option<u64>| {
            let start = start as usize;
            found(start + range.start..start + range.end, line, start)
        },
    };
    let result = searcher()
        // The text is already decoded, and a NUL in it is just another character.
        .bom_sniffing(false)
        .line_number(false)
        .binary_detection(BinaryDetection::none())
        .build()
        .search_reader(matcher, reader, sink);
    match result {
        Err(_) if cancel.load(Ordering::Relaxed) => Ok(()),
        Err(error) if error.to_string().contains("allocation limit") => {
            Err("A line is too long to search".into())
        }
        Err(error) => Err(error.to_string()),
        Ok(()) => Ok(()),
    }
}

/// What a replacement template makes of the match at `at` in `line`: `$1` and `$name` stand
/// for capture groups in a regular expression, and in literal text nothing is special.
pub fn expand(
    matcher: &RegexMatcher,
    regex: bool,
    template: &str,
    line: &[u8],
    at: usize,
) -> String {
    if !regex {
        return template.to_owned();
    }
    let mut output = Vec::new();
    let expanded = matcher.new_captures().ok().and_then(|mut captures| {
        matcher.captures_at(line, at, &mut captures).ok()?;
        captures.get(0)?;
        captures.interpolate(
            |name| matcher.capture_index(name),
            line,
            template.as_bytes(),
            &mut output,
        );
        Some(())
    });
    match expanded {
        Some(()) => String::from_utf8_lossy(&output).into_owned(),
        None => template.to_owned(),
    }
}

/// One matching line in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The file it is in.
    pub path: PathBuf,
    /// Its line, counting from one.
    pub line: usize,
    /// The line's text, shortened to keep the match in view.
    pub text: String,
}

/// The part of a matched line worth showing, with the match near its start.
fn excerpt(line: &[u8], match_start: usize) -> String {
    let text = String::from_utf8_lossy(line);
    let lead = text.get(..match_start).unwrap_or_default();
    let skip = lead.chars().count().saturating_sub(CONTEXT_CHARS);
    let start = if lead.trim_start().chars().count() <= CONTEXT_CHARS {
        text.len() - text.trim_start().len()
    } else {
        lead.char_indices().nth(skip).map_or(0, |(at, _)| at)
    };
    text[start..]
        .trim_end()
        .chars()
        .take(LINE_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Search every file under `root` that ripgrep would, sending what each file holds. Files
/// that `.gitignore` excludes, hidden files and binary files are all left out.
fn search_files(
    root: &Path,
    matcher: &RegexMatcher,
    cancel: &AtomicBool,
    mut send: impl FnMut(Vec<Hit>),
) {
    let mut total = 0;
    let mut searcher = searcher()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let walk = WalkBuilder::new(root)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();
    for entry in walk.flatten() {
        if cancel.load(Ordering::Relaxed) || total >= MAX_HITS {
            return;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(file) = File::open(entry.path()) else {
            continue;
        };
        let mut hits = Vec::new();
        let mut last_line = None;
        let sink = Matches {
            matcher,
            found: |range: Range<usize>, line: &[u8], _: u64, number: Option<u64>| {
                // Each line is listed once, however many matches it holds.
                if last_line != number {
                    last_line = number;
                    hits.push(Hit {
                        path: entry.path().to_owned(),
                        line: number.unwrap_or(1) as usize,
                        text: excerpt(line, range.start),
                    });
                }
                total + hits.len() < MAX_HITS
            },
        };
        let reader = Cancellable {
            inner: file,
            cancel,
        };
        // A file that cannot be read, or has too long a line, just has no results.
        let _ = searcher.search_reader(matcher, reader, sink);
        if !hits.is_empty() {
            total += hits.len();
            send(hits);
        }
    }
}

/// A search across files in progress. Dropping it stops the search.
struct Job {
    cancel: Arc<AtomicBool>,
    results: Receiver<Vec<Hit>>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The latest search across files, run in the background as its query changes.
#[derive(Default)]
pub(super) struct GlobalSearch {
    query: String,
    root: PathBuf,
    hits: Vec<Hit>,
    job: Option<Job>,
    problem: Option<String>,
    /// Counts changes to the results, so they are only listed again when they differ.
    generation: usize,
}

impl GlobalSearch {
    pub fn hits(&self) -> &[Hit] {
        &self.hits
    }
    pub fn generation(&self) -> usize {
        self.generation
    }
    pub fn searching(&self) -> bool {
        self.job.is_some()
    }
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }
    /// Whether the results reached the most that are kept, so that there may be more.
    pub fn full(&self) -> bool {
        self.hits.len() >= MAX_HITS
    }

    /// Search for `query` unless that is already done. Text between slashes, as in `/a.c/`,
    /// is a regular expression; other text is literal. Case matters only if it has capitals.
    pub fn request(&mut self, query: &str, root: &Path, waker: &Waker) {
        if query == self.query && root == self.root {
            return;
        }
        self.query = query.to_owned();
        self.root = root.to_owned();
        self.hits.clear();
        self.job = None;
        self.problem = None;
        self.generation += 1;
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return;
        }
        let pattern = trimmed
            .strip_prefix('/')
            .and_then(|rest| rest.strip_suffix('/'))
            .filter(|pattern| !pattern.is_empty());
        let options = Options {
            smart_case: true,
            regex: pattern.is_some(),
            ..Options::default()
        };
        let matcher = match compile(pattern.unwrap_or(trimmed), options) {
            Ok(matcher) => matcher,
            Err(problem) => {
                self.problem = Some(problem);
                return;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (send, results) = mpsc::channel();
        let (flag, root, waker) = (cancel.clone(), root.to_owned(), waker.clone());
        std::thread::spawn(move || {
            search_files(&root, &matcher, &flag, |hits| {
                let _ = send.send(hits);
                waker();
            });
            drop(send);
            waker();
        });
        self.job = Some(Job { cancel, results });
    }

    /// Forget the search, stopping it if it is still going.
    pub fn clear(&mut self) {
        if self.query.is_empty() && self.root.as_os_str().is_empty() {
            return;
        }
        *self = Self {
            generation: self.generation,
            ..Self::default()
        };
    }

    /// Take what has been found since the last call.
    pub fn poll(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        loop {
            match job.results.try_recv() {
                Ok(hits) => {
                    self.hits.extend(hits);
                    self.generation += 1;
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.job = None;
                    self.generation += 1;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn a_line_too_long_to_hold_stops_the_search() {
        let long = TextBuffer::from_text(&"x".repeat(MAX_LINE_BYTES + 1024));
        let matcher = compile("needle", literal()).unwrap();
        let result = scan(&long, &matcher, &AtomicBool::new(false), |_, _, _| true);
        assert_eq!(result, Err("A line is too long to search".into()));
    }

    fn literal() -> Options {
        Options::default()
    }
    fn found(text: &str, query: &str, options: Options) -> Vec<Range<usize>> {
        let matcher = compile(query, options).unwrap();
        let mut matches = Vec::new();
        let buffer = TextBuffer::from_text(text);
        scan(&buffer, &matcher, &AtomicBool::new(false), |range, _, _| {
            matches.push(range);
            true
        })
        .unwrap();
        matches
    }

    #[test]
    fn options_change_what_matches() {
        let text = "Cat cat concat\ncat.\n";
        assert_eq!(found(text, "cat", literal()).len(), 4);
        let case = Options {
            case_sensitive: true,
            ..literal()
        };
        assert_eq!(found(text, "Cat", case), [0..3]);
        assert_eq!(
            found(
                text,
                "cat",
                Options {
                    smart_case: true,
                    ..literal()
                }
            )
            .len(),
            4
        );
        assert_eq!(
            found(
                text,
                "Cat",
                Options {
                    smart_case: true,
                    ..literal()
                }
            ),
            [0..3]
        );
        let word = Options {
            whole_word: true,
            ..literal()
        };
        assert_eq!(found(text, "cat", word), [0..3, 4..7, 15..18]);
        // Literal text means what it says, and a regular expression does not.
        assert_eq!(found(text, "cat.", literal()), [15..19]);
        let regex = Options {
            regex: true,
            ..literal()
        };
        assert_eq!(found(text, "c.t$", regex), [11..14]);
        assert_eq!(found("a\nb", "a*", regex), [0..1], "no empty matches");
        // A line ending in CRLF still ends where `$` says.
        assert_eq!(found("cat\r\n", "t$", regex), [2..3]);
        assert!(compile("(", regex).is_err());
        assert!(
            compile("a\nb", regex).is_err(),
            "patterns stay within a line"
        );
    }

    #[test]
    fn matches_are_found_across_the_pieces_of_a_buffer() {
        let mut text = TextBuffer::from_text("one needle\n");
        text.append(&TextBuffer::from_text("two nee"));
        text.append(&TextBuffer::from_text("dle\nthré"));
        text.append(&TextBuffer::from_text("e needle"));
        assert_eq!(text.to_text(), "one needle\ntwo needle\nthrée needle");
        let matcher = compile("needle", literal()).unwrap();
        let mut matches = Vec::new();
        scan(
            &text,
            &matcher,
            &AtomicBool::new(false),
            |range, line, start| {
                assert_eq!(&line[range.start - start..range.end - start], b"needle");
                matches.push(range);
                true
            },
        )
        .unwrap();
        assert_eq!(matches, [4..10, 15..21, 29..35]);
    }

    #[test]
    fn a_search_stops_when_asked() {
        let matcher = compile("a", literal()).unwrap();
        let text = TextBuffer::from_text(&"a\n".repeat(1000));
        let mut count = 0;
        scan(&text, &matcher, &AtomicBool::new(false), |_, _, _| {
            count += 1;
            count < 3
        })
        .unwrap();
        assert_eq!(count, 3);
        let cancelled = AtomicBool::new(true);
        let mut count = 0;
        let result = scan(&text, &matcher, &cancelled, |_, _, _| {
            count += 1;
            true
        });
        assert_eq!((result, count), (Ok(()), 0));
    }

    #[test]
    fn templates_fill_in_capture_groups() {
        let regex = compile(
            r"(\w+)=(?P<value>\d+)",
            Options {
                regex: true,
                ..literal()
            },
        )
        .unwrap();
        let line = b"skip a=1 b=2";
        assert_eq!(expand(&regex, true, "$2:$1", line, 5), "1:a");
        assert_eq!(expand(&regex, true, "$value:$1", line, 9), "2:b");
        // A literal search leaves a template alone.
        let plain = compile("a=1", literal()).unwrap();
        assert_eq!(expand(&plain, false, "$1", line, 5), "$1");
    }

    #[test]
    fn excerpts_keep_the_match_in_view() {
        assert_eq!(excerpt(b"    let x = 1;\r\n", 8), "let x = 1;");
        let long = format!("{}needle end\n", "é".repeat(100));
        let start = long.find("needle").unwrap();
        let shown = excerpt(long.as_bytes(), start);
        assert_eq!(shown.chars().filter(|c| *c == 'é').count(), CONTEXT_CHARS);
        assert!(shown.ends_with("needle end"));
        assert_eq!(excerpt(b"a\tb\n", 0), "a b");
    }

    fn run(root: &Path, query: &str) -> Vec<Hit> {
        let matcher = compile(
            query,
            Options {
                smart_case: true,
                ..literal()
            },
        )
        .unwrap();
        let mut hits = Vec::new();
        search_files(root, &matcher, &AtomicBool::new(false), |found| {
            hits.extend(found);
        });
        hits
    }

    #[test]
    fn searching_files_skips_what_ripgrep_skips() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("ignored")).unwrap();
        std::fs::write(root.join("a.txt"), "first\nneedle one needle\n").unwrap();
        std::fs::write(root.join("src/b.txt"), "needle two\n").unwrap();
        std::fs::write(root.join("ignored/c.txt"), "needle ignored\n").unwrap();
        std::fs::write(root.join(".hidden"), "needle hidden\n").unwrap();
        std::fs::write(root.join("binary"), b"needle\0binary\n").unwrap();

        let hits = run(root, "needle");
        let listed: Vec<_> = hits
            .iter()
            .map(|hit| (hit.path.strip_prefix(root).unwrap().to_owned(), hit.line))
            .collect();
        assert_eq!(
            listed,
            [(PathBuf::from("a.txt"), 2), (PathBuf::from("src/b.txt"), 1)],
            "one row per line, in file order"
        );
        assert_eq!(hits[0].text, "needle one needle");
    }

    #[test]
    fn results_stop_at_the_limit() {
        let workspace = tempfile::tempdir().unwrap();
        let line = "needle\n".repeat(MAX_HITS / 2 + 1);
        for name in ["a", "b", "c"] {
            std::fs::write(workspace.path().join(name), &line).unwrap();
        }
        assert_eq!(run(workspace.path(), "needle").len(), MAX_HITS);
    }

    #[test]
    fn global_search_follows_its_query_in_the_background() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("a.txt"), "alpha\nBeta\n").unwrap();
        let waker: Waker = Arc::new(|| {});
        let mut search = GlobalSearch::default();
        let finish = |search: &mut GlobalSearch| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while search.searching() {
                assert!(std::time::Instant::now() < deadline);
                search.poll();
            }
        };
        search.request("alpha", workspace.path(), &waker);
        finish(&mut search);
        assert_eq!(search.hits()[0].line, 1);
        // Asking again changes nothing, so nothing has to be listed again.
        let generation = search.generation();
        search.request("alpha", workspace.path(), &waker);
        assert_eq!(search.generation(), generation);
        // A pattern between slashes is a regular expression, and capitals make it exact.
        search.request("/a.ph/", workspace.path(), &waker);
        finish(&mut search);
        assert_eq!(search.hits().len(), 1);
        search.request("BETA", workspace.path(), &waker);
        finish(&mut search);
        assert!(search.hits().is_empty());
        search.request("/(/", workspace.path(), &waker);
        assert!(search.problem().is_some());
        search.request("  ", workspace.path(), &waker);
        assert!(!search.searching() && search.problem().is_none());
    }
}
