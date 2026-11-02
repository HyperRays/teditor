//! Workspace symbol search: a tag index built in the background when first needed.

use crate::tags::{Ctags, Tag, TagFile, languages_connect};
use std::{
    io,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, Instant},
};

/// How old an index may get before it is rebuilt, to notice changes made outside the editor.
const MAX_AGE: Duration = Duration::from_secs(60);

/// A name as written in code, with what it was reached through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Reference {
    pub name: String,
    qualifier: Option<Qualifier>,
    /// Where the name starts in its document.
    start: usize,
}

/// What comes before a name: the `a` of `a::name` or `a.name`, with `None` when it is an
/// expression such as `f().name`. `path` tells `::` from `.` and `->`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Qualifier {
    name: Option<String>,
    path: bool,
}

impl Reference {
    /// Read a name and the text just before it on its line.
    pub fn new(name: String, before: &str) -> Self {
        let separated = |separator| before.strip_suffix(separator);
        let qualifier = match (separated("::"), separated("->"), separated(".")) {
            (Some(rest), ..) => Some((rest, true)),
            (_, Some(rest), _) => Some((rest, false)),
            // Two dots make a range, as in `0..name`, not a member.
            (.., Some(rest)) if !rest.ends_with('.') => Some((rest, false)),
            _ => None,
        };
        let qualifier = qualifier.map(|(rest, path)| {
            let start = rest
                .rfind(|c: char| !c.is_alphanumeric() && c != '_')
                .map_or(0, |index| {
                    index + rest[index..].chars().next().map_or(1, char::len_utf8)
                });
            let name = &rest[start..];
            Qualifier {
                name: (!name.is_empty()).then(|| name.to_owned()),
                path,
            }
        });
        Self {
            name,
            qualifier,
            start: 0,
        }
    }
    /// Note where the name starts in its document.
    pub fn at(mut self, start: usize) -> Self {
        self.start = start;
        self
    }
    /// Where the name starts in its document.
    pub fn start(&self) -> usize {
        self.start
    }
}

/// A definition being looked for, and where the search started from.
#[derive(Clone, Debug)]
pub(super) struct DefinitionQuery {
    pub reference: Reference,
    pub file: Option<PathBuf>,
    pub language: Option<String>,
}

impl DefinitionQuery {
    /// Narrow the definitions sharing a name to those this reference can mean, using what ctags
    /// records about each. Ctags lists definitions rather than uses, so this reads the scope a
    /// reference needs: `File` alone cannot be the variant `Choice::File` of another file.
    pub fn narrow(&self, mut tags: Vec<Tag>) -> Vec<Tag> {
        let in_file = |tag: &Tag| self.file.as_deref() == Some(tag.path.as_path());
        tags.retain(|tag| {
            let same_language = match (&self.language, &tag.language) {
                (Some(ours), Some(theirs)) => languages_connect(ours, theirs),
                (Some(_), None) => false,
                (None, _) => true,
            };
            same_language && (!tag.file_only || in_file(tag))
        });
        let local_first = |tags: Vec<Tag>| {
            if tags.iter().any(in_file) {
                tags.into_iter().filter(in_file).collect()
            } else {
                tags
            }
        };
        let members =
            |tags: Vec<Tag>| -> Vec<Tag> { tags.into_iter().filter(Tag::is_member).collect() };
        let mut found: Vec<Tag> = match &self.reference.qualifier {
            None => local_first(
                tags.into_iter()
                    .filter(|tag| !tag.is_member() || in_file(tag))
                    .collect(),
            ),
            Some(Qualifier { name: None, .. }) => members(tags),
            Some(Qualifier {
                name: Some(name),
                path,
            }) => match name.as_str() {
                "self" | "this" | "Self" | "cls" if tags.iter().any(Tag::is_member) => {
                    local_first(members(tags))
                }
                "crate" | "super" | "self" => {
                    tags.into_iter().filter(|tag| !tag.is_member()).collect()
                }
                name => {
                    let scoped = |tag: &Tag| tag.scope_name() == Some(name);
                    let in_module = |tag: &Tag| !tag.is_member() && file_is_module(&tag.path, name);
                    if tags.iter().any(scoped) {
                        tags.into_iter().filter(scoped).collect()
                    } else if tags.iter().any(in_module) {
                        tags.into_iter().filter(in_module).collect()
                    } else if *path {
                        // A path into something not indexed, such as `std::fs::File`.
                        Vec::new()
                    } else {
                        // A variable, whose type ctags cannot know.
                        members(tags)
                    }
                }
            },
        };
        // `impl Shape` says more about `Shape` but does not define it, unless nothing else does.
        let implementation = |tag: &Tag| tag.kind == "implementation";
        if !found.iter().all(implementation) {
            found.retain(|tag| !implementation(tag));
        }
        found
    }
}

