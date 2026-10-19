mod imp;

use adw::{prelude::*, subclass::prelude::*};
use gtk::glib;

use crate::ui::objects::RepoObject;

glib::wrapper! {
    /// Asks for the password of a repository and opens it.
    ///
    /// Stays open while connecting and after a wrong password, so the password
    /// can be corrected in place.
    pub struct UnlockDialog(ObjectSubclass<imp::UnlockDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::ShortcutManager;
}

impl UnlockDialog {
    /// Shows the dialog; resolves to `true` once `repo` is unlocked, `false` if cancelled.
    pub async fn run(parent: &impl IsA<gtk::Widget>, repo: &RepoObject) -> bool {
        let dialog: Self = glib::Object::new();
        let imp = dialog.imp();
        imp.description_label.set_label(&format!(
            "Enter the password of “{}”. It is not stored; until you lock the repository or quit, only the unlocked key is kept in memory.",
            repo.name()
        ));
        imp.repo.replace(Some(repo.clone()));

        let (sender, receiver) = async_channel::bounded(1);
        imp.result.replace(Some(sender));
        dialog.connect_closed(|dialog| drop(dialog.imp().result.take()));
        dialog.present(Some(parent));
        receiver.recv().await.is_ok()
    }
}
