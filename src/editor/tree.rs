//! The file tree pane and the quick-open file index.

use crate::{
    renderer::CellRect,
    terminal::{
        InputEvent, Style,
        input::{Key, MouseButton, NamedKey, PointerKind},
    },
    ui::{Canvas, controls::Scroll},
};
use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub directory: bool,
    pub prefix: String,
    pub label: String,
}
pub struct FileTree {
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub expanded: HashSet<PathBuf>,
    pub show_hidden: bool,
    pub error: Option<String>,
    scroll: Scroll,
    reveal_selection: bool,
}
impl FileTree {
    pub fn new(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::other("Workspace must be a directory"));
        }
        let mut tree = Self {
            root,
            entries: Vec::new(),
            selected: 0,
            expanded: HashSet::new(),
            show_hidden: false,
            error: None,
            scroll: Scroll::default(),
            reveal_selection: true,
        };
        tree.refresh();
        Ok(tree)
    }
    /// The folders whose contents are shown.
    pub fn shown_folders(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.root).chain(&self.expanded)
    }
    pub fn refresh(&mut self) {
        let selected = self.entries.get(self.selected).map(|e| e.path.clone());
        self.entries.clear();
        self.error = None;
        self.walk(self.root.clone(), String::new(), 0);
        self.selected = selected
            .and_then(|p| self.entries.iter().position(|e| e.path == p))
            .unwrap_or(self.selected.min(self.entries.len().saturating_sub(1)));
    }
    fn walk(&mut self, path: PathBuf, prefix: String, depth: usize) {
        if depth > 64 || self.entries.len() >= 20_000 {
            self.error = Some("Tree limit reached; collapse a folder".into());
            return;
        }
        let entries = match children(&path, self.show_hidden) {
            Ok(entries) => entries,
            Err(error) => {
                self.error = Some(format!("{}: {error}", path.display()));
                return;
            }
        };
        let count = entries.len();
        for (index, (path, directory)) in entries.into_iter().enumerate() {
            if self.entries.len() >= 20_000 {
                break;
            }
            let last = index + 1 == count;
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let label = format!("{name}{}", if directory { "/" } else { "" });
            self.entries.push(Entry {
                path: path.clone(),
                directory,
                prefix: format!("{prefix}{}", if last { "└─ " } else { "├─ " }),
                label,
            });
            if directory && self.expanded.contains(&path) {
                self.walk(
                    path,
                    format!("{prefix}{}", if last { "   " } else { "│  " }),
                    depth + 1,
                );
            }
        }
    }
    pub fn reveal(&mut self, path: &Path) {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        let path = canonical.as_path();
        if !path.starts_with(&self.root) {
            return;
        }
        let mut parent = path.parent();
        while let Some(p) = parent.filter(|p| p.starts_with(&self.root) && *p != self.root) {
            self.expanded.insert(p.into());
            parent = p.parent();
        }
        self.refresh();
        if let Some(index) = self.entries.iter().position(|e| e.path == path) {
            self.selected = index;
            self.reveal_selection = true;
        }
    }
    /// Select a folder inside the workspace and show its contents.
    pub fn reveal_folder(&mut self, path: &Path) {
        self.expanded.insert(path.to_owned());
        self.reveal(path);
    }
    fn toggle(&mut self) -> Option<PathBuf> {
        let entry = self.entries.get(self.selected)?;
        if !entry.directory {
            return Some(entry.path.clone());
        }
        if !self.expanded.remove(&entry.path) {
            self.expanded.insert(entry.path.clone());
        }
        self.refresh();
        None
    }
    pub fn draw(&mut self, canvas: &mut Canvas<'_>, active: Option<&Path>) -> Option<PathBuf> {
        let [width, height] = canvas.size();
        self.scroll.inset = 1;
        let mut open = None;
        let mut reveal = std::mem::take(&mut self.reveal_selection);
        self.scroll.maximum = self.entries.len().saturating_sub(height as usize);
        for event in std::mem::take(&mut canvas.events) {
            if self.scroll.event(&event, canvas, self.entries.len()) {
                continue;
            }
            match event {
                InputEvent::Pointer(p)
                    if matches!(p.kind, PointerKind::Down(MouseButton::Left)) =>
                {
                    if let Some(position) = p.position {
                        let point = canvas.local(position.grid);
                        let index = self.scroll.offset + point[1].max(0.0) as usize;
                        if index < self.entries.len() {
                            self.selected = index;
                            open = self.toggle();
                            reveal = true;
                        }
                    }
                }
                InputEvent::Key(key) => {
                    match key.logical_key {
                        Key::Named(NamedKey::ArrowUp) => {
                            self.selected = self.selected.saturating_sub(1)
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            self.selected =
                                (self.selected + 1).min(self.entries.len().saturating_sub(1))
                        }
                        Key::Named(NamedKey::Home) => self.selected = 0,
                        Key::Named(NamedKey::End) => {
                            self.selected = self.entries.len().saturating_sub(1)
                        }
                        Key::Named(NamedKey::PageUp) => {
                            self.selected = self.selected.saturating_sub(height as usize)
                        }
                        Key::Named(NamedKey::PageDown) => {
                            self.selected = (self.selected + height as usize)
                                .min(self.entries.len().saturating_sub(1))
                        }
                        Key::Named(NamedKey::Enter | NamedKey::Space) => open = self.toggle(),
                        Key::Named(NamedKey::ArrowRight) => {
                            if let Some(entry) = self.entries.get(self.selected) {
                                if entry.directory && !self.expanded.contains(&entry.path) {
                                    self.expanded.insert(entry.path.clone());
                                    self.refresh();
                                } else if entry.directory {
                                    self.selected = (self.selected + 1)
                                        .min(self.entries.len().saturating_sub(1));
                                } else {
                                    open = Some(entry.path.clone());
                                }
                            }
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            if let Some(entry) = self.entries.get(self.selected) {
                                if self.expanded.remove(&entry.path) {
                                    self.refresh();
                                } else if let Some(parent) = entry.path.parent()
                                    && let Some(index) =
                                        self.entries.iter().position(|e| e.path == parent)
                                {
                                    self.selected = index;
                                }
                            }
                        }
                        _ => continue,
                    }
                    reveal = true;
                }
                _ => {}
            }
        }
        self.scroll.maximum = self.entries.len().saturating_sub(height as usize);
        if reveal {
            self.scroll.reveal(self.selected, height as usize);
        }
        self.scroll.offset = self.scroll.offset.min(self.scroll.maximum);
        let theme = canvas.theme;
        canvas.fill(
            CellRect::new(0, 0, width, height),
            Style::new(theme.text, theme.panel),
        );
        if self.entries.is_empty() {
            canvas.text(1, 1, "Empty folder", Style::new(theme.muted, theme.panel));
        }
        // The last column belongs to the divider, and the one before it to any scrollbar.
        let row_width = width.saturating_sub(if self.scroll.maximum > 0 { 2 } else { 1 });
        let hovered = canvas
            .pointer()
            .filter(|point| point[0] < f64::from(row_width))
            .map(|point| self.scroll.offset + point[1].max(0.0) as usize)
            .filter(|index| *index < self.entries.len());
        if hovered.is_some() {
            canvas.set_cursor(crate::ui::Cursor::Pointer);
        }
        for (row, (index, entry)) in self
            .entries
            .iter()
            .enumerate()
            .skip(self.scroll.offset)
            .take(height as usize)
            .enumerate()
        {
            let selected = index == self.selected;
            let background = if hovered == Some(index) {
                theme.hover
            } else if selected {
                theme.control
            } else {
                theme.panel
            };
            canvas.fill(
                CellRect::new(0, row as u32, row_width, 1),
                Style::new(theme.text, background),
            );
            let text = format!(" {}{}", entry.prefix, entry.label);
            let foreground = if Some(entry.path.as_path()) == active || entry.directory {
                theme.accent
            } else {
                theme.text
            };
            let text: String = text.chars().take(row_width as usize).collect();
            canvas.text(0, row as i32, &text, Style::new(foreground, background));
            if selected && canvas.response.focused {
                canvas.graphic(
                    CellRect::new(0, row as u32, 1, 1),
                    crate::renderer::Graphic::Selection(theme.accent),
                );
            }
        }
        self.scroll.draw(canvas, self.entries.len());
        open
    }
}

