use std::{
    ffi::OsStr,
    fs::{self, DirBuilder, OpenOptions, Permissions},
    io::{BufWriter, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use uuid::Uuid;

use super::{Error, Result, Session, Snapshot, archive::ZipRef, session::Location};

impl Session {
    /// Writes the file at `path` to a new, read-only file below `dir` and returns its path.
    ///
    /// Each extraction gets its own subfolder, so the file keeps its name and
    /// repeated or same-named extractions don't collide. Files inside disk images
    /// and ZIP archives are read from them, they are not downloaded as a whole.
    pub fn extract_file(&self, snapshot: &Snapshot, path: &Path, dir: &Path) -> Result<PathBuf> {
        let repo = self.indexed()?;
        match self.locate(&repo, snapshot, path)? {
            Location::Image {
                node,
                path: image_path,
                inner,
                siblings,
            } if inner.components().next().is_some() => {
                let name = path
                    .file_name()
                    .ok_or_else(|| Error::NotAFile(path.to_path_buf()))?;
                let image = self.image_ref(&repo, snapshot, &node, &image_path, siblings);
                extract_to(dir, name, |out| self.images.write_file(&image, &inner, out))
            }
            Location::Archive {
                node,
                path: zip_path,
                inner,
            } if inner.components().next().is_some() => {
                let name = path
                    .file_name()
                    .ok_or_else(|| Error::NotAFile(path.to_path_buf()))?;
                let zip = ZipRef::new(&repo, snapshot, &node, &zip_path);
                extract_to(dir, name, |out| self.zips.write_file(&zip, &inner, out))
            }
            Location::Image { node, .. }
            | Location::Archive { node, .. }
            | Location::Node(node) => {
                if !node.is_file() {
                    return Err(Error::NotAFile(path.to_path_buf()));
                }
                extract_to(dir, &node.name(), |out| {
                    repo.dump(&node, out).map_err(Error::from)
                })
            }
        }
    }
}

/// Creates the file `name` in a new subfolder of `dir`, fills it with `write` and returns its path.
fn extract_to(
    dir: &Path,
    name: &OsStr,
    write: impl FnOnce(&mut BufWriter<fs::File>) -> Result<()>,
) -> Result<PathBuf> {
    let folder = dir.join(Uuid::new_v4().simple().to_string());
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)?;
    let target = folder.join(name);

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&target)?;
    let mut writer = BufWriter::new(file);
    let written = write(&mut writer).and_then(|()| writer.flush().map_err(Error::from));
    if let Err(err) = written {
        let _ = fs::remove_dir_all(&folder);
        return Err(err);
    }

    // Read-only, so viewers don't suggest editing a file that lives in a temp folder.
    fs::set_permissions(&target, Permissions::from_mode(0o400))?;
    Ok(target)
}
