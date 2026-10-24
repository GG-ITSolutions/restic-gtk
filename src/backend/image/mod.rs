//! Disk images inside snapshots, browsed without downloading them.
//!
//! A path inside an image continues the path of the image file in the snapshot:
//! `/disk0.img/Partition 3 (C:)/Users`. The first level below the image are its
//! partitions, below that the files of the filesystem.
//!
//! Between the two there may be more layers: an LVM physical volume is a folder of its
//! logical volumes (`/disk0.img/Partition 2 (LVM)/vg0-root/etc`), and an encrypted volume
//! is browsed like a plain one once it is unlocked. Every layer is a [`Layer`], opened the
//! same way whatever lies above it: read the first sectors, detect what is there, dispatch.
//! So ext4 under LUKS, LUKS inside a logical volume or LVM inside BitLocker all work, also
//! inside a qcow2 or VHDX container, which sits below the partition table.

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

use rustic_core::{jiff::Timestamp, repofile::Node};

use self::{
    container::Disk,
    crypt::{Secret, VolumeId},
    ext::ExtVolume,
    fat::FatVolume,
    ntfs::NtfsVolume,
    partition::{FsKind, Partition},
    reader::{Shared, Slice, lock},
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
    partitions: Vec<Arc<Layer>>,
}

/// A block device inside an image and what was found on it: a partition, a logical volume or
/// the plaintext of an encrypted volume.
struct Layer {
    /// The folder name, e.g. "Partition 3 (C:)" or "vg0-root"; empty for a plaintext.
    name: String,
    /// Partitions are found by their number (the rest of the name may be left out), the other
    /// layers by their name.
    number: Option<u32>,
    size: u64,
    mtime: Option<Timestamp>,
    /// The bytes and the kind of filesystem at their start, or why there is nothing to read.
    block: std::result::Result<(Source, FsKind), String>,
    /// What is on the layer, opened on first use; errors aren't kept, they may be passing
    /// network trouble.
    opened: Mutex<Option<Opened>>,
    /// Encrypted volumes: the plaintext as a layer of its own, once unlocked.
    plain: Mutex<Option<Arc<Layer>>>,
}

/// What is on a layer.
#[derive(Clone)]
enum Opened {
    Filesystem(Arc<Filesystem>),
    /// The logical volumes of an LVM physical volume.
    Volumes(Vec<Arc<Layer>>),
}

enum Filesystem {
    Ntfs(NtfsVolume),
    Fat,
    Ext,
}

/// The bytes of a layer: a part of the image, or a reader on top of another layer.
#[derive(Clone)]
enum Source {
    Raw(Slice<ImageReader>),
    Shared(Shared),
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Raw(slice) => slice.read(buf),
            Self::Shared(shared) => shared.read(buf),
        }
    }
}

impl Seek for Source {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Raw(slice) => slice.seek(pos),
            Self::Shared(shared) => shared.seek(pos),
        }
    }
}

/// What a path inside an image names.
enum Found<'p> {
    /// A folder of layers: the partitions of the image, or the logical volumes of a group.
    Layers(Vec<Arc<Layer>>),
    /// A path inside a filesystem.
    Inside {
        fs: Arc<Filesystem>,
        source: Source,
        rest: &'p Path,
    },
}

impl Layer {
    fn new(
        name: String,
        number: Option<u32>,
        size: u64,
        mtime: Option<Timestamp>,
        block: std::result::Result<(Source, FsKind), String>,
    ) -> Self {
        Self {
            name,
            number,
            size,
            mtime,
            block,
            opened: Mutex::new(None),
            plain: Mutex::new(None),
        }
    }

    /// The encrypted volume `fs` on `source`, if that is one.
    fn encrypted(&self) -> Option<(&Source, FsKind)> {
        match &self.block {
            Ok((source, fs)) if fs.is_encrypted() => Some((source, *fs)),
            _ => None,
        }
    }

