//! Finds where symbols are defined, using Universal Ctags and its tag files.
//!
//! Universal Ctags is a separate GPL program that `teditor` only ever runs as a process, so
//! its licence does not reach this crate; see `licenses/README.md`. The reader here is written
//! from the published format described in its `tags(5)` manual.
//!
//! A tag file is sorted by name, so a lookup binary-searches the file on disk. An index of
//! any size costs a handful of small reads per lookup, and is never loaded into memory. It is
//! sorted ignoring case, so that searching by a few typed letters finds any capitalization.

use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Where a project lists what to leave out of its index, relative to its root.
pub const IGNORE_FILE: &str = ".teditor/.ctagsignore";

/// What a new ignore file says: generated folders, and bundled copies of other projects whose
/// names would crowd out the project's own. Ctags reads the file itself, so these are its rules.
const DEFAULT_IGNORES: &str = "\
# What symbol search leaves out: one name, wildcard or folder path per line, such as
# vendor, *.min.js or src/generated. A name is matched in every folder. Spaces count as
# part of a name. A line starting with # only ever matches a file named just like it.
.git
.teditor
target
node_modules
.venv
build
vendor
third_party
";

/// The root of the project a folder belongs to: the nearest folder, starting from itself, that
/// has a `.git` or `.teditor`. Failing that, the folder is taken to be a root.
fn project_root(folder: &Path) -> PathBuf {
    let folder = folder.canonicalize().unwrap_or_else(|_| folder.to_owned());
    folder
        .ancestors()
        .find(|path| path.join(".git").exists() || path.join(".teditor").exists())
        .unwrap_or(&folder)
        .to_owned()
}

