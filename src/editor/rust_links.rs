//! Compiler-accurate Cmd+click for Rust, as a layer over the ctags lookup.
//!
//! Rustdoc's `--generate-link-to-definition` writes each source file as a page in which every
//! name links to what the compiler resolved it to, inside function bodies too. That knows what
//! ctags cannot, such as the type of a local variable. So this runs `cargo doc` after saves, with
//! a nightly toolchain as the flag requires, and reads the page for the clicked file. Whenever it has no answer, for any reason, the click
//! goes on to the ctags lookup as if this did not exist.
//!
//! The flag is unstable, and the pages are meant for people rather than programs, so a Rust
//! update could break this. To turn it off, set [`ENABLED`] to false. To remove it, delete this
//! file and the lines that use `rust_links` in `mod.rs`, and `Reference::at` in `symbols.rs`.
//!
//! Running `cargo doc` runs the project's build scripts and procedural macros, so it only
//! starts after a Cmd+click in a Rust file, never just from opening a folder.

use super::{Editor, symbols::Reference};
use crate::tags::Ctags;
use std::{
    env, fs,
    ops::Range,
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
};

/// Whether Cmd+click asks rustdoc first.
const ENABLED: bool = true;

/// The running `cargo doc` and what it is for. Dropping this stops the run.
#[derive(Default)]
pub(super) struct RustLinks {
    /// The project being documented, once a click has asked for one.
    project: Option<PathBuf>,
    job: Option<Child>,
    /// Files were saved since the current or last run started.
    stale: bool,
    /// The installed nightly toolchain to use, once looked for.
    toolchain: Option<Option<String>>,
    /// Where the standard library's source is, once looked for.
    library: Option<Option<PathBuf>>,
}

impl Drop for RustLinks {
    fn drop(&mut self) {
        if let Some(job) = &mut self.job {
            let _ = job.kill();
            let _ = job.wait();
        }
    }
}

impl RustLinks {
    /// Note that a file was saved, so the pages are behind.
    pub fn saved(&mut self) {
        self.stale = self.project.is_some();
    }

    /// Whether `cargo doc` is running.
    #[cfg(test)]
    pub fn running(&self) -> bool {
        self.job.is_some()
    }

    /// Notice a finished run, and start another if files were saved in the meantime.
    pub fn poll(&mut self) {
        if let Some(job) = &mut self.job {
            match job.try_wait() {
                Ok(None) => return,
                Ok(Some(_)) | Err(_) => self.job = None,
            }
        }
        if self.stale {
            self.start();
        }
    }

    /// Document `project` if it is not what the pages are for.
    fn request(&mut self, project: &Path) {
        if self.project.as_deref() != Some(project) {
            *self = Self {
                project: Some(project.to_owned()),
                stale: true,
                toolchain: self.toolchain.take(),
                library: self.library.take(),
                job: self.job.take(),
            };
            self.poll();
        }
    }

    fn start(&mut self) {
        self.stale = false;
        let (Some(project), Some(toolchain)) = (self.project.clone(), self.toolchain()) else {
            return;
        };
        // Offline, as this runs unasked. Each toolchain keeps its own build results, so this
        // neither disturbs the user's own builds nor is disturbed by them.
        self.job = no_auto_install(Command::new("cargo"))
            .arg(format!("+{toolchain}"))
            .args(["doc", "--offline", "--no-deps", "--document-private-items"])
            .arg("--workspace")
            .current_dir(project)
            .env(
                "RUSTDOCFLAGS",
                "-Z unstable-options --generate-link-to-definition",
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
    }

    /// The nightly toolchain that is installed, which is never one to be downloaded.
    fn toolchain(&mut self) -> Option<String> {
        self.toolchain
            .get_or_insert_with(|| {
                let output = no_auto_install(Command::new("rustup"))
                    .args(["toolchain", "list"])
                    .output()
                    .ok()?;
                pick_nightly(&String::from_utf8_lossy(&output.stdout))
            })
            .clone()
    }

    /// The standard library's source, if the `rust-src` component is installed.
    fn library(&mut self, project: &Path) -> Option<PathBuf> {
        let toolchain = self.toolchain();
        self.library
            .get_or_insert_with(|| {
                let toolchain = toolchain.clone()?;
                let output = no_auto_install(Command::new("rustc"))
                    .arg(format!("+{toolchain}"))
                    .args(["--print", "sysroot"])
                    .current_dir(project)
                    .output()
                    .ok()?;
                let sysroot = String::from_utf8(output.stdout).ok()?;
                let library = Path::new(sysroot.trim()).join("lib/rustlib/src/rust/library");
                library.is_dir().then_some(library)
            })
            .clone()
    }
}

/// Keep rustup from downloading a toolchain that a command names, which it otherwise does.
fn no_auto_install(mut command: Command) -> Command {
    command.env("RUSTUP_AUTO_INSTALL", "0");
    command
}

/// The best nightly in the output of `rustup toolchain list`: the newest dated one, since a plain
/// `nightly` moves with each update and every move means building all dependencies again.
fn pick_nightly(list: &str) -> Option<String> {
    let names = list
        .lines()
        .filter_map(|line| line.split_whitespace().next());
    let nightlies = names.filter(|name| name.starts_with("nightly"));
    let dated = |name: &&str| {
        name.strip_prefix("nightly-")
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    };
    nightlies
        .max_by_key(|name| (dated(name), *name))
        .map(str::to_owned)
}

/// What a page says about a clicked name.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// Defined at this file and line.
    At(PathBuf, usize),
    /// Defined somewhere that cannot be opened, which ctags would only guess at.
    Outside(String),
}

