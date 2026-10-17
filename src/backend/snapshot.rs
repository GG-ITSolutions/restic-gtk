use rustic_core::{TreeId, jiff::Zoned, repofile::SnapshotFile};

/// The parts of a snapshot the app shows and navigates.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: String,
    pub time: Zoned,
    pub hostname: String,
    pub username: String,
    pub paths: Vec<String>,
    pub tags: Vec<String>,
    /// Total size of the backed up files, if the snapshot has a summary.
    pub size: Option<u64>,
    pub(crate) tree: TreeId,
}

impl Snapshot {
    /// The 8-character id restic shows.
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }
}

impl From<SnapshotFile> for Snapshot {
    fn from(snap: SnapshotFile) -> Self {
        Self {
            id: snap.id.to_string(),
            time: snap.time,
            hostname: snap.hostname,
            username: snap.username,
            paths: snap.paths.iter().cloned().collect(),
            tags: snap.tags.iter().cloned().collect(),
            size: snap.summary.map(|s| s.total_bytes_processed),
            tree: snap.tree,
        }
    }
}
