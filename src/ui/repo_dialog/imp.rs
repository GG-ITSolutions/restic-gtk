use std::cell::RefCell;

use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, glib};
use restic_gtk::backend::{RepoConfig, validate_location};

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/repo-dialog.ui")]
pub struct RepoDialog {
    #[template_child]
    pub save_button: TemplateChild<gtk::Button>,
    #[template_child]
    pub name_row: TemplateChild<adw::EntryRow>,
    #[template_child]
    pub location_row: TemplateChild<adw::EntryRow>,
    #[template_child]
    pub error_label: TemplateChild<gtk::Label>,

    pub existing: RefCell<Option<RepoConfig>>,
    pub result: RefCell<Option<async_channel::Sender<RepoConfig>>>,
}

#[glib::object_subclass]
impl ObjectSubclass for RepoDialog {
    const NAME: &'static str = "ResticGtkRepoDialog";
    type Type = super::RepoDialog;
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
impl RepoDialog {
    #[template_callback]
    fn on_changed(&self) {
        let location = self.location_row.text();
        // Only complain once there is something to complain about.
        let error = match validate_location(&location) {
            Err(reason) if !location.trim().is_empty() => Some(reason),
            _ => None,
        };
        if error.is_some() {
            self.location_row.add_css_class("error");
        } else {
            self.location_row.remove_css_class("error");
        }
        self.error_label
            .set_label(error.as_deref().unwrap_or_default());
        self.error_label.set_visible(error.is_some());

        let valid = !self.name_row.text().trim().is_empty() && validate_location(&location).is_ok();
        self.save_button.set_sensitive(valid);
    }

    #[template_callback]
    fn on_cancel(&self) {
        self.obj().close();
    }

    #[template_callback]
    fn on_save(&self) {
        // Also reached through activates-default, which ignores sensitivity.
        if !self.save_button.is_sensitive() {
            return;
        }
        let name = self.name_row.text().trim().to_owned();
        let location = self.location_row.text().trim().to_owned();
        let config = match self.existing.borrow().clone() {
            Some(existing) => RepoConfig {
                name,
                location,
                ..existing
            },
            None => RepoConfig::new(name, location),
        };
        if let Some(sender) = self.result.take() {
            let _ = sender.try_send(config);
        }
        self.obj().close();
    }
}

impl ObjectImpl for RepoDialog {}
impl WidgetImpl for RepoDialog {}
impl AdwDialogImpl for RepoDialog {}