/// Whether a file is the module called `name`: `name.rs`, or a folder's `name/mod.rs`.
fn file_is_module(path: &Path, name: &str) -> bool {
    let stem = path.file_stem().and_then(|stem| stem.to_str());
    let folder = || path.parent()?.file_name()?.to_str();
    stem == Some(name) || (stem == Some("mod") && folder() == Some(name))
}

/// The index for one workspace, and any rebuild in progress. The last index stays usable
/// during a rebuild, so results never disappear while typing.
#[derive(Default)]
pub(super) struct SymbolSearch {
    index: Option<TagFile>,
    built: Option<Instant>,
    /// Counts finished builds, so results are filtered again when the index changes.
    generation: usize,
    job: Option<Receiver<io::Result<TagFile>>>,
    stale: bool,
    /// The build in progress is for files known to have changed, not just the index's age.
    catching_up: bool,
    problem: Option<String>,
}

impl SymbolSearch {
    pub fn index(&self) -> Option<&TagFile> {
        self.index.as_ref()
    }

    pub fn generation(&self) -> usize {
        self.generation
    }

    pub fn indexing(&self) -> bool {
        self.job.is_some()
    }

    /// Why there is no index, if there is a reason.
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// Whether the index reflects files as saved, so its line numbers can be relied on.
    pub fn ready(&self) -> bool {
        self.index.is_some() && !self.stale && !(self.catching_up && self.job.is_some())
    }

    /// Note that files changed, so the next search rebuilds the index.
    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    /// Start a build if the index is missing or out of date.
    pub fn request(&mut self, root: &Path) {
        let current = self.index.is_some()
            && !self.stale
            && self.built.is_some_and(|built| built.elapsed() < MAX_AGE);
        if current || self.job.is_some() || (self.index.is_none() && self.problem.is_some()) {
            return;
        }
        let Some(ctags) = Ctags::locate() else {
            self.problem = Some("Symbol search needs ctags".into());
            return;
        };
        let (send, receive) = mpsc::channel();
        let root = root.to_owned();
        std::thread::spawn(move || {
            let _ = send.send(ctags.index(&root));
        });
        self.catching_up = self.stale;
        self.stale = false;
        self.job = Some(receive);
    }