/// Give the project an opened folder belongs to its ignore file, leaving one that already
/// exists as it is, and return where it is.
pub fn ensure_ignore_file(folder: &Path) -> io::Result<PathBuf> {
    let path = project_root(folder).join(IGNORE_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let created = OpenOptions::new().write(true).create_new(true).open(&path);
    match created {
        Ok(mut file) => file.write_all(DEFAULT_IGNORES.as_bytes())?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    Ok(path)
}

/// One definition found in a source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// The symbol's name.
    pub name: String,
    /// The file defining it.
    pub path: PathBuf,
    /// Its line, counting from one.
    pub line: usize,
    /// What it is, such as `function` or `struct`; the words vary by language.
    pub kind: String,
    /// The name of what contains it, such as a method's type.
    pub scope: Option<String>,
    /// What contains it, such as `enum` or `module`; the words vary by language.
    pub scope_kind: Option<String>,
    /// Ctags found it to be visible only within its own file.
    pub file_only: bool,
    /// The language it is written in, such as `Rust`, spelled as ctags spells it.
    pub language: Option<String>,
}

impl Tag {
    /// Whether it belongs to something it must be reached through, such as a method, field or
    /// enum variant, rather than standing on its own within a module or namespace.
    pub fn is_member(&self) -> bool {
        self.scope_kind
            .as_deref()
            .is_some_and(|kind| !matches!(kind, "module" | "namespace" | "package"))
    }

    /// The innermost name of its scope: `K` for both `ns::K` and `main.K`.
    pub fn scope_name(&self) -> Option<&str> {
        let scope = self.scope.as_deref()?;
        scope.rsplit("::").next()?.rsplit('.').next()
    }
}

/// Whether code in one language can refer to a definition in another. Headers make C and its
/// relatives one family, and TypeScript commonly uses JavaScript directly.
pub fn languages_connect(a: &str, b: &str) -> bool {
    let family = |language| match language {
        "C" | "C++" | "ObjectiveC" => "C",
        "JavaScript" | "TypeScript" => "JavaScript",
        other => other,
    };
    family(a) == family(b)
}

/// The installed Universal Ctags program.
#[derive(Clone, Debug)]
pub struct Ctags {
    binary: PathBuf,
}

impl Ctags {
    /// Find the copy shipped with this application, or the one this build compiled.
    ///
    /// This deliberately ignores `PATH`: another `ctags` may be a different program with
    /// different options, as on macOS, or something planted in an opened folder.
    pub fn locate() -> Option<Self> {
        let name = format!("ctags{}", std::env::consts::EXE_SUFFIX);
        let beside_application = std::env::current_exe()
            .ok()
            .and_then(|path| Some(path.parent()?.join(&name)));
        let built = option_env!("TEDITOR_CTAGS").map(PathBuf::from);
        let binary = beside_application
            .into_iter()
            .chain(built)
            .find(|path| path.is_file())?;
        Some(Self { binary })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        // Options in an opened folder's `.ctags.d` must not change what this program does.
        command
            .args(["--options=NONE", "--sort=foldcase", "--excmd=number"])
            .args(["--fields=+nKl", "--tag-relative=never", "--links=no"])
            .stdin(Stdio::null());
        command
    }

    /// Index every source file under `root` into a private temporary tag file, which is
    /// removed along with the returned [`TagFile`]. What [`IGNORE_FILE`] lists is left out,
    /// or the defaults in a folder where it cannot be created.
    pub fn index(&self, root: &Path) -> io::Result<TagFile> {
        let output = tempfile::NamedTempFile::new()?.into_temp_path();
        let excludes: Vec<OsString> = match ensure_ignore_file(root) {
            Ok(path) => {
                let mut option = OsString::from("--exclude=@");
                option.push(path);
                vec![option]
            }
            Err(_) => DEFAULT_IGNORES
                .lines()
                .filter(|line| !line.starts_with('#'))
                .map(|name| format!("--exclude={name}").into())
                .collect(),
        };
        let mut command = self.command();
        command
            .current_dir(root)
            .arg("-R")
            .args(excludes)
            .arg("-f")
            .arg(&output)
            .arg(".");
        run(&mut command)?;
        let mut tags = TagFile::open(&output)?;
        tags._temporary = Some(output);
        Ok(tags)
    }

    /// The language ctags reads a file as, if it recognizes it.
    pub fn language_of(&self, file: &Path) -> io::Result<Option<String>> {
        let file = std::path::absolute(file)?;
        let mut command = self.command();
        command.arg("--print-language").arg(OsStr::new(&file));
        let output = run(&mut command)?;
        let output = String::from_utf8_lossy(&output);
        let language = output.rsplit_once(": ").map_or("", |(_, name)| name.trim());
        Ok((!language.is_empty() && language != "NONE").then(|| language.to_owned()))
    }

    /// The definitions in one file, in order of name.
    pub fn symbols(&self, file: &Path) -> io::Result<Vec<Tag>> {
        // An absolute path can never be mistaken for an option.
        let file = std::path::absolute(file)?;
        let mut command = self.command();
        command.args(["-f", "-"]).arg(OsStr::new(&file));
        let output = run(&mut command)?;
        Ok(output
            .split(|&b| b == b'\n')
            .filter_map(parse_line)
            .collect())
    }
}

/// Run a command to completion, returning what it printed.
fn run(command: &mut Command) -> io::Result<Vec<u8>> {
    let output = command.output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(io::Error::other(format!(
            "ctags failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

/// A sorted tag file on disk.
pub struct TagFile {
    file: File,
    length: u64,
    /// Removes the file when this is dropped, if it is a private index.
    _temporary: Option<tempfile::TempPath>,
}

impl TagFile {
    /// Open a tag file, which must be sorted as `ctags --sort=foldcase` writes it.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let length = file.metadata()?.len();
        let mut lines = BufReader::new(&file);
        let mut line = Vec::new();
        // The headers come first and sort before ordinary names.
        while lines.read_until(b'\n', &mut line)? > 0 && line.starts_with(b"!_TAG_") {
            if line.starts_with(b"!_TAG_FILE_SORTED\t")
                && !line.starts_with(b"!_TAG_FILE_SORTED\t2")
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "this tag file is not sorted ignoring case",
                ));
            }
            line.clear();
        }
        Ok(Self {
            file,
            length,
            _temporary: None,
        })
    }

    /// Every definition of exactly this name.
    pub fn find(&self, name: &str) -> io::Result<Vec<Tag>> {
        let target = name.as_bytes();
        self.collect(
            name,
            usize::MAX,
            |found| folded(found).eq(folded(target)),
            |found| found == target,
        )
    }

    /// Up to `limit` definitions whose names start with `prefix`, ignoring case, in order
    /// of name.
    pub fn find_prefix(&self, prefix: &str, limit: usize) -> io::Result<Vec<Tag>> {
        let within = |found: &[u8]| {
            found.len() >= prefix.len()
                && folded(&found[..prefix.len()]).eq(folded(prefix.as_bytes()))
        };
        self.collect(prefix, limit, within, |_| true)
    }

    /// From the first name at or after `start`, go through the lines `within` accepts,
    /// reading those `wanted` also accepts.
    fn collect(
        &self,
        start: &str,
        limit: usize,
        within: impl Fn(&[u8]) -> bool,
        wanted: impl Fn(&[u8]) -> bool,
    ) -> io::Result<Vec<Tag>> {
        let mut lines = BufReader::new(&self.file);
        lines.seek(SeekFrom::Start(self.first_at_or_after(start.as_bytes())?))?;
        let mut tags = Vec::new();
        let mut line = Vec::new();
        while tags.len() < limit {
            line.clear();
            if lines.read_until(b'\n', &mut line)? == 0 || !within(name_of(&line)) {
                break;
            }
            if wanted(name_of(&line)) {
                tags.extend(parse_line(&line));
            }
        }
        Ok(tags)
    }

    /// The offset of the first line whose name is not below `target`.
    fn first_at_or_after(&self, target: &[u8]) -> io::Result<u64> {
        let (mut low, mut high) = (0, self.length);
        let mut line = Vec::new();
        // `low` is always the start of a line; each probe reads the first line after `middle`.
        while low < high {
            let middle = low + (high - low) / 2;
            let mut lines = BufReader::new(&self.file);
            lines.seek(SeekFrom::Start(middle))?;
            let mut line_start = middle;
            if middle > low {
                line.clear();
                line_start += lines.read_until(b'\n', &mut line)? as u64;
            }
            line.clear();
            let read = lines.read_until(b'\n', &mut line)? as u64;
            if read > 0 && folded(name_of(&line)).lt(folded(target)) {
                low = line_start + read;
            } else {
                high = middle;
            }
        }
        Ok(low)
    }
}

/// A name as ctags compares it when sorting: bytes in ASCII upper case. Lower case would not
/// do, because it would put `_` on the other side of the letters.
fn folded(name: &[u8]) -> impl Iterator<Item = u8> + '_ {
    name.iter().map(u8::to_ascii_uppercase)
}

