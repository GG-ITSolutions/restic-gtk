use std::cell::{Cell, RefCell};

use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, glib, glib::clone};
use restic_gtk::{
    backend::{Error, Session},
    config,
};

use crate::ui::{objects::RepoObject, run_blocking};

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/unlock-dialog.ui")]
pub struct UnlockDialog {
    #[template_child]
    pub unlock_button: TemplateChild<gtk::Button>,
    #[template_child]
    pub unlock_stack: TemplateChild<gtk::Stack>,
    #[template_child]
    pub description_label: TemplateChild<gtk::Label>,
    #[template_child]
    pub password_row: TemplateChild<adw::PasswordEntryRow>,
    /// The secret key of S3 repositories, hidden for others.
    #[template_child]
    pub secret_row: TemplateChild<adw::PasswordEntryRow>,
    #[template_child]
    pub error_label: TemplateChild<gtk::Label>,

    pub repo: RefCell<Option<RepoObject>>,
    /// Present until the dialog is closed; sending signals success.
    pub result: RefCell<Option<async_channel::Sender<()>>>,
    busy: Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for UnlockDialog {
    const NAME: &'static str = "ResticGtkUnlockDialog";
    type Type = super::UnlockDialog;
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
impl UnlockDialog {
    #[template_callback]
    fn on_changed(&self) {
        self.password_row.remove_css_class("error");
        self.secret_row.remove_css_class("error");
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
        let Some(repo) = self.repo.borrow().clone() else {
            return;
        };
        self.set_busy(true);
        self.error_label.set_visible(false);

        let config = repo.config();
        let password = self.password_row.text().to_string();
        let secret_key = self
            .secret_row
            .is_visible()
            .then(|| self.secret_row.text().to_string());
        let cache_dir = config::cache_dir();
        glib::spawn_future_local(clone!(
            #[weak(rename_to = dialog)]
            self.obj(),
            async move {
                let result = run_blocking(move || {
                    Session::open(&config, &password, secret_key.as_deref(), &cache_dir)
                })
                .await;
                let imp = dialog.imp();
                // Cancelled while connecting: the user no longer wants this unlocked.
                let Some(sender) = imp.result.take() else {
                    return;
                };
                imp.set_busy(false);
                match result {
                    Ok(session) => {
                        imp.password_row.set_text("");
                        imp.secret_row.set_text("");
                        repo.set_session(session);
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

impl UnlockDialog {
    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.unlock_stack
            .set_visible_child_name(if busy { "busy" } else { "label" });
        self.password_row.set_sensitive(!busy);
        self.secret_row.set_sensitive(!busy);
        self.update_sensitivity();
    }

    fn update_sensitivity(&self) {
        let secret_missing = self.secret_row.is_visible() && self.secret_row.text().is_empty();
        self.unlock_button.set_sensitive(
            !self.busy.get() && !self.password_row.text().is_empty() && !secret_missing,
        );
    }

    fn show_error(&self, err: &Error) {
        let (message, row) = match err {
            Error::WrongPassword => ("Wrong password".to_owned(), Some(&*self.password_row)),
            Error::AccessDenied => (err.to_string(), Some(&*self.secret_row)),
            Error::NoRepository => (err.to_string(), None),
            err => (format!("Could not open the repository: {err}"), None),
        };
        self.error_label.set_label(&message);
        self.error_label.set_visible(true);
        if let Some(row) = row {
            row.add_css_class("error");
            row.grab_focus();
            row.select_region(0, -1);
        }
    }
}

impl ObjectImpl for UnlockDialog {}
impl WidgetImpl for UnlockDialog {}
impl AdwDialogImpl for UnlockDialog {}
