mod imp;

use adw::{prelude::*, subclass::prelude::*};
use gtk::glib;
use restic_gtk::backend::Snapshot;

glib::wrapper! {
    /// Everything a snapshot records about itself and the backup that created it.
    pub struct SnapshotInfoDialog(ObjectSubclass<imp::SnapshotInfoDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::ShortcutManager;
}

impl SnapshotInfoDialog {
    pub fn show(parent: &impl IsA<gtk::Widget>, snapshot: &Snapshot) {
        let dialog: Self = glib::Object::new();
        dialog.imp().set_snapshot(snapshot);
        dialog.present(Some(parent));
    }
}
