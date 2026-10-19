//! The GTK user interface. Talks to repositories only through [`restic_gtk::backend`].

mod browser_page;
mod format;
mod objects;
mod path_bar;
mod repo_dialog;
mod repo_info_dialog;
mod restore_dialog;
mod snapshot_info_dialog;
mod snapshots_page;
mod unlock_dialog;
mod window;

use gtk::{gio, glib, prelude::*};

pub use window::ResticGtkWindow;

/// Runs blocking backend work on a worker thread and awaits its result.
///
/// Panics of `f` are resumed on the main thread instead of being swallowed.
pub async fn run_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    gio::spawn_blocking(f)
        .await
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Shows `toast` in the window containing `widget`.
pub fn add_toast(widget: &impl IsA<gtk::Widget>, toast: adw::Toast) {
    if let Some(window) = widget.root().and_downcast::<ResticGtkWindow>() {
        window.add_toast(toast);
    }
}

/// Shows a plain message as toast in the window containing `widget`.
pub fn show_toast(widget: &impl IsA<gtk::Widget>, message: &str) {
    add_toast(widget, adw::Toast::new(&glib::markup_escape_text(message)));
}