fn name_of(line: &[u8]) -> &[u8] {
    line.split(|&b| b == b'\t').next().unwrap_or(line)
}

/// Read one line: `name<TAB>file<TAB>line;"<TAB>field...`. Headers and anything else
/// unexpected are not tags.
fn parse_line(line: &[u8]) -> Option<Tag> {
    if line.starts_with(b"!_TAG_") {
        return None;
    }
    let line = std::str::from_utf8(line)
        .ok()?
        .trim_end_matches(['\n', '\r']);
    let mut fields = line.split('\t');
    let name = unescape(fields.next().filter(|name| !name.is_empty())?);
    let path = PathBuf::from(unescape(fields.next()?));
    let line = fields.next()?.strip_suffix(";\"")?.parse().ok()?;
    let mut tag = Tag {
        name,
        path,
        line,
        kind: String::new(),
        scope: None,
        scope_kind: None,
        file_only: false,
        language: None,
    };
    for field in fields {
        match field.split_once(':') {
            None => tag.kind = unescape(field),
            Some(("language", language)) => tag.language = Some(unescape(language)),
            // Fields this reader does not ask for, or has no use for.
            Some(("line" | "typeref" | "roles" | "access" | "signature", _)) => {}
            Some(("file", _)) => tag.file_only = true,
            // Anything else is the kind of the enclosing symbol, then its name.
            Some((kind, scope)) => {
                tag.scope_kind = Some(unescape(kind));
                tag.scope = Some(unescape(scope));
            }
        }
    }
    Some(tag)
}