/// The project a source file belongs to, where Cargo should run: the nearest manifest, or the
/// workspace that one is part of.
fn project_of(file: &Path) -> Option<PathBuf> {
    let mut nearest = None;
    for folder in file.ancestors().skip(1) {
        let manifest = folder.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        if nearest.is_none() {
            nearest = Some(folder.to_owned());
        }
        if fs::read_to_string(&manifest).is_ok_and(|text| text.contains("[workspace]")) {
            return Some(folder.to_owned());
        }
    }
    nearest
}

/// The page rustdoc wrote for a source file, and that file's path relative to its crate.
fn page_for(file: &Path, project: &Path) -> Option<(PathBuf, PathBuf)> {
    let target = match env::var_os("CARGO_TARGET_DIR") {
        Some(target) => project.join(target),
        None => project.join("target"),
    };
    let crate_root = file
        .ancestors()
        .skip(1)
        .find(|folder| folder.join("Cargo.toml").is_file())?;
    let relative = file.strip_prefix(crate_root.join("src")).ok()?.to_owned();
    let mut name = relative.clone().into_os_string();
    name.push(".html");
    let pages = target.join("doc/src");
    let package = crate_root.file_name()?.to_string_lossy().replace('-', "_");
    let in_crate = |entry: &Path| Some(entry.join(&name)).filter(|page| page.is_file());
    let page = in_crate(&pages.join(package)).or_else(|| {
        let found: Vec<_> = fs::read_dir(&pages)
            .ok()?
            .flatten()
            .filter_map(|entry| in_crate(&entry.path()))
            .collect();
        // Another crate may have a file of the same name, which would be a wrong answer.
        (found.len() == 1).then(|| found.into_iter().next())?
    })?;
    Some((page, relative))
}

/// One source line from a page, and what its names link to.
#[derive(Debug, PartialEq, Eq)]
struct PageLine {
    text: String,
    links: Vec<(Range<usize>, String)>,
}

