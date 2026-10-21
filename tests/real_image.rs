//! Browses a real Windows disk image backup made by `backup-windows`.
//!
//! Needs a repository with such a backup, so it is ignored by default:
//!
//! ```sh
//! RESTIC_GTK_TEST_REPOSITORY=rest:https://… RESTIC_GTK_TEST_PASSWORD=… \
//!     cargo test --release --test real_image -- --ignored --nocapture
//! ```
//!
//! Expects the latest snapshot to hold `/disk0.img` with an EFI partition 1 and
//! Windows on partition 3 (`C:`), as `backup-windows` writes them. Reads only.

use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Instant,
};

use restic_gtk::backend::{Entry, EntryKind, RepoConfig, RestoreRequest, Session, Snapshot};

const IMAGE: &str = "/disk0.img";
const WINDOWS: &str = "/disk0.img/Partition 3 (C:)";

fn open() -> Option<(Session, Snapshot, tempfile::TempDir)> {
    let (Ok(location), Ok(password)) = (
        env::var("RESTIC_GTK_TEST_REPOSITORY"),
        env::var("RESTIC_GTK_TEST_PASSWORD"),
    ) else {
        eprintln!("RESTIC_GTK_TEST_REPOSITORY and RESTIC_GTK_TEST_PASSWORD not set, skipping");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let config = RepoConfig::new("Test", location);
    let session = Session::open(&config, &password, None, &dir.path().join("cache")).unwrap();
    let snapshot = session.snapshots().unwrap().remove(0);
    session.load_index(|_| {}).unwrap();
    Some((session, snapshot, dir))
}

fn list(session: &Session, snapshot: &Snapshot, path: &str) -> Vec<Entry> {
    let started = Instant::now();
    let entries = session.list_dir(snapshot, Path::new(path)).unwrap();
    eprintln!(
        "{path}: {} entries in {:.2} s",
        entries.len(),
        started.elapsed().as_secs_f64()
    );
    entries
}

fn extract(session: &Session, snapshot: &Snapshot, path: &Path, dir: &Path) -> Vec<u8> {
    let started = Instant::now();
    let file = session.extract_file(snapshot, path, dir).unwrap();
    let data = fs::read(&file).unwrap();
    eprintln!(
        "{}: {} bytes in {:.2} s",
        path.display(),
        data.len(),
        started.elapsed().as_secs_f64()
    );
    data
}

/// All files below `path`, at most `limit` of them.
fn files_below(session: &Session, snapshot: &Snapshot, path: &str, limit: usize) -> Vec<Entry> {
    let mut files = Vec::new();
    let mut dirs = vec![PathBuf::from(path)];
    while let Some(dir) = dirs.pop() {
        for entry in session.list_dir(snapshot, &dir).unwrap() {
            match entry.kind {
                EntryKind::Dir => dirs.push(entry.path),
                EntryKind::File if files.len() < limit => files.push(entry),
                _ => {}
            }
        }
    }
    files
}

#[test]
#[ignore = "needs a real repository, see the module docs"]
fn browses_a_windows_image() {
    let Some((session, snapshot, dir)) = open() else {
        return;
    };

    let partitions = list(&session, &snapshot, IMAGE);
    for p in &partitions {
        eprintln!("  {} {:?} {} bytes", p.name, p.kind, p.size);
    }
    assert!(partitions.iter().any(|p| p.name == "Partition 1 (EFI)"));
    let windows = partitions
        .iter()
        .find(|p| p.path == Path::new(WINDOWS))
        .expect("Windows partition");
    assert_eq!(windows.kind, EntryKind::Partition);

    let root = list(&session, &snapshot, WINDOWS);
    assert!(root.iter().any(|e| e.name == "Windows" && e.is_dir()));
    assert!(!root.iter().any(|e| e.name.starts_with("$MFT")));
    // Junctions show as links, not as folders.
    if let Some(link) = root.iter().find(|e| e.name == "Documents and Settings") {
        assert!(matches!(link.kind, EntryKind::Symlink { .. }), "{link:?}");
    }
    list(&session, &snapshot, &format!("{WINDOWS}/Users"));
    list(&session, &snapshot, &format!("{WINDOWS}/Windows/System32"));

    let hosts = extract(
        &session,
        &snapshot,
        &Path::new(WINDOWS).join("Windows/System32/drivers/etc/hosts"),
        dir.path(),
    );
    assert!(String::from_utf8_lossy(&hosts).contains("localhost"));

    let efi = list(&session, &snapshot, "/disk0.img/Partition 1 (EFI)/EFI");
    assert!(efi.iter().any(|e| e.name.eq_ignore_ascii_case("Microsoft")));
    let bootmgr = extract(
        &session,
        &snapshot,
        Path::new("/disk0.img/Partition 1 (EFI)/EFI/Microsoft/Boot/bootmgfw.efi"),
        dir.path(),
    );
    assert_eq!(&bootmgr[..2], b"MZ");
}

/// Windows compresses its archived CBS logs with NTFS compression (LZNT1).
///
/// Takes a few minutes: the clusters of such a log are spread over the volume,
/// so reading it fetches many blobs.
#[test]
#[ignore = "needs a real repository, see the module docs"]
fn reads_ntfs_compressed_logs() {
    let Some((session, snapshot, dir)) = open() else {
        return;
    };
    let cbs = format!("{WINDOWS}/Windows/Logs/CBS");
    let log = list(&session, &snapshot, &cbs)
        .into_iter()
        .filter(|e| e.name.starts_with("CbsPersist_") && e.name.ends_with(".log"))
        .max_by(|a, b| a.name.cmp(&b.name))
        .expect("an archived CBS log");
    let data = extract(&session, &snapshot, &log.path, dir.path());
    assert_eq!(data.len() as u64, log.size);
    // Plain text throughout; LZNT1 data read raw, or a chunk misplaced, would not be.
    let text = String::from_utf8(data).expect("UTF-8 text");
    assert!(!text.contains('\0'));
    let text = text.trim_start_matches('\u{feff}');
    assert!(text.starts_with("20"), "{:?}", &text[..100]);
}

/// Microsoft Edge keeps its translations WOF-compressed (LZX) on gg-lu-vw2.
#[test]
#[ignore = "needs a real repository, see the module docs"]
fn reads_wof_compressed_files() {
    let Some((session, snapshot, dir)) = open() else {
        return;
    };
    let root = env::var("RESTIC_GTK_TEST_WOF_DIR")
        .unwrap_or_else(|_| format!("{WINDOWS}/Program Files (x86)/Microsoft/Edge/Application"));
    let files: Vec<_> = files_below(&session, &snapshot, &root, usize::MAX)
        .into_iter()
        .filter(|f| f.path.parent().is_some_and(|p| p.ends_with("Locales")))
        .collect();
    eprintln!("{} files in Locales folders", files.len());
    assert!(!files.is_empty());
    for file in files {
        let data = extract(&session, &snapshot, &file.path, dir.path());
        assert_eq!(data.len() as u64, file.size, "{}", file.path.display());
        // Chromium .pak files start with their format version, 5.
        assert_eq!(&data[..4], &[5, 0, 0, 0], "{}", file.path.display());
    }
}

#[test]
#[ignore = "needs a real repository, see the module docs"]
fn restores_a_folder_from_a_windows_image() {
    let Some((session, snapshot, dir)) = open() else {
        return;
    };
    let request = RestoreRequest {
        items: vec![Path::new(WINDOWS).join("Windows/System32/drivers/etc")],
        target: dir.path().join("target"),
    };
    fs::create_dir(&request.target).unwrap();
    let started = Instant::now();
    let preview = session.preview_restore(&snapshot, &request).unwrap();
    let summary = session.restore(&snapshot, &request, |_| {}).unwrap();
    eprintln!(
        "etc: {} files, {} bytes, {} skipped in {:.2} s",
        preview.files,
        preview.bytes,
        summary.skipped,
        started.elapsed().as_secs_f64()
    );
    let hosts = fs::read_to_string(request.target.join("etc/hosts")).unwrap();
    assert!(hosts.contains("localhost"));
}
