use std::{
    fs::{self, File},
    io::{self, Write},
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use rustic_core::{
    LocalDestination, LsOptions, RestoreOptions,
    jiff::Timestamp,
    repofile::{Node, NodeType},
};

use super::{
    Entry, EntryKind, Error, ProgressKind, ProgressUpdate, Result, Session, Snapshot,
    session::{IndexedRepo, Location},
};

/// A file from a disk image is written under this name in its destination's folder, then
/// renamed; the name itself may be up to 255 bytes, so no suffix fits behind it.
const PART_NAME: &str = ".restic-gtk.part";

/// Bytes between two progress reports while writing a file from a disk image.
const REPORT_EVERY: u64 = 256 * 1024;

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
    /// Links inside disk images, which aren't restored.
    pub skipped: u64,
}

/// What a finished restore left out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreSummary {
    /// Links and files inside disk images that couldn't be restored, e.g. encrypted ones.
    pub skipped: u64,
}

/// One requested entry, resolved to what it restores.
#[allow(clippy::large_enum_variant)] // Short-lived.
enum Item {
    /// An entry of the snapshot, restored by rustic to `destination`.
    Node { node: Node, destination: PathBuf },
    /// A file or folder inside a disk image.
    Image(ImagePlan),
}

/// Everything to write for one entry inside a disk image.
struct ImagePlan {
    /// The image file and the files next to it.
    node: Node,
    path: PathBuf,
    siblings: Vec<Node>,
    /// Folders to create, parents first.
    dirs: Vec<PathBuf>,
    files: Vec<ImageFile>,
    /// Links, not restored.
    skipped: u64,
}

struct ImageFile {
    /// The path inside the image.
    inner: PathBuf,
    destination: PathBuf,
    size: u64,
    mtime: Option<Timestamp>,
    state: FileState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileState {
    New,
    /// Exists with another size or time, is replaced.
    Changed,
    /// Exists with the same size and time, is kept.
    Unchanged,
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
            match item {
                Item::Node { node, destination } => {
                    let plan = repo.prepare_restore(
                        &options,
                        repo.ls(&node, &LsOptions::default())?,
                        &local_destination(&destination, &node, false)?,
                        true,
                    )?;
                    preview.files += plan.stats.files.restore;
                    preview.bytes += plan.restore_size;
                    preview.conflicts += plan.stats.files.modify;
                }
                Item::Image(plan) => {
                    for file in plan.files_to_write() {
                        preview.files += 1;
                        preview.bytes += file.size;
                        preview.conflicts += u64::from(file.state == FileState::Changed);
                    }
                    preview.skipped += plan.skipped;
                }
            }
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
    ) -> Result<RestoreSummary> {
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
            prepared.push(match item {
                Item::Node { node, destination } => {
                    let destination = local_destination(&destination, &node, true)?;
                    let plan = repo.prepare_restore(
                        &options,
                        repo.ls(&node, &LsOptions::default())?,
                        &destination,
                        false,
                    )?;
                    let size = plan.restore_size;
                    (Prepared::Node(node, destination, plan), size)
                }
                Item::Image(plan) => {
                    let size = plan.files_to_write().map(|f| f.size).sum();
                    (Prepared::Image(plan), size)
                }
            });
        }
        let total: u64 = prepared.iter().map(|(_, size)| size).sum();
        let bytes = move |position: u64| {
            on_progress(ProgressUpdate {
                kind: ProgressKind::Bytes,
                title: String::new(),
                position,
                length: total,
                finished: position == total,
            })
        };
        let bytes = Arc::new(bytes);

        let mut summary = RestoreSummary::default();
        let mut done = 0;
        for (prepared, size) in prepared {
            let report = {
                let bytes = bytes.clone();
                move |position: u64| bytes(done + position.min(size))
            };
            match prepared {
                Prepared::Node(node, destination, plan) => {
                    let report = move |update: ProgressUpdate| {
                        if update.kind == ProgressKind::Bytes {
                            report(update.position);
                        }
                    };
                    self.with_progress(Arc::new(report), || {
                        repo.restore(
                            plan,
                            &options,
                            repo.ls(&node, &LsOptions::default())?,
                            &destination,
                        )
                    })?;
                }
                Prepared::Image(plan) => {
                    summary.skipped += self.restore_image(&repo, snapshot, &plan, report)?;
                }
            }
            done += size;
        }

