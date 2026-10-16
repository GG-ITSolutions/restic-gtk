use adw::subclass::prelude::*;
use gtk::{CompositeTemplate, glib};

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/window.ui")]
pub struct ResticGtkWindow {
    #[template_child]
    pub add_repo_button: TemplateChild<gtk::Button>,
}

#[glib::object_subclass]
impl ObjectSubclass for ResticGtkWindow {
    const NAME: &'static str = "ResticGtkWindow";
    type Type = super::ResticGtkWindow;
    type ParentType = adw::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
    }

    fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for ResticGtkWindow {}
impl WidgetImpl for ResticGtkWindow {}
impl WindowImpl for ResticGtkWindow {}
impl ApplicationWindowImpl for ResticGtkWindow {}
impl AdwApplicationWindowImpl for ResticGtkWindow {}
