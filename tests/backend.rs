mod common;

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use common::{Fixture, write};
use restic_gtk::backend::{
    EntryKind, Error, ProgressKind, ProgressUpdate, RepoConfig, RepoStore, RestoreRequest,
    S3_LOCATION, S3Location, Session, validate_location,
};

/// An S3-compatible repository as entered in the dialog.
fn s3_location() -> S3Location {
    S3Location {
        endpoint: "http://127.0.0.1:8333".into(),
        bucket: "backups".into(),
        root: "host/repo".into(),
        access_key_id: "KEY".into(),
        ..S3Location::default()
    }
}

fn s3_config() -> RepoConfig {
    RepoConfig {
        options: s3_location().to_options(),
        ..RepoConfig::new("S3", S3_LOCATION)
    }
}

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
    let mut store = RepoStore::new(&dir.path().join("config"));
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
fn store_keeps_s3_options_without_secret() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = RepoStore::new(dir.path());
    let repos = vec![s3_config()];
    store.save(&repos).unwrap();

    assert_eq!(store.load().unwrap(), repos);
    let text = fs::read_to_string(store.path()).unwrap();
    assert!(text.contains("\"bucket\": \"backups\""), "{text}");
    assert!(!text.contains("secret"), "{text}");
}

#[test]
fn version_1_store_loads_without_options() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = RepoStore::new(dir.path());
    fs::write(
        store.path(),
        r#"{"version": 1, "repositories": [
            {"id": "6f3f2b0e-4a8e-4c1e-9d2a-3b5c7d9e1f20", "name": "Local", "location": "/srv/restic"}
        ]}"#,
    )
    .unwrap();

    let repos = store.load().unwrap();
    assert_eq!(repos[0].location, "/srv/restic");
    assert!(repos[0].options.is_empty());
}

#[test]
fn invalid_store_is_moved_aside() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = RepoStore::new(dir.path());
    fs::write(store.path(), "not json").unwrap();

    assert!(matches!(store.load(), Err(Error::Store(_))));
    assert!(!store.path().exists());
    assert_eq!(fs::read_to_string(store.broken_path()).unwrap(), "not json");

    let repos = vec![RepoConfig::new("Local", "/srv/restic")];
    store.save(&repos).unwrap();
    assert_eq!(store.load().unwrap(), repos);
}

#[test]
fn unreadable_store_is_kept_and_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = RepoStore::new(dir.path());
    // A folder fails to read even as root, unlike missing permissions.
    fs::create_dir(store.path()).unwrap();

    assert!(matches!(store.load(), Err(Error::Io(_))));
    assert!(store.path().is_dir());
    assert!(!store.broken_path().exists());

    let repos = vec![RepoConfig::new("Local", "/srv/restic")];
    assert!(matches!(store.save(&repos), Err(Error::StoreReadOnly)));
    assert!(store.path().is_dir());
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
fn display_location_of_s3_is_restic_url() {
    assert_eq!(
        s3_config().display_location(),
        "s3:http://127.0.0.1:8333/backups/host/repo"
    );
}

#[test]
fn s3_location_round_trips_through_options() {
    let options = s3_location().to_options();
    assert_eq!(options["root"], "/host/repo");
    // Never from the environment, which differs per user and shell.
    assert_eq!(options["region"], "auto");
    assert_eq!(options["disable_config_load"], "true");
    assert!(!options.contains_key("enable_virtual_host_style"));
    assert_eq!(S3Location::from_config(&s3_config()), Some(s3_location()));

    let with_style = S3Location {
        region: "fsn1".into(),
        virtual_host_style: true,
        endpoint: "https://fsn1.your-objectstorage.com".into(),
        ..s3_location()
    };
    let config = RepoConfig {
        options: with_style.to_options(),
        ..RepoConfig::new("S3", S3_LOCATION)
    };
    assert_eq!(S3Location::from_config(&config), Some(with_style));
    assert_eq!(S3Location::from_config(&RepoConfig::new("x", "/srv")), None);
}

#[test]
fn s3_region_comes_from_aws_endpoint() {
    let aws = S3Location {
        endpoint: "s3.eu-central-1.amazonaws.com".into(),
        ..s3_location()
    };
    let options = aws.to_options();
    assert_eq!(options["endpoint"], "https://s3.eu-central-1.amazonaws.com");
    assert_eq!(options["region"], "eu-central-1");
}

#[test]
fn validates_s3_locations() {
    let ok = [
        s3_location(),
        S3Location {
            endpoint: "fsn1.your-objectstorage.com/".into(),
            root: String::new(),
            ..s3_location()
        },
        S3Location {
            endpoint: "https://s3.amazonaws.com".into(),
            region: "eu-west-1".into(),
            ..s3_location()
        },
        S3Location {
            endpoint: "https://oss-cn-hangzhou.aliyuncs.com".into(),
            virtual_host_style: true,
            ..s3_location()
        },
    ];
    for location in ok {
        assert_eq!(location.validate(), Ok(()), "{location:?}");
    }
    let bad = [
        S3Location {
            bucket: " ".into(),
            ..s3_location()
        },
        S3Location {
            bucket: "backups/host".into(),
            ..s3_location()
        },
        S3Location {
            access_key_id: String::new(),
            ..s3_location()
        },
        S3Location {
            endpoint: "ftp://host".into(),
            ..s3_location()
        },
        S3Location {
            endpoint: "https://host/backups".into(),
            ..s3_location()
        },
        S3Location {
            endpoint: "https://s3.amazonaws.com".into(),
            ..s3_location()
        },
        S3Location {
            virtual_host_style: true,
            ..s3_location()
        },
        S3Location {
            endpoint: "https://host.example".into(),
            bucket: "my.backups".into(),
            virtual_host_style: true,
            ..s3_location()
        },
    ];
    for location in bad {
        assert!(location.validate().is_err(), "{location:?}");
    }
}