/// Read line `wanted`, counting from one, out of a page's HTML.
fn page_line(html: &str, wanted: usize) -> Option<PageLine> {
    const CODE: &str = "<pre class=\"rust\"><code>";
    let code = &html[html.find(CODE)? + CODE.len()..];
    let code = &code[..code.find("</code>")?];
    let mut line = PageLine {
        text: String::new(),
        links: Vec::new(),
    };
    let mut number = 0;
    let mut open = None;
    let mut rest = code;
    while let Some(at) = rest.find(['<', '&', '\n']) {
        line.text.push_str(&rest[..at]);
        match rest.as_bytes()[at] {
            b'\n' => {
                if number == wanted {
                    return Some(line);
                }
                line.text.clear();
                line.links.clear();
                rest = &rest[at + 1..];
            }
            b'&' => {
                let tail = &rest[at..];
                let end = tail.find(';').map_or(1, |end| end + 1);
                match decode_entity(&tail[..end]) {
                    Some(decoded) => line.text.push(decoded),
                    None => line.text.push_str(&tail[..end]),
                }
                rest = &tail[end..];
            }
            _ => {
                let tail = &rest[at..];
                let end = tail.find('>')? + 1;
                let tag = &tail[1..end - 1];
                rest = &tail[end..];
                if tag.contains("data-nosnippet") {
                    // A line's number, which marks where the line begins.
                    let close = rest.find("</a>")?;
                    number = rest[..close].parse().ok()?;
                    rest = &rest[close + 4..];
                } else if tag == "/a" {
                    if let Some((start, href)) = open.take() {
                        line.links.push((start..line.text.len(), href));
                    }
                } else if let Some(href) = tag.strip_prefix("a href=\"") {
                    let href = &href[..href.find('"')?];
                    open = Some((line.text.len(), href.replace("&amp;", "&")));
                }
            }
        }
    }
    line.text.push_str(rest);
    (number == wanted).then_some(line)
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "&lt;" => Some('<'),
        "&gt;" => Some('>'),
        "&amp;" => Some('&'),
        "&quot;" => Some('"'),
        _ => {
            let code = entity.strip_prefix("&#")?.strip_suffix(';')?;
            let code = match code.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => code.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// `base` joined with a relative link, with its `..` parts applied.
fn resolve_relative(base: &Path, relative: &str) -> PathBuf {
    let mut path = base.to_owned();
    for part in Path::new(relative).components() {
        match part {
            Component::ParentDir => {
                path.pop();
            }
            Component::Normal(name) => path.push(name),
            _ => {}
        }
    }
    path
}

/// Where a link on `page`, which shows `file`, leads. `relative` is `file` within its crate.
fn follow(
    href: &str,
    page: &Path,
    file: &Path,
    relative: &Path,
    library: impl FnOnce() -> Option<PathBuf>,
) -> Option<Found> {
    if href.starts_with("http") {
        return Some(std_item(href, library));
    }
    let (target, fragment) = href.split_once('#').unwrap_or((href, ""));
    if target.is_empty() {
        return Some(Found::At(file.to_owned(), first_line(fragment)?));
    }
    let linked = resolve_relative(page.parent()?, target);
    if target.ends_with(".rs.html") {
        return source_at(&linked, fragment, page, file, relative);
    }
    // An item's page has a link to its source, for each member under the member's own heading.
    let html = fs::read_to_string(&linked).ok()?;
    let from = match fragment {
        "" => 0,
        id => html.find(&format!("id=\"{id}\""))?,
    };
    let source = html[from..].find("class=\"src")? + from;
    let source = &html[source..];
    let source = &source[source.find("href=\"")? + 6..];
    let source = &source[..source.find('"')?];
    let (target, fragment) = source.split_once('#')?;
    let source_page = resolve_relative(linked.parent()?, target);
    source_at(&source_page, fragment, page, file, relative)
}

/// The fragment of a source page is a line, or a range of them.
fn first_line(fragment: &str) -> Option<usize> {
    fragment.split('-').next()?.parse().ok()
}

/// The source file a page in the same crate as `page` shows. `page` shows `file`.
fn source_at(
    linked: &Path,
    fragment: &str,
    page: &Path,
    file: &Path,
    relative: &Path,
) -> Option<Found> {
    // Up from the known page to its crate's folder, and from its file to the crate's source.
    let mut pages = page.to_owned();
    let mut source = file.to_owned();
    for _ in relative.components() {
        pages.pop();
        source.pop();
    }
    let inner = linked.strip_prefix(&pages).ok()?.to_str()?;
    let path = source.join(inner.strip_suffix(".html")?);
    Some(Found::At(path, first_line(fragment)?))
}

/// A link into the online standard library documentation, as a place in its local source.
fn std_item(href: &str, library: impl FnOnce() -> Option<PathBuf>) -> Found {
    let (address, fragment) = href.split_once('#').unwrap_or((href, ""));
    let parts: Vec<&str> = address.split('/').collect();
    let name = parts.last().copied().unwrap_or_default();
    let (_, item) = name
        .strip_suffix(".html")
        .and_then(|name| name.split_once('.'))
        .unwrap_or(("", ""));
    let member = fragment.split_once('.').map(|(_, member)| member);
    let shown = match member {
        Some(member) => format!("{item}::{member}"),
        None => item.to_owned(),
    };
    let outside = || Found::Outside(format!("{shown} is outside this project"));
    if !address.contains("doc.rust-lang.org") {
        return outside();
    }
    // `https://doc.rust-lang.org/1.96.0/std/process/struct.Child.html`: the crate, then modules.
    let Some(modules) = parts.len().checked_sub(1).and_then(|end| parts.get(4..end)) else {
        return outside();
    };
    let Some((documented, modules)) = modules.split_first() else {
        return outside();
    };
    let Some(library) = library() else {
        return outside();
    };
    // `std` re-exports much of `alloc` and `core`, so the source may be in any of them.
    for crate_name in [*documented, "alloc", "core", "std"] {
        let source = library.join(crate_name).join("src");
        let module = modules.join("/");
        for file in [
            source.join(format!("{module}.rs")),
            source.join(&module).join("mod.rs"),
        ] {
            if let Some(line) = line_in_source(&file, item, member) {
                return Found::At(file, line);
            }
        }
    }
    outside()
}

/// Where ctags finds an item, or a member of it, in one file.
fn line_in_source(file: &Path, item: &str, member: Option<&str>) -> Option<usize> {
    if !file.is_file() {
        return None;
    }
    let tags = Ctags::locate()?.symbols(file).ok()?;
    let found = tags.iter().find(|tag| match member {
        Some(member) => tag.name == member && tag.scope_name() == Some(item),
        None => tag.name == item && tag.kind != "implementation",
    })?;
    Some(found.line)
}

impl Editor {
    /// Follow a Cmd+clicked name with what rustdoc knows. Returns false if it knows nothing.
    pub(super) fn follow_rust_link(&mut self, reference: &Reference) -> bool {
        if !ENABLED {
            return false;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        let document = &tab.document;
        let Some(file) = document.path.clone() else {
            return false;
        };
        if file.extension().is_none_or(|extension| extension != "rs") {
            return false;
        }
        let Some(project) = project_of(&file) else {
            return false;
        };
        let row = document.row_at(reference.start());
        let column = reference.start() - document.line_start(row);
        let text = document.line(row);
        self.rust_links.request(&project);
        let Some((page, relative)) = page_for(&file, &project) else {
            return false;
        };
        let Ok(html) = fs::read_to_string(&page) else {
            return false;
        };
        let Some(line) = page_line(&html, row + 1) else {
            return false;
        };
        // A page older than the file is about different code, and would send the click astray.
        if line.text.trim_end_matches('\r') != text.trim_end_matches(['\r', '\n']) {
            return false;
        }
        let Some((_, href)) = line.links.iter().find(|(range, _)| range.contains(&column)) else {
            return false;
        };
        let links = &mut self.rust_links;
        let found = follow(href, &page, &file, &relative, || links.library(&project));
        match found {
            Some(Found::At(path, line)) => {
                self.status.clear();
                self.open_at_line(&path, line);
                true
            }
            Some(Found::Outside(message)) => {
                self.status = message;
                true
            }
            None => false,
        }
    }

    /// Keep the pages up to date.
    pub(super) fn poll_rust_links(&mut self) {
        if ENABLED {
            self.rust_links.poll();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "<html><div><pre class=\"rust\"><code>\
<a href=#1 id=1 data-nosnippet>1</a><span class=\"doccomment\">//! A &lt;note&gt;.\n\
<a href=#2 id=2 data-nosnippet>2</a></span><span class=\"kw\">use </span>a::{<a href=\"a.rs.html#27-31\">B</a>};\n\
<a href=#3 id=3 data-nosnippet>3</a>    <span class=\"kw\">let _ </span>= <a href=\"#87\">child</a>.<a href=\"https://doc.rust-lang.org/1.96.0/std/process/struct.Child.html#method.wait\">wait</a>();\n\
<a href=#4 id=4 data-nosnippet>4</a>}\n\
</code></pre></div></html>";

    #[test]
    fn a_nightly_is_chosen_from_what_is_installed() {
        let host = "aarch64-apple-darwin";
        let list = format!(
            "stable-{host} (active, default)\nnightly-{host}\n\
             nightly-2025-11-08-{host}\nnightly-2026-01-02-{host}\nnightly-2024-05-01-{host}\n"
        );
        assert_eq!(
            pick_nightly(&list),
            Some(format!("nightly-2026-01-02-{host}")),
            "the newest that stays put"
        );
        let plain = format!("stable-{host} (default)\nnightly-{host}\n");
        assert_eq!(pick_nightly(&plain), Some(format!("nightly-{host}")));
        assert_eq!(pick_nightly(&format!("stable-{host} (default)\n")), None);
        assert_eq!(pick_nightly(""), None);
    }

    #[test]
    fn a_line_and_its_links_are_read_from_a_page() {
        let line = page_line(PAGE, 3).unwrap();
        assert_eq!(line.text, "    let _ = child.wait();");
        assert_eq!(line.links[0], (12..17, "#87".to_owned()));
        assert_eq!(&line.text[line.links[1].0.clone()], "wait");
        assert!(line.links[1].1.ends_with("#method.wait"));
        // Text is decoded, a comment's span can cross lines, and links do not carry over.
        assert_eq!(page_line(PAGE, 1).unwrap().text, "//! A <note>.");
        assert_eq!(page_line(PAGE, 2).unwrap().text, "use a::{B};");
        assert_eq!(page_line(PAGE, 2).unwrap().links.len(), 1);
        assert_eq!(page_line(PAGE, 4).unwrap().links, []);
        assert_eq!(page_line(PAGE, 5), None);
        assert_eq!(page_line("<html>not a page</html>", 1), None);
    }

    #[test]
    fn links_lead_to_source_lines() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let pages = root.join("target/doc");
        let page = pages.join("src/krate/vt/a.rs.html");
        fs::create_dir_all(page.parent().unwrap()).unwrap();
        fs::create_dir_all(pages.join("krate/vt")).unwrap();
        fs::write(
            pages.join("krate/vt/struct.Pty.html"),
            "<a class=\"src\" href=\"../../src/krate/vt/b.rs.html#1-9\">Source</a>\
             <section id=\"method.spawn\"><a class=\"src rightside\" \
             href=\"../../src/krate/vt/b.rs.html#55-60\">Source</a></section>",
        )
        .unwrap();
        let file = root.join("src/vt/a.rs");
        let relative = Path::new("vt/a.rs");
        let go = |href| follow(href, &page, &file, relative, || None);
        // In this file, in a sibling, and through an item's page to it or a member.
        assert_eq!(go("#87"), Some(Found::At(file.clone(), 87)));
        let b = root.join("src/vt/b.rs");
        assert_eq!(go("b.rs.html#34-40"), Some(Found::At(b.clone(), 34)));
        let item = "../../../krate/vt/struct.Pty.html";
        assert_eq!(go(item), Some(Found::At(b.clone(), 1)));
        let method = format!("{item}#method.spawn");
        assert_eq!(go(&method), Some(Found::At(b, 55)));
        // What cannot be found leaves the question to ctags, not a wrong answer.
        assert_eq!(go("../../../krate/vt/struct.Gone.html"), None);
    }

    #[test]
    fn standard_library_links_find_their_source() {
        let wait = "https://doc.rust-lang.org/1.96.0/std/process/struct.Child.html#method.wait";
        // Without the source, it says where the name is rather than guessing.
        assert_eq!(
            std_item(wait, || None),
            Found::Outside("Child::wait is outside this project".into())
        );
        let primitive = "https://doc.rust-lang.org/nightly/std/primitive.u32.html";
        let library = tempfile::tempdir().unwrap();
        assert!(matches!(
            std_item(primitive, || Some(library.path().to_owned())),
            Found::Outside(_)
        ));
        if Ctags::locate().is_none() {
            eprintln!("skipping: this build has no ctags");
            return;
        }
        // `String` is documented under `std` but lives in `alloc`.
        let source = library.path().join("alloc/src");
        fs::create_dir_all(&source).unwrap();
        let text = "pub struct String;\nimpl String {\n    pub fn new() {}\n}\n";
        fs::write(source.join("string.rs"), text).unwrap();
        let new = "https://doc.rust-lang.org/nightly/std/string/struct.String.html#method.new";
        let library = library.path().to_owned();
        assert_eq!(
            std_item(new, || Some(library.clone())),
            Found::At(library.join("alloc/src/string.rs"), 3)
        );
    }

    #[test]
    fn the_project_is_the_workspace_a_file_belongs_to() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let member = root.join("crates/one");
        fs::create_dir_all(member.join("src")).unwrap();
        fs::write(member.join("Cargo.toml"), "[package]\n").unwrap();
        let file = member.join("src/lib.rs");
        assert_eq!(project_of(&file), Some(member.clone()));
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        assert_eq!(project_of(&file), Some(root.to_owned()));
        // Outside any project, or outside a crate's source, there is no page.
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(project_of(&elsewhere.path().join("a.rs")), None);
        assert_eq!(page_for(&member.join("build.rs"), &member), None);
    }
}
