//! The folder tree of an archive, built from its flat list of entries.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use rustic_core::jiff::Timestamp;

use crate::backend::{Entry, EntryKind};

/// What the central directory says about one entry, as far as the tree needs it.
pub struct Raw {
    /// Position in the archive, for opening it.
    pub index: usize,
    /// The decoded name, '/'-separated by the spec, but Windows tools also write '\\'.
    pub name: String,
    pub is_dir: bool,
    /// A link, device or the like: listed, never extracted.
    pub special: bool,
    pub size: u64,
    pub mtime: Option<Timestamp>,
    /// Why the entry can't be extracted, if so.
    pub problem: Option<&'static str>,
}

/// A file of the archive that can be extracted or at least was meant to.
pub struct FileData {
    pub index: usize,
    pub size: u64,
    pub problem: Option<&'static str>,
}

/// Everything listings and extractions need, without touching the archive again.
pub struct Index {
    /// The entries of each folder; the key is the path inside the archive, empty for the root.
    pub folders: HashMap<PathBuf, Vec<Entry>>,
    /// Extractable files by their path inside the archive.
    pub files: HashMap<PathBuf, FileData>,
}

impl Index {
    /// Builds the tree of `raws`; `archive_path` is the path of the archive file in the snapshot.
    ///
    /// Folders come from explicit entries and from the paths of all others, an explicit
    /// entry supplies the time. Entries with names that could leave the folder they are
    /// restored or opened into are left out. A name taken twice keeps its first entry, and a
    /// folder wins over a file of the same name.
    pub fn build(archive_path: &Path, raws: impl IntoIterator<Item = Raw>) -> Self {
        let mut builder = Builder {
            archive_path,
            folders: HashMap::from([(PathBuf::new(), BTreeMap::new())]),
            files: HashMap::new(),
        };
        for raw in raws {
            let Some((parts, trailing_separator)) = components(&raw.name) else {
                continue;
            };
            let (last, parents) = parts.split_last().expect("components are not empty");
            let mut folder = PathBuf::new();
            for part in parents {
                folder = builder.folder(&folder, part, None);
            }
            if raw.is_dir || trailing_separator {
                builder.folder(&folder, last, Some(raw.mtime));
            } else {
                builder.file(&folder, last, &raw);
            }
        }
        Self {
            folders: builder
                .folders
                .into_iter()
                .map(|(path, children)| (path, children.into_values().collect()))
                .collect(),
            files: builder.files,
        }
    }
}

struct Builder<'a> {
    archive_path: &'a Path,
    folders: HashMap<PathBuf, BTreeMap<String, Entry>>,
    files: HashMap<PathBuf, FileData>,
}

impl Builder<'_> {
    /// Makes sure folder `name` exists in `parent` and returns its path. `explicit` is
    /// the time of an entry of its own.
    fn folder(
        &mut self,
        parent: &Path,
        name: &str,
        explicit: Option<Option<Timestamp>>,
    ) -> PathBuf {
        let path = parent.join(name);
        let children = self.folders.entry(parent.to_path_buf()).or_default();
        match children.get_mut(name) {
            Some(entry) if entry.kind == EntryKind::Dir => {
                // The first readable time wins, a folder seen implicitly has none yet.
                if let Some(Some(mtime)) = explicit {
                    entry.mtime.get_or_insert(mtime);
                }
            }
            // A file of this name came first: the folder wins.
            existing => {
                let entry = Entry {
                    name: name.to_owned(),
                    path: self.archive_path.join(&path),
                    kind: EntryKind::Dir,
                    size: 0,
                    mtime: explicit.flatten(),
                    inside_image: true,
                };
                if existing.is_some() {
                    self.files.remove(&path);
                }
                children.insert(name.to_owned(), entry);
            }
        }
        self.folders.entry(path.clone()).or_default();
        path
    }

    fn file(&mut self, parent: &Path, name: &str, raw: &Raw) {
        let children = self.folders.entry(parent.to_path_buf()).or_default();
        if children.contains_key(name) {
            return;
        }
        let path = parent.join(name);
        let kind = match (raw.special, raw.problem) {
            (true, _) => EntryKind::Special,
            (false, Some(reason)) => EntryKind::Unavailable {
                reason: reason.to_owned(),
            },
            (false, None) => EntryKind::File,
        };
        children.insert(
            name.to_owned(),
            Entry {
                name: name.to_owned(),
                path: self.archive_path.join(&path),
                kind,
                size: raw.size,
                mtime: raw.mtime,
                inside_image: true,
            },
        );
        if !raw.special {
            self.files.insert(
                path,
                FileData {
                    index: raw.index,
                    size: raw.size,
                    problem: raw.problem,
                },
            );
        }
    }
}