#[test]
fn parses_restic_s3_urls() {
    let parsed =
        S3Location::parse_restic("s3:https://s3.eu-central-1.amazonaws.com/bucket/a/b/").unwrap();
    assert_eq!(
        parsed,
        S3Location {
            endpoint: "https://s3.eu-central-1.amazonaws.com".into(),
            bucket: "bucket".into(),
            root: "a/b".into(),
            region: "eu-central-1".into(),
            ..S3Location::default()
        }
    );
    let parsed = S3Location::parse_restic(" s3:http://127.0.0.1:8333/testbucket/repo").unwrap();
    assert_eq!(parsed.endpoint, "http://127.0.0.1:8333");
    assert_eq!(parsed.root, "repo");
    let parsed = S3Location::parse_restic("s3://minio.example/backups").unwrap();
    assert_eq!(parsed.endpoint, "https://minio.example");
    assert_eq!(
        (parsed.bucket.as_str(), parsed.root.as_str()),
        ("backups", "")
    );
    let parsed = S3Location::parse_restic("s3:s3.amazonaws.com/backups/repo").unwrap();
    assert_eq!(parsed.endpoint, "https://s3.amazonaws.com");
    assert_eq!(parsed.region, "");

    for bad in [
        "s3:host",
        "s3:https://host/",
        "s3:",
        "rest:https://host/repo",
        "/srv",
    ] {
        assert_eq!(S3Location::parse_restic(bad), None, "{bad}");
    }
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
    let result = Session::open(&fixture.config, "wrong", None, &fixture.cache_dir());
    assert!(matches!(result, Err(Error::WrongPassword)), "{result:?}");
}

#[test]
fn backend_options_reach_the_backend() {
    let fixture = Fixture::new();
    // OpenDAL's file system service takes the folder only as option, like S3 takes the bucket.
    let config = RepoConfig {
        options: [("root".to_owned(), fixture.config.location.clone())].into(),
        ..RepoConfig::new("fs", "opendal:fs")
    };
    let session = Session::open(&config, common::PASSWORD, None, &fixture.cache_dir()).unwrap();
    assert_eq!(session.snapshots().unwrap().len(), 2);
}

#[test]
fn missing_repository_is_reported_as_such() {
    let fixture = Fixture::new();
    let missing = fixture
        .dir
        .path()
        .join("nothing")
        .to_str()
        .unwrap()
        .to_owned();
    let config = RepoConfig::new("gone", &missing);
    let result = Session::open(&config, common::PASSWORD, None, &fixture.cache_dir());
    assert!(matches!(result, Err(Error::NoRepository)), "{result:?}");

    // Through OpenDAL, like a wrong bucket or path on S3.
    let config = RepoConfig {
        options: [("root".to_owned(), missing)].into(),
        ..RepoConfig::new("gone", "opendal:fs")
    };
    let result = Session::open(&config, common::PASSWORD, None, &fixture.cache_dir());
    assert!(matches!(result, Err(Error::NoRepository)), "{result:?}");
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

#[test]
fn restore_refuses_entries_of_another_type_in_target() {
    let fixture = Fixture::new();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let outside = fixture.folder("outside");
    write(&outside.join("victim.txt"), "victim\n");

    type Setup = fn(&Path, &Path);
    let cases: [(&str, &str, &str, Setup); 7] = [
        ("/data/größe.txt", "größe.txt", "a folder", |t, _| {
            fs::create_dir(t.join("größe.txt")).unwrap()
        }),
        ("/data/docs", "docs", "a file", |t, _| {
            write(&t.join("docs"), "mine\n")
        }),
        ("/data/docs", "docs/sub", "a file", |t, _| {
            write(&t.join("docs/sub"), "mine\n")
        }),
        ("/data/docs", "docs/readme.txt", "a folder", |t, _| {
            fs::create_dir_all(t.join("docs/readme.txt")).unwrap()
        }),
        // rustic would follow these and write outside of the target.
        ("/data/docs", "docs", "a link", |t, o| {
            symlink(o, t.join("docs")).unwrap()
        }),
        ("/data/docs", "docs/readme.txt", "a link", |t, o| {
            fs::create_dir(t.join("docs")).unwrap();
            symlink(o.join("victim.txt"), t.join("docs/readme.txt")).unwrap()
        }),
        ("/data/link", "link", "a file", |t, _| {
            write(&t.join("link"), "mine\n")
        }),
    ];
    for (i, (item, clash, existing, setup)) in cases.into_iter().enumerate() {
        let target = fixture.folder(&format!("target-{i}"));
        setup(&target, &outside);
        let request = RestoreRequest {
            items: vec![item.into()],
            target: target.clone(),
        };
        for result in [
            session.preview_restore(&snapshot, &request).map(|_| ()),
            session.restore(&snapshot, &request, |_| {}),
        ] {
            match result {
                Err(Error::TargetExists { path, existing: e }) => {
                    assert_eq!(
                        (path, e),
                        (target.join(clash), existing),
                        "{item} onto {clash}"
                    )
                }
                other => panic!("{item} onto {clash}: {other:?}"),
            }
        }
    }
    assert_eq!(
        fs::read_to_string(outside.join("victim.txt")).unwrap(),
        "victim\n"
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
}

#[test]
fn restoring_the_same_entries_again_is_no_clash() {
    let fixture = Fixture::new();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let request = RestoreRequest {
        items: vec!["/data/docs".into(), "/data/link".into()],
        target: fixture.folder("target"),
    };
    session.restore(&snapshot, &request, |_| {}).unwrap();
    session.restore(&snapshot, &request, |_| {}).unwrap();
}
