mod imp;

use adw::{prelude::*, subclass::prelude::*};
use gtk::glib;

use crate::ui::objects::RepoObject;

glib::wrapper! {
    /// Size, makeup and format of an unlocked repository.
    pub struct RepoInfoDialog(ObjectSubclass<imp::RepoInfoDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::ShortcutManager;
}

impl RepoInfoDialog {
    /// Shows the dialog and reads the statistics in the background.
    pub fn show(parent: &impl IsA<gtk::Widget>, repo: &RepoObject) {
        let Some(session) = repo.session() else {
            return;
        };
        let dialog: Self = glib::Object::new();
        dialog.present(Some(parent));
        dialog.imp().load(repo, session);
    }
}
