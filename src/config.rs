use std::path::PathBuf;

pub const APP_ID: &str = "de.gg_itsystemhaus.ResticGtk";

/// Name of the per-user directories below the XDG base dirs.
const DIR_NAME: &str = "restic-gtk";

/// `~/.config/restic-gtk`: the repository list.
pub fn config_dir() -> PathBuf {
    user_dir(dirs::config_dir())
}

/// `~/.cache/restic-gtk`: rustic's encrypted index/snapshot cache.
pub fn cache_dir() -> PathBuf {
    user_dir(dirs::cache_dir())
}

/// `$XDG_RUNTIME_DIR/restic-gtk`: files extracted for viewing, removed on exit.
///
/// Without `XDG_RUNTIME_DIR` (`sudo`, SSH) falls back to the cache like GLib does, into a
/// subfolder of its own: the folder is wiped on start, which must not hit rustic's cache.
pub fn runtime_dir() -> PathBuf {
    dirs::runtime_dir()
        .map(|dir| dir.join(DIR_NAME))
        .unwrap_or_else(|| cache_dir().join("opened"))
}

/// Never falls back to the shared temp folder: other users could read or replace the files.
fn user_dir(base: Option<PathBuf>) -> PathBuf {
    base.expect("no home folder: neither $HOME nor a passwd entry is set")
        .join(DIR_NAME)
}
