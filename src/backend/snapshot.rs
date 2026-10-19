use std::time::Duration;

use rustic_core::{
    TreeId,
    jiff::Zoned,
    repofile::{SnapshotFile, SnapshotSummary},
};

/// The parts of a snapshot the app shows and navigates.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub time: Zoned,
    pub hostname: String,
    pub username: String,
    pub paths: Vec<String>,
    pub tags: Vec<String>,
    /// The program that created the snapshot, e.g. `restic 0.18.0`.
    pub program_version: String,
    /// Recorded by restic since 0.17 and by rustic; `None` for older snapshots.
    pub summary: Option<BackupSummary>,
    pub(crate) tree: TreeId,
}

impl Snapshot {
    /// The 8-character id restic shows.
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }

    /// Total size of the backed up files, if the snapshot has a summary.
    pub fn size(&self) -> Option<u64> {
        self.summary.as_ref().map(|s| s.total_bytes)
    }
}

/// What the backup that created a snapshot recorded about itself.
///
/// New, changed and unmodified are relative to the parent snapshot.
#[derive(Debug, Clone)]
pub struct BackupSummary {
    pub files_new: u64,
    pub files_changed: u64,
    pub files_unmodified: u64,
    pub total_files: u64,
    pub total_dirs: u64,
    /// Total size of the backed up files.
    pub total_bytes: u64,
    /// Bytes the backup added to the repository, uncompressed.
    pub data_added: u64,
    /// Bytes the backup added to the repository, as stored.
    pub data_added_packed: u64,
    pub backup_start: Zoned,
    pub backup_end: Zoned,
}

impl BackupSummary {
    pub fn duration(&self) -> Duration {
        // restic doesn't write rustic's `backup_duration`, so take the difference.
        self.backup_end
            .duration_since(&self.backup_start)
            .unsigned_abs()
    }
}

impl From<SnapshotSummary> for BackupSummary {
    fn from(s: SnapshotSummary) -> Self {
        Self {
            files_new: s.files_new,
            files_changed: s.files_changed,
            files_unmodified: s.files_unmodified,
            total_files: s.total_files_processed,
            total_dirs: s.total_dirs_processed,
            total_bytes: s.total_bytes_processed,
            data_added: s.data_added,
            data_added_packed: s.data_added_packed,
            backup_start: s.backup_start,
            backup_end: s.backup_end,
        }
    }
}

impl From<SnapshotFile> for Snapshot {
    fn from(snap: SnapshotFile) -> Self {
        Self {
            // `Display` of rustic ids is shortened to 8 characters.
            id: snap.id.to_hex().to_string(),
            time: snap.time,
            hostname: snap.hostname,
            username: snap.username,
            paths: snap.paths.iter().cloned().collect(),
            tags: snap.tags.iter().cloned().collect(),
            program_version: snap.program_version,
            summary: snap.summary.map(BackupSummary::from),
            tree: snap.tree,
        }
    }
}
