//! Disk images inside snapshots, browsed without downloading them.
//!
//! A path inside an image continues the path of the image file in the snapshot:
//! `/disk0.img/Partition 3 (C:)/Users`. The first level below the image are its
//! partitions, below that the files of the filesystem.

mod container;
mod crypt;
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

pub(crate) use reader::SnapshotReader;

use std::{
    collections::HashMap,
    ffi::OsStr,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use rustic_core::repofile::Node;

use self::{
    container::Disk,
    crypt::{Decrypted, Secret, VolumeId},
    ext::ExtVolume,
    fat::FatVolume,
    lvm::LvmVolume,
    ntfs::NtfsVolume,
    partition::{FsKind, Partition},
    reader::Slice,
};
use super::{Entry, EntryKind, Error, Result, session::IndexedRepo};

/// File name extensions of raw disk images and VM disks.
const EXTENSIONS: [&str; 8] = ["img", "raw", "dd", "qcow2", "qcow", "vhd", "vhdx", "vmdk"];

/// The virtual disk of an image file in a snapshot, whatever its container format.
type ImageReader = Disk<SnapshotReader>;

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
    /// What unlocked the encrypted volumes, kept until the session ends. Not tied to the open
    /// images, so a volume that shows up again (another snapshot, an evicted image) opens
    /// without asking again.
    keys: KeyCache,
}

type KeyCache = Mutex<HashMap<VolumeId, Secret>>;

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
    /// The files in the folder of the image, itself included.
    pub siblings: Vec<Node>,
}

impl ImageRef<'_> {
    /// The file `name` next to the image (exact name).
    pub fn sibling(&self, name: &str) -> Option<&Node> {
        self.siblings
            .iter()
            .find(|n| n.is_file() && n.name() == OsStr::new(name))
    }
}

struct OpenImage {
    key: (String, PathBuf),
    reader: ImageReader,
    partitions: Vec<OpenPartition>,
}

struct OpenPartition {
    info: Partition,
    /// Drive letter from `system.json`, e.g. 'C'.
    drive: Option<char>,
    /// Opened on first use; errors aren't kept, they may be passing network trouble.
    volume: Mutex<Option<Arc<Volume>>>,
    /// Encrypted partitions: what they decrypt to, once unlocked.
    unlocked: Mutex<Option<Decrypted>>,
}

enum Volume {
    Ntfs(NtfsVolume),
    Fat,
    Ext,
    Lvm(LvmVolume),
}

/// The bytes of a filesystem: a partition of the image, or what an encrypted one decrypts to.
enum Source {
    Raw(Slice<ImageReader>),
    Decrypted(Decrypted),
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Raw(slice) => slice.read(buf),
            Self::Decrypted(plain) => plain.read(buf),
        }
    }
}