pub(super) fn children(path: &Path, hidden: bool) -> io::Result<Vec<(PathBuf, bool)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if !hidden && entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        // Directory symlinks are leaves, so expanding a tree cannot loop.
        let directory = entry.file_type()?.is_dir();
        entries.push((entry.path(), directory));
    }
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.file_name().cmp(&b.0.file_name()))
    });
    Ok(entries)
}

/// Bounded background quick-open index. Generated directories and symlinked
/// directories are excluded; they remain accessible through the tree or a path.
pub fn index_files(root: &Path, hidden: bool) -> Vec<PathBuf> {
    let mut stack = vec![(root.to_owned(), 0)];
    let mut files = Vec::new();
    let mut visited = 0;
    while let Some((directory, depth)) = stack.pop() {
        visited += 1;
        if visited > 10_000 || files.len() >= 20_000 {
            break;
        }
        let Ok(entries) = children(&directory, hidden) else {
            continue;
        };
        for (path, directory) in entries {
            if directory {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if depth < 32
                    && !matches!(
                        name.as_ref(),
                        ".git" | "target" | "node_modules" | ".venv" | "build"
                    )
                {
                    stack.push((path, depth + 1));
                }
            } else {
                files.push(path);
                if files.len() >= 20_000 {
                    break;
                }
            }
        }
    }
    files.sort();
    files
}
