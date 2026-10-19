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
    pub summary: Option<SnapshotSummary>,
    pub(crate) tree: TreeId,
}

impl Snapshot {
    /// The 8-character id restic shows.
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }

    /// Total size of the backed up files, if the snapshot has a summary.
    pub fn size(&self) -> Option<u64> {
        self.summary.as_ref().map(|s| s.total_bytes_processed)
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
            summary: snap.summary,
            tree: snap.tree,
        }
    }
}