    /// Take a finished build.
    pub fn poll(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        let result = match job.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(io::Error::other("Indexing stopped")),
        };
        self.job = None;
        match result {
            Ok(index) => {
                self.index = Some(index);
                self.built = Some(Instant::now());
                self.generation += 1;
                self.problem = None;
            }
            Err(error) => self.problem = Some(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(path: &str, kind: &str, scope: Option<(&str, &str)>) -> Tag {
        Tag {
            name: "x".into(),
            path: path.into(),
            line: 1,
            kind: kind.into(),
            scope: scope.map(|(_, name)| name.into()),
            scope_kind: scope.map(|(kind, _)| kind.into()),
            file_only: false,
            language: Some("Rust".into()),
        }
    }
    /// Where each definition `before` the name `x` in `/a.rs` resolves, by path and scope.
    fn narrow<'a>(before: &str, tags: &'a [Tag]) -> Vec<(&'a str, Option<&'a str>)> {
        let query = DefinitionQuery {
            reference: Reference::new("x".into(), before),
            file: Some("/a.rs".into()),
            language: Some("Rust".into()),
        };
        let found = query.narrow(tags.to_vec());
        let same = |a: &Tag, b: &Tag| a.path == b.path && a.scope == b.scope && a.kind == b.kind;
        found
            .iter()
            .map(|f| {
                let original = tags.iter().find(|t| same(t, f)).unwrap();
                (original.path.to_str().unwrap(), original.scope.as_deref())
            })
            .collect()
    }

    #[test]
    fn qualifiers_are_read_from_the_text_before_a_name() {
        let qualifier = |before: &str| Reference::new("x".into(), before).qualifier;
        let named = |name: &str, path| {
            Some(Qualifier {
                name: Some(name.into()),
                path,
            })
        };
        assert_eq!(qualifier("let y = "), None);
        assert_eq!(qualifier("    Kind::"), named("Kind", true));
        assert_eq!(qualifier("std::fs::"), named("fs", true));
        assert_eq!(qualifier("self."), named("self", false));
        assert_eq!(qualifier("p->"), named("p", false));
        assert_eq!(qualifier("(Größe."), named("Größe", false));
        let expression = Some(Qualifier {
            name: None,
            path: false,
        });
        assert_eq!(qualifier("f()."), expression);
        assert_eq!(qualifier("    ."), expression);
        assert_eq!(qualifier("0.."), None, "a range");
    }

    #[test]
    fn a_bare_name_cannot_mean_a_member_of_another_file() {
        let variant = tag("/b.rs", "enumerator", Some(("enum", "Choice")));
        assert_eq!(narrow("field: ", std::slice::from_ref(&variant)), []);
        // A top-level or module-level definition can be imported from anywhere.
        let free = tag("/b.rs", "struct", None);
        let nested = tag("/c.rs", "function", Some(("module", "inner")));
        assert_eq!(
            narrow("", &[variant, free, nested]),
            [("/b.rs", None), ("/c.rs", Some("inner"))]
        );
        // Its own file's members are in reach, and win over other files.
        let local = tag("/a.rs", "enumerator", Some(("enum", "Mine")));
        let free = tag("/b.rs", "struct", None);
        assert_eq!(narrow("", &[local, free]), [("/a.rs", Some("Mine"))]);
    }

    #[test]
    fn a_qualified_name_goes_to_what_it_names() {
        let tags = [
            tag("/b.rs", "enumerator", Some(("enum", "Choice"))),
            tag("/c.rs", "method", Some(("implementation", "Shape"))),
            tag("/shapes.rs", "function", None),
        ];
        assert_eq!(narrow("Choice::", &tags), [("/b.rs", Some("Choice"))]);
        assert_eq!(narrow("Shape::", &tags), [("/c.rs", Some("Shape"))]);
        assert_eq!(narrow("shapes::", &tags), [("/shapes.rs", None)]);
        assert_eq!(narrow("crate::", &tags), [("/shapes.rs", None)]);
        // Paths into what is not indexed lead nowhere, rather than to a namesake.
        assert_eq!(narrow("String::", &tags), []);
        assert_eq!(narrow("std::fs::", &tags), []);
        // The type of a variable or expression is unknown, but it does have a member.
        let members = [("/b.rs", Some("Choice")), ("/c.rs", Some("Shape"))];
        assert_eq!(narrow("shape.", &tags), members);
        assert_eq!(narrow("f().", &tags), members);
        // Scopes may be written with their own qualifiers.
        let tags = [tag("/d.cpp", "member", Some(("class", "ns::K")))];
        assert_eq!(narrow("K::", &tags), [("/d.cpp", Some("ns::K"))]);
        let module = [tag("/shapes/mod.rs", "function", None)];
        assert_eq!(narrow("shapes::", &module), [("/shapes/mod.rs", None)]);
    }

    #[test]
    fn self_prefers_the_members_of_its_own_file() {
        let here = tag("/a.rs", "method", Some(("implementation", "Mine")));
        let there = tag("/b.rs", "method", Some(("implementation", "Other")));
        let free = tag("/c.rs", "function", None);
        let tags = [here, there.clone(), free];
        assert_eq!(narrow("self.", &tags), [("/a.rs", Some("Mine"))]);
        assert_eq!(narrow("Self::", &tags), [("/a.rs", Some("Mine"))]);
        // A provided trait method can live elsewhere.
        assert_eq!(narrow("self.", &[there]), [("/b.rs", Some("Other"))]);
    }

    #[test]
    fn what_ctags_marks_is_respected() {
        // Visible only in its file.
        let mut hidden = tag("/b.cpp", "function", None);
        hidden.file_only = true;
        assert_eq!(narrow("", &[hidden]), []);
        // Another language is a coincidence.
        let mut python = tag("/b.py", "function", None);
        python.language = Some("Python".into());
        assert_eq!(narrow("", &[python]), []);
        // `impl Shape` yields to `struct Shape`, wherever each is, and stands in without one.
        let implementation = tag("/b.rs", "implementation", None);
        let definition = tag("/c.rs", "struct", None);
        assert_eq!(
            narrow("", &[implementation.clone(), definition]),
            [("/c.rs", None)]
        );
        assert_eq!(narrow("", &[implementation]), [("/b.rs", None)]);
    }
}
