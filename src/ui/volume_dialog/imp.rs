use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    sync::Arc,
};

use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, glib, glib::clone};
use restic_gtk::backend::{Error, Session, Snapshot};

use crate::ui::run_blocking;

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/volume-dialog.ui")]
pub struct VolumeDialog {
    #[template_child]
    pub unlock_button: TemplateChild<gtk::Button>,
    #[template_child]
    pub unlock_stack: TemplateChild<gtk::Stack>,
    #[template_child]
    pub description_label: TemplateChild<gtk::Label>,
    #[template_child]
    pub password_row: TemplateChild<adw::PasswordEntryRow>,
    #[template_child]
    pub error_label: TemplateChild<gtk::Label>,

    /// The session, snapshot and path of the partition to unlock.
    pub target: RefCell<Option<(Arc<Session>, Snapshot, PathBuf)>>,
    /// Present until the dialog is closed; sending signals success.
    pub result: RefCell<Option<async_channel::Sender<()>>>,
    busy: Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for VolumeDialog {
    const NAME: &'static str = "ResticGtkVolumeDialog";
    type Type = super::VolumeDialog;
    type ParentType = adw::Dialog;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
        obj.init_template();
    }
}

#[gtk::template_callbacks]
impl VolumeDialog {
    #[template_callback]
    fn on_changed(&self) {
        self.password_row.remove_css_class("error");
        self.update_sensitivity();
    }

    #[template_callback]
    fn on_cancel(&self) {
        self.obj().close();
    }

    #[template_callback]
    fn on_unlock(&self) {
        if !self.unlock_button.is_sensitive() {
            return;
        }
        let Some((session, snapshot, path)) = self.target.borrow().clone() else {
            return;
        };
        self.set_busy(true);
        self.error_label.set_visible(false);

        let secret = self.password_row.text().to_string();
        glib::spawn_future_local(clone!(
            #[weak(rename_to = dialog)]
            self.obj(),
            async move {
                let result =
                    run_blocking(move || session.unlock_volume(&snapshot, &path, &secret)).await;
                let imp = dialog.imp();
                // Cancelled while unlocking: nobody waits for the result any more. The unlock
                // itself has run; a right key stays cached in the session, so the next visit
                // opens the volume without asking.
                let Some(sender) = imp.result.take() else {
                    return;
                };
                imp.set_busy(false);
                match result {
                    Ok(()) => {
                        imp.password_row.set_text("");
                        let _ = sender.try_send(());
                        dialog.close();
                    }
                    Err(err) => {
                        imp.result.replace(Some(sender));
                        imp.show_error(&err);
                    }
                }
            }
        ));
    }
}

impl VolumeDialog {
    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.unlock_stack
            .set_visible_child_name(if busy { "busy" } else { "label" });
        self.password_row.set_sensitive(!busy);
        self.update_sensitivity();
    }

    fn update_sensitivity(&self) {
        self.unlock_button
            .set_sensitive(!self.busy.get() && !self.password_row.text().is_empty());
    }

    fn show_error(&self, err: &Error) {
        let (message, wrong_key) = match err {
            Error::WrongPassword => ("Wrong recovery key or passphrase".to_owned(), true),
            err => (format!("Could not unlock the volume: {err}"), false),
        };
        self.error_label.set_label(&message);
        self.error_label.set_visible(true);
        if wrong_key {
            self.password_row.add_css_class("error");
            self.password_row.grab_focus();
            self.password_row.select_region(0, -1);
        }
    }
}

impl ObjectImpl for VolumeDialog {}
impl WidgetImpl for VolumeDialog {}
impl AdwDialogImpl for VolumeDialog {}
