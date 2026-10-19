use std::{
    cmp::Ordering,
    collections::BTreeMap,
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use rustic_backend::BackendOptions;
use rustic_core::{
    Credentials, IndexedFullStatus, OpenStatus, Repository, RepositoryBackends, RepositoryOptions,
    repofile::Node,
};

use super::{
    Entry, Error, ProgressUpdate, RepoConfig, Result, Snapshot,
    image::{self, ImageCache, ImageRef},
    progress::{ProgressCallback, ProgressRouter},
    s3,
};

pub(crate) type IndexedRepo = Repository<IndexedFullStatus>;

/// What a path in a snapshot refers to.
#[allow(clippy::large_enum_variant)] // Short-lived.
pub(crate) enum Location {
    /// A file or folder of the snapshot itself.
    Node(Node),
    /// A disk image file, or a path inside it.
    Image {
        node: Node,
        /// The path of the image file.
        path: PathBuf,
        /// The path inside the image, empty for the image itself.
        inner: PathBuf,
        /// A `system.json` next to the image, if there is one.
        system_json: Option<Node>,
    },
}

/// An unlocked repository.
///
/// Holds the decrypted key instead of the password, so callers can drop the
/// password right after [`Session::open`]. All methods block; share the session
/// between threads with an `Arc`.
#[derive(Debug)]
pub struct Session {
    pub(crate) repo: Repository<OpenStatus>,
    /// Holds an S3 secret key like `repo` holds the decrypted key, until the session is dropped.
    backends: RepositoryBackends,
    options: RepositoryOptions,
    /// Loaded on first use; browsing, extracting and restoring need it.
    indexed: Mutex<Option<Arc<IndexedRepo>>>,
    progress: ProgressRouter,
    /// Serializes operations that report progress, the router has one slot.
    progress_slot: Mutex<()>,
    pub(crate) images: ImageCache,
}

impl Session {
    /// Connects to the repository and decrypts its key.
    ///
    /// `secret_key` is the secret access key of an S3 repository, ignored for others.
    /// Fails with [`Error::WrongPassword`] if the password matches no key, with
    /// [`Error::AccessDenied`] if the storage refuses the credentials and with
    /// [`Error::NoRepository`] if there is no repository, e.g. a wrong bucket or path.
    pub fn open(
        config: &RepoConfig,
        password: &str,
        secret_key: Option<&str>,
        cache_dir: &Path,
    ) -> Result<Self> {
        let backends = BackendOptions::default()
            .repository(config.location.trim())
            .options(backend_options(config, secret_key))
            .to_backends()?;
        let options = RepositoryOptions::default().cache_dir(cache_dir);
        let progress = ProgressRouter::default();
        let repo = Repository::new_with_progress(&options, &backends, progress.clone())?;
        // rustic reports this as a generic configuration error.
        if repo.config_id()?.is_none() {
            return Err(Error::NoRepository);
        }
        let repo = repo.open(&Credentials::password(password))?;
        Ok(Self {
            repo,
            backends,
            options,
            indexed: Mutex::new(None),
            progress,
            progress_slot: Mutex::new(()),
            images: ImageCache::default(),
        })
    }

    /// All snapshots, newest first.
    pub fn snapshots(&self) -> Result<Vec<Snapshot>> {
        let mut snapshots: Vec<Snapshot> = self
            .repo
            .get_all_snapshots()?
            .into_iter()
            .map(Snapshot::from)
            .collect();
        snapshots.sort_by(|a, b| b.time.cmp(&a.time));
        Ok(snapshots)
    }

    pub fn is_indexed(&self) -> bool {
        self.indexed.lock().unwrap().is_some()
    }

    /// Loads the repository index unless it is already loaded.
    ///
    /// This is the slow step on large remote repositories; it happens once per session.
    pub fn load_index(
        &self,
        on_progress: impl Fn(ProgressUpdate) + Send + Sync + 'static,
    ) -> Result<()> {
        self.indexed_with_progress(Some(Arc::new(on_progress)))
            .map(drop)
    }

    pub(crate) fn indexed(&self) -> Result<Arc<IndexedRepo>> {
        self.indexed_with_progress(None)
    }

    fn indexed_with_progress(
        &self,
        on_progress: Option<ProgressCallback>,
    ) -> Result<Arc<IndexedRepo>> {
        // Held during loading, so concurrent callers wait instead of loading twice.
        let mut indexed = self.indexed.lock().unwrap();
        if let Some(repo) = indexed.as_ref() {
            return Ok(repo.clone());
        }
        let load = || self.reopen()?.to_indexed().map_err(Error::from);
        let repo = match on_progress {
            Some(callback) => self.with_progress(callback, load)?,
            None => load()?,
        };
        let repo = Arc::new(repo);
        *indexed = Some(repo.clone());
        Ok(repo)
    }

    /// A second handle to the repository; indexing consumes it.
    /// Uses the decrypted key, so this skips the slow password derivation.
    fn reopen(&self) -> Result<Repository<OpenStatus>> {
        Ok(
            Repository::new_with_progress(&self.options, &self.backends, self.progress.clone())?
                .open(&Credentials::Masterkey(self.repo.key()))?,
        )
    }

    /// Runs `f` with rustic's progress reported to `callback`.
    pub(crate) fn with_progress<T>(&self, callback: ProgressCallback, f: impl FnOnce() -> T) -> T {
        let _slot = self.progress_slot.lock().unwrap();
        let _attached = self.progress.attach(callback);
        f()
    }

    /// The node at `path` (absolute, inside the snapshot).
    pub(crate) fn node(
        &self,
        repo: &IndexedRepo,
        snapshot: &Snapshot,
        path: &Path,
    ) -> Result<Node> {
        Ok(repo.node_from_path(snapshot.tree, path)?)
    }

    /// Finds what `path` refers to, stopping at the first disk image on the way.
    pub(crate) fn locate(
        &self,
        repo: &IndexedRepo,
        snapshot: &Snapshot,
        path: &Path,
    ) -> Result<Location> {
        let mut tree = snapshot.tree;
        let mut walked = PathBuf::from("/");
        let mut components = path.components();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                continue;
            };
            walked.push(name);
            let nodes = repo.get_tree(&tree)?.nodes;
            let Some(node) = nodes.iter().find(|n| n.name() == name) else {
                return Err(Error::NotFound(walked));
            };
            if node.is_file() && image::is_image_name(name) {
                let system_json = nodes
                    .iter()
                    .find(|n| n.is_file() && n.name() == OsStr::new(image::SYSTEM_JSON))
                    .cloned();
                return Ok(Location::Image {
                    node: node.clone(),
                    path: walked,
                    inner: components.as_path().to_path_buf(),
                    system_json,
                });
            }
            match node.subtree {
                Some(subtree) if node.is_dir() => tree = subtree,
                _ if components.as_path().components().next().is_some() => {
                    return Err(Error::NotADirectory(walked));
                }
                _ => return Ok(Location::Node(node.clone())),
            }
        }
        // The root or a folder.
        Ok(Location::Node(self.node(repo, snapshot, path)?))
    }

    /// Lists or extracts inside a disk image found by [`Self::locate`].
    pub(crate) fn image_ref<'a>(
        &self,
        repo: &'a Arc<IndexedRepo>,
        snapshot: &Snapshot,
        node: &'a Node,
        path: &'a Path,
        system_json: Option<Node>,
    ) -> ImageRef<'a> {
        ImageRef {
            repo,
            tree: snapshot.tree.to_string(),
            node,
            path,
            system_json,
        }
    }

    /// The contents of the folder at `path`: folders first, then by name.
    ///
    /// Disk images are folders of their partitions here, see [`image`].
    pub fn list_dir(&self, snapshot: &Snapshot, path: &Path) -> Result<Vec<Entry>> {
        let repo = self.indexed()?;
        let mut entries: Vec<Entry> = match self.locate(&repo, snapshot, path)? {
            Location::Node(node) => {
                let subtree = node
                    .subtree
                    .filter(|_| node.is_dir())
                    .ok_or_else(|| Error::NotADirectory(path.to_path_buf()))?;
                repo.get_tree(&subtree)?
                    .nodes
                    .iter()
                    .map(|node| Entry::from_node(path, node))
                    .collect()
            }
            Location::Image {
                node,
                path: image_path,
                inner,
                system_json,
            } => {
                let image = self.image_ref(&repo, snapshot, &node, &image_path, system_json);
                self.images.list(&image, &inner)?
            }
        };
        entries.sort_by(compare_entries);
        Ok(entries)
    }
}

/// The stored options of `config`, plus the secret key for S3.
fn backend_options(config: &RepoConfig, secret_key: Option<&str>) -> BTreeMap<String, String> {
    let mut options = config.options.clone();
    if let Some(secret_key) = secret_key.filter(|_| config.location == s3::LOCATION) {
        options.insert(s3::SECRET_OPTION.to_owned(), secret_key.to_owned());
    }
    options
}

fn compare_entries(a: &Entry, b: &Entry) -> Ordering {
    b.is_dir()
        .cmp(&a.is_dir())
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_is_only_added_for_s3() {
        let mut config = RepoConfig::new("x", s3::LOCATION);
        config.options.insert("bucket".into(), "b".into());
        let options = backend_options(&config, Some("secret"));
        assert_eq!(options["bucket"], "b");
        assert_eq!(options[s3::SECRET_OPTION], "secret");
        assert!(!backend_options(&config, None).contains_key(s3::SECRET_OPTION));

        let local = RepoConfig::new("x", "/srv/restic");
        assert!(backend_options(&local, Some("secret")).is_empty());
    }
}
