use std::path::PathBuf;

pub const APP_ID: &str = "de.gg_itsystemhaus.ResticGtk";

/// Name of the per-user directories below the XDG base dirs.
const DIR_NAME: &str = "restic-gtk";

/// `~/.config/restic-gtk`: the repository list.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(DIR_NAME)
}

/// `~/.cache/restic-gtk`: rustic's encrypted index/snapshot cache.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(DIR_NAME)
}

/// `$XDG_RUNTIME_DIR/restic-gtk`: files extracted for viewing, removed on exit.
pub fn runtime_dir() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(DIR_NAME)
}
