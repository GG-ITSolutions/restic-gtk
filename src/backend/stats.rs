use rustic_core::FileType;

use super::{Result, Session};

/// Size and makeup of a whole repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStats {
    pub id: String,
    /// Repository format; version 2 supports compression.
    pub version: u32,
    pub snapshots: u64,
    pub packs: u64,
    /// Bytes all repository files take on the storage.
    pub stored_size: u64,
    /// Uncompressed size of all file contents and folder listings, after deduplication.
    pub raw_size: u64,
    /// The same data as stored in the packs, compressed and encrypted.
    pub packed_size: u64,
}

impl Session {
    /// Lists all repository files and reads the whole index, so this takes a
    /// moment on large remote repositories.
    pub fn repo_stats(&self) -> Result<RepoStats> {
        let files = self.repo.infos_files()?;
        let count = |tpe| {
            files
                .repo
                .iter()
                .filter(|info| info.tpe == tpe)
                .map(|info| info.count)
                .sum()
        };
        // Blobs of packs marked for deletion are left out, prune removes them.
        let blobs = self.repo.infos_index()?.blobs;

        let config = self.repo.config();
        Ok(RepoStats {
            id: config.id.to_hex().to_string(),
            version: config.version,
            snapshots: count(FileType::Snapshot),
            packs: count(FileType::Pack),
            stored_size: files.repo.iter().map(|info| info.size).sum(),
            raw_size: blobs.iter().map(|info| info.data_size).sum(),
            packed_size: blobs.iter().map(|info| info.size).sum(),
        })
    }
}
