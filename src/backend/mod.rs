//! Repository access on top of `rustic_core`.
//!
//! Everything in here is blocking and free of GTK, so the UI runs it on worker
//! threads and the logic can be tested headless against real repositories.

mod error;
mod extract;
mod progress;
mod restore;
mod session;
mod snapshot;
mod store;
mod tree;

pub use rustic_core::jiff;

pub use error::{Error, Result};
pub use progress::{ProgressKind, ProgressUpdate};
pub use restore::{RestorePreview, RestoreRequest};
pub use session::Session;
pub use snapshot::Snapshot;
pub use store::{RepoConfig, RepoId, RepoStore, validate_location};
pub use tree::{Entry, EntryKind};