    /// The entry of the layer in the folder `parent`.
    fn entry(&self, parent: &Path) -> Entry {
        Entry {
            path: parent.join(&self.name),
            name: self.name.clone(),
            kind: match &self.block {
                Err(reason) => EntryKind::Unavailable {
                    reason: reason.clone(),
                },
                Ok((_, fs)) if fs.is_encrypted() => EntryKind::Encrypted,
                Ok((_, fs)) => match fs.unsupported_reason() {
                    None => EntryKind::Partition,
                    Some(reason) => EntryKind::Unavailable {
                        reason: reason.to_owned(),
                    },
                },
            },
            size: self.size,
            mtime: self.mtime,
            inside_image: true,
        }
    }

    /// Whether the path component `name` means this layer.
    fn is_named(&self, name: &OsStr) -> bool {
        match self.number {
            Some(number) => {
                name.to_str()
                    .and_then(|name| name.strip_prefix(PARTITION_PREFIX))
                    .map(|rest| rest.split(' ').next().unwrap_or_default())
                    .and_then(|number| number.parse::<u32>().ok())
                    == Some(number)
            }
            None => name == OsStr::new(&self.name),
        }
    }

    /// What is on the layer and the bytes to read it from; an encrypted layer answers for its
    /// plaintext. `path` is the virtual path of the layer, named in errors.
    ///
    /// Fails with [`Error::VolumeLocked`] if the layer is encrypted and the session doesn't
    /// know its key.
    fn open(&self, keys: &KeyCache, path: &Path) -> Result<(Opened, Source)> {
        let (source, fs) = match &self.block {
            Ok((source, fs)) => (source, *fs),
            Err(reason) => return Err(Error::UnsupportedFilesystem(reason.clone())),
        };
        if fs.is_encrypted() {
            return self.unlocked(keys, path)?.open(keys, path);
        }
        let mut opened = lock(&self.opened);
        if let Some(opened) = opened.as_ref() {
            return Ok((opened.clone(), source.clone()));
        }
        let found = match fs {
            FsKind::Ntfs => Opened::Filesystem(Arc::new(Filesystem::Ntfs(NtfsVolume::open(
                &mut source.clone(),
            )?))),
            FsKind::Fat => Opened::Filesystem(Arc::new(Filesystem::Fat)),
            FsKind::Ext => Opened::Filesystem(Arc::new(Filesystem::Ext)),
            FsKind::Lvm => Opened::Volumes(
                lvm::logical_volumes(source.clone())?
                    .into_iter()
                    .map(|lv| {
                        let block = lv.content.map(|(reader, fs)| (Source::Shared(reader), fs));
                        Arc::new(Layer::new(lv.name, None, lv.size, lv.mtime, block))
                    })
                    .collect(),
            ),
            other => {
                return Err(Error::UnsupportedFilesystem(
                    other.unsupported_reason().unwrap_or_default().to_owned(),
                ));
            }
        };
        *opened = Some(found.clone());
        Ok((found, source.clone()))
    }

    /// The plaintext of the encrypted layer, unlocking it first if the session knows its key.
    fn unlocked(&self, keys: &KeyCache, path: &Path) -> Result<Arc<Layer>> {
        if let Some(plain) = lock(&self.plain).as_ref() {
            return Ok(plain.clone());
        }
        let (source, fs) = self.encrypted().expect("checked by the caller");
        let id = crypt::volume_id(fs, &mut source.clone())?;
        let Some(secret) = lock(keys).get(&id).cloned() else {
            return Err(Error::VolumeLocked(path.to_path_buf()));
        };
        match self.decrypt(source, fs, &secret) {
            // Not the key of this volume any more, e.g. it was encrypted anew: ask again.
            Err(Error::WrongPassword) => {
                lock(keys).remove(&id);
                Err(Error::VolumeLocked(path.to_path_buf()))
            }
            plain => plain,
        }
    }

