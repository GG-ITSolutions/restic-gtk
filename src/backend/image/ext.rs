//! ext2/3/4 volumes, read with `ext4-view`, which replays the journal in memory.

use std::{
    error::Error as StdError,
    ffi::OsStr,
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

use ext4_view::{Ext4, Ext4Error, Ext4Read, FileType, Metadata};
use rustic_core::jiff::Timestamp;

use crate::backend::{Entry, EntryKind, Error, Result};

/// ext volumes are opened per call: `ext4-view` isn't `Send`, and opening only reads the
/// superblock, the group descriptors and the journal.
pub struct ExtVolume;

/// Lets `ext4-view` read from any `Read + Seek`, e.g. a partition or a logical volume.
struct Source<R>(R);

impl<R: Read + Seek> Ext4Read for Source<R> {
    fn read(
        &mut self,
        start_byte: u64,
        dst: &mut [u8],
    ) -> std::result::Result<(), Box<dyn StdError + Send + Sync + 'static>> {
        self.0.seek(SeekFrom::Start(start_byte))?;
        self.0.read_exact(dst)?;
        Ok(())
    }
}

impl ExtVolume {
    fn open<R: Read + Seek + 'static>(volume: R, shown: &Path) -> Result<Ext4> {
        Ext4::load(Box::new(Source(volume))).map_err(|err| match err {
            Ext4Error::Incompatible(_) => Error::UnsupportedFilesystem(incompatible(&err)),
            err => map_error(err, shown),
        })
    }

    /// The entries of the folder at `path`, named below the virtual path `parent`.
    pub fn list<R: Read + Seek + 'static>(
        volume: R,
        path: &Path,
        parent: &Path,
    ) -> Result<Vec<Entry>> {
        let fs = Self::open(volume, parent)?;
        let dir = ext_path(path, parent)?;
        let mut entries = Vec::new();
        for entry in fs.read_dir(&dir).map_err(|err| map_error(err, parent))? {
            let entry = entry.map_err(|err| map_error(err, parent))?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let name: &[u8] = name.as_ref();
            // Without its inode (ext4-view 1.0 calls a time before 1970 corrupt) an entry is
            // listed as unavailable, so a restore skips it instead of failing the whole folder.
            let metadata = match entry.metadata() {
                Ok(metadata) => Some(metadata),
                Err(err @ Ext4Error::Io(_)) => return Err(map_error(err, parent)),
                Err(_) => None,
            };
            let kind = match &metadata {
                None => EntryKind::Unavailable {
                    reason: "Its inode can't be read (e.g. a time before 1970)".to_owned(),
                },
                Some(_) => match entry.file_type().map_err(|err| map_error(err, parent))? {
                    FileType::Directory => EntryKind::Dir,
                    FileType::Regular => EntryKind::File,
                    FileType::Symlink => EntryKind::Symlink {
                        target: fs
                            .read_link(&entry.path())
                            .map(|target| PathBuf::from(OsStr::from_bytes(target.as_ref())))
                            .unwrap_or_default(),
                    },
                    _ => EntryKind::Special,
                },
            };
            entries.push(Entry {
                // The raw name, so non-UTF-8 names still resolve.
                path: parent.join(OsStr::from_bytes(name)),
                name: String::from_utf8_lossy(name).into_owned(),
                size: match (&kind, &metadata) {
                    (EntryKind::File, Some(metadata)) => metadata.len(),
                    _ => 0,
                },
                kind,
                mtime: metadata.as_ref().and_then(mtime),
                inside_image: true,
            });
        }
        Ok(entries)
    }

    /// Writes the contents of the file at `path` to `out`.
    ///
    /// `shown` is the path to name in errors.
    pub fn write_file<R: Read + Seek + 'static>(
        volume: R,
        path: &Path,
        shown: &Path,
        out: &mut dyn Write,
    ) -> Result<()> {
        let fs = Self::open(volume, shown)?;
        let file = ext_path(path, shown)?;
        let unsupported = |reason: String| Error::UnsupportedFile {
            path: shown.to_path_buf(),
            reason,
        };
        let metadata = fs
            .symlink_metadata(&file)
            .map_err(|err| map_error(err, shown))?;
        if metadata.is_symlink() {
            let target = fs.read_link(&file).map(|t| t.display().to_string());
            return Err(unsupported(format!(
                "It is a link to “{}”",
                target.unwrap_or_default()
            )));
        }
        let mut file = fs.open(&file).map_err(|err| match err {
            Ext4Error::IsASpecialFile => unsupported("It is a device, pipe or socket".to_owned()),
            err => map_error(err, shown),
        })?;
        io::copy(&mut file, out)?;
        Ok(())
    }
}

