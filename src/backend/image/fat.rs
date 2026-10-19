//! FAT12/16/32 volumes, e.g. EFI system partitions.

use std::{
    io::{self, Read, Seek, Write},
    path::{Component, Path},
};

use fatfs::{FileSystem, FsOptions};
use rustic_core::jiff::{civil, tz::TimeZone};

use super::reader::ReadOnly;
use crate::backend::{Entry, EntryKind, Error, Result};

/// FAT volumes are opened per call: `fatfs` isn't `Send`, and opening only reads
/// the boot sector. FAT volumes in images are small boot partitions anyway.
pub struct FatVolume;

impl FatVolume {
    fn open<R: Read + Seek>(volume: R) -> Result<FileSystem<ReadOnly<R>>> {
        Ok(FileSystem::new(ReadOnly(volume), FsOptions::new())?)
    }

    /// The entries of the folder at `path`, named below the virtual path `parent`.
    pub fn list<R: Read + Seek>(volume: R, path: &Path, parent: &Path) -> Result<Vec<Entry>> {
        let fs = Self::open(volume)?;
        let root = fs.root_dir();
        let dir = match fat_path(path) {
            Some(path) => root.open_dir(&path).map_err(|err| not_found(err, parent))?,
            None => root,
        };
        let mut entries = Vec::new();
        for entry in dir.iter() {
            let entry = entry?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let modified = entry.modified();
            entries.push(Entry {
                path: parent.join(&name),
                name,
                kind: if entry.is_dir() {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                size: if entry.is_dir() { 0 } else { entry.len() },
                mtime: fat_time(&modified),
                inside_image: true,
            });
        }
        Ok(entries)
    }

    /// Writes the contents of the file at `path` to `out`.
    pub fn write_file<R: Read + Seek>(
        volume: R,
        path: &Path,
        shown: &Path,
        out: &mut dyn Write,
    ) -> Result<()> {
        let fs = Self::open(volume)?;
        let path = fat_path(path).ok_or_else(|| Error::NotAFile(shown.to_path_buf()))?;
        let mut file = fs
            .root_dir()
            .open_file(&path)
            .map_err(|err| not_found(err, shown))?;
        io::copy(&mut file, out)?;
        Ok(())
    }
}

/// `path` as `fatfs` wants it: '/'-separated, relative, `None` for the root.
fn fat_path(path: &Path) -> Option<String> {
    let parts: Vec<_> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy()),
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn not_found(err: io::Error, path: &Path) -> Error {
    match err.kind() {
        io::ErrorKind::NotFound => Error::NotFound(path.to_path_buf()),
        _ => Error::Io(err),
    }
}

/// FAT stores local time without a zone; this assumes the zone of this computer.
fn fat_time(time: &fatfs::DateTime) -> Option<rustic_core::jiff::Timestamp> {
    // Checked construction: corrupt entries must not panic.
    civil::DateTime::new(
        i16::try_from(time.date.year).ok()?,
        i8::try_from(time.date.month).ok()?,
        i8::try_from(time.date.day).ok()?,
        i8::try_from(time.time.hour).ok()?,
        i8::try_from(time.time.min).ok()?,
        i8::try_from(time.time.sec).ok()?,
        0,
    )
    .ok()?
    .to_zoned(TimeZone::system())
    .ok()
    .map(|zoned| zoned.timestamp())
}
