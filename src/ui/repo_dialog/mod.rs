mod imp;

use adw::{prelude::*, subclass::prelude::*};
use gtk::glib;
use restic_gtk::backend::{RepoConfig, S3Location};

glib::wrapper! {
    /// Adds a repository or edits name and location of an existing one, local, rest-server or S3.
    pub struct RepoDialog(ObjectSubclass<imp::RepoDialog>)
        @extends adw::Dialog, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::ShortcutManager;
}

impl RepoDialog {
    /// Shows the dialog and resolves to the saved configuration, `None` if cancelled.
    pub async fn run(
        parent: &impl IsA<gtk::Widget>,
        existing: Option<RepoConfig>,
    ) -> Option<RepoConfig> {
        let dialog: Self = glib::Object::new();
        let imp = dialog.imp();
        if let Some(config) = &existing {
            dialog.set_title("Edit Repository");
            imp.save_button.set_label("_Save");
            imp.name_row.set_text(&config.name);
            match S3Location::from_config(config) {
                Some(s3) => {
                    imp.set_s3(&s3);
                    imp.type_row.set_selected(imp::TYPE_S3);
                }
                None => imp.location_row.set_text(&config.location),
            }
        } else {
            dialog.set_title("Add Repository");
            imp.save_button.set_label("_Add");
        }
        imp.existing.replace(existing);

        let (sender, receiver) = async_channel::bounded(1);
        imp.result.replace(Some(sender));
        // Dropping the sender on close ends `recv` for cancelled dialogs.
        dialog.connect_closed(|dialog| drop(dialog.imp().result.take()));
        dialog.present(Some(parent));
        receiver.recv().await.ok()
    }
}
