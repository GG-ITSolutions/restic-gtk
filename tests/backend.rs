mod common;

use std::{
    fs,
    io::{Cursor, Read, Write},
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
            session.restore(&snapshot, &request, |_| {}).map(|_| ()),
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

// --- disk images ---------------------------------------------------------

const MIB: usize = 1 << 20;
const IMAGE: &str = "/data/disk.img";

/// GPT type GUIDs as stored on disk.
const EFI_TYPE: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];
const BASIC_DATA_TYPE: [u8; 16] = [
    0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7,
];
const LINUX_TYPE: [u8; 16] = [
    0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d, 0xe4,
];

/// The decompressed zstd fixture at `path`.
fn unzstd(path: &Path) -> Vec<u8> {
    let file = fs::File::open(path).unwrap();
    let mut data = Vec::new();
    ruzstd::decoding::StreamingDecoder::new(file)
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    data
}

/// The NTFS volume of `tests/fixtures/generate-ntfs.sh`.
fn ntfs_volume() -> Vec<u8> {
    volume_fixture("ntfs.img")
}

/// A volume checked in as `tests/fixtures/<name>.zst`.
fn volume_fixture(name: &str) -> Vec<u8> {
    unzstd(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(format!("{name}.zst")),
    )
}

/// A FAT volume with `/EFI/BOOT/hello.txt`.
fn fat_volume() -> Vec<u8> {
    let mut volume = Cursor::new(vec![0; 2 * MIB]);
    fatfs::format_volume(&mut volume, fatfs::FormatVolumeOptions::new()).unwrap();
    {
        let fs = fatfs::FileSystem::new(&mut volume, fatfs::FsOptions::new()).unwrap();
        let boot = fs
            .root_dir()
            .create_dir("EFI")
            .unwrap()
            .create_dir("BOOT")
            .unwrap();
        boot.create_file("hello.txt")
            .unwrap()
            .write_all(b"hello fat\n")
            .unwrap();
    }
    volume.into_inner()
}

/// A GPT disk: 1 EFI (FAT), 2 NTFS, 3 a Linux partition without a filesystem.
fn disk_image() -> Vec<u8> {
    gpt_disk(&[
        (fat_volume(), EFI_TYPE),
        (ntfs_volume(), BASIC_DATA_TYPE),
        (vec![0; MIB], LINUX_TYPE),
    ])
}

/// A GPT disk with a partition per volume, in 512 byte sectors starting at 1 MiB.
fn gpt_disk(partitions: &[(Vec<u8>, [u8; 16])]) -> Vec<u8> {
    let size = 2 * MIB + partitions.iter().map(|p| p.0.len()).sum::<usize>();
    let mut disk = Cursor::new(vec![0; size]);
    let mut gpt = gptman::GPT::new_from(&mut disk, 512, [7; 16]).unwrap();
    let mut lba = 2048;
    for (number, (data, kind)) in (1..).zip(partitions) {
        let sectors = data.len() as u64 / 512;
        gpt[number] = gptman::GPTPartitionEntry {
            partition_type_guid: *kind,
            unique_partition_guid: [number as u8; 16],
            starting_lba: lba,
            ending_lba: lba + sectors - 1,
            attribute_bits: 0,
            partition_name: "".into(),
        };
        disk.get_mut()[lba as usize * 512..][..data.len()].copy_from_slice(data);
        lba += sectors;
    }
    gpt.write_into(&mut disk).unwrap();
    disk.into_inner()
}

fn image_fixture() -> Fixture {
    let disk = disk_image();
    Fixture::with_source(|source| fs::write(source.join("disk.img"), disk).unwrap())
}

