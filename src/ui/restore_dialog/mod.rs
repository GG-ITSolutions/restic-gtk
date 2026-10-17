mod imp;

use std::sync::Arc;

use adw::{prelude::*, subclass::prelude::*};
use gtk::glib;
use restic_gtk::backend::{RestoreRequest, Session, Snapshot};

glib::wrapper! {
    /// Restores entries of a snapshot: checks the target, asks before replacing
    /// files, then shows the progress. Can't be closed while files are written.
    pub struct RestoreDialog(ObjectSubclass<imp::RestoreDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::ShortcutManager;
}

impl RestoreDialog {
    /// Shows the dialog; resolves to `true` once everything is restored.
    pub async fn run(
        parent: &impl IsA<gtk::Widget>,
        session: Arc<Session>,
        snapshot: Snapshot,
        request: RestoreRequest,
    ) -> bool {
        let dialog: Self = glib::Object::new();
        let (sender, receiver) = async_channel::bounded(1);
        dialog.imp().result.replace(Some(sender));
        dialog.connect_closed(|dialog| drop(dialog.imp().result.take()));
        dialog.present(Some(parent));
        dialog.imp().start(session, snapshot, request);
        receiver.recv().await.is_ok()
    }

    /// Whether files are being written right now.
    pub fn is_restoring(&self) -> bool {
        !self.can_close()
    }
}