/// Undo the backslash escapes tag files use for characters that would break a line.
fn unescape(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some('t') => output.push('\t'),
            Some('n') => output.push('\n'),
            Some('r') => output.push('\r'),
            Some('\\') => output.push('\\'),
            // Reserved for other uses, so kept as written: not every backslash is an escape.
            Some(other) => {
                output.push('\\');
                output.push(other);
            }
            None => output.push('\\'),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tag_file(lines: &str) -> (tempfile::NamedTempFile, TagFile) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(lines.as_bytes()).unwrap();
        let tags = TagFile::open(file.path()).unwrap();
        (file, tags)
    }

    #[test]
    fn lines_parse_into_tags() {
        let tag = parse_line(
            b"gamma\t/w/a.rs\t4;\"\tmethod\tline:4\tlanguage:Rust\timplementation:Beta\n",
        )
        .unwrap();
        assert_eq!(
            tag,
            Tag {
                name: "gamma".into(),
                path: "/w/a.rs".into(),
                line: 4,
                kind: "method".into(),
                scope: Some("Beta".into()),
                scope_kind: Some("implementation".into()),
                file_only: false,
                language: Some("Rust".into()),
            }
        );
        // Escapes in names and paths are undone, and other backslashes kept.
        let tag = parse_line(b"a\\tb\tdir/a\\tb.py\t1;\"\tfunction\tline:1").unwrap();
        assert_eq!(tag.name, "a\tb");
        assert_eq!(tag.path, Path::new("dir/a\tb.py"));
        assert_eq!(unescape(r"C:\Users\x"), r"C:\Users\x");
        // Fields that are not scopes are not mistaken for one.
        let tag = parse_line(b"x\tf\t1;\"\tvariable\tline:1\ttyperef:typename:int\tfile:").unwrap();
        assert_eq!(tag.scope, None);
        assert!(tag.file_only);
        // Headers, and search patterns this reader never asks ctags for, are not tags.
        assert_eq!(parse_line(b"!_TAG_FILE_SORTED\t1\t//"), None);
        assert_eq!(parse_line(b"x\tf\t/^fn x$/;\"\tf"), None);
        assert_eq!(parse_line(b""), None);
        assert_eq!(parse_line(b"just one field"), None);
    }

    #[test]
    fn lookups_find_every_definition_at_any_position() {
        let header = "!_TAG_FILE_SORTED\t2\t/sorted/\n";
        let mut names: Vec<String> = (0..50_000).map(|n| format!("name{n:06}")).collect();
        names.sort();
        let mut lines = header.to_owned();
        for (index, name) in names.iter().enumerate() {
            for definition in 0..=index % 3 {
                lines += &format!("{name}\t/w/{definition}.rs\t{index};\"\tfunction\n");
            }
        }
        let (_file, tags) = tag_file(&lines);
        for index in [0, 1, 2, 3, 17, 25_000, 49_998, 49_999] {
            let found = tags.find(&names[index]).unwrap();
            assert_eq!(found.len(), index % 3 + 1, "{}", names[index]);
            assert!(
                found
                    .iter()
                    .all(|tag| tag.name == names[index] && tag.line == index)
            );
        }
        // Names missing before, between and after the ones present.
        for missing in ["a", "name000000x", "zzz", ""] {
            assert!(tags.find(missing).unwrap().is_empty(), "{missing:?}");
        }
        let prefix = tags.find_prefix("name00001", 5).unwrap();
        assert_eq!(prefix.len(), 5);
        assert!(prefix.iter().all(|tag| tag.name.starts_with("name00001")));
        assert_eq!(tags.find_prefix("nothing", 5).unwrap(), []);
    }

    #[test]
    fn edge_case_files_are_handled() {
        let (_file, empty) = tag_file("");
        assert!(empty.find("x").unwrap().is_empty());
        let (_file, last_line) = tag_file("only\t/a\t1;\"\tf");
        assert_eq!(last_line.find("only").unwrap().len(), 1);
        // Names sharing a start must not match each other, and order ignores case.
        let (_file, close) =
            tag_file("ab\t/a\t2;\"\tf\nabc\t/a\t3;\"\tf\nZebra\t/a\t1;\"\tf\n_x\t/a\t4;\"\tf\n");
        assert_eq!(close.find("ab").unwrap()[0].line, 2);
        assert_eq!(close.find("Zebra").unwrap()[0].line, 1);
        assert_eq!(close.find("_x").unwrap()[0].line, 4);
        assert!(close.find("zebra").unwrap().is_empty(), "exact means exact");
        assert_eq!(close.find_prefix("ZE", 5).unwrap()[0].name, "Zebra");
        assert_eq!(close.find_prefix("AB", 5).unwrap().len(), 2);
        // A file in another order would give wrong answers, so it is refused.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"!_TAG_FILE_SORTED\t1\t/by bytes/\n")
            .unwrap();
        assert!(TagFile::open(file.path()).is_err());
    }

    /// Runs the real program, which a build without its tools does not have.
    #[test]
    fn the_ignore_file_decides_what_is_indexed() {
        let Some(ctags) = Ctags::locate() else {
            eprintln!("skipping: this build has no ctags");
            return;
        };
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let write = |file: &str, name: &str| {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("fn {name}() {{}}\n")).unwrap();
        };
        write("own.rs", "own");
        write("vendor/lib.rs", "bundled");
        write("deep/vendor/lib.rs", "deep_bundled");
        write("src/generated/out.rs", "generated");
        write("mine.min.rs", "minified");
        let found = |tags: &TagFile, name: &str| !tags.find(name).unwrap().is_empty();

        // Created with the defaults when first needed.
        let tags = ctags.index(&root).unwrap();
        let ignore = root.join(IGNORE_FILE);
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), DEFAULT_IGNORES);
        assert!(found(&tags, "own") && found(&tags, "generated"));
        assert!(!found(&tags, "bundled") && !found(&tags, "deep_bundled"));

        // What the user writes replaces them, and is never overwritten.
        let edited = "# my list\n\nsrc/generated\r\n*.min.rs\n";
        std::fs::write(&ignore, edited).unwrap();
        ensure_ignore_file(&root).unwrap();
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), edited);
        let tags = ctags.index(&root).unwrap();
        assert!(found(&tags, "own") && found(&tags, "bundled"));
        assert!(!found(&tags, "generated") && !found(&tags, "minified"));

        // An emptied list leaves nothing out, and a deleted one comes back.
        std::fs::write(&ignore, "").unwrap();
        assert!(found(&ctags.index(&root).unwrap(), "generated"));
        std::fs::remove_file(&ignore).unwrap();
        assert!(!found(&ctags.index(&root).unwrap(), "bundled"));
        assert!(ignore.exists());

        // Where it cannot be created, the defaults still apply.
        std::fs::remove_dir_all(root.join(".teditor")).unwrap();
        std::fs::write(root.join(".teditor"), "not a folder").unwrap();
        assert!(ensure_ignore_file(&root).is_err());
        let tags = ctags.index(&root).unwrap();
        assert!(found(&tags, "own") && !found(&tags, "bundled"));
    }

    /// Runs the real program, which a build without its tools does not have.
    #[test]
    fn only_a_project_root_gets_an_ignore_file() {
        let Some(ctags) = Ctags::locate() else {
            eprintln!("skipping: this build has no ctags");
            return;
        };
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let nested = root.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(nested.join("a.rs"), "fn kept() {}\n").unwrap();
        std::fs::write(nested.join("b.rs"), "fn listed() {}\n").unwrap();

        // From anywhere in a repository the file is at its top, and only there.
        let path = ensure_ignore_file(&nested).unwrap();
        assert_eq!(path, root.join(IGNORE_FILE));
        assert!(!nested.join(".teditor").exists());
        assert!(!root.join("src/.teditor").exists());
        // Indexing a nested folder follows the root's list, and creates nothing there.
        std::fs::write(&path, "b.rs\n").unwrap();
        let tags = ctags.index(&nested).unwrap();
        assert_eq!(tags.find("kept").unwrap().len(), 1);
        assert!(tags.find("listed").unwrap().is_empty());
        assert!(!nested.join(".teditor").exists());

        // Without a repository, an existing `.teditor` marks the root, and otherwise the
        // folder opened is one.
        let plain = tempfile::tempdir().unwrap();
        let plain = plain.path().canonicalize().unwrap();
        let inner = plain.join("inner");
        std::fs::create_dir(&inner).unwrap();
        assert_eq!(ensure_ignore_file(&plain).unwrap(), plain.join(IGNORE_FILE));
        assert_eq!(ensure_ignore_file(&inner).unwrap(), plain.join(IGNORE_FILE));
        assert!(!inner.join(".teditor").exists());
    }

    #[test]
    fn related_languages_connect() {
        assert!(languages_connect("Rust", "Rust"));
        assert!(languages_connect("C", "C++"));
        assert!(languages_connect("TypeScript", "JavaScript"));
        assert!(!languages_connect("Rust", "YACC"));
        assert!(!languages_connect("C", "JavaScript"));
    }

    /// Runs the real program, which a build without its tools does not have.
    #[test]
    fn the_built_ctags_indexes_a_workspace() {
        let Some(ctags) = Ctags::locate() else {
            eprintln!("skipping: this build has no ctags");
            return;
        };
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let source = "fn alpha() {}\nstruct Beta;\nimpl Beta {\n    fn gamma(&self) {}\n}\nconst DÉLTA: i32 = 1;\n";
        std::fs::write(root.join("a.rs"), source).unwrap();
        std::fs::write(root.join("b.py"), "def alpha():\n    pass\n").unwrap();
        // Names around `_`, which sorts between the upper and lower case letters in bytes.
        let names = [
            "_under", "Zeta", "aardvark", "ZZtop", "zebra", "__dunder", "b_c", "B9", "bA",
        ];
        let python: String = names
            .iter()
            .map(|n| format!("def {n}():\n    pass\n"))
            .collect();
        std::fs::write(root.join("mixed.py"), python).unwrap();
        // Generated folders stay out of the index.
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target/generated.rs"), "fn generated() {}\n").unwrap();
        // So do bundled copies of other projects.
        std::fs::create_dir_all(root.join("vendor/other")).unwrap();
        std::fs::write(root.join("vendor/other/lib.rs"), "fn bundled() {}\n").unwrap();
        // A folder's own ctags options must have no effect: this would hide everything.
        std::fs::create_dir(root.join(".ctags.d")).unwrap();
        std::fs::write(root.join(".ctags.d/evil.ctags"), "--languages=-all\n").unwrap();

        let tags = ctags.index(&root).unwrap();
        for name in names {
            assert_eq!(tags.find(name).unwrap().len(), 1, "{name}");
        }
        assert_eq!(
            tags.find_prefix("ZE", 9).unwrap().len(),
            2,
            "Zeta and zebra"
        );
        let mut alpha = tags.find("alpha").unwrap();
        alpha.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(alpha.len(), 2);
        assert_eq!(alpha[0].path, root.join("a.rs"));
        assert_eq!(alpha[0].line, 1);
        assert_eq!(alpha[0].kind, "function");
        assert_eq!(alpha[1].path, root.join("b.py"));
        let gamma = &tags.find("gamma").unwrap()[0];
        assert_eq!(gamma.line, 4);
        assert_eq!(gamma.scope.as_deref(), Some("Beta"));
        assert_eq!(tags.find("DÉLTA").unwrap()[0].line, 6);
        assert_eq!(alpha[0].language.as_deref(), Some("Rust"));
        assert_eq!(alpha[1].language.as_deref(), Some("Python"));
        assert!(tags.find("generated").unwrap().is_empty());
        assert!(tags.find("bundled").unwrap().is_empty());
        let language = |file: &str| ctags.language_of(&root.join(file)).unwrap();
        assert_eq!(language("a.rs").as_deref(), Some("Rust"));
        assert_eq!(language("b.py").as_deref(), Some("Python"));
        std::fs::write(root.join("notes.unknown"), "text\n").unwrap();
        assert_eq!(language("notes.unknown"), None);
        // The index stays out of the workspace.
        assert!(!root.join("tags").exists());

        let symbols = ctags.symbols(&root.join("a.rs")).unwrap();
        let names: Vec<_> = symbols.iter().map(|tag| tag.name.as_str()).collect();
        assert!(
            names.contains(&"alpha") && names.contains(&"gamma"),
            "{names:?}"
        );
    }
}
