mod imp;

use std::{path::Path, sync::Arc};

use adw::{prelude::*, subclass::prelude::*};
use gtk::glib;
use restic_gtk::backend::{Session, Snapshot};

glib::wrapper! {
    /// Asks for the recovery key or passphrase of an encrypted partition of a disk image, and unlocks it.
    ///
    /// Stays open while unlocking and after a wrong key, so the key can be corrected in place.
    pub struct VolumeDialog(ObjectSubclass<imp::VolumeDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::ShortcutManager;
}

impl VolumeDialog {
    /// Shows the dialog for the partition at `partition` in `snapshot`; resolves to `true` once
    /// it is unlocked, `false` if cancelled.
    pub async fn run(
        parent: &impl IsA<gtk::Widget>,
        session: Arc<Session>,
        snapshot: Snapshot,
        partition: &Path,
    ) -> bool {
        let dialog: Self = glib::Object::new();
        let imp = dialog.imp();
        let name = partition.file_name().unwrap_or_default().to_string_lossy();
        imp.description_label.set_label(&format!(
            "“{name}” is encrypted. Enter its BitLocker recovery key or password, or its LUKS passphrase. It is not stored; until you lock the repository or quit, it is only kept in memory."
        ));
        imp.target
            .replace(Some((session, snapshot, partition.to_path_buf())));

        let (sender, receiver) = async_channel::bounded(1);
        imp.result.replace(Some(sender));
        dialog.connect_closed(|dialog| drop(dialog.imp().result.take()));
        dialog.present(Some(parent));
        receiver.recv().await.is_ok()
    }
}
