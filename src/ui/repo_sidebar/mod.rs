//! The repository list in the sidebar, reorderable by drag and drop.

mod imp;
mod row;

use adw::subclass::prelude::*;
use gtk::{gio, glib, prelude::*};

use crate::ui::objects::RepoObject;

use row::RepoSidebarRow;

glib::wrapper! {
    pub struct RepoSidebar(ObjectSubclass<imp::RepoSidebar>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl RepoSidebar {
    /// Shows the repositories of `model`, in its order.
    pub fn set_model(&self, model: &gio::ListStore) {
        self.imp().set_model(model);
    }

    /// Marks `repo` as the one shown, also after it was moved.
    pub fn select(&self, repo: &RepoObject) {
        self.imp().select(repo);
    }

    pub fn unselect(&self) {
        self.imp().unselect();
    }

    /// Moves the keyboard focus to `repo`, e.g. after it was moved with the menu.
    pub fn focus_repo(&self, repo: &RepoObject) {
        self.imp().focus_repo(repo);
    }

    fn activate_repo(&self, repo: &RepoObject) {
        self.imp().select(repo);
        self.emit_by_name::<()>("activated", &[repo]);
    }

    fn popup_menu(&self, repo: &RepoObject, row: &RepoSidebarRow, x: f64, y: f64) {
        self.emit_by_name::<()>("setup-menu", &[repo]);
        self.imp().popup_menu(row, x, y);
    }

    fn set_drop_marker(&self, row: &RepoSidebarRow) {
        self.imp().set_drop_marker(row);
    }

    /// Handles a drop of `dragged` before or after `target`.
    fn drop_repo(&self, dragged: &RepoObject, target: &RepoObject, after: bool) {
        if let Some(index) = self.imp().drop_index(dragged, target, after) {
            self.emit_by_name::<()>("reorder", &[dragged, &index]);
        }
    }

    /// Emitted when a repository is clicked or Enter is pressed on it.
    pub fn connect_activated<F: Fn(&Self, &RepoObject) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_closure(
            "activated",
            false,
            glib::closure_local!(move |sidebar: &Self, repo: &RepoObject| f(sidebar, repo)),
        )
    }

    /// Emitted before the context menu opens for a repository.
    pub fn connect_setup_menu<F: Fn(&Self, &RepoObject) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_closure(
            "setup-menu",
            false,
            glib::closure_local!(move |sidebar: &Self, repo: &RepoObject| f(sidebar, repo)),
        )
    }

    /// Emitted when a repository was dropped elsewhere, with its new position.
    pub fn connect_reorder<F: Fn(&Self, &RepoObject, u32) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_closure(
            "reorder",
            false,
            glib::closure_local!(move |sidebar: &Self, repo: &RepoObject, index: u32| {
                f(sidebar, repo, index)
            }),
        )
    }
}
