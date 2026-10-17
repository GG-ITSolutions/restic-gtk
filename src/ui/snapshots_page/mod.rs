mod imp;

use adw::subclass::prelude::*;
use gtk::glib;

use crate::ui::objects::RepoObject;

glib::wrapper! {
    /// The snapshots of one repository, grouped by date. Also shows the locked state.
    pub struct SnapshotsPage(ObjectSubclass<imp::SnapshotsPage>)
        @extends adw::NavigationPage, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl SnapshotsPage {
    pub fn new(repo: &RepoObject) -> Self {
        let page: Self = glib::Object::new();
        page.imp().set_repo(repo.clone());
        page
    }

    /// Asks for the password of the repository.
    pub fn unlock(&self) {
        self.imp().unlock();
    }
}
