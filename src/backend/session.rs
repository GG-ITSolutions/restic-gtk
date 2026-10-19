use std::{
    cmp::Ordering,
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

use rustic_backend::BackendOptions;
use rustic_core::{
    Credentials, IndexedFullStatus, OpenStatus, Repository, RepositoryBackends, RepositoryOptions,
    repofile::Node,
};

use super::{
    Entry, Error, ProgressUpdate, RepoConfig, Result, Snapshot,
    progress::{ProgressCallback, ProgressRouter},
    s3,
};

pub(crate) type IndexedRepo = Repository<IndexedFullStatus>;

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

    /// The contents of the folder at `path`: folders first, then by name.
    pub fn list_dir(&self, snapshot: &Snapshot, path: &Path) -> Result<Vec<Entry>> {
        let repo = self.indexed()?;
        let node = self.node(&repo, snapshot, path)?;
        let subtree = node
            .subtree
            .filter(|_| node.is_dir())
            .ok_or_else(|| Error::NotADirectory(path.to_path_buf()))?;
        let mut entries: Vec<Entry> = repo
            .get_tree(&subtree)?
            .nodes
            .iter()
            .map(|node| Entry::from_node(path, node))
            .collect();
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
