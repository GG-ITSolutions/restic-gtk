//! One repository in the sidebar: drag source, drop target and context menu.

use adw::{prelude::*, subclass::prelude::*};
use gtk::{gdk, glib, glib::clone};

use super::RepoSidebar;
use crate::ui::objects::RepoObject;

mod imp {
    use std::cell::RefCell;

    use super::*;

    #[derive(Debug, Default)]
    pub struct RepoSidebarRow {
        pub icon: gtk::Image,
        pub title: gtk::Label,
        pub subtitle: gtk::Label,
        /// Set by the list factory; rows are recycled for other repositories.
        pub repo: RefCell<Option<RepoObject>>,
        pub bindings: RefCell<Vec<glib::Binding>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RepoSidebarRow {
        const NAME: &'static str = "ResticGtkRepoSidebarRow";
        type Type = super::RepoSidebarRow;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("sidebar-row");
        }
    }

    impl ObjectImpl for RepoSidebarRow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            for label in [&self.title, &self.subtitle] {
                label.set_xalign(0.0);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            }
            self.subtitle.add_css_class("caption");
            self.subtitle.add_css_class("dimmed");
            let labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
            labels.set_valign(gtk::Align::Center);
            labels.append(&self.title);
            labels.append(&self.subtitle);
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            content.append(&self.icon);
            content.append(&labels);
            obj.set_child(Some(&content));
            // Drop markers span the whole row.
            obj.set_hexpand(true);

            obj.setup_drag();
            obj.setup_drop();
            obj.setup_clicks();
        }

        fn dispose(&self) {
            for binding in self.bindings.take() {
                binding.unbind();
            }
        }
    }

    impl WidgetImpl for RepoSidebarRow {}
    impl BinImpl for RepoSidebarRow {}
}