/// The components of an entry name, and whether it ends with a separator; `None` for names
/// that must not show up: absolute ones, with `..` or NUL, or without any component.
///
/// Both '/' and '\\' separate components: Windows tools write the latter, and a name
/// with a real backslash in it is rare enough to be shown as a folder. "." and empty
/// components (`a//b`) are skipped. No component contains a separator afterwards,
/// so a component is always a safe file name.
fn components(name: &str) -> Option<(Vec<&str>, bool)> {
    let is_separator = |c: char| c == '/' || c == '\\';
    if name.contains('\0') || name.starts_with(is_separator) {
        return None;
    }
    let mut parts = Vec::new();
    for part in name.split(is_separator) {
        match part {
            "" | "." => {}
            ".." => return None,
            part => parts.push(part),
        }
    }
    // A Windows drive: `C:\Windows`.
    if parts.first().is_some_and(
        |first| matches!(first.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic()),
    ) {
        return None;
    }
    (!parts.is_empty()).then(|| (parts, name.ends_with(is_separator)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(index: usize, name: &str) -> Raw {
        Raw {
            index,
            name: name.to_owned(),
            is_dir: name.ends_with('/'),
            special: false,
            size: index as u64,
            mtime: None,
            problem: None,
        }
    }

    fn names(index: &Index, folder: &str) -> Vec<(String, EntryKind)> {
        index.folders[Path::new(folder)]
            .iter()
            .map(|e| (e.name.clone(), e.kind.clone()))
            .collect()
    }

    #[test]
    fn splits_names_into_components() {
        assert_eq!(components("a/b.txt"), Some((vec!["a", "b.txt"], false)));
        assert_eq!(components("a/b/"), Some((vec!["a", "b"], true)));
        assert_eq!(components("a//./b"), Some((vec!["a", "b"], false)));
        assert_eq!(
            components("win\\dir\\f"),
            Some((vec!["win", "dir", "f"], false))
        );
        assert_eq!(components("..x/y..z"), Some((vec!["..x", "y..z"], false)));
        assert_eq!(components("ä/Ü.txt"), Some((vec!["ä", "Ü.txt"], false)));
    }

    #[test]
    fn drops_names_that_could_escape() {
        for name in [
            "", ".", "/", "./", "/abs", "\\abs", "../x", "a/../b", "a/b/..", "a\\..\\b", "x\0y",
            "C:\\x", "c:/x", "Z:",
        ] {
            assert_eq!(components(name), None, "{name:?}");
        }
        // Only a leading drive letter counts.
        assert!(components("a/C:/x").is_some());
        assert!(components("C/x").is_some());
    }

    #[test]
    fn builds_implicit_and_explicit_folders() {
        let when = Timestamp::from_second(7).unwrap();
        let mut docs = raw(0, "docs/");
        docs.mtime = Some(when);
        let index = Index::build(
            Path::new("/data/a.zip"),
            [
                raw(1, "docs/sub/deep.txt"),
                docs,
                raw(2, "top.txt"),
                raw(3, "empty/"),
                raw(4, "../evil.txt"),
            ],
        );
        assert_eq!(
            names(&index, ""),
            [
                ("docs".into(), EntryKind::Dir),
                ("empty".into(), EntryKind::Dir),
                ("top.txt".into(), EntryKind::File),
            ]
        );
        assert_eq!(index.folders[Path::new("")][0].mtime, Some(when));
        assert_eq!(
            index.folders[Path::new("")][0].path,
            Path::new("/data/a.zip/docs")
        );
        assert_eq!(names(&index, "docs"), [("sub".into(), EntryKind::Dir)]);
        assert_eq!(
            names(&index, "docs/sub"),
            [("deep.txt".into(), EntryKind::File)]
        );
        assert!(index.folders[Path::new("empty")].is_empty());
        assert_eq!(index.files[Path::new("docs/sub/deep.txt")].index, 1);
        assert!(!index.files.contains_key(Path::new("docs")));
    }

    #[test]
    fn the_folder_wins_over_a_file_and_the_first_duplicate_wins() {
        let when = Timestamp::from_second(7).unwrap();
        let mut c = raw(2, "c/");
        c.mtime = Some(when);
        let mut later = raw(9, "c/");
        later.mtime = Some(Timestamp::from_second(8).unwrap());
        let index = Index::build(
            Path::new("/z.zip"),
            [
                raw(0, "a"),
                raw(1, "a/b"),
                c,
                raw(3, "c"),
                raw(4, "d"),
                raw(5, "d"),
                raw(6, "e\\f"),
                raw(7, "e/f"),
                raw(8, "c/"),
                later,
            ],
        );
        assert_eq!(
            names(&index, ""),
            [
                ("a".into(), EntryKind::Dir),
                ("c".into(), EntryKind::Dir),
                ("d".into(), EntryKind::File),
                ("e".into(), EntryKind::Dir),
            ]
        );
        assert!(!index.files.contains_key(Path::new("a")));
        // The second "c/" has no readable time, the third a later one: neither
        // replaces the first.
        assert_eq!(index.folders[Path::new("")][1].mtime, Some(when));
        assert_eq!(index.files[Path::new("a/b")].index, 1);
        assert_eq!(index.files[Path::new("d")].index, 4);
        assert_eq!(index.files[Path::new("e/f")].index, 6);
    }

    #[test]
    fn marks_links_and_unusable_entries() {
        let mut link = raw(0, "link");
        link.special = true;
        let mut secret = raw(1, "secret");
        secret.problem = Some("Encrypted");
        let index = Index::build(Path::new("/z.zip"), [link, secret]);
        assert_eq!(
            names(&index, ""),
            [
                ("link".into(), EntryKind::Special),
                (
                    "secret".into(),
                    EntryKind::Unavailable {
                        reason: "Encrypted".into()
                    }
                ),
            ]
        );
        assert!(!index.files.contains_key(Path::new("link")));
        assert_eq!(index.files[Path::new("secret")].problem, Some("Encrypted"));
    }

    #[test]
    fn an_empty_archive_has_an_empty_root() {
        let index = Index::build(Path::new("/z.zip"), []);
        assert!(index.folders[Path::new("")].is_empty());
    }
}
