use std::{io, path::PathBuf};

use rustic_core::RusticError;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Wrong password")]
    WrongPassword,

    /// The storage refused the credentials, e.g. a wrong S3 access key or secret key.
    #[error("Access denied, check access key and secret key")]
    AccessDenied,

    /// The location has no repository config, e.g. a wrong path or S3 bucket.
    #[error("No repository found at this location")]
    NoRepository,

    #[error("“{0}” is not a folder")]
    NotADirectory(PathBuf),

    #[error("“{0}” is not a regular file")]
    NotAFile(PathBuf),

    #[error("“{0}” does not exist")]
    NotFound(PathBuf),

    /// A file inside a disk image whose contents can't be read, e.g. an encrypted one.
    #[error("“{path}” can't be opened. {reason}")]
    UnsupportedFile { path: PathBuf, reason: String },

    /// A partition of a disk image without a filesystem that can be browsed.
    #[error("{0}")]
    UnsupportedFilesystem(String),

    /// A disk image that can't be read, e.g. a damaged filesystem.
    #[error("{0}")]
    Image(String),

    /// The restore target has something else where an entry goes.
    #[error("“{path}” already exists as {existing}")]
    TargetExists {
        path: PathBuf,
        existing: &'static str,
    },

    #[error("Invalid repository list: {0}")]
    Store(#[from] serde_json::Error),

    /// The repository list exists but couldn't be read, see `RepoStore::load`.
    #[error("The repository list could not be read, so changes are not saved")]
    StoreReadOnly,

    #[error(transparent)]
    Io(#[from] io::Error),

    /// Everything rustic reports besides a wrong password. rustic's own messages
    /// are multi-line reports, so only the one-line log form is kept.
    #[error("{0}")]
    Repository(String),
}

impl From<Box<RusticError>> for Error {
    fn from(err: Box<RusticError>) -> Self {
        if err.is_incorrect_password() {
            Self::WrongPassword
        } else if is_permission_denied(&err) {
            Self::AccessDenied
        } else {
            Self::Repository(err.display_log())
        }
    }
}

/// Whether OpenDAL reported a refused request somewhere in the chain of `err`.
fn is_permission_denied(err: &RusticError) -> bool {
    let mut source = std::error::Error::source(err);
    while let Some(err) = source {
        if err
            .downcast_ref::<opendal::Error>()
            .is_some_and(|err| err.kind() == opendal::ErrorKind::PermissionDenied)
        {
            return true;
        }
        source = err.source();
    }
    false
}
