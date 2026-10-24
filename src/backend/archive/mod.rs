//! ZIP archives inside snapshots, browsed without downloading them.
//!
//! A path inside an archive continues the path of the archive file in the snapshot:
//! `/backup.zip/docs/readme.txt`. Opening an archive reads only its central directory,
//! the list of entries at the end of the file; opening an entry reads only its data.

mod index;
mod window;

use std::{
    ffi::OsStr,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, atomic::Ordering},
};

use rustic_core::{
    jiff::{Timestamp, civil, tz::TimeZone},
    repofile::Node,
};
use zip::{CompressionMethod, DateTime, ZipArchive, read::ZipFileEntry, result::ZipError};

use self::{
    index::{Index, Raw},
    window::{BLOCK, Window},
};
use super::{Entry, Error, Result, Snapshot, image::SnapshotReader, session::IndexedRepo};

/// File name extensions of ZIP archives that are browsed like folders. Office and package
/// formats are ZIP files too, but are opened by their app.
const EXTENSIONS: [&str; 1] = ["zip"];

/// A ZIP file without entries is just its 22 byte end record; anything smaller isn't one.
const MIN_SIZE: u64 = 22;

/// The end record starts with this signature.
const END_RECORD: &[u8; 4] = b"PK\x05\x06";

/// The end record may be followed by a comment of at most 65535 bytes, so it begins
/// somewhere in this many last bytes of every ZIP file.
const MAX_TAIL: u64 = MIN_SIZE + 65_535;

/// Archives kept open per session; each holds its parsed list of entries.
const OPEN_ARCHIVES: usize = 4;

/// Most bytes opening may read. The list of entries is read about twice, so this allows
/// a list of about 60 MiB, which is plenty for [`MAX_ENTRIES`].
const MAX_DIRECTORY_READ: u64 = 128 << 20;

/// More entries than this are refused, their list would take too much memory.
const MAX_ENTRIES: usize = 500_000;

/// What Deflate can compress at most (1032:1), so an entry claiming more is damaged.
const MAX_DEFLATE_RATIO: u64 = 1032;

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// Whether the file at `path` is browsed as a ZIP archive, judged by its name.
pub fn is_zip_archive(path: &Path) -> bool {
    path.file_name().is_some_and(is_zip_name)
}

/// Whether a file with this name is browsed as a ZIP archive.
pub fn is_zip_name(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// Whether a snapshot file with this name and size is browsed as a ZIP archive.
pub(crate) fn is_zip_file(name: &OsStr, size: u64) -> bool {
    size >= MIN_SIZE && is_zip_name(name)
}

/// The archives opened in a session.
#[derive(Default)]
pub(crate) struct ZipCache {
    /// Most recently used last.
    archives: Mutex<Vec<Arc<OpenZip>>>,
}

impl std::fmt::Debug for ZipCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZipCache").finish_non_exhaustive()
    }
}

/// Where to find an archive: the snapshot tree, the archive node and its path.
pub(crate) struct ZipRef<'a> {
    pub repo: &'a Arc<IndexedRepo>,
    pub tree: String,
    pub node: &'a Node,
    /// The path of the archive file in the snapshot.
    pub path: &'a Path,
}

impl<'a> ZipRef<'a> {
    pub fn new(
        repo: &'a Arc<IndexedRepo>,
        snapshot: &Snapshot,
        node: &'a Node,
        path: &'a Path,
    ) -> Self {
        Self {
            repo,
            tree: snapshot.tree.to_string(),
            node,
            path,
        }
    }
}

struct OpenZip {
    key: (String, PathBuf),
    /// Cloned per extraction; clones share the parsed entries and the read buffer.
    archive: ZipArchive<Window<SnapshotReader>>,
    index: Index,
}

impl ZipCache {
    /// The entries of the folder `inner` in an archive, empty for its top level.
    pub fn list(&self, zip: &ZipRef, inner: &Path) -> Result<Vec<Entry>> {
        let open = self.open(zip)?;
        let shown = zip.path.join(inner);
        let key = folder_key(inner, &shown)?;
        match open.index.folders.get(&key) {
            Some(entries) => Ok(entries.clone()),
            None if open.index.files.contains_key(&key) => Err(Error::NotADirectory(shown)),
            None => Err(Error::NotFound(shown)),
        }
    }

