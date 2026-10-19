use std::{cell::RefCell, collections::BTreeMap};

use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, glib};
use restic_gtk::backend::{RepoConfig, S3_LOCATION, S3Location, validate_location};

/// Position of S3 in the type row; 0 is a local path or rest-server.
pub const TYPE_S3: u32 = 1;

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/repo-dialog.ui")]
pub struct RepoDialog {
    #[template_child]
    pub save_button: TemplateChild<gtk::Button>,
    #[template_child]
    pub name_row: TemplateChild<adw::EntryRow>,
    #[template_child]
    pub type_row: TemplateChild<adw::ComboRow>,
    #[template_child]
    pub location_group: TemplateChild<adw::PreferencesGroup>,
    #[template_child]
    pub location_row: TemplateChild<adw::EntryRow>,
    #[template_child]
    pub s3_group: TemplateChild<adw::PreferencesGroup>,
    #[template_child]
    pub endpoint_row: TemplateChild<adw::EntryRow>,
    #[template_child]
    pub bucket_row: TemplateChild<adw::EntryRow>,
    #[template_child]
    pub root_row: TemplateChild<adw::EntryRow>,
    #[template_child]
    pub region_row: TemplateChild<adw::EntryRow>,
    #[template_child]
    pub access_key_row: TemplateChild<adw::EntryRow>,
    #[template_child]
    pub virtual_host_row: TemplateChild<adw::SwitchRow>,
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
    fn on_type_changed(&self) {
        let s3 = self.is_s3();
        self.location_group.set_visible(!s3);
        self.s3_group.set_visible(s3);
        self.on_changed();
    }

    /// A pasted restic S3 URL switches to the S3 form.
    #[template_callback]
    fn on_location_changed(&self) {
        if let Some(s3) = S3Location::parse_restic(&self.location_row.text()) {
            self.location_row.set_text("");
            self.set_s3(&s3);
            self.type_row.set_selected(TYPE_S3);
            self.access_key_row.grab_focus();
        }
        self.on_changed();
    }

    #[template_callback]
    fn on_changed(&self) {
        let location = self.location_row.text();
        let (error, started) = if self.is_s3() {
            (self.s3().validate().err(), self.s3_fields_filled())
        } else {
            (
                validate_location(&location).err(),
                !location.trim().is_empty(),
            )
        };
        // Only complain once there is something to complain about.
        let shown = error.as_deref().filter(|_| started);
        if shown.is_some() && !self.is_s3() {
            self.location_row.add_css_class("error");
        } else {
            self.location_row.remove_css_class("error");
        }
        self.error_label.set_label(shown.unwrap_or_default());
        self.error_label.set_visible(shown.is_some());

        let valid = !self.name_row.text().trim().is_empty() && error.is_none();
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
        let (location, options) = if self.is_s3() {
            (S3_LOCATION.to_owned(), self.s3().to_options())
        } else {
            (self.location_row.text().trim().to_owned(), BTreeMap::new())
        };
        let config = match self.existing.borrow().clone() {
            Some(existing) => RepoConfig {
                name,
                location,
                options,
                ..existing
            },
            None => RepoConfig {
                options,
                ..RepoConfig::new(name, location)
            },
        };
        if let Some(sender) = self.result.take() {
            let _ = sender.try_send(config);
        }
        self.obj().close();
    }
}

impl RepoDialog {
    fn is_s3(&self) -> bool {
        self.type_row.selected() == TYPE_S3
    }

    fn s3(&self) -> S3Location {
        S3Location {
            endpoint: self.endpoint_row.text().into(),
            bucket: self.bucket_row.text().into(),
            root: self.root_row.text().into(),
            region: self.region_row.text().into(),
            access_key_id: self.access_key_row.text().into(),
            virtual_host_style: self.virtual_host_row.is_active(),
        }
    }

    pub fn set_s3(&self, s3: &S3Location) {
        self.endpoint_row.set_text(&s3.endpoint);
        self.bucket_row.set_text(&s3.bucket);
        self.root_row.set_text(&s3.root);
        self.region_row.set_text(&s3.region);
        self.access_key_row.set_text(&s3.access_key_id);
        self.virtual_host_row.set_active(s3.virtual_host_style);
    }

    /// Whether the required S3 fields are filled.
    fn s3_fields_filled(&self) -> bool {
        [&self.endpoint_row, &self.bucket_row, &self.access_key_row]
            .iter()
            .all(|row| !row.text().trim().is_empty())
    }
}

impl ObjectImpl for RepoDialog {}
impl WidgetImpl for RepoDialog {}
impl AdwDialogImpl for RepoDialog {}