impl Seek for Source {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Raw(slice) => slice.seek(pos),
            Self::Decrypted(plain) => plain.seek(pos),
        }
    }
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

    /// The partition as it is on disk.
    fn raw(&self, image: &ImageReader) -> Slice<ImageReader> {
        Slice::new(image.clone(), self.info.start, self.info.len)
    }

    /// The bytes of the filesystem: the partition, or for an unlocked encrypted one its plaintext.
    fn slice(&self, image: &ImageReader) -> Source {
        match &*self.unlocked.lock().unwrap() {
            Some(plain) => Source::Decrypted(plain.rewound()),
            None => Source::Raw(self.raw(image)),
        }
    }

    /// The filesystem of the partition; `None` if it is encrypted and no key for it is known.
    fn volume(&self, image: &ImageReader, keys: &KeyCache) -> Result<Option<Arc<Volume>>> {
        let mut volume = self.volume.lock().unwrap();
        if let Some(volume) = volume.as_ref() {
            return Ok(Some(volume.clone()));
        }
        let fs = if self.info.fs.is_encrypted() {
            if !self.is_unlocked(image, keys)? {
                return Ok(None);
            }
            // The plaintext can hold anything a partition can.
            partition::detect_fs(&mut self.slice(image), 0)?
        } else {
            self.info.fs
        };
        let opened = Arc::new(match fs {
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
        Ok(Some(opened))
    }

    /// Whether the encrypted partition is unlocked, unlocking it first if the session knows its key.
    fn is_unlocked(&self, image: &ImageReader, keys: &KeyCache) -> Result<bool> {
        if self.unlocked.lock().unwrap().is_some() {
            return Ok(true);
        }
        let id = crypt::volume_id(self.info.fs, &mut self.raw(image))?;
        let Some(secret) = keys.lock().unwrap().get(&id).cloned() else {
            return Ok(false);
        };
        match crypt::unlock(self.info.fs, self.raw(image), &secret) {
            Ok(plain) => {
                *self.unlocked.lock().unwrap() = Some(plain);
                Ok(true)
            }
            // Not the key of this volume any more, e.g. it was encrypted anew: ask again.
            Err(Error::WrongPassword) => {
                keys.lock().unwrap().remove(&id);
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }

    /// Unlocks the encrypted partition with `secret` and remembers it for the session.
    fn unlock(&self, image: &ImageReader, secret: &str, keys: &KeyCache) -> Result<()> {
        if !self.info.fs.is_encrypted() {
            return Err(Error::Image(format!("“{}” is not encrypted", self.name())));
        }
        let plain = crypt::unlock(self.info.fs, self.raw(image), secret)?;
        let id = crypt::volume_id(self.info.fs, &mut self.raw(image))?;
        {
            // Same order as in `volume()`. A filesystem opened before is dropped, it was read
            // from the other source.
            let mut volume = self.volume.lock().unwrap();
            *self.unlocked.lock().unwrap() = Some(plain);
            *volume = None;
        }
        keys.lock()
            .unwrap()
            .insert(id, Secret::new(secret.to_owned()));
        Ok(())
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
        match &*open.volume(partition, &self.keys)? {
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
        match &*open.volume(partition, &self.keys)? {
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

    /// Unlocks the encrypted partition that `inner` is or lies in, with a recovery password or
    /// passphrase, and remembers it for the session.
    ///
    /// Fails with [`Error::WrongPassword`] if the secret doesn't fit the volume.
    pub fn unlock(&self, image: &ImageRef, inner: &Path, secret: &str) -> Result<()> {
        let open = self.open(image)?;
        let (partition, _) = open
            .partition(inner)?
            .ok_or_else(|| Error::NotFound(image.path.join(inner)))?;
        partition.unlock(&open.reader, secret, &self.keys)
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
        let sibling = |name: &str| {
            image
                .sibling(name)
                .map(|node| SnapshotReader::new(image.repo.clone(), node))
                .transpose()
        };
        let mut reader = container::open(
            SnapshotReader::new(image.repo.clone(), image.node)?,
            &sibling,
        )?;
        let size = reader.size();
        let table = partition::read_partitions(&mut reader, size)?;
        let image_name = image.path.file_name().unwrap_or_default();
        let drives = image
            .sibling(system_json::FILE_NAME)
            .and_then(|node| system_json::drive_letters(image.repo, node, image_name))
            .unwrap_or_default();
        let partitions = table
            .into_iter()
            .map(|info| OpenPartition {
                drive: drives.iter().find(|d| d.0 == info.start).map(|d| d.1),
                info,
                volume: Mutex::new(None),
                unlocked: Mutex::new(None),
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
    /// The filesystem of `partition`; fails with [`Error::VolumeLocked`] if it is encrypted and
    /// the session doesn't know its key.
    fn volume(&self, partition: &OpenPartition, keys: &KeyCache) -> Result<Arc<Volume>> {
        partition
            .volume(&self.reader, keys)?
            .ok_or_else(|| Error::VolumeLocked(self.key.1.join(partition.name())))
    }

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
        kind: if partition.info.fs.is_encrypted() {
            EntryKind::Encrypted
        } else {
            match partition.info.fs.unsupported_reason() {
                None => EntryKind::Partition,
                Some(reason) => EntryKind::Unavailable {
                    reason: reason.to_owned(),
                },
            }
        },
        size: partition.info.len,
        mtime: None,
        inside_image: true,
    }
}