    /// Unlocks the encrypted layer with `secret` and remembers it for the session.
    fn unlock(&self, secret: &str, keys: &KeyCache, path: &Path) -> Result<()> {
        let Some((source, fs)) = self.encrypted() else {
            return Err(Error::Image(format!(
                "“{}” is not encrypted",
                path.display()
            )));
        };
        // Already open: keep what is open below instead of reading it again.
        if lock(&self.plain).is_some() {
            return Ok(());
        }
        self.decrypt(source, fs, secret)?;
        let id = crypt::volume_id(fs, &mut source.clone())?;
        lock(keys).insert(id, Secret::new(secret.to_owned()));
        Ok(())
    }

    /// Decrypts the layer with `secret` and keeps the plaintext as a layer; whatever an earlier
    /// unlock opened is dropped.
    fn decrypt(&self, source: &Source, fs: FsKind, secret: &str) -> Result<Arc<Layer>> {
        let plain = crypt::unlock(fs, source.clone(), secret)?;
        let size = plain.size();
        let mut plain = Source::Shared(plain);
        // The plaintext can hold anything a partition can. Filesystem libraries expect a
        // source at its start, and clones keep the position.
        let fs = partition::detect_fs(&mut plain, 0)?;
        plain.seek(SeekFrom::Start(0))?;
        let block = if fs.is_encrypted() {
            Err("Encrypted volumes inside encrypted volumes can't be browsed".to_owned())
        } else {
            Ok((plain, fs))
        };
        let layer = Arc::new(Layer::new(String::new(), None, size, None, block));
        *lock(&self.plain) = Some(layer.clone());
        Ok(layer)
    }
}

impl Filesystem {
    /// The entries of the folder at `path`, named below the virtual path `parent`.
    fn list(&self, mut source: Source, path: &Path, parent: &Path) -> Result<Vec<Entry>> {
        match self {
            Self::Ntfs(volume) => volume.list(&mut source, path, parent),
            Self::Fat => FatVolume::list(source, path, parent),
            Self::Ext => ExtVolume::list(source, path, parent),
        }
    }

    /// Writes the file at `path`, shown as `shown` in errors, to `out`.
    fn write_file(
        &self,
        mut source: Source,
        path: &Path,
        shown: &Path,
        out: &mut dyn Write,
    ) -> Result<()> {
        match self {
            Self::Ntfs(volume) => volume.write_file(&mut source, path, shown, out),
            Self::Fat => FatVolume::write_file(source, path, shown, out),
            Self::Ext => ExtVolume::write_file(source, path, shown, out),
        }
    }
}

impl ImageCache {
    /// The entries of `inner` in an image: its partitions if empty, else a folder.
    pub fn list(&self, image: &ImageRef, inner: &Path) -> Result<Vec<Entry>> {
        let open = self.open(image)?;
        let virtual_path = image.path.join(inner);
        match open.find(inner, &self.keys)? {
            Found::Layers(layers) => Ok(layers.iter().map(|l| l.entry(&virtual_path)).collect()),
            Found::Inside { fs, source, rest } => fs.list(source, rest, &virtual_path),
        }
    }

    /// Writes the file at `inner` in an image to `out`.
    pub fn write_file(&self, image: &ImageRef, inner: &Path, out: &mut dyn Write) -> Result<()> {
        let open = self.open(image)?;
        let shown = image.path.join(inner);
        match open.find(inner, &self.keys)? {
            Found::Layers(_) => Err(Error::NotAFile(shown)),
            Found::Inside { fs, source, rest } => fs.write_file(source, rest, &shown, out),
        }
    }

    /// Unlocks the encrypted volume that `inner` is or lies in, with a recovery password or
    /// passphrase, and remembers it for the session.
    ///
    /// Of nested encrypted volumes, the deepest one on the way to `inner` that can be reached
    /// is meant: the locked one, which is the one an [`Error::VolumeLocked`] named.
    /// Fails with [`Error::WrongPassword`] if the secret doesn't fit the volume.
    pub fn unlock(&self, image: &ImageRef, inner: &Path, secret: &str) -> Result<()> {
        let open = self.open(image)?;
        let (layer, path) = open.encrypted(inner, &self.keys)?;
        layer.unlock(secret, &self.keys, &path)
    }

