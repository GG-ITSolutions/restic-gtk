//! Creates real repositories with rustic_core for the integration tests.

use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

use restic_gtk::backend::{RepoConfig, Session, Snapshot, jiff::Zoned};
use rustic_backend::BackendOptions;
use rustic_core::{
    BackupOptions, ConfigOptions, Credentials, KeyOptions, PathList, Repository, RepositoryOptions,
    SnapshotOptions,
};
use tempfile::TempDir;

pub const PASSWORD: &str = "correct horse battery staple";

/// A local repository with two snapshots of the same source, stored as `/data`:
///
/// ```text
/// /data/docs/readme.txt      "hello\n"
/// /data/docs/sub/deep.txt    "deep\n"
/// /data/empty/
/// /data/größe.txt            "unicode\n"
/// /data/link -> docs/readme.txt
/// ```
pub struct Fixture {
    pub dir: TempDir,
    pub config: RepoConfig,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        write(&source.join("docs/readme.txt"), "hello\n");
        write(&source.join("docs/sub/deep.txt"), "deep\n");
        fs::create_dir_all(source.join("empty")).unwrap();
        write(&source.join("größe.txt"), "unicode\n");
        symlink("docs/readme.txt", source.join("link")).unwrap();

        let location = dir.path().join("repo").to_str().unwrap().to_owned();
        let backends = BackendOptions::default()
            .repository(&location)
            .to_backends()
            .unwrap();
        let repo = Repository::new(&RepositoryOptions::default().no_cache(true), &backends)
            .unwrap()
            .init(
                &Credentials::password(PASSWORD),
                &KeyOptions::default(),
                &ConfigOptions::default(),
            )
            .unwrap()
            .to_indexed_ids()
            .unwrap();

        let paths = PathList::from_string(source.to_str().unwrap())
            .unwrap()
            .sanitize()
            .unwrap();
        let backup = BackupOptions::default().as_path(Some(PathBuf::from("/data")));
        for time in [
            "2026-01-01T10:00:00+01:00[+01:00]",
            "2026-02-01T10:00:00+01:00[+01:00]",
        ] {
            let mut snapshot = SnapshotOptions::default();
            snapshot.time = Some(time.parse::<Zoned>().unwrap());
            repo.backup(&backup, &paths, snapshot.to_snapshot().unwrap())
                .unwrap();
        }

        Self {
            config: RepoConfig::new("Test", location),
            dir,
        }
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.dir.path().join("cache")
    }

    pub fn open(&self) -> Session {
        Session::open(&self.config, PASSWORD, &self.cache_dir()).unwrap()
    }

    /// The newest snapshot.
    pub fn snapshot(&self, session: &Session) -> Snapshot {
        session.snapshots().unwrap().remove(0)
    }

    /// A fresh, empty folder inside the fixture.
    pub fn folder(&self, name: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

pub fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}
