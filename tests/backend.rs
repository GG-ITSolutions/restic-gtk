mod common;

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use common::{Fixture, write};
use restic_gtk::backend::{
    EntryKind, Error, ProgressKind, ProgressUpdate, RepoConfig, RepoStore, RestoreRequest, Session,
    validate_location,
};

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Collects progress updates for assertions.
fn recorder() -> (
    Arc<Mutex<Vec<ProgressUpdate>>>,
    impl Fn(ProgressUpdate) + Send + Sync + 'static,
) {
    let updates = Arc::new(Mutex::new(Vec::new()));
    let sink = updates.clone();
    (updates, move |update| sink.lock().unwrap().push(update))
}

// --- store ---------------------------------------------------------------

#[test]
fn store_round_trips_and_is_private() {
    let dir = tempfile::tempdir().unwrap();
    let store = RepoStore::new(&dir.path().join("config"));
    assert!(store.load().unwrap().is_empty());

    let repos = vec![
        RepoConfig::new("Server", "rest:https://user:secret@backup.example/host/"),
        RepoConfig::new("Local", "/srv/restic"),
    ];
    store.save(&repos).unwrap();

    assert_eq!(store.load().unwrap(), repos);
    assert_eq!(mode(store.path()), 0o600);
    assert_eq!(mode(store.path().parent().unwrap()), 0o700);
}

#[test]
fn display_location_hides_basic_auth_password() {
    let repo = RepoConfig::new("x", "rest:https://user:secret@backup.example/host/");
    assert_eq!(
        repo.display_location(),
        "rest:https://user:***@backup.example/host/"
    );
    let repo = RepoConfig::new("x", "rest:https://backup.example/host/");
    assert_eq!(repo.display_location(), "rest:https://backup.example/host/");
}

#[test]
fn validates_locations() {
    for ok in [
        "rest:https://backup.example/host/",
        "rest:http://user:pw@10.0.0.1:8000/repo",
        "/srv/restic",
        "local:/srv/a:b",
    ] {
        assert_eq!(validate_location(ok), Ok(()), "{ok}");
    }
    for bad in [
        "",
        "  ",
        "relative/path",
        "rest:",
        "rest:ftp://host/",
        "s3:bucket",
        "/srv/a:b",
    ] {
        assert!(validate_location(bad).is_err(), "{bad}");
    }
}

// --- open & snapshots ----------------------------------------------------

#[test]
fn wrong_password_is_reported_as_such() {
    let fixture = Fixture::new();
    let result = Session::open(&fixture.config, "wrong", &fixture.cache_dir());
    assert!(matches!(result, Err(Error::WrongPassword)), "{result:?}");
}

#[test]
fn missing_repository_is_an_error() {
    let fixture = Fixture::new();
    let config = RepoConfig::new("gone", fixture.dir.path().join("nothing").to_str().unwrap());
    let result = Session::open(&config, common::PASSWORD, &fixture.cache_dir());
    assert!(matches!(result, Err(Error::Repository(_))), "{result:?}");
}

#[test]
fn snapshots_are_newest_first() {
    let fixture = Fixture::new();
    let snapshots = fixture.open().snapshots().unwrap();

    assert_eq!(snapshots.len(), 2);
    assert!(snapshots[0].time > snapshots[1].time);
    assert_eq!(snapshots[0].paths, ["/data"]);
    assert_eq!(snapshots[0].id.len(), 64);
    assert_eq!(snapshots[0].short_id().len(), 8);
    assert!(snapshots[0].size().is_some_and(|size| size > 0));
}

// --- stats ---------------------------------------------------------------

#[test]
fn snapshots_carry_backup_summary() {
    let fixture = Fixture::new();
    let snapshots = fixture.open().snapshots().unwrap();
    let newest = snapshots[0].summary.as_ref().unwrap();
    let first = snapshots[1].summary.as_ref().unwrap();

    // readme.txt, deep.txt, größe.txt and the symlink.
    assert_eq!(first.total_files_processed, 4);
    assert_eq!(first.files_new, 4);
    assert!(first.data_added > 0);
    assert!(first.data_added_packed > 0);
    assert!(first.backup_end >= first.backup_start);
    assert!(snapshots[0].program_version.starts_with("rustic"));

    assert_eq!(newest.total_files_processed, 4);
    assert_eq!(newest.total_bytes_processed, first.total_bytes_processed);
}

#[test]
fn repo_stats_count_files_and_data() {
    let fixture = Fixture::new();
    let stats = fixture.open().repo_stats().unwrap();

    assert_eq!(stats.id.len(), 64);
    assert_eq!(stats.version, 2);
    assert_eq!(stats.snapshots, 2);
    assert!(stats.packs > 0);
    assert!(stats.raw_size > 0);
    assert!(stats.packed_size > 0);
    // Packs are only part of the repository files.
    assert!(stats.stored_size > stats.packed_size);
}

