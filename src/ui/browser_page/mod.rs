mod imp;

use adw::subclass::prelude::*;
use gtk::glib;
use restic_gtk::backend::Snapshot;

use crate::ui::objects::RepoObject;

glib::wrapper! {
    /// Browses the folders of one snapshot; opens files and restores the selection.
    pub struct BrowserPage(ObjectSubclass<imp::BrowserPage>)
        @extends adw::NavigationPage, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl BrowserPage {
    pub fn new(repo: &RepoObject, snapshot: Snapshot) -> Self {
        let page: Self = glib::Object::new();
        page.imp().setup(repo.clone(), snapshot);
        page
    }
}
