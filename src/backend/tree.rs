use std::path::{Path, PathBuf};

use rustic_core::{
    jiff::Timestamp,
    repofile::{Node, NodeType},
};

use super::image;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Symlink {
        target: PathBuf,
    },
    /// Devices, fifos and sockets: listed, but can't be opened.
    Special,
    /// A raw disk image, browsed like a folder of its partitions.
    DiskImage,
    /// A partition of a disk image, browsed like a folder.
    Partition,
    /// An encrypted partition of a disk image (BitLocker, LUKS), browsed like a folder once
    /// its key is known.
    Encrypted,
    /// A partition of a disk image that can't be browsed, e.g. one with an unsupported filesystem.
    Unavailable {
        reason: String,
    },
}

/// One item of a folder inside a snapshot.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    /// Absolute path inside the snapshot.
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: Option<Timestamp>,
    /// Inside a disk image: can be opened, but not restored.
    pub inside_image: bool,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    pub(crate) fn from_node(parent: &Path, node: &Node) -> Self {
        let name = node.name().to_string_lossy().into_owned();
        let kind = match &node.node_type {
            NodeType::Dir => EntryKind::Dir,
            NodeType::File if image::is_image_name(node.name().as_ref()) => EntryKind::DiskImage,
            NodeType::File => EntryKind::File,
            symlink @ NodeType::Symlink { .. } => EntryKind::Symlink {
                target: symlink.to_link().to_path_buf(),
            },
            _ => EntryKind::Special,
        };
        Self {
            // The raw name, so non-UTF-8 paths still resolve.
            path: parent.join(node.name()),
            name,
            kind,
            size: node.meta.size,
            mtime: node.meta.mtime,
            inside_image: false,
        }
    }
}
