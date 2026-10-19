use std::{io, path::PathBuf};

use rustic_core::RusticError;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Wrong password")]
    WrongPassword,

    #[error("“{0}” is not a folder")]
    NotADirectory(PathBuf),

    #[error("“{0}” is not a regular file")]
    NotAFile(PathBuf),

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
        } else {
            Self::Repository(err.display_log())
        }
    }
}