fn names(entries: &[restic_gtk::backend::Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

#[test]
fn lists_the_partitions_of_a_disk_image() {
    let fixture = image_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let data = session.list_dir(&snapshot, Path::new("/data")).unwrap();
    assert_eq!(data[0].kind, EntryKind::DiskImage);
    assert!(!data[0].inside_image);

    let partitions = session.list_dir(&snapshot, Path::new(IMAGE)).unwrap();
    assert_eq!(
        names(&partitions),
        ["Partition 1 (EFI)", "Partition 2", "Partition 3"]
    );
    assert_eq!(partitions[0].kind, EntryKind::Partition);
    assert_eq!(partitions[1].kind, EntryKind::Partition);
    assert!(matches!(partitions[2].kind, EntryKind::Unavailable { .. }));
    assert_eq!(partitions[1].size, 4 * MIB as u64);
    assert!(partitions.iter().all(|p| p.inside_image));
    assert_eq!(partitions[1].path, Path::new(IMAGE).join("Partition 2"));
}

#[test]
fn browses_and_opens_files_on_ntfs() {
    let fixture = image_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let volume = Path::new(IMAGE).join("Partition 2");

    let root = session.list_dir(&snapshot, &volume).unwrap();
    assert_eq!(names(&root), ["docs", "empty", "big.bin", "link"]);
    assert!(root.iter().all(|e| e.inside_image && e.mtime.is_some()));
    assert_eq!(root[2].size, 300_000);
    assert_eq!(
        root[3].kind,
        EntryKind::Symlink {
            target: "docs".into()
        }
    );

    let docs = session.list_dir(&snapshot, &volume.join("docs")).unwrap();
    assert_eq!(names(&docs), ["sub", "größe.txt", "readme.txt"]);
    // Lookups ignore case, like Windows.
    let sub = session
        .list_dir(&snapshot, &volume.join("DOCS/Sub"))
        .unwrap();
    assert_eq!(names(&sub), ["deep.txt"]);

    let dir = fixture.dir.path().join("runtime");
    let extract = |path: &str| {
        let file = session
            .extract_file(&snapshot, &volume.join(path), &dir)
            .unwrap();
        assert_eq!(mode(&file), 0o400);
        fs::read(file).unwrap()
    };
    assert_eq!(extract("docs/größe.txt"), b"unicode\n");
    assert_eq!(extract("docs/sub/deep.txt"), b"deep\n");
    let big = extract("big.bin");
    assert!(
        big.iter()
            .enumerate()
            .all(|(i, &b)| b == (i * 7 % 251) as u8)
    );
    assert_eq!(big.len(), 300_000);
}

#[test]
fn browses_and_opens_files_on_fat() {
    let fixture = image_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let boot = Path::new(IMAGE).join("Partition 1 (EFI)/EFI/BOOT");

    let entries = session.list_dir(&snapshot, &boot).unwrap();
    assert_eq!(names(&entries), ["hello.txt"]);
    assert_eq!(entries[0].size, 10);

    let file = session
        .extract_file(&snapshot, &boot.join("hello.txt"), fixture.dir.path())
        .unwrap();
    assert_eq!(fs::read_to_string(file).unwrap(), "hello fat\n");
}

#[test]
fn reports_what_a_disk_image_cannot_show() {
    let fixture = image_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let image = Path::new(IMAGE);

    let result = session.list_dir(&snapshot, &image.join("Partition 3"));
    assert!(
        matches!(result, Err(Error::UnsupportedFilesystem(_))),
        "{result:?}"
    );
    let result = session.list_dir(&snapshot, &image.join("Partition 9"));
    assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    let result = session.list_dir(&snapshot, &image.join("Partition 2/missing"));
    assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    let result = session.list_dir(&snapshot, &image.join("Partition 2/big.bin"));
    assert!(matches!(result, Err(Error::NotADirectory(_))), "{result:?}");
    let result = session.extract_file(
        &snapshot,
        &image.join("Partition 2/docs"),
        fixture.dir.path(),
    );
    assert!(matches!(result, Err(Error::NotAFile(_))), "{result:?}");
    let result = session.extract_file(
        &snapshot,
        &image.join("Partition 2/link"),
        fixture.dir.path(),
    );
    assert!(
        matches!(result, Err(Error::UnsupportedFile { .. })),
        "{result:?}"
    );
}

#[test]
fn restores_a_disk_image_as_a_file() {
    let fixture = image_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let whole = RestoreRequest {
        items: vec![IMAGE.into()],
        target: fixture.folder("whole"),
    };
    session.restore(&snapshot, &whole, |_| {}).unwrap();
    assert_eq!(
        fs::read(whole.target.join("disk.img")).unwrap(),
        fs::read(fixture.dir.path().join("source/disk.img")).unwrap()
    );
}

#[test]
fn restores_folders_from_a_disk_image() {
    let fixture = image_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let volume = Path::new(IMAGE).join("Partition 2");
    let target = fixture.folder("target");
    let request = RestoreRequest {
        items: vec![volume.join("docs")],
        target: target.clone(),
    };

    let preview = session.preview_restore(&snapshot, &request).unwrap();
    assert_eq!((preview.files, preview.conflicts), (3, 0));
    assert_eq!(preview.bytes, 11 + 8 + 5);
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);

    let (updates, sink) = recorder();
    let summary = session.restore(&snapshot, &request, sink).unwrap();
    assert_eq!(summary.skipped, 0);
    let last = updates.lock().unwrap().last().cloned().unwrap();
    assert!(last.finished && last.position == preview.bytes);

    let docs = target.join("docs");
    assert_eq!(
        fs::read_to_string(docs.join("readme.txt")).unwrap(),
        "hello ntfs\n"
    );
    assert_eq!(
        fs::read_to_string(docs.join("größe.txt")).unwrap(),
        "unicode\n"
    );
    assert_eq!(
        fs::read_to_string(docs.join("sub/deep.txt")).unwrap(),
        "deep\n"
    );
    let listed = session.list_dir(&snapshot, &volume.join("docs")).unwrap();
    let readme = listed.iter().find(|e| e.name == "readme.txt").unwrap();
    assert_eq!(
        fs::metadata(docs.join("readme.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        std::time::SystemTime::from(readme.mtime.unwrap())
    );
    // Nothing half written left behind.
    assert_eq!(fs::read_dir(&docs).unwrap().count(), 3);

    // Unchanged files are kept, changed ones replaced after asking.
    let preview = session.preview_restore(&snapshot, &request).unwrap();
    assert_eq!((preview.files, preview.conflicts), (0, 0));
    write(&docs.join("readme.txt"), "mine\n");
    let preview = session.preview_restore(&snapshot, &request).unwrap();
    assert_eq!((preview.files, preview.conflicts), (1, 1));
    session.restore(&snapshot, &request, |_| {}).unwrap();
    assert_eq!(
        fs::read_to_string(docs.join("readme.txt")).unwrap(),
        "hello ntfs\n"
    );
}

#[test]
fn restores_files_and_partitions_from_a_disk_image() {
    let fixture = image_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let target = fixture.folder("target");
    let request = RestoreRequest {
        items: vec![
            Path::new(IMAGE).join("Partition 2/big.bin"),
            Path::new(IMAGE).join("Partition 1 (EFI)"),
        ],
        target: target.clone(),
    };
    let summary = session.restore(&snapshot, &request, |_| {}).unwrap();
    assert_eq!(summary.skipped, 0);
    let big = fs::read(target.join("big.bin")).unwrap();
    assert!(
        big.len() == 300_000
            && big
                .iter()
                .enumerate()
                .all(|(i, b)| *b == (i * 7 % 251) as u8)
    );
    assert_eq!(
        fs::read_to_string(target.join("Partition 1 (EFI)/EFI/BOOT/hello.txt")).unwrap(),
        "hello fat\n"
    );

    // A whole NTFS volume: links are left out.
    let request = RestoreRequest {
        items: vec![Path::new(IMAGE).join("Partition 2")],
        target: target.clone(),
    };
    assert_eq!(
        session
            .preview_restore(&snapshot, &request)
            .unwrap()
            .skipped,
        1
    );
    let summary = session.restore(&snapshot, &request, |_| {}).unwrap();
    assert_eq!(summary.skipped, 1);
    let volume = target.join("Partition 2");
    assert!(volume.join("empty").is_dir());
    assert!(volume.join("docs/sub/deep.txt").is_file());
    assert!(fs::symlink_metadata(volume.join("link")).is_err());
}

#[test]
fn restore_from_a_disk_image_checks_the_target_first() {
    let fixture = image_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let target = fixture.folder("target");
    write(&target.join("docs/sub"), "mine\n");
    let request = RestoreRequest {
        items: vec![Path::new(IMAGE).join("Partition 2/docs")],
        target: target.clone(),
    };
    for result in [
        session.preview_restore(&snapshot, &request).map(|_| ()),
        session.restore(&snapshot, &request, |_| {}).map(|_| ()),
    ] {
        match result {
            Err(Error::TargetExists { path, existing }) => {
                assert_eq!((path, existing), (target.join("docs/sub"), "a file"));
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(!target.join("docs/readme.txt").exists());

    let unavailable = RestoreRequest {
        items: vec![Path::new(IMAGE).join("Partition 3")],
        target: fixture.folder("other"),
    };
    let result = session.restore(&snapshot, &unavailable, |_| {});
    assert!(
        matches!(result, Err(Error::UnsupportedFilesystem(_))),
        "{result:?}"
    );
}

// VM disk formats: the NTFS volume in qcow2, VHD, VHDX and VMDK containers.

/// A VM disk of `tests/fixtures/generate-containers.sh`.
fn container(name: &str) -> Vec<u8> {
    unzstd(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/containers")
            .join(format!("{name}.zst")),
    )
}

/// A snapshot whose `/data` holds `files`.
fn with_files(files: &[(&str, Vec<u8>)]) -> Fixture {
    Fixture::with_source(|source| {
        for (name, data) in files {
            fs::write(source.join(name), data).unwrap();
        }
    })
}

fn vmdk_descriptor(create_type: &str, parent_cid: &str, extents: &[&str]) -> Vec<u8> {
    format!(
        "# Disk DescriptorFile\nversion=1\nCID=12345678\nparentCID={parent_cid}\n\
         createType=\"{create_type}\"\n\n# Extent description\n{}\n\n# The Disk Data Base\n#DDB\n",
        extents.join("\n")
    )
    .into_bytes()
}

/// Browses `/data/<image>` down to its files and reads them whole.
fn check_ntfs_image(fixture: &Fixture, session: &Session, image: &str) {
    let snapshot = fixture.snapshot(session);
    let image = Path::new("/data").join(image);
    let partitions = session.list_dir(&snapshot, &image).unwrap();
    assert_eq!(names(&partitions), ["Partition 1"], "{image:?}");
    let volume = image.join("Partition 1");
    let root = session.list_dir(&snapshot, &volume).unwrap();
    assert_eq!(
        names(&root),
        ["docs", "empty", "big.bin", "link"],
        "{image:?}"
    );
    assert_eq!(root[2].size, 300_000);

    let dir = fixture.dir.path().join("runtime");
    let readme = session
        .extract_file(&snapshot, &volume.join("docs/readme.txt"), &dir)
        .unwrap();
    assert_eq!(fs::read(readme).unwrap(), b"hello ntfs\n");
    let big = session
        .extract_file(&snapshot, &volume.join("big.bin"), &dir)
        .unwrap();
    let big = fs::read(big).unwrap();
    assert!(
        big.len() == 300_000
            && big
                .iter()
                .enumerate()
                .all(|(i, b)| *b == (i * 7 % 251) as u8),
        "{image:?}"
    );
}

#[test]
fn browses_every_container_format() {
    let images = [
        "plain.qcow2",
        "zlib.qcow2",
        "zstd.qcow2",
        "fixed.vhd",
        "dynamic.vhd",
        "fixed.vhdx",
        "dynamic.vhdx",
        "sparse.vmdk",
        "stream.vmdk",
    ];
    let files: Vec<(&str, Vec<u8>)> = images.iter().map(|name| (*name, container(name))).collect();
    let fixture = with_files(&files);
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let data = session.list_dir(&snapshot, Path::new("/data")).unwrap();
    assert!(
        data.iter().all(|e| e.kind == EntryKind::DiskImage),
        "{data:?}"
    );
    for image in images {
        check_ntfs_image(&fixture, &session, image);
    }
}

#[test]
fn browses_vmdk_descriptors() {
    let ntfs = ntfs_volume();
    let (first, second) = ntfs.split_at(ntfs.len() / 2);
    let fixture = with_files(&[
        (
            "flat.vmdk",
            vmdk_descriptor(
                "monolithicFlat",
                "ffffffff",
                &["RW 8192 FLAT \"flat-flat.vmdk\" 0"],
            ),
        ),
        ("flat-flat.vmdk", ntfs.clone()),
        (
            "split.vmdk",
            vmdk_descriptor(
                "twoGbMaxExtentFlat",
                "ffffffff",
                &[
                    "RW 4096 FLAT \"split-f001.vmdk\" 0",
                    "RW 4096 FLAT \"split-f002.vmdk\" 0",
                ],
            ),
        ),
        ("split-f001.vmdk", first.to_vec()),
        ("split-f002.vmdk", second.to_vec()),
        (
            "sparse.vmdk",
            vmdk_descriptor(
                "twoGbMaxExtentSparse",
                "ffffffff",
                &["RW 8192 SPARSE \"sparse-s001.vmdk\""],
            ),
        ),
        ("sparse-s001.vmdk", container("sparse.vmdk")),
    ]);
    let session = fixture.open();
    for image in ["flat.vmdk", "split.vmdk", "sparse.vmdk"] {
        check_ntfs_image(&fixture, &session, image);
    }
}

#[test]
fn refuses_images_that_need_a_parent() {
    let mut vhd = container("dynamic.vhd");
    let len = vhd.len();
    for footer in [0, len - 512] {
        vhd[footer + 60..footer + 64].copy_from_slice(&4u32.to_be_bytes());
    }
    let mut vhdx = container("dynamic.vhdx");
    vhdx[0x31_0004] |= 2;
    let descriptor =
        |create_type, parent_cid, extent| vmdk_descriptor(create_type, parent_cid, &[extent]);
    let cases = [
        ("backing.qcow2", container("backing.qcow2"), "backing file"),
        ("child.vmdk", container("child.vmdk"), "parent"),
        ("diff.vhd", vhd, "parent"),
        ("diff.vhdx", vhdx, "parent"),
        (
            "missing.vmdk",
            descriptor(
                "monolithicFlat",
                "ffffffff",
                "RW 8192 FLAT \"gone-flat.vmdk\" 0",
            ),
            "not in the snapshot",
        ),
        (
            "path.vmdk",
            descriptor(
                "monolithicFlat",
                "ffffffff",
                "RW 8192 FLAT \"../x-flat.vmdk\" 0",
            ),
            "path",
        ),
        (
            "type.vmdk",
            descriptor("vmfsSparse", "ffffffff", "RW 8192 SPARSE \"x.vmdk\""),
            "can't be browsed",
        ),
        (
            "snap.vmdk",
            descriptor(
                "monolithicFlat",
                "deadbeef",
                "RW 8192 FLAT \"x-flat.vmdk\" 0",
            ),
            "parent",
        ),
    ];
    let files: Vec<(&str, Vec<u8>)> = cases.iter().map(|c| (c.0, c.1.clone())).collect();
    let fixture = with_files(&files);
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let data = session.list_dir(&snapshot, Path::new("/data")).unwrap();
    assert!(
        data.iter().all(|e| e.kind == EntryKind::DiskImage),
        "{data:?}"
    );
    for (name, _, word) in &cases {
        let result = session.list_dir(&snapshot, &Path::new("/data").join(name));
        assert!(
            matches!(&result, Err(Error::Image(reason)) if reason.contains(word)),
            "{name}: {result:?}"
        );
    }
}

#[test]
fn restores_from_a_compressed_container() {
    let fixture = with_files(&[("disk.qcow2", container("zlib.qcow2"))]);
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let target = fixture.folder("target");
    let request = RestoreRequest {
        items: vec![PathBuf::from("/data/disk.qcow2/Partition 1/docs")],
        target: target.clone(),
    };
    let summary = session.restore(&snapshot, &request, |_| {}).unwrap();
    assert_eq!(summary.skipped, 0);
    assert_eq!(
        fs::read_to_string(target.join("docs/readme.txt")).unwrap(),
        "hello ntfs\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("docs/sub/deep.txt")).unwrap(),
        "deep\n"
    );
}

#[test]
fn recognizes_vm_disk_names() {
    use restic_gtk::backend::is_disk_image;
    for name in [
        "a.qcow2", "a.QCOW2", "a.qcow", "a.vhd", "a.vhdx", "a.vmdk", "/x/y.img",
    ] {
        assert!(is_disk_image(Path::new(name)), "{name}");
    }
    for name in ["a.avhdx", "a.txt", "a.vmdk.txt", "a"] {
        assert!(!is_disk_image(Path::new(name)), "{name}");
    }
}

// --- ext and LVM ------------------------------------------------------------

const LINUX_IMAGE: &str = "/data/linux.img";
const EXT2_IMAGE: &str = "/data/ext2.img";

/// A GPT disk with 1 the ext4 volume of `tests/fixtures/generate-ext.sh` and 2 the LVM physical
/// volume of `generate-lvm.sh`, plus the ext2 volume as an image without partition table.
fn linux_fixture() -> Fixture {
    let disk = gpt_disk(&[
        (volume_fixture("ext4.img"), LINUX_TYPE),
        (volume_fixture("lvm.img"), LINUX_TYPE),
    ]);
    Fixture::with_source(|source| {
        fs::write(source.join("linux.img"), disk).unwrap();
        fs::write(source.join("ext2.img"), volume_fixture("ext2.img")).unwrap();
    })
}

fn is_pattern(data: &[u8]) -> bool {
    data.len() == 300_000
        && data
            .iter()
            .enumerate()
            .all(|(i, &b)| b == (i * 7 % 251) as u8)
}

/// Checks the files of `generate-ext.sh` in the volume at `volume`, which is the same for
/// ext2, ext4 and the ext4 volume in LVM.
fn check_ext_files(fixture: &Fixture, volume: &Path) {
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let root = session.list_dir(&snapshot, volume).unwrap();
    assert_eq!(
        names(&root),
        [
            "docs",
            "empty",
            "lost+found",
            "big.bin",
            "hard.txt",
            "link",
            "sparse.bin"
        ]
    );
    let time = restic_gtk::backend::jiff::Timestamp::from_second(1_700_000_000).unwrap();
    assert!(
        root.iter()
            .all(|e| e.inside_image && e.mtime == Some(time) && e.path.starts_with(volume))
    );
    assert_eq!(root[0].kind, EntryKind::Dir);
    assert_eq!(root[3].size, 300_000);
    assert_eq!(root[4].size, 10);
    assert_eq!(
        root[5].kind,
        EntryKind::Symlink {
            target: "docs/readme.txt".into()
        }
    );
    assert_eq!(root[6].size, 200_000);

    let docs = session.list_dir(&snapshot, &volume.join("docs")).unwrap();
    assert_eq!(names(&docs), ["sub", "größe.txt", "readme.txt"]);
    let sub = session
        .list_dir(&snapshot, &volume.join("docs/sub"))
        .unwrap();
    assert_eq!(names(&sub), ["deep.txt"]);
    assert!(
        session
            .list_dir(&snapshot, &volume.join("empty"))
            .unwrap()
            .is_empty()
    );
    // Names are case-sensitive.
    let result = session.list_dir(&snapshot, &volume.join("DOCS"));
    assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");

    let dir = fixture.dir.path().join("runtime");
    let extract = |path: &str| {
        let file = session
            .extract_file(&snapshot, &volume.join(path), &dir)
            .unwrap();
        assert_eq!(mode(&file), 0o400);
        fs::read(file).unwrap()
    };
    assert_eq!(extract("docs/readme.txt"), b"hello ext\n");
    assert_eq!(extract("hard.txt"), b"hello ext\n");
    assert_eq!(extract("docs/größe.txt"), b"unicode\n");
    assert_eq!(extract("docs/sub/deep.txt"), b"deep\n");
    assert!(is_pattern(&extract("big.bin")));
    let sparse = extract("sparse.bin");
    assert_eq!(sparse.len(), 200_000);
    assert!(sparse[..199_996].iter().all(|&b| b == 0));
    assert_eq!(&sparse[199_996..], b"tail");

    let result = session.list_dir(&snapshot, &volume.join("big.bin"));
    assert!(matches!(result, Err(Error::NotADirectory(_))), "{result:?}");
    let result = session.list_dir(&snapshot, &volume.join("docs/missing"));
    assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    let result = session.extract_file(&snapshot, &volume.join("docs"), &dir);
    assert!(matches!(result, Err(Error::NotAFile(_))), "{result:?}");
    let result = session.extract_file(&snapshot, &volume.join("link"), &dir);
    assert!(
        matches!(&result, Err(Error::UnsupportedFile { reason, .. }) if reason.contains("docs/readme.txt")),
        "{result:?}"
    );
}

#[test]
fn browses_and_opens_files_on_ext4() {
    let fixture = linux_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let partitions = session.list_dir(&snapshot, Path::new(LINUX_IMAGE)).unwrap();
    assert_eq!(names(&partitions), ["Partition 1", "Partition 2 (LVM)"]);
    assert!(partitions.iter().all(|p| p.kind == EntryKind::Partition));
    check_ext_files(&fixture, &Path::new(LINUX_IMAGE).join("Partition 1"));
}

#[test]
fn browses_and_opens_files_on_ext2_without_partition_table() {
    let fixture = linux_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let partitions = session.list_dir(&snapshot, Path::new(EXT2_IMAGE)).unwrap();
    assert_eq!(names(&partitions), ["Partition 1"]);
    assert_eq!(partitions[0].size, 4 * MIB as u64);
    check_ext_files(&fixture, &Path::new(EXT2_IMAGE).join("Partition 1"));
}

#[test]
fn browses_logical_volumes() {
    let fixture = linux_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let pv = Path::new(LINUX_IMAGE).join("Partition 2 (LVM)");

    // Hidden volumes (the parts of the thin pool) are left out.
    let volumes = session.list_dir(&snapshot, &pv).unwrap();
    assert_eq!(
        names(&volumes),
        ["vg0-pool0", "vg0-root", "vg0-swap", "vg0-thin1", "vg0-wide"]
    );
    assert!(volumes.iter().all(|v| v.inside_image));
    assert_eq!(volumes[1].kind, EntryKind::Partition);
    assert_eq!(volumes[1].path, pv.join("vg0-root"));
    assert_eq!(volumes[1].size, 4 * MIB as u64);
    assert_eq!(
        volumes[1].mtime,
        Some(restic_gtk::backend::jiff::Timestamp::from_second(1_700_000_000).unwrap())
    );
    let reason = |entry: &restic_gtk::backend::Entry| match &entry.kind {
        EntryKind::Unavailable { reason } => reason.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(reason(&volumes[0]), "LVM thin pools can't be browsed");
    assert_eq!(reason(&volumes[2]), "No supported filesystem");
    assert_eq!(reason(&volumes[3]), "Thin LVM volumes can't be browsed yet");
    assert!(reason(&volumes[4]).starts_with("This kind of LVM volume can't be browsed yet"));
    assert_eq!(volumes[3].size, 8 * MIB as u64);

    check_ext_files(&fixture, &pv.join("vg0-root"));

    for name in ["vg0-pool0", "vg0-thin1", "vg0-wide", "vg0-swap"] {
        let result = session.list_dir(&snapshot, &pv.join(name));
        assert!(
            matches!(result, Err(Error::UnsupportedFilesystem(_))),
            "{name}: {result:?}"
        );
    }
    for name in ["vg0-missing", "vg0-pool0_tdata", "root"] {
        let result = session.list_dir(&snapshot, &pv.join(name));
        assert!(
            matches!(result, Err(Error::NotFound(_))),
            "{name}: {result:?}"
        );
    }
    let result = session.extract_file(&snapshot, &pv.join("vg0-root"), fixture.dir.path());
    assert!(matches!(result, Err(Error::NotAFile(_))), "{result:?}");
}

#[test]
fn restores_folders_and_volumes_from_a_logical_volume() {
    let fixture = linux_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let pv = Path::new(LINUX_IMAGE).join("Partition 2 (LVM)");

    let target = fixture.folder("docs");
    let request = RestoreRequest {
        items: vec![pv.join("vg0-root/docs")],
        target: target.clone(),
    };
    let preview = session.preview_restore(&snapshot, &request).unwrap();
    assert_eq!(
        (preview.files, preview.conflicts, preview.bytes),
        (3, 0, 23)
    );
    let summary = session.restore(&snapshot, &request, |_| {}).unwrap();
    assert_eq!(summary.skipped, 0);
    assert_eq!(
        fs::read_to_string(target.join("docs/sub/deep.txt")).unwrap(),
        "deep\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("docs/größe.txt")).unwrap(),
        "unicode\n"
    );

    // A whole volume: the link is left out.
    let target = fixture.folder("volume");
    let request = RestoreRequest {
        items: vec![pv.join("vg0-root")],
        target: target.clone(),
    };
    let summary = session.restore(&snapshot, &request, |_| {}).unwrap();
    assert_eq!(summary.skipped, 1);
    let volume = target.join("vg0-root");
    assert!(is_pattern(&fs::read(volume.join("big.bin")).unwrap()));
    assert_eq!(
        fs::metadata(volume.join("sparse.bin")).unwrap().len(),
        200_000
    );
    assert!(volume.join("empty").is_dir());
    assert!(fs::symlink_metadata(volume.join("link")).is_err());

    // The whole physical volume: the volumes that can't be read are left out too.
    let target = fixture.folder("pv");
    let request = RestoreRequest {
        items: vec![pv.clone()],
        target: target.clone(),
    };
    let summary = session.restore(&snapshot, &request, |_| {}).unwrap();
    assert_eq!(summary.skipped, 4 + 1);
    assert!(
        target
            .join("Partition 2 (LVM)/vg0-root/docs/readme.txt")
            .is_file()
    );

    let request = RestoreRequest {
        items: vec![pv.join("vg0-thin1")],
        target: fixture.folder("thin"),
    };
    let result = session.restore(&snapshot, &request, |_| {});
    assert!(
        matches!(result, Err(Error::UnsupportedFilesystem(_))),
        "{result:?}"
    );
}