        bytes(total);
        Ok(summary)
    }

    /// Writes the files of `plan`; returns how many were skipped.
    fn restore_image(
        &self,
        repo: &Arc<IndexedRepo>,
        snapshot: &Snapshot,
        plan: &ImagePlan,
        report: impl Fn(u64),
    ) -> Result<u64> {
        for dir in &plan.dirs {
            fs::create_dir_all(dir)?;
        }
        let image = self.image_ref(
            repo,
            snapshot,
            &plan.node,
            &plan.path,
            plan.siblings.clone(),
        );
        let mut skipped = plan.skipped;
        let mut done = 0;
        for file in plan.files_to_write() {
            // Files are written one after the other, so one temp name per folder is enough.
            let part = file.destination.with_file_name(PART_NAME);
            let written = (|| {
                // Never through a link someone left at this name.
                match fs::remove_file(&part) {
                    Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
                    _ => {}
                }
                let out = File::options().write(true).create_new(true).open(&part)?;
                let mut counting = CountingWriter {
                    out: io::BufWriter::new(out),
                    written: 0,
                    reported: 0,
                    report: |written: u64| report(done + written.min(file.size)),
                };
                self.images.write_file(&image, &file.inner, &mut counting)?;
                let out = counting.out.into_inner().map_err(|err| err.into_error())?;
                if let Some(mtime) = file.mtime {
                    out.set_modified(SystemTime::from(mtime))?;
                }
                drop(out);
                fs::rename(&part, &file.destination)?;
                Ok(())
            })();
            match written {
                Ok(()) => {}
                Err(err) => {
                    let _ = fs::remove_file(&part);
                    match err {
                        // Encrypted, deduplicated or stored elsewhere: leave it out.
                        Error::UnsupportedFile { .. } => skipped += 1,
                        err => return Err(err),
                    }
                }
            }
            done += file.size;
            report(done);
        }
        Ok(skipped)
    }

    /// Finds the requested entries and checks their places in the target, before anything is written.
    fn resolve(
        &self,
        repo: &Arc<IndexedRepo>,
        snapshot: &Snapshot,
        request: &RestoreRequest,
    ) -> Result<Vec<Item>> {
        request
            .items
            .iter()
            .map(|path| {
                let node = match self.locate(repo, snapshot, path)? {
                    Location::Node(node) => node,
                    // A disk image file restores as a file.
                    Location::Image { node, inner, .. } if inner.components().next().is_none() => {
                        node
                    }
                    Location::Image {
                        node,
                        path: image_path,
                        inner,
                        siblings,
                    } => {
                        return self
                            .plan_image(
                                repo,
                                snapshot,
                                ImagePlan {
                                    node,
                                    path: image_path,
                                    siblings,
                                    dirs: Vec::new(),
                                    files: Vec::new(),
                                    skipped: 0,
                                },
                                &inner,
                                &request.target,
                            )
                            .map(Item::Image);
                    }
                };
                let destination = request.target.join(node.name());
                check_target(&destination, &node)?;
                if node.is_dir() {
                    for entry in repo.ls(&node, &LsOptions::default())? {
                        let (path, node) = entry?;
                        check_target(&destination.join(path), &node)?;
                    }
                }
                Ok(Item::Node { node, destination })
            })
            .collect()
    }

    /// Fills `plan` with everything below `inner` in the image, checking the target on the way.
    fn plan_image(
        &self,
        repo: &Arc<IndexedRepo>,
        snapshot: &Snapshot,
        mut plan: ImagePlan,
        inner: &Path,
        target: &Path,
    ) -> Result<ImagePlan> {
        let image = self.image_ref(
            repo,
            snapshot,
            &plan.node,
            &plan.path,
            plan.siblings.clone(),
        );
        let shown = plan.path.join(inner);
        // The parent's listing has the size and time of the entry.
        let entry = self
            .images
            .list(&image, inner.parent().unwrap_or(Path::new("")))?
            .into_iter()
            .find(|e| e.path.file_name() == inner.file_name())
            .ok_or_else(|| Error::NotFound(shown.clone()))?;
        if let EntryKind::Unavailable { reason } = entry.kind {
            return Err(Error::UnsupportedFilesystem(reason));
        }
        let destination = target.join(entry.path.file_name().ok_or(Error::NotFound(shown))?);

        let top = entry.path.clone();
        let mut pending = vec![(entry, destination)];
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        let mut skipped = 0;
        while let Some((entry, destination)) = pending.pop() {
            let inner = entry
                .path
                .strip_prefix(&plan.path)
                .expect("entries of an image are below it")
                .to_path_buf();
            match entry.kind {
                EntryKind::Dir | EntryKind::Partition => {
                    check_existing(&destination, |kind| Ok(kind.is_dir()))?;
                    let children = match self.images.list(&image, &inner) {
                        // An encrypted folder below the picked entry is left out like a link.
                        Err(Error::UnsupportedFile { .. }) if entry.path != top => {
                            skipped += 1;
                            continue;
                        }
                        children => children?,
                    };
                    for child in children {
                        let name = child.path.file_name().unwrap_or_default().to_owned();
                        pending.push((child, destination.join(name)));
                    }
                    dirs.push(destination);
                }
                EntryKind::File | EntryKind::DiskImage => {
                    let existing = check_existing(&destination, |kind| Ok(kind.is_file()))?;
                    files.push(ImageFile {
                        state: file_state(existing.as_ref(), &entry),
                        inner,
                        destination,
                        size: entry.size,
                        mtime: entry.mtime,
                    });
                }
                // Links point to paths inside the image, which mean nothing here.
                EntryKind::Symlink { .. } | EntryKind::Special | EntryKind::Unavailable { .. } => {
                    skipped += 1
                }
            }
        }
        plan.dirs = dirs;
        plan.files = files;
        plan.skipped = skipped;
        Ok(plan)
    }
}