    /// The opened image, reading its partition table if it isn't open yet.
    fn open(&self, image: &ImageRef) -> Result<Arc<OpenImage>> {
        let key = (image.tree.clone(), image.path.to_path_buf());
        {
            let mut images = lock(&self.images);
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
            .map(|info| {
                let drive = drives.iter().find(|d| d.0 == info.start).map(|d| d.1);
                let slice = Slice::new(reader.clone(), info.start, info.len);
                Arc::new(Layer::new(
                    partition_name(&info, drive),
                    Some(info.number),
                    info.len,
                    None,
                    Ok((Source::Raw(slice), info.fs)),
                ))
            })
            .collect();
        let open = Arc::new(OpenImage { key, partitions });

        let mut images = lock(&self.images);
        images.retain(|i| i.key != open.key);
        images.push(open.clone());
        if images.len() > OPEN_IMAGES {
            images.remove(0);
        }
        Ok(open)
    }
}

impl OpenImage {
    /// Follows `inner` down the layers, to a filesystem or to a folder of layers.
    fn find<'p>(&self, inner: &'p Path, keys: &KeyCache) -> Result<Found<'p>> {
        let mut layers = self.partitions.clone();
        let mut path = self.key.1.clone();
        let mut components = inner.components();
        loop {
            let name = loop {
                match components.next() {
                    Some(Component::Normal(name)) => break name,
                    Some(_) => continue,
                    None => return Ok(Found::Layers(layers)),
                }
            };
            let layer = child(&layers, name, &path)?;
            path.push(&layer.name);
            match layer.open(keys, &path)? {
                (Opened::Volumes(volumes), _) => layers = volumes,
                (Opened::Filesystem(fs), source) => {
                    return Ok(Found::Inside {
                        fs,
                        source,
                        rest: components.as_path(),
                    });
                }
            }
        }
    }

    /// The encrypted layer that `inner` is or lies in, with its virtual path: the deepest one on
    /// the way that can be reached, so a locked one or the last one before the filesystem.
    fn encrypted(&self, inner: &Path, keys: &KeyCache) -> Result<(Arc<Layer>, PathBuf)> {
        let mut layers = self.partitions.clone();
        let mut path = self.key.1.clone();
        let mut found = None;
        let names: Vec<&OsStr> = inner
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        if names.is_empty() {
            return Err(Error::NotFound(path));
        }
        for (index, name) in names.iter().enumerate() {
            let layer = child(&layers, name, &path)?;
            path.push(&layer.name);
            if layer.encrypted().is_some() {
                found = Some((layer.clone(), path.clone()));
            }
            if index + 1 == names.len() {
                break;
            }
            match layer.open(keys, &path) {
                Ok((Opened::Volumes(volumes), _)) => layers = volumes,
                // The rest of the path lies inside the filesystem.
                Ok((Opened::Filesystem(_), _)) | Err(Error::VolumeLocked(_)) => break,
                Err(err) => return Err(err),
            }
        }
        found.ok_or_else(|| {
            Error::Image(format!(
                "“{}” is not encrypted",
                self.key.1.join(inner).display()
            ))
        })
    }
}

/// The layer named by the path component `name` among `layers`, which lie in `parent`.
fn child(layers: &[Arc<Layer>], name: &OsStr, parent: &Path) -> Result<Arc<Layer>> {
    layers
        .iter()
        .find(|layer| layer.is_named(name))
        .cloned()
        .ok_or_else(|| Error::NotFound(parent.join(name)))
}

/// The folder name of a partition, e.g. "Partition 3 (C:)" with `drive` from `system.json`.
fn partition_name(info: &Partition, drive: Option<char>) -> String {
    let number = info.number;
    let role = info.role.or((info.fs == FsKind::Lvm).then_some("LVM"));
    match (drive, role) {
        (Some(drive), _) => format!("{PARTITION_PREFIX}{number} ({drive}:)"),
        (None, Some(role)) => format!("{PARTITION_PREFIX}{number} ({role})"),
        (None, None) => format!("{PARTITION_PREFIX}{number}"),
    }
}