glib::wrapper! {
    pub struct RepoSidebarRow(ObjectSubclass<imp::RepoSidebarRow>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl RepoSidebarRow {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn repo(&self) -> Option<RepoObject> {
        self.imp().repo.borrow().clone()
    }

    pub fn set_repo(&self, repo: Option<RepoObject>) {
        let imp = self.imp();
        for binding in imp.bindings.take() {
            binding.unbind();
        }
        if let Some(repo) = &repo {
            imp.bindings.replace(vec![
                repo.bind_property("name", &imp.title, "label")
                    .sync_create()
                    .build(),
                repo.bind_property("display-location", &imp.subtitle, "label")
                    .sync_create()
                    .build(),
                repo.bind_property("unlocked", &imp.icon, "icon-name")
                    .transform_to(|_, unlocked: bool| {
                        Some(if unlocked {
                            "changes-allow-symbolic"
                        } else {
                            "changes-prevent-symbolic"
                        })
                    })
                    .sync_create()
                    .build(),
            ]);
        }
        imp.repo.replace(repo);
        self.clear_drop_marker();
    }

    fn sidebar(&self) -> Option<RepoSidebar> {
        self.ancestor(RepoSidebar::static_type()).and_downcast()
    }

    /// Focuses and opens the repository, as a click does.
    fn click(&self) {
        if let (Some(sidebar), Some(repo)) = (self.sidebar(), self.repo()) {
            // Dialogs opened for it give the focus back to this row.
            sidebar.focus_repo(&repo);
            sidebar.activate_repo(&repo);
        }
    }

    /// Opens the repository, as Enter does.
    pub fn activate_repo(&self) {
        if let (Some(sidebar), Some(repo)) = (self.sidebar(), self.repo()) {
            sidebar.activate_repo(&repo);
        }
    }

    /// Opens the context menu at `point` in row coordinates, or at the middle of the row.
    pub fn popup_menu(&self, point: Option<(f64, f64)>) {
        if let (Some(sidebar), Some(repo)) = (self.sidebar(), self.repo()) {
            let (x, y) = point.unwrap_or((
                f64::from(self.width()) / 2.0,
                f64::from(self.height()) / 2.0,
            ));
            sidebar.popup_menu(&repo, self, x, y);
        }
    }

    fn setup_drag(&self) {
        let source = gtk::DragSource::new();
        source.set_actions(gdk::DragAction::MOVE);
        source.connect_prepare(clone!(
            #[weak(rename_to = row)]
            self,
            #[upgrade_or]
            None,
            move |source, x, y| {
                let repo = row.repo()?;
                source.set_icon(
                    Some(&gtk::WidgetPaintable::new(Some(&row))),
                    x as i32,
                    y as i32,
                );
                Some(gdk::ContentProvider::for_value(&repo.to_value()))
            }
        ));
        self.add_controller(source);
    }

    fn setup_drop(&self) {
        let target = gtk::DropTarget::new(RepoObject::static_type(), gdk::DragAction::MOVE);
        // The dragged repository is needed while hovering, to refuse drops onto itself.
        target.set_preload(true);
        target.connect_motion(clone!(
            #[weak(rename_to = row)]
            self,
            #[upgrade_or]
            gdk::DragAction::empty(),
            move |target, _, y| {
                let dragged = target
                    .value()
                    .and_then(|value| value.get::<RepoObject>().ok());
                if dragged.is_none() || dragged == row.repo() {
                    row.clear_drop_marker();
                    return gdk::DragAction::empty();
                }
                row.show_drop_marker(row.is_lower_half(y));
                gdk::DragAction::MOVE
            }
        ));
        target.connect_leave(clone!(
            #[weak(rename_to = row)]
            self,
            move |_| row.clear_drop_marker()
        ));
        target.connect_drop(clone!(
            #[weak(rename_to = row)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, y| {
                row.clear_drop_marker();
                let (Ok(dragged), Some(repo), Some(sidebar)) =
                    (value.get::<RepoObject>(), row.repo(), row.sidebar())
                else {
                    return false;
                };
                if dragged == repo {
                    return false;
                }
                sidebar.drop_repo(&dragged, &repo, row.is_lower_half(y));
                true
            }
        ));
        self.add_controller(target);
    }

    fn setup_clicks(&self) {
        let primary = gtk::GestureClick::new();
        // Only single clicks: the second click of a double click would open it again.
        primary.connect_released(clone!(
            #[weak(rename_to = row)]
            self,
            move |_, n_press, _, _| {
                if n_press == 1 {
                    row.click();
                }
            }
        ));
        self.add_controller(primary);

        let secondary = gtk::GestureClick::new();
        secondary.set_button(gdk::BUTTON_SECONDARY);
        secondary.connect_pressed(clone!(
            #[weak(rename_to = row)]
            self,
            move |gesture, _, x, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                row.popup_menu(Some((x, y)));
            }
        ));
        self.add_controller(secondary);

        let long_press = gtk::GestureLongPress::new();
        long_press.set_touch_only(true);
        long_press.connect_pressed(clone!(
            #[weak(rename_to = row)]
            self,
            move |gesture, x, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                row.popup_menu(Some((x, y)));
            }
        ));
        self.add_controller(long_press);
    }

    fn is_lower_half(&self, y: f64) -> bool {
        y > f64::from(self.height()) / 2.0
    }

    /// Shows where a drop lands. Only one row at a time: while the list scrolls under a resting
    /// pointer, the row that was hovered gets no `leave`.
    fn show_drop_marker(&self, after: bool) {
        if let Some(sidebar) = self.sidebar() {
            sidebar.set_drop_marker(self);
        }
        self.clear_drop_marker();
        self.add_css_class(if after { "drop-after" } else { "drop-before" });
    }

    pub fn clear_drop_marker(&self) {
        self.remove_css_class("drop-before");
        self.remove_css_class("drop-after");
    }
}

impl Default for RepoSidebarRow {
    fn default() -> Self {
        Self::new()
    }
}
