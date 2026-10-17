use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use super::Result;

/// Stable identity of a repository entry, independent of name and location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoId(Uuid);

impl RepoId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RepoId {
    fn default() -> Self {
        Self::new()
    }
}

/// A repository as configured by the user.
///
/// Never contains the repository password. The location may contain HTTP
/// basic-auth credentials for a rest-server (`rest:https://user:pass@host/repo/`),
/// which is why the store file is only readable by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub id: RepoId,
    pub name: String,
    pub location: String,
}

impl RepoConfig {
    pub fn new(name: impl Into<String>, location: impl Into<String>) -> Self {
        Self {
            id: RepoId::new(),
            name: name.into(),
            location: location.into(),
        }
    }

    /// The location with a basic-auth password replaced by `***`, for display.
    pub fn display_location(&self) -> String {
        let Some(rest) = self.location.strip_prefix("rest:") else {
            return self.location.clone();
        };
        match Url::parse(rest) {
            Ok(mut url) if url.password().is_some() => {
                let _ = url.set_password(Some("***"));
                format!("rest:{url}")
            }
            _ => self.location.clone(),
        }
    }
}

/// Checks a repository location before it is stored.
///
/// Supported are rest-servers (`rest:http(s)://…`) and local repositories
/// (absolute path or `local:<path>`). Returns a short, user-facing reason otherwise.
pub fn validate_location(location: &str) -> Result<(), String> {
    let location = location.trim();
    if location.is_empty() {
        return Err("Enter a location".into());
    }
    if let Some(rest) = location.strip_prefix("rest:") {
        let url = Url::parse(rest).map_err(|_| "Not a valid URL".to_string())?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err("Use rest:https://host/repository/".into());
        }
        return Ok(());
    }
    let path = location.strip_prefix("local:").unwrap_or(location);
    if !Path::new(path).is_absolute() {
        return Err("Use an absolute path or rest:https://…".into());
    }
    // rustic treats everything before the first ':' as backend type.
    if location.starts_with('/') && location.contains(':') {
        return Err("Prefix paths containing “:” with local:".into());
    }
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    repositories: Vec<RepoConfig>,
}

const STORE_VERSION: u32 = 1;

/// The persisted repository list (`repos.json`).
#[derive(Debug, Clone)]
pub struct RepoStore {
    path: PathBuf,
}

impl RepoStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("repos.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Vec<RepoConfig>> {
        match fs::read(&self.path) {
            Ok(data) => Ok(serde_json::from_slice::<StoreFile>(&data)?.repositories),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(err.into()),
        }
    }

    /// Writes the list atomically (temp file + rename), readable only by the user.
    pub fn save(&self, repos: &[RepoConfig]) -> Result<()> {
        let dir = self.path.parent().expect("store path has a parent");
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;

        let data = serde_json::to_vec_pretty(&StoreFile {
            version: STORE_VERSION,
            repositories: repos.to_vec(),
        })?;

        let tmp = self.path.with_extension("json.tmp");
        // A leftover could have other permissions; `mode` only applies on creation.
        let _ = fs::remove_file(&tmp);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&data)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}