// --- browsing ------------------------------------------------------------

#[test]
fn lists_folders_first_with_kinds() {
    let fixture = Fixture::new();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let root = session.list_dir(&snapshot, Path::new("/")).unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].name, "data");
    assert_eq!(root[0].path, Path::new("/data"));

    let data = session.list_dir(&snapshot, Path::new("/data")).unwrap();
    let names: Vec<_> = data.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["docs", "empty", "größe.txt", "link"]);
    assert_eq!(data[2].kind, EntryKind::File);
    assert_eq!(data[2].size, 8);
    assert!(data[2].mtime.is_some());
    assert_eq!(
        data[3].kind,
        EntryKind::Symlink {
            target: PathBuf::from("docs/readme.txt")
        }
    );

    assert!(
        session
            .list_dir(&snapshot, Path::new("/data/empty"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn listing_a_file_fails() {
    let fixture = Fixture::new();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let result = session.list_dir(&snapshot, Path::new("/data/größe.txt"));
    assert!(matches!(result, Err(Error::NotADirectory(_))), "{result:?}");
}

#[test]
fn index_loads_once_with_progress() {
    let fixture = Fixture::new();
    let session = fixture.open();
    assert!(!session.is_indexed());

    let (updates, sink) = recorder();
    session.load_index(sink).unwrap();
    assert!(session.is_indexed());
    assert!(updates.lock().unwrap().iter().any(|u| u.finished));

    // Already loaded: no further progress.
    let (updates, sink) = recorder();
    session.load_index(sink).unwrap();
    assert!(updates.lock().unwrap().is_empty());
}

// --- extract -------------------------------------------------------------

#[test]
fn extracts_files_read_only() {
    let fixture = Fixture::new();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let dir = fixture.dir.path().join("runtime");

    let first = session
        .extract_file(&snapshot, Path::new("/data/docs/readme.txt"), &dir)
        .unwrap();
    let second = session
        .extract_file(&snapshot, Path::new("/data/docs/readme.txt"), &dir)
        .unwrap();

    assert_ne!(first, second);
    assert_eq!(first.file_name().unwrap(), "readme.txt");
    assert_eq!(fs::read_to_string(&first).unwrap(), "hello\n");
    assert_eq!(mode(&first), 0o400);
}

#[test]
fn extracting_a_folder_fails() {
    let fixture = Fixture::new();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let result = session.extract_file(&snapshot, Path::new("/data/docs"), fixture.dir.path());
    assert!(matches!(result, Err(Error::NotAFile(_))), "{result:?}");
}

// --- restore -------------------------------------------------------------

#[test]
fn restores_entries_by_name_into_target() {
    let fixture = Fixture::new();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let target = fixture.folder("target");
    let request = RestoreRequest {
        items: vec![
            "/data/docs".into(),
            "/data/größe.txt".into(),
            "/data/link".into(),
        ],
        target: target.clone(),
    };

    let preview = session.preview_restore(&snapshot, &request).unwrap();
    assert_eq!(preview.conflicts, 0);
    assert_eq!(preview.files, 3);
    assert_eq!(preview.bytes, 6 + 5 + 8);
    // The preview doesn't touch the target.
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);

    let (updates, sink) = recorder();
    session.restore(&snapshot, &request, sink).unwrap();

    assert_eq!(
        fs::read_to_string(target.join("docs/readme.txt")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("docs/sub/deep.txt")).unwrap(),
        "deep\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("größe.txt")).unwrap(),
        "unicode\n"
    );
    assert_eq!(
        fs::read_link(target.join("link")).unwrap(),
        Path::new("docs/readme.txt")
    );

    let updates = updates.lock().unwrap();
    assert_eq!(updates.first().unwrap().kind, ProgressKind::Activity);
    let last = updates.last().unwrap();
    assert!(last.finished);
    assert_eq!((last.position, last.length), (19, 19));
    assert!(
        updates
            .windows(2)
            .all(|w| { w[0].kind != ProgressKind::Bytes || w[1].position >= w[0].position })
    );
}

#[test]
fn preview_counts_files_that_would_be_overwritten() {
    let fixture = Fixture::new();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let target = fixture.folder("target");
    let request = RestoreRequest {
        items: vec!["/data/docs".into()],
        target: target.clone(),
    };
    session.restore(&snapshot, &request, |_| {}).unwrap();

    // Unchanged files are no conflict.
    assert_eq!(
        session
            .preview_restore(&snapshot, &request)
            .unwrap()
            .conflicts,
        0
    );

    write(&target.join("docs/readme.txt"), "changed locally\n");
    assert_eq!(
        session
            .preview_restore(&snapshot, &request)
            .unwrap()
            .conflicts,
        1
    );

    session.restore(&snapshot, &request, |_| {}).unwrap();
    assert_eq!(
        fs::read_to_string(target.join("docs/readme.txt")).unwrap(),
        "hello\n"
    );
}