/// `path`, relative to the root of the volume, as the absolute path `ext4-view` wants.
fn ext_path(path: &Path, shown: &Path) -> Result<ext4_view::PathBuf> {
    let mut bytes = Vec::from(*b"/");
    for component in path.components() {
        if let Component::Normal(name) = component {
            if bytes.len() > 1 {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(name.as_bytes());
        }
    }
    // Names with NUL or over 255 bytes can't exist in the filesystem.
    ext4_view::PathBuf::try_from(bytes).map_err(|_| Error::NotFound(shown.to_path_buf()))
}

/// Why a filesystem `ext4-view` refuses to load can't be browsed.
///
/// `ext4-view` 1.0 keeps the kind of incompatibility private, so the journal case is told
/// from its text. The journal is only loaded when the filesystem needs recovery, so any
/// journal complaint means the image was taken from a running or crashed system.
fn incompatible(err: &Ext4Error) -> String {
    if err.to_string().contains("journal") {
        format!(
            "The filesystem wasn't cleanly unmounted and its journal can't be replayed here \
             ({err})."
        )
    } else {
        format!("This ext filesystem can't be browsed: {err}")
    }
}

fn map_error(err: Ext4Error, shown: &Path) -> Error {
    match err {
        Ext4Error::NotFound => Error::NotFound(shown.to_path_buf()),
        Ext4Error::NotADirectory => Error::NotADirectory(shown.to_path_buf()),
        Ext4Error::IsADirectory => Error::NotAFile(shown.to_path_buf()),
        // A file or folder under fscrypt; a restore skips it.
        Ext4Error::Encrypted => Error::UnsupportedFile {
            path: shown.to_path_buf(),
            reason: "It is encrypted".to_owned(),
        },
        // Keep the kind of the reader's error, e.g. when the network fails.
        Ext4Error::Io(err) => Error::Io(match err.downcast::<io::Error>() {
            Ok(err) => *err,
            Err(err) => io::Error::other(err),
        }),
        err => Error::Image(format!(
            "The ext filesystem is damaged or unsupported: {err}"
        )),
    }
}

/// `None` for timestamps that were never set or that jiff can't represent.
fn mtime(metadata: &Metadata) -> Option<Timestamp> {
    let time = metadata.modified();
    if time.seconds() == 0 && time.nanoseconds() == 0 {
        return None;
    }
    Timestamp::new(time.seconds(), i32::try_from(time.nanoseconds()).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_absolute_in_the_volume() {
        let path = |p: &str| {
            ext_path(Path::new(p), Path::new("shown")).map(|p| p.to_str().unwrap().to_owned())
        };
        assert_eq!(path("").unwrap(), "/");
        assert_eq!(path("docs/sub").unwrap(), "/docs/sub");
        assert_eq!(path("./docs/../x").unwrap(), "/docs/x");
        assert!(matches!(path(&"a".repeat(256)), Err(Error::NotFound(_))));
        assert!(matches!(path("a\0b"), Err(Error::NotFound(_))));
    }

    #[test]
    fn reads_what_the_source_gives() {
        let mut source = Source(io::Cursor::new(vec![1, 2, 3, 4]));
        let mut buf = [0; 2];
        source.read(1, &mut buf).unwrap();
        assert_eq!(buf, [2, 3]);
        // Short data is an error, not a short read.
        assert!(source.read(3, &mut buf).is_err());
    }

    #[test]
    fn encrypted_entries_are_unsupported_files() {
        assert!(matches!(
            map_error(Ext4Error::Encrypted, Path::new("x")),
            Error::UnsupportedFile { path, reason } if path == Path::new("x") && reason == "It is encrypted"
        ));
    }

    #[test]
    fn io_errors_keep_their_kind() {
        let err = Ext4Error::Io(Box::new(io::Error::from(io::ErrorKind::TimedOut)));
        assert!(
            matches!(map_error(err, Path::new("x")), Error::Io(e) if e.kind() == io::ErrorKind::TimedOut)
        );
    }
}
