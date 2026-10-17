use std::{
    fs::{self, DirBuilder, OpenOptions, Permissions},
    io::{BufWriter, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use super::{Error, Result, Session, Snapshot};

impl Session {
    /// Writes the file at `path` to a new, read-only file below `dir` and returns its path.
    ///
    /// Each extraction gets its own subfolder, so the file keeps its name and
    /// repeated or same-named extractions don't collide.
    pub fn extract_file(&self, snapshot: &Snapshot, path: &Path, dir: &Path) -> Result<PathBuf> {
        let repo = self.indexed()?;
        let node = self.node(&repo, snapshot, path)?;
        if !node.is_file() {
            return Err(Error::NotAFile(path.to_path_buf()));
        }

        let folder = dir.join(Uuid::new_v4().simple().to_string());
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&folder)?;
        let target = folder.join(node.name());

        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?;
        let mut writer = BufWriter::new(file);
        let written = repo
            .dump(&node, &mut writer)
            .map_err(Error::from)
            .and_then(|()| writer.flush().map_err(Error::from));
        if let Err(err) = written {
            let _ = fs::remove_dir_all(&folder);
            return Err(err);
        }

        // Read-only, so viewers don't suggest editing a file that lives in a temp folder.
        fs::set_permissions(&target, Permissions::from_mode(0o400))?;
        Ok(target)
    }
}