impl ImagePlan {
    fn files_to_write(&self) -> impl Iterator<Item = &ImageFile> {
        self.files
            .iter()
            .filter(|f| f.state != FileState::Unchanged)
    }
}

/// How a file from an image relates to what is at its place in the target.
fn file_state(existing: Option<&fs::Metadata>, entry: &Entry) -> FileState {
    let Some(existing) = existing else {
        return FileState::New;
    };
    let same_time = entry
        .mtime
        .is_some_and(|mtime| existing.modified().ok() == Some(SystemTime::from(mtime)));
    if same_time && existing.len() == entry.size {
        FileState::Unchanged
    } else {
        FileState::Changed
    }
}

/// A restore prepared to run.
#[allow(clippy::large_enum_variant)] // Short-lived.
enum Prepared {
    Node(Node, LocalDestination, rustic_core::RestorePlan),
    Image(ImagePlan),
}

/// Counts what goes through and reports it every [`REPORT_EVERY`] bytes.
struct CountingWriter<W, F> {
    out: W,
    written: u64,
    reported: u64,
    report: F,
}

impl<W: Write, F: Fn(u64)> Write for CountingWriter<W, F> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.out.write(buf)?;
        self.written += n as u64;
        if self.written - self.reported >= REPORT_EVERY {
            self.reported = self.written;
            (self.report)(self.written);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Fails if `path` holds something rustic can't restore `node` onto.
///
/// rustic_core replaces nothing of a different type without deleting everything else not in
/// the snapshot as well, and follows symlinks it finds: it then writes outside of the target,
/// skips the entry silently or panics.
fn check_target(path: &Path, node: &Node) -> Result<()> {
    check_existing(path, |kind| {
        Ok(match &node.node_type {
            NodeType::Dir => kind.is_dir(),
            NodeType::File => kind.is_file(),
            NodeType::Symlink { .. } => {
                kind.is_symlink() && fs::read_link(path)? == node.node_type.to_link()
            }
            NodeType::Dev { .. } => kind.is_block_device(),
            NodeType::Chardev { .. } => kind.is_char_device(),
            NodeType::Fifo => kind.is_fifo(),
            NodeType::Socket => kind.is_socket(),
        })
    })
    .map(|_| ())
}

/// Fails if `path` holds something that doesn't `fit`; returns what is there, if anything.
fn check_existing(
    path: &Path,
    fits: impl FnOnce(&fs::FileType) -> io::Result<bool>,
) -> Result<Option<fs::Metadata>> {
    let meta = match fs::symlink_metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        meta => meta?,
    };
    let kind = meta.file_type();
    if fits(&kind)? {
        return Ok(Some(meta));
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

fn local_destination(path: &Path, node: &Node, create: bool) -> Result<LocalDestination> {
    Ok(LocalDestination::new(
        path_str(path)?,
        create,
        !node.is_dir(),
    )?)
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
