//! Disk images inside snapshots, browsed without downloading them.
//!
//! A path inside an image continues the path of the image file in the snapshot:
//! `/disk0.img/Partition 3 (C:)/Users`. The first level below the image are its
//! partitions, below that the files of the filesystem.

mod ext;
mod fat;
mod lvm;
mod lznt1;
mod lzx;
mod ntfs;
mod partition;
mod reader;
mod system_json;
mod wof;

pub(crate) use system_json::FILE_NAME as SYSTEM_JSON;

use std::{
    ffi::OsStr,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use rustic_core::repofile::Node;

use self::{
    ext::ExtVolume,
    fat::FatVolume,
    lvm::LvmVolume,
    ntfs::NtfsVolume,
    partition::{FsKind, Partition},
    reader::{Slice, SnapshotReader},
};
use super::{Entry, EntryKind, Error, Result, session::IndexedRepo};

/// File name extensions of raw disk images.
const EXTENSIONS: [&str; 3] = ["img", "raw", "dd"];

/// Images kept open per session; each holds its partition table and filesystems.
const OPEN_IMAGES: usize = 2;

const PARTITION_PREFIX: &str = "Partition ";

/// Whether the file at `path` is browsed as a disk image, judged by its name.
pub fn is_disk_image(path: &Path) -> bool {
    path.file_name().is_some_and(is_image_name)
}

/// Whether a file with this name is browsed as a disk image.
pub fn is_image_name(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// The disk images opened in a session.
#[derive(Default)]
pub(crate) struct ImageCache {
    /// Most recently used last.
    images: Mutex<Vec<Arc<OpenImage>>>,
}

impl std::fmt::Debug for ImageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageCache").finish_non_exhaustive()
    }
}

/// Where to find an image: the snapshot tree, the image node and its path.
pub(crate) struct ImageRef<'a> {
    pub repo: &'a Arc<IndexedRepo>,
    pub tree: String,
    pub node: &'a Node,
    /// The path of the image file in the snapshot.
    pub path: &'a Path,
    /// A `system.json` next to the image, if there is one.
    pub system_json: Option<Node>,
}

struct OpenImage {
    key: (String, PathBuf),
    reader: SnapshotReader,
    partitions: Vec<OpenPartition>,
}

struct OpenPartition {
    info: Partition,
    /// Drive letter from `system.json`, e.g. 'C'.
    drive: Option<char>,
    /// Opened on first use; errors aren't kept, they may be passing network trouble.
    volume: Mutex<Option<Arc<Volume>>>,
}

enum Volume {
    Ntfs(NtfsVolume),
    Fat,
    Ext,
    Lvm(LvmVolume),
}

impl OpenPartition {
    /// The folder name of the partition, e.g. "Partition 3 (C:)".
    fn name(&self) -> String {
        let number = self.info.number;
        let role = self
            .info
            .role
            .or((self.info.fs == FsKind::Lvm).then_some("LVM"));
        match (self.drive, role) {
            (Some(drive), _) => format!("{PARTITION_PREFIX}{number} ({drive}:)"),
            (None, Some(role)) => format!("{PARTITION_PREFIX}{number} ({role})"),
            (None, None) => format!("{PARTITION_PREFIX}{number}"),
        }
    }

    fn slice(&self, image: &SnapshotReader) -> Slice<SnapshotReader> {
        Slice::new(image.clone(), self.info.start, self.info.len)
    }

    fn volume(&self, image: &SnapshotReader) -> Result<Arc<Volume>> {
        let mut volume = self.volume.lock().unwrap();
        if let Some(volume) = volume.as_ref() {
            return Ok(volume.clone());
        }
        let opened = Arc::new(match self.info.fs {
            FsKind::Ntfs => Volume::Ntfs(NtfsVolume::open(&mut self.slice(image))?),
            FsKind::Fat => Volume::Fat,
            FsKind::Ext => Volume::Ext,
            FsKind::Lvm => Volume::Lvm(LvmVolume::open(self.slice(image))?),
            other => {
                return Err(Error::UnsupportedFilesystem(
                    other.unsupported_reason().unwrap_or_default().to_owned(),
                ));
            }
        });
        *volume = Some(opened.clone());
        Ok(opened)
    }
}