    /// Writes the file at `inner` in an archive to `out`.
    ///
    /// The entry name is never used as a path here, only to find the entry in the index.
    pub fn write_file(&self, zip: &ZipRef, inner: &Path, out: &mut dyn Write) -> Result<()> {
        let open = self.open(zip)?;
        let shown = zip.path.join(inner);
        let key = folder_key(inner, &shown)?;
        let Some(file) = open.index.files.get(&key) else {
            return Err(if open.index.folders.contains_key(&key) {
                Error::NotAFile(shown)
            } else {
                Error::NotFound(shown)
            });
        };
        if let Some(reason) = file.problem {
            return Err(Error::UnsupportedFile {
                path: shown,
                reason: reason.to_owned(),
            });
        }
        let damaged = |detail: &dyn std::fmt::Display| {
            Error::Archive(format!("“{}” is damaged: {detail}", shown.display()))
        };

        let mut archive = open.archive.clone();
        let mut entry = archive.by_index(file.index).map_err(|err| match err {
            ZipError::Io(err) => Error::Io(err),
            ZipError::UnsupportedArchive(reason) => Error::UnsupportedFile {
                path: shown.clone(),
                reason: reason.to_owned(),
            },
            err => damaged(&err),
        })?;
        // One byte more than declared shows an entry that is larger than it says.
        let copied = io::copy(&mut entry.by_ref().take(file.size.saturating_add(1)), out).map_err(
            |err| match err.kind() {
                io::ErrorKind::InvalidData => damaged(&err),
                _ => Error::Io(err),
            },
        )?;
        if copied != file.size {
            return Err(damaged(&format_args!(
                "{} bytes instead of {}",
                copied, file.size
            )));
        }
        Ok(())
    }

    /// The opened archive, reading its list of entries if it isn't open yet.
    fn open(&self, zip: &ZipRef) -> Result<Arc<OpenZip>> {
        let key = (zip.tree.clone(), zip.path.to_path_buf());
        {
            let mut archives = self.archives.lock().unwrap();
            if let Some(index) = archives.iter().position(|a| a.key == key) {
                let open = archives.remove(index);
                archives.push(open.clone());
                return Ok(open);
            }
        }

        // Read without holding the lock; a concurrent caller may open it as well.
        let name = zip.path.file_name().unwrap_or_default().to_string_lossy();
        let reader = SnapshotReader::new(zip.repo.clone(), zip.node)?;
        let (mut window, budget) = Window::new(reader, BLOCK)?.with_budget(MAX_DIRECTORY_READ);
        if !has_end_record(&mut window)? {
            return Err(Error::NotAnArchive(format!(
                "“{name}” is not a valid ZIP archive"
            )));
        }
        let archive = ZipArchive::new(window).map_err(|err| match err {
            ZipError::Io(err) if err.kind() == io::ErrorKind::FileTooLarge => {
                Error::NotAnArchive(format!("The list of entries of “{name}” is too large"))
            }
            ZipError::Io(err) => Error::Io(err),
            ZipError::UnsupportedArchive(reason) => {
                Error::NotAnArchive(format!("“{name}” can't be browsed: {reason}"))
            }
            _ => Error::NotAnArchive(format!("“{name}” is not a valid ZIP archive")),
        })?;
        budget.store(u64::MAX, Ordering::Relaxed);
        if archive.len() > MAX_ENTRIES {
            return Err(Error::NotAnArchive(format!(
                "“{name}” has too many entries to browse"
            )));
        }
        let raws = (0..archive.len()).filter_map(|i| {
            let entry = archive.by_index_data(i).ok()?;
            Some(raw_entry(i, &entry))
        });
        let open = Arc::new(OpenZip {
            index: Index::build(zip.path, raws),
            archive,
            key,
        });

        let mut archives = self.archives.lock().unwrap();
        archives.retain(|a| a.key != open.key);
        archives.push(open.clone());
        if archives.len() > OPEN_ARCHIVES {
            archives.remove(0);
        }
        Ok(open)
    }
}

/// Whether the file ends like a ZIP file, with the end record in its last [`MAX_TAIL`]
/// bytes. A file without it is no ZIP, and skipping the crate for it spares its search
/// backwards through the whole file; the block read here stays cached for the crate.
fn has_end_record<R: Read + Seek>(window: &mut Window<R>) -> io::Result<bool> {
    let len = window.seek(SeekFrom::End(0))?;
    let tail = len.min(MAX_TAIL);
    window.seek(SeekFrom::Start(len - tail))?;
    let mut buf = vec![0; tail as usize];
    window.read_exact(&mut buf)?;
    Ok(buf.windows(END_RECORD.len()).any(|w| w == END_RECORD))
}

/// The path `inner` as key of the index: relative, without `.` and root components.
fn folder_key(inner: &Path, shown: &Path) -> Result<PathBuf> {
    let mut key = PathBuf::new();
    for component in inner.components() {
        match component {
            Component::Normal(name) => key.push(name),
            Component::CurDir | Component::RootDir => {}
            _ => return Err(Error::NotFound(shown.to_path_buf())),
        }
    }
    Ok(key)
}

