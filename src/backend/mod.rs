//! Repository access on top of `rustic_core`.
//!
//! Everything in here is blocking and free of GTK, so the UI runs it on worker
//! threads and the logic can be tested headless against real repositories.

mod archive;
mod error;
mod extract;
mod image;
mod progress;
mod restore;
mod s3;
mod session;
mod snapshot;
mod stats;
mod store;
mod tree;

pub use rustic_core::{jiff, repofile::SnapshotSummary};

pub use archive::is_zip_archive;
pub use error::{Error, Result};
pub use image::is_disk_image;
pub use progress::{ProgressKind, ProgressUpdate};
pub use restore::{RestorePreview, RestoreRequest, RestoreSummary};
pub use s3::{LOCATION as S3_LOCATION, S3Location};
pub use session::Session;
pub use snapshot::Snapshot;
pub use stats::RepoStats;
pub use store::{RepoConfig, RepoId, RepoStore, validate_location};
pub use tree::{Entry, EntryKind};