impl ImageCache {
    /// The entries of `inner` in an image: its partitions if empty, else a folder.
    pub fn list(&self, image: &ImageRef, inner: &Path) -> Result<Vec<Entry>> {
        let open = self.open(image)?;
        let virtual_path = image.path.join(inner);
        let Some((partition, rest)) = open.partition(inner)? else {
            return Ok(open
                .partitions
                .iter()
                .map(|p| partition_entry(p, image.path))
                .collect());
        };
        match &*partition.volume(&open.reader)? {
            Volume::Ntfs(volume) => {
                volume.list(&mut partition.slice(&open.reader), rest, &virtual_path)
            }
            Volume::Fat => FatVolume::list(partition.slice(&open.reader), rest, &virtual_path),
            Volume::Ext => ExtVolume::list(partition.slice(&open.reader), rest, &virtual_path),
            Volume::Lvm(volume) => {
                volume.list(|| partition.slice(&open.reader), rest, &virtual_path)
            }
        }
    }

    /// Writes the file at `inner` in an image to `out`.
    pub fn write_file(&self, image: &ImageRef, inner: &Path, out: &mut dyn Write) -> Result<()> {
        let open = self.open(image)?;
        let shown = image.path.join(inner);
        let (partition, rest) = open
            .partition(inner)?
            .ok_or_else(|| Error::NotAFile(shown.clone()))?;
        match &*partition.volume(&open.reader)? {
            Volume::Ntfs(volume) => {
                volume.write_file(&mut partition.slice(&open.reader), rest, &shown, out)
            }
            Volume::Fat => FatVolume::write_file(partition.slice(&open.reader), rest, &shown, out),
            Volume::Ext => ExtVolume::write_file(partition.slice(&open.reader), rest, &shown, out),
            Volume::Lvm(volume) => {
                volume.write_file(|| partition.slice(&open.reader), rest, &shown, out)
            }
        }
    }

    /// The opened image, reading its partition table if it isn't open yet.
    fn open(&self, image: &ImageRef) -> Result<Arc<OpenImage>> {
        let key = (image.tree.clone(), image.path.to_path_buf());
        {
            let mut images = self.images.lock().unwrap();
            if let Some(index) = images.iter().position(|i| i.key == key) {
                let open = images.remove(index);
                images.push(open.clone());
                return Ok(open);
            }
        }

        // Read without holding the lock; a concurrent caller may open it as well.
        let mut reader = SnapshotReader::new(image.repo.clone(), image.node)?;
        let size = reader.size();
        let table = partition::read_partitions(&mut reader, size)?;
        let image_name = image.path.file_name().unwrap_or_default();
        let drives = image
            .system_json
            .as_ref()
            .and_then(|node| system_json::drive_letters(image.repo, node, image_name))
            .unwrap_or_default();
        let partitions = table
            .into_iter()
            .map(|info| OpenPartition {
                drive: drives.iter().find(|d| d.0 == info.start).map(|d| d.1),
                info,
                volume: Mutex::new(None),
            })
            .collect();
        let open = Arc::new(OpenImage {
            key,
            reader,
            partitions,
        });

        let mut images = self.images.lock().unwrap();
        images.retain(|i| i.key != open.key);
        images.push(open.clone());
        if images.len() > OPEN_IMAGES {
            images.remove(0);
        }
        Ok(open)
    }
}

impl OpenImage {
    /// The partition named by the first component of `inner`, and the rest of the path.
    fn partition<'p>(&self, inner: &'p Path) -> Result<Option<(&OpenPartition, &'p Path)>> {
        let mut components = inner.components();
        let first = loop {
            match components.next() {
                Some(Component::Normal(name)) => break name,
                Some(_) => continue,
                None => return Ok(None),
            }
        };
        let number = first
            .to_str()
            .and_then(|name| name.strip_prefix(PARTITION_PREFIX))
            .map(|rest| rest.split(' ').next().unwrap_or_default())
            .and_then(|number| number.parse::<u32>().ok());
        let partition = self
            .partitions
            .iter()
            .find(|p| Some(p.info.number) == number)
            .ok_or_else(|| Error::NotFound(PathBuf::from(first)))?;
        Ok(Some((partition, components.as_path())))
    }
}

fn partition_entry(partition: &OpenPartition, image_path: &Path) -> Entry {
    let name = partition.name();
    Entry {
        path: image_path.join(&name),
        name,
        kind: match partition.info.fs.unsupported_reason() {
            None => EntryKind::Partition,
            Some(reason) => EntryKind::Unavailable {
                reason: reason.to_owned(),
            },
        },
        size: partition.info.len,
        mtime: None,
        inside_image: true,
    }
}