/// What the tree needs to know about a central directory entry; reads nothing.
fn raw_entry(index: usize, entry: &ZipFileEntry<'_>) -> Raw {
    // Valid UTF-8 is taken as is, anything else is CP437 as the specification says.
    let name = entry
        .name()
        .map(|name| name.into_owned())
        .unwrap_or_else(|_| String::from_utf8_lossy(entry.name_raw()).into_owned());
    let file_type = entry.unix_mode().map(|mode| mode & S_IFMT);
    Raw {
        index,
        name,
        is_dir: entry.is_dir() || file_type == Some(S_IFDIR),
        special: entry.is_symlink()
            || file_type.is_some_and(|t| t != 0 && t != S_IFREG && t != S_IFDIR),
        size: entry.size(),
        mtime: entry.last_modified().and_then(dos_time),
        problem: problem(entry),
    }
}

/// Why `entry` can't be extracted, judged by its central directory record.
fn problem(entry: &ZipFileEntry<'_>) -> Option<&'static str> {
    let (size, packed) = (entry.size(), entry.compressed_size());
    if entry.encrypted() {
        Some("It is encrypted, and passwords aren't supported yet")
    } else if !matches!(
        entry.compression(),
        CompressionMethod::Stored
            | CompressionMethod::Deflated
            | CompressionMethod::Deflate64
            | CompressionMethod::Bzip2
            | CompressionMethod::Lzma
            | CompressionMethod::Xz
    ) {
        Some("Its compression method isn't supported")
    } else if match entry.compression() {
        CompressionMethod::Stored => size != packed,
        CompressionMethod::Deflated => {
            size > packed
                .saturating_mul(MAX_DEFLATE_RATIO)
                .saturating_add(1024)
        }
        _ => false,
    } {
        Some("Its sizes don't add up, the archive is damaged")
    } else {
        None
    }
}

/// ZIP stores local time without a zone; this assumes the zone of this computer, like FAT.
fn dos_time(time: DateTime) -> Option<Timestamp> {
    // Checked construction: corrupt entries must not panic.
    civil::DateTime::new(
        i16::try_from(time.year()).ok()?,
        i8::try_from(time.month()).ok()?,
        i8::try_from(time.day()).ok()?,
        i8::try_from(time.hour()).ok()?,
        i8::try_from(time.minute()).ok()?,
        i8::try_from(time.second()).ok()?,
        0,
    )
    .ok()?
    .to_zoned(TimeZone::system())
    .ok()
    .map(|zoned| zoned.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_zip_names() {
        assert!(is_zip_archive(Path::new("/data/Backup.ZIP")));
        assert!(is_zip_name(OsStr::new("a.b.zip")));
        for name in ["a.zipx", "a.docx", "a.jar", "zip", ".zip.txt", "a.tar.gz"] {
            assert!(!is_zip_name(OsStr::new(name)), "{name}");
        }
        assert!(!is_zip_file(OsStr::new("a.zip"), 21));
        assert!(is_zip_file(OsStr::new("a.zip"), 22));
    }

    #[test]
    fn finds_the_end_record_in_the_tail() {
        let check = |data: Vec<u8>| {
            has_end_record(&mut Window::new(io::Cursor::new(data), 16).unwrap()).unwrap()
        };
        let (data, record) = (b"x".repeat(100), [&END_RECORD[..], &[0; 18]].concat());
        assert!(check([&data[..], &record].concat()));
        // Followed by the longest comment.
        let comment = b"c".repeat(65_535);
        assert!(check([&data[..], &record, &comment].concat()));
        // Pushed beyond it, or absent.
        assert!(!check([&data[..], &record, &comment, b"!"].concat()));
        assert!(!check(b"this is not a ZIP archive, just text\n".repeat(3)));
        assert!(!check(b"PK\x03\x04".repeat(10)));
    }

    #[test]
    fn converts_dos_times() {
        let time = DateTime::from_date_and_time(2024, 2, 3, 4, 5, 6).unwrap();
        let expected = civil::date(2024, 2, 3)
            .at(4, 5, 6, 0)
            .to_zoned(TimeZone::system())
            .unwrap()
            .timestamp();
        assert_eq!(dos_time(time), Some(expected));
    }

    #[test]
    fn folder_keys_have_normal_components_only() {
        let shown = Path::new("/a.zip/x");
        assert_eq!(
            folder_key(Path::new("a/./b"), shown).unwrap(),
            Path::new("a/b")
        );
        assert_eq!(folder_key(Path::new("/a"), shown).unwrap(), Path::new("a"));
        assert_eq!(folder_key(Path::new(""), shown).unwrap(), Path::new(""));
        assert!(folder_key(Path::new("a/../b"), shown).is_err());
        assert!(folder_key(Path::new("../b"), shown).is_err());
    }
}
