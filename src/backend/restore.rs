use std::{
    fs, io,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use rustic_core::{
    LocalDestination, LsOptions, RestoreOptions,
    repofile::{Node, NodeType},
};

use super::{
    Error, ProgressKind, ProgressUpdate, Result, Session, Snapshot,
    session::{IndexedRepo, Location},
};

/// Entries of one snapshot to restore into a folder.
///
/// Each entry ends up as `<target>/<entry name>`, like copying it there.
#[derive(Debug, Clone)]
pub struct RestoreRequest {
    /// Absolute paths inside the snapshot.
    pub items: Vec<PathBuf>,
    pub target: PathBuf,
}

/// What a restore would do, computed without touching the target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestorePreview {
    /// Files that will be written.
    pub files: u64,
    /// Bytes that will be written.
    pub bytes: u64,
    /// Existing files in the target that will be overwritten with different content.
    pub conflicts: u64,
}

/// One requested entry, resolved to its node and its place in the target.
struct Item {
    node: Node,
    destination: PathBuf,
}

impl Session {
    pub fn preview_restore(
        &self,
        snapshot: &Snapshot,
        request: &RestoreRequest,
    ) -> Result<RestorePreview> {
        let repo = self.indexed()?;
        let options = RestoreOptions::default();
        let mut preview = RestorePreview::default();
        for item in self.resolve(&repo, snapshot, request)? {
            let destination = item.local_destination(false)?;
            let plan = repo.prepare_restore(
                &options,
                repo.ls(&item.node, &LsOptions::default())?,
                &destination,
                true,
            )?;
            preview.files += plan.stats.files.restore;
            preview.bytes += plan.restore_size;
            preview.conflicts += plan.stats.files.modify;
        }
        Ok(preview)
    }

    /// Restores the request, overwriting changed files in the target.
    ///
    /// Reports one [`ProgressKind::Activity`] while preparing, then
    /// [`ProgressKind::Bytes`] over all entries together.
    pub fn restore(
        &self,
        snapshot: &Snapshot,
        request: &RestoreRequest,
        on_progress: impl Fn(ProgressUpdate) + Send + Sync + 'static,
    ) -> Result<()> {
        let on_progress = Arc::new(on_progress);
        on_progress(ProgressUpdate {
            kind: ProgressKind::Activity,
            title: String::new(),
            position: 0,
            length: 0,
            finished: false,
        });

        let repo = self.indexed()?;
        let options = RestoreOptions::default();

        // Prepare everything first: creates the folders and yields the total size.
        let mut prepared = Vec::new();
        for item in self.resolve(&repo, snapshot, request)? {
            let destination = item.local_destination(true)?;
            let plan = repo.prepare_restore(
                &options,
                repo.ls(&item.node, &LsOptions::default())?,
                &destination,
                false,
            )?;
            prepared.push((item, destination, plan));
        }
        let total: u64 = prepared.iter().map(|(_, _, plan)| plan.restore_size).sum();

        let mut done = 0;
        for (item, destination, plan) in prepared {
            let size = plan.restore_size;
            let on_progress = on_progress.clone();
            let report = move |update: ProgressUpdate| {
                if update.kind == ProgressKind::Bytes {
                    on_progress(ProgressUpdate {
                        kind: ProgressKind::Bytes,
                        title: String::new(),
                        position: done + update.position.min(size),
                        length: total,
                        finished: false,
                    });
                }
            };
            self.with_progress(Arc::new(report), || {
                repo.restore(
                    plan,
                    &options,
                    repo.ls(&item.node, &LsOptions::default())?,
                    &destination,
                )
            })?;
            done += size;
        }

        on_progress(ProgressUpdate {
            kind: ProgressKind::Bytes,
            title: String::new(),
            position: total,
            length: total,
            finished: true,
        });
        Ok(())
    }

    /// Finds the requested nodes and checks their places in the target, before anything is written.
    fn resolve(
        &self,
        repo: &IndexedRepo,
        snapshot: &Snapshot,
        request: &RestoreRequest,
    ) -> Result<Vec<Item>> {
        request
            .items
            .iter()
            .map(|path| {
                // A disk image file restores as a file, but nothing inside it.
                let node = match self.locate(repo, snapshot, path)? {
                    Location::Node(node) => node,
                    Location::Image { node, inner, .. } if inner.components().next().is_none() => {
                        node
                    }
                    Location::Image { .. } => return Err(Error::RestoreFromImage),
                };
                let destination = request.target.join(node.name());
                check_target(&destination, &node)?;
                if node.is_dir() {
                    for entry in repo.ls(&node, &LsOptions::default())? {
                        let (path, node) = entry?;
                        check_target(&destination.join(path), &node)?;
                    }
                }
                Ok(Item { node, destination })
            })
            .collect()
    }
}

/// Fails if `path` holds something rustic can't restore `node` onto.
///
/// rustic_core replaces nothing of a different type without deleting everything else not in
/// the snapshot as well, and follows symlinks it finds: it then writes outside of the target,
/// skips the entry silently or panics.
fn check_target(path: &Path, node: &Node) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        meta => meta?,
    };
    let kind = meta.file_type();
    let fits = match &node.node_type {
        NodeType::Dir => kind.is_dir(),
        NodeType::File => kind.is_file(),
        NodeType::Symlink { .. } => {
            kind.is_symlink() && fs::read_link(path)? == node.node_type.to_link()
        }
        NodeType::Dev { .. } => kind.is_block_device(),
        NodeType::Chardev { .. } => kind.is_char_device(),
        NodeType::Fifo => kind.is_fifo(),
        NodeType::Socket => kind.is_socket(),
    };
    if fits {
        return Ok(());
    }
    let existing = if kind.is_symlink() {
        "a link"
    } else if kind.is_dir() {
        "a folder"
    } else if kind.is_file() {
        "a file"
    } else {
        "a special file"
    };
    Err(Error::TargetExists {
        path: path.to_path_buf(),
        existing,
    })
}

impl Item {
    fn local_destination(&self, create: bool) -> Result<LocalDestination> {
        Ok(LocalDestination::new(
            path_str(&self.destination)?,
            create,
            !self.node.is_dir(),
        )?)
    }
}

/// rustic takes destinations as `&str`.
fn path_str(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        Error::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("“{}” is not a valid UTF-8 path", path.display()),
        ))
    })
}
