//! ZIP archives inside snapshots, against real repositories created in temp folders.

// Not every helper is needed here.
#[allow(dead_code)]
mod common;

use std::{
    fs,
    io::{Cursor, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use common::{Fixture, write};
use restic_gtk::backend::{EntryKind, Error, RestoreRequest};

const MIB: usize = 1 << 20;

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn names(entries: &[restic_gtk::backend::Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

const ZIP: &str = "/data/sample.zip";

/// `tests/fixtures/sample.zip`, see `generate-zip.sh` for what is in it.
fn sample_zip() -> Vec<u8> {
    fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.zip")).unwrap()
}

fn zip_fixture() -> Fixture {
    zip_fixture_with("sample.zip", sample_zip())
}

/// A ZIP archive made by the `zip` crate in memory: `(name, contents)`, all stored.
fn stored_zip(entries: impl IntoIterator<Item = (String, Vec<u8>)>) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, contents) in entries {
        zip.start_file(name, options).unwrap();
        zip.write_all(&contents).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn zip_fixture_with(name: &'static str, data: Vec<u8>) -> Fixture {
    Fixture::with_source(|source| fs::write(source.join(name), data).unwrap())
}

#[test]
fn lists_the_entries_of_a_zip_archive() {
    use restic_gtk::backend::jiff::{civil, tz::TimeZone};
    let local = |date: civil::Date, h, m, s| {
        date.at(h, m, s, 0)
            .to_zoned(TimeZone::system())
            .unwrap()
            .timestamp()
    };

    let fixture = zip_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let data = session.list_dir(&snapshot, Path::new("/data")).unwrap();
    assert_eq!(data[0].kind, EntryKind::ZipArchive);
    assert!(!data[0].inside_image);

    // Folders first. Nothing of ../evil.txt, /abs.txt and docs/../up.txt shows up.
    let root = session.list_dir(&snapshot, Path::new(ZIP)).unwrap();
    assert_eq!(
        names(&root),
        [
            "docs",
            "empty",
            "win",
            "big-stored.bin",
            "big.bin",
            "bzip2.txt",
            "café.txt",
            "größe.txt",
            "link",
            "lzma.txt",
            "naïve.txt",
            "top.txt",
            "zero.txt",
            "zstd.txt",
        ]
    );
    assert!(root.iter().all(|e| e.inside_image));
    let entry = |name: &str| root.iter().find(|e| e.name == name).unwrap();
    assert_eq!(entry("docs").kind, EntryKind::Dir);
    assert_eq!(entry("docs").path, Path::new(ZIP).join("docs"));
    // The explicit folder entry has the time, the implicit one none.
    assert_eq!(
        entry("docs").mtime,
        Some(local(civil::date(2024, 1, 2), 3, 4, 6))
    );
    assert_eq!(entry("win").mtime, None);
    assert_eq!(entry("top.txt").kind, EntryKind::File);
    assert_eq!(entry("top.txt").size, 4);
    assert_eq!(
        entry("top.txt").mtime,
        Some(local(civil::date(2024, 2, 3), 4, 5, 6))
    );
    assert_eq!(entry("big.bin").size, 300_000);
    assert_eq!(entry("zero.txt").size, 0);
    assert_eq!(entry("link").kind, EntryKind::Special);
    assert!(matches!(
        entry("zstd.txt").kind,
        EntryKind::Unavailable { .. }
    ));

    let docs = session
        .list_dir(&snapshot, &Path::new(ZIP).join("docs"))
        .unwrap();
    assert_eq!(names(&docs), ["sub", "readme.txt", "secret.txt"]);
    assert_eq!(docs[0].kind, EntryKind::Dir);
    assert_eq!(docs[1].size, 30);
    assert!(matches!(docs[2].kind, EntryKind::Unavailable { .. }));
    let sub = session
        .list_dir(&snapshot, &Path::new(ZIP).join("docs/sub"))
        .unwrap();
    assert_eq!(names(&sub), ["deep.txt"]);
    let windows = session
        .list_dir(&snapshot, &Path::new(ZIP).join("win/dir"))
        .unwrap();
    assert_eq!(names(&windows), ["file.txt"]);
    assert!(
        session
            .list_dir(&snapshot, &Path::new(ZIP).join("empty"))
            .unwrap()
            .is_empty()
    );

    // Names are found, not computed: nothing escapes the archive.
    for path in ["docs/readme.txt", "nope"] {
        let result = session.list_dir(&snapshot, &Path::new(ZIP).join(path));
        match (path, result) {
            ("docs/readme.txt", Err(Error::NotADirectory(_))) => {}
            ("nope", Err(Error::NotFound(_))) => {}
            (path, other) => panic!("{path}: {other:?}"),
        }
    }
    assert!(
        session
            .list_dir(&snapshot, &Path::new(ZIP).join("docs/../.."))
            .is_err()
    );
}

#[test]
fn opens_entries_of_a_zip_archive() {
    let fixture = zip_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let dir = fixture.dir.path().join("runtime");
    let extract = |path: &str| {
        let file = session
            .extract_file(&snapshot, &Path::new(ZIP).join(path), &dir)
            .unwrap();
        assert_eq!(mode(&file), 0o400);
        assert_eq!(
            file.file_name().unwrap(),
            Path::new(path).file_name().unwrap()
        );
        fs::read(file).unwrap()
    };
    assert_eq!(extract("docs/readme.txt"), b"hello zip\n".repeat(3)); // deflated
    assert_eq!(extract("docs/sub/deep.txt"), b"deep\n"); // stored
    assert_eq!(extract("größe.txt"), b"unicode\n");
    assert_eq!(extract("café.txt"), b"cp437\n");
    assert_eq!(extract("naïve.txt"), b"utf8\n");
    assert_eq!(extract("win/dir/file.txt"), b"win\n");
    assert_eq!(extract("zero.txt"), b"");
    assert_eq!(extract("bzip2.txt"), b"bzip2\n".repeat(100));
    assert_eq!(extract("lzma.txt"), b"lzma\n".repeat(100));
    let big = extract("big.bin");
    assert_eq!(big.len(), 300_000);
    assert!(
        big.iter()
            .enumerate()
            .all(|(i, &b)| b == (i * 7 % 251) as u8)
    );
    let big = extract("big-stored.bin");
    assert_eq!(big.len(), 300_000);
    assert!(
        big.iter()
            .enumerate()
            .all(|(i, &b)| b == (i * 11 % 241) as u8)
    );

    // What can't be opened says why; folders and missing names are no files.
    for path in ["docs/secret.txt", "zstd.txt"] {
        let result = session.extract_file(&snapshot, &Path::new(ZIP).join(path), &dir);
        assert!(
            matches!(result, Err(Error::UnsupportedFile { .. })),
            "{path}: {result:?}"
        );
    }
    for (path, folder) in [
        ("docs", true),
        ("link", false),
        ("nope", false),
        ("../evil.txt", false),
    ] {
        let result = session.extract_file(&snapshot, &Path::new(ZIP).join(path), &dir);
        match result {
            Err(Error::NotAFile(_)) if folder || path == "link" => {}
            Err(Error::NotFound(_)) if !folder => {}
            other => panic!("{path}: {other:?}"),
        }
    }
    // Nothing was left behind by the failures.
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 11);

    // The archive itself still extracts as a file.
    let file = session
        .extract_file(&snapshot, Path::new(ZIP), &dir)
        .unwrap();
    assert!(fs::read(file).unwrap() == sample_zip());
}

#[test]
fn restores_files_and_folders_from_a_zip_archive() {
    let fixture = zip_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);
    let target = fixture.folder("target");
    let request = RestoreRequest {
        items: vec![
            Path::new(ZIP).join("docs"),
            Path::new(ZIP).join("top.txt"),
            Path::new(ZIP).join("big.bin"),
        ],
        target: target.clone(),
    };
    let preview = session.preview_restore(&snapshot, &request).unwrap();
    assert_eq!((preview.files, preview.bytes), (4, 30 + 5 + 4 + 300_000));
    assert_eq!((preview.conflicts, preview.skipped), (0, 1));

    // The encrypted entry in docs is left out.
    let summary = session.restore(&snapshot, &request, |_| {}).unwrap();
    assert_eq!(summary.skipped, 1);
    assert!(!target.join("docs/secret.txt").exists());
    assert_eq!(
        fs::read(target.join("docs/readme.txt")).unwrap(),
        b"hello zip\n".repeat(3)
    );
    assert_eq!(
        fs::read(target.join("docs/sub/deep.txt")).unwrap(),
        b"deep\n"
    );
    assert_eq!(fs::read(target.join("big.bin")).unwrap().len(), 300_000);
    // The time of the entry, DOS times have two seconds.
    let top = fs::metadata(target.join("top.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let expected = restic_gtk::backend::jiff::civil::date(2024, 2, 3)
        .at(4, 5, 6, 0)
        .to_zoned(restic_gtk::backend::jiff::tz::TimeZone::system())
        .unwrap();
    assert_eq!(top, std::time::SystemTime::from(expected.timestamp()));
    assert!(fs::read_dir(&target).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".restic-gtk-part")
    }));

    // Everything on the top level that isn't marked unavailable, which the UI doesn't offer
    // either: the link and the encrypted entry in docs are left out, and nothing lands
    // outside of the target.
    let all = RestoreRequest {
        items: session
            .list_dir(&snapshot, Path::new(ZIP))
            .unwrap()
            .into_iter()
            .filter(|e| !matches!(e.kind, EntryKind::Unavailable { .. }))
            .map(|e| e.path)
            .collect(),
        target: fixture.folder("everything"),
    };
    let summary = session.restore(&snapshot, &all, |_| {}).unwrap();
    assert_eq!(summary.skipped, 2);
    let everything = fixture.dir.path().join("everything");
    assert!(everything.join("empty").is_dir());
    assert_eq!(fs::read(everything.join("café.txt")).unwrap(), b"cp437\n");
    assert_eq!(
        fs::read(everything.join("win/dir/file.txt")).unwrap(),
        b"win\n"
    );
    for outside in ["evil.txt", "abs.txt", "up.txt"] {
        assert!(!everything.join(outside).exists());
        assert!(!fixture.dir.path().join(outside).exists());
    }

    // The ZIP file itself restores as a file.
    let file = RestoreRequest {
        items: vec![PathBuf::from(ZIP)],
        target: fixture.folder("file"),
    };
    session.restore(&snapshot, &file, |_| {}).unwrap();
    assert!(fs::read(fixture.dir.path().join("file/sample.zip")).unwrap() == sample_zip());
}

#[test]
fn restore_from_a_zip_archive_checks_the_target_first() {
    let fixture = zip_fixture();
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let target = fixture.folder("target");
    write(&target.join("docs/sub"), "mine\n");
    let request = RestoreRequest {
        items: vec![Path::new(ZIP).join("docs")],
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
        items: vec![Path::new(ZIP).join("docs/secret.txt")],
        target: fixture.folder("other"),
    };
    let result = session.restore(&snapshot, &unavailable, |_| {});
    assert!(
        matches!(result, Err(Error::UnsupportedFilesystem(_))),
        "{result:?}"
    );
}

#[test]
fn a_file_named_zip_that_is_none_opens_as_a_plain_file() {
    let fixture = Fixture::with_source(|source| {
        fs::write(
            source.join("fake.zip"),
            b"this is not a ZIP archive, just text\n",
        )
        .unwrap();
        fs::write(source.join("tiny.zip"), b"PK").unwrap();
    });
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let data = session.list_dir(&snapshot, Path::new("/data")).unwrap();
    assert_eq!(names(&data), ["fake.zip", "tiny.zip"]);
    // Too small to be an archive: a file from the start.
    assert_eq!(data[0].kind, EntryKind::ZipArchive);
    assert_eq!(data[1].kind, EntryKind::File);

    let result = session.list_dir(&snapshot, Path::new("/data/fake.zip"));
    match result {
        Err(Error::NotAnArchive(message)) => assert!(message.contains("fake.zip"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        session.list_dir(&snapshot, Path::new("/data/tiny.zip")),
        Err(Error::NotADirectory(_))
    ));

    // What the UI falls back to.
    for (path, contents) in [
        (
            "/data/fake.zip",
            &b"this is not a ZIP archive, just text\n"[..],
        ),
        ("/data/tiny.zip", b"PK"),
    ] {
        let file = session
            .extract_file(&snapshot, Path::new(path), fixture.dir.path())
            .unwrap();
        assert_eq!(fs::read(file).unwrap(), contents);
    }
    assert!(
        session
            .extract_file(&snapshot, Path::new("/data/fake.zip/x"), fixture.dir.path())
            .is_err()
    );
}

#[test]
fn opens_entries_across_the_read_buffer_of_a_zip_archive() {
    // Entries of 1.5 and 2.5 MiB: the buffer holds 1 MiB, so reads refill it several times,
    // and the file list at the end is in another block than the start of the archive.
    let pattern =
        |len: usize, step: usize| -> Vec<u8> { (0..len).map(|i| (i * step % 251) as u8).collect() };
    let archive = stored_zip([
        ("pad.bin".to_owned(), pattern(3 * MIB / 2, 3)),
        ("mid/big.bin".to_owned(), pattern(5 * MIB / 2, 13)),
        ("tail.txt".to_owned(), b"tail\n".to_vec()),
    ]);
    let fixture = zip_fixture_with("big.zip", archive);
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let root = session
        .list_dir(&snapshot, Path::new("/data/big.zip"))
        .unwrap();
    assert_eq!(names(&root), ["mid", "pad.bin", "tail.txt"]);
    assert_eq!(root[1].size, (3 * MIB / 2) as u64);
    let dir = fixture.dir.path().join("runtime");
    let big = session
        .extract_file(&snapshot, Path::new("/data/big.zip/mid/big.bin"), &dir)
        .unwrap();
    assert!(fs::read(big).unwrap() == pattern(5 * MIB / 2, 13));
    let tail = session
        .extract_file(&snapshot, Path::new("/data/big.zip/tail.txt"), &dir)
        .unwrap();
    assert_eq!(fs::read(tail).unwrap(), b"tail\n");
}

#[test]
fn lists_a_zip_archive_with_zip64_end_of_directory() {
    // More than 65535 entries need the ZIP64 end of central directory record.
    let archive = stored_zip((0..70_000).map(|i| (format!("d{}/f{i}.txt", i % 100), Vec::new())));
    let fixture = zip_fixture_with("many.zip", archive);
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let root = session
        .list_dir(&snapshot, Path::new("/data/many.zip"))
        .unwrap();
    assert_eq!(root.len(), 100);
    let d7 = session
        .list_dir(&snapshot, Path::new("/data/many.zip/d7"))
        .unwrap();
    assert_eq!(d7.len(), 700);
    assert!(d7.iter().all(|e| e.kind == EntryKind::File && e.size == 0));
}

#[test]
fn entries_with_inconsistent_sizes_are_unavailable() {
    // Patch the central directory entry of "lie.txt" (stored, 10 bytes).
    let mut data = stored_zip([("lie.txt".to_owned(), b"0123456789".to_vec())]);
    let central = data.windows(4).rposition(|w| w == b"PK\x01\x02").unwrap();
    assert_eq!(&data[central + 46..central + 53], b"lie.txt");
    // Declared size 5, packed size 10: a stored entry can't shrink.
    data[central + 24..central + 28].copy_from_slice(&5u32.to_le_bytes());
    let fixture = zip_fixture_with("lie.zip", data);
    let session = fixture.open();
    let snapshot = fixture.snapshot(&session);

    let entries = session
        .list_dir(&snapshot, Path::new("/data/lie.zip"))
        .unwrap();
    assert!(matches!(entries[0].kind, EntryKind::Unavailable { .. }));
    let result = session.extract_file(
        &snapshot,
        Path::new("/data/lie.zip/lie.txt"),
        fixture.dir.path(),
    );
    assert!(
        matches!(result, Err(Error::UnsupportedFile { .. })),
        "{result:?}"
    );
}

#[test]
fn entries_that_do_not_match_their_declared_size_fail() {
    let sample = sample_zip();
    // The central directory entry of the deflated big.bin: 300000 bytes in 1482.
    let central = (0..sample.len() - 53)
        .rev()
        .find(|&i| &sample[i..i + 4] == b"PK\x01\x02" && &sample[i + 46..i + 53] == b"big.bin")
        .unwrap();
    for declared in [100u32, 400_000] {
        let mut data = sample.clone();
        data[central + 24..central + 28].copy_from_slice(&declared.to_le_bytes());
        let fixture = zip_fixture_with("lie.zip", data);
        let session = fixture.open();
        let snapshot = fixture.snapshot(&session);
        let dir = fixture.dir.path().join("runtime");
        let result = session.extract_file(&snapshot, Path::new("/data/lie.zip/big.bin"), &dir);
        match result {
            Err(Error::Archive(message)) => assert!(message.contains("damaged"), "{message}"),
            other => panic!("{declared}: {other:?}"),
        }
        // No half-written file stays behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }
}
