use std::{
    cell::{Cell, OnceCell, RefCell},
    sync::OnceLock,
};

use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, gdk, gio, glib, glib::clone, glib::subclass::Signal, graphene};

use super::RepoSidebarRow;
use crate::ui::objects::RepoObject;

#[derive(Debug, Default, CompositeTemplate, glib::Properties)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/repo-sidebar.ui")]
#[properties(wrapper_type = super::RepoSidebar)]
pub struct RepoSidebar {
    #[template_child]
    stack: TemplateChild<gtk::Stack>,
    #[template_child]
    list_view: TemplateChild<gtk::ListView>,

    /// The context menu of a repository.
    #[property(get, set = Self::set_menu_model, nullable)]
    menu_model: RefCell<Option<gio::MenuModel>>,
    popover: OnceCell<gtk::PopoverMenu>,
    /// The row showing where a dragged repository would be dropped.
    drop_marker: glib::WeakRef<RepoSidebarRow>,
    /// The repository whose row had the keyboard focus when its menu opened.
    menu_focus: RefCell<Option<RepoObject>>,

    model: OnceCell<gio::ListStore>,
    tree: OnceCell<gtk::TreeListModel>,
    selection: OnceCell<gtk::SingleSelection>,
    /// The repository shown; kept selected when the list changes.
    selected: RefCell<Option<RepoObject>>,
    /// Set while the sidebar is a page of its own, where rows aren't shown selected.
    #[property(get, set = Self::set_collapsed)]
    collapsed: Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for RepoSidebar {
    const NAME: &'static str = "ResticGtkRepoSidebar";
    type Type = super::RepoSidebar;
    type ParentType = adw::Bin;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
    }

    fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
        obj.init_template();
    }
}

#[glib::derived_properties]
impl ObjectImpl for RepoSidebar {
    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![
                Signal::builder("activated")
                    .param_types([RepoObject::static_type()])
                    .build(),
                Signal::builder("setup-menu")
                    .param_types([RepoObject::static_type()])
                    .build(),
                Signal::builder("reorder")
                    .param_types([RepoObject::static_type(), u32::static_type()])
                    .build(),
            ]
        })
    }

    fn constructed(&self) {
        self.parent_constructed();

        let popover = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_parent(&*self.obj());
        // Closing the menu doesn't give the focus back to the row it was opened for.
        popover.connect_closed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |_| {
                // After the popover itself moved the focus.
                if let Some(repo) = imp.menu_focus.take() {
                    glib::idle_add_local_once(clone!(
                        #[weak]
                        imp,
                        move || imp.focus_repo(&repo)
                    ));
                }
            }
        ));
        self.popover.set(popover).expect("constructed once");

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item
                .downcast_ref::<gtk::ListItem>()
                .expect("list views hold list items");
            // Clicks are handled by the row and keys by the list (`list_shortcuts`): the list
            // would open a repository twice on a double click. Only the repository shown is
            // selected, not every one clicked or moved to with the arrow keys.
            item.set_activatable(false);
            item.set_selectable(false);
            // The list item keeps the focus, so the row shows the focus ring.
            let expander = gtk::TreeExpander::builder()
                .hide_expander(true)
                .indent_for_icon(false)
                .focusable(false)
                .child(&RepoSidebarRow::new())
                .build();
            item.set_child(Some(&expander));
        });
        factory.connect_bind(|_, item| {
            let item = item
                .downcast_ref::<gtk::ListItem>()
                .expect("list views hold list items");
            let list_row = item
                .item()
                .and_downcast::<gtk::TreeListRow>()
                .expect("the model is a tree list model");
            let expander = item
                .child()
                .and_downcast::<gtk::TreeExpander>()
                .expect("set up with an expander");
            expander.set_list_row(Some(&list_row));
            row_of(&expander).set_repo(list_row.item().and_downcast());
        });
        factory.connect_unbind(|_, item| {
            let item = item
                .downcast_ref::<gtk::ListItem>()
                .expect("list views hold list items");
            let expander = item
                .child()
                .and_downcast::<gtk::TreeExpander>()
                .expect("set up with an expander");
            expander.set_list_row(None);
            row_of(&expander).set_repo(None);
        });
        self.list_view.set_factory(Some(&factory));
        self.list_view.add_controller(self.list_shortcuts());

        // While dragging near an edge the list scrolls under the resting pointer, without motion
        // events: the marked row no longer is the one under the pointer until it moves again.
        if let Some(adjustment) = self.list_view.vadjustment() {
            adjustment.connect_value_changed(clone!(
                #[weak(rename_to = imp)]
                self,
                move |_| {
                    if let Some(row) = imp.drop_marker.upgrade() {
                        row.clear_drop_marker();
                    }
                }
            ));
        }
    }

    fn dispose(&self) {
        if let Some(popover) = self.popover.get() {
            popover.unparent();
        }
    }
}

impl WidgetImpl for RepoSidebar {
    fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
        self.parent_size_allocate(width, height, baseline);
        if let Some(popover) = self.popover.get() {
            popover.present();
        }
    }
}

impl BinImpl for RepoSidebar {}

impl RepoSidebar {
    fn set_menu_model(&self, menu_model: Option<gio::MenuModel>) {
        if let Some(popover) = self.popover.get() {
            popover.set_menu_model(menu_model.as_ref());
        }
        self.menu_model.replace(menu_model);
    }

    fn set_collapsed(&self, collapsed: bool) {
        self.collapsed.set(collapsed);
        self.update_selection();
    }

    pub(super) fn set_model(&self, model: &gio::ListStore) {
        // Flat for now: no item has children.
        let tree = gtk::TreeListModel::new(model.clone(), false, false, |_| None);
        let selection = gtk::SingleSelection::builder()
            .model(&tree)
            .autoselect(false)
            .can_unselect(true)
            .build();
        self.list_view.set_model(Some(&selection));

        model.connect_items_changed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |_, _, _, _| {
                imp.update_stack();
                // A moved repository is removed and inserted again, which drops the selection.
                imp.update_selection();
            }
        ));

        self.model.set(model.clone()).expect("model set once");
        self.tree.set(tree).expect("model set once");
        self.selection.set(selection).expect("model set once");
        self.update_stack();
        self.update_selection();
    }

    /// Selects the row of the repository shown, unless collapsed.
    fn update_selection(&self) {
        let Some(selection) = self.selection.get() else {
            return;
        };
        let selected = self.selected.borrow().clone();
        let position = selected
            .filter(|_| !self.collapsed.get())
            .and_then(|repo| self.position(&repo));
        selection.set_selected(position.unwrap_or(gtk::INVALID_LIST_POSITION));
    }

    fn update_stack(&self) {
        let empty = self.model.get().is_none_or(|model| model.n_items() == 0);
        self.stack
            .set_visible_child_name(if empty { "empty" } else { "list" });
    }

    /// The position of `repo` in the list shown.
    fn position(&self, repo: &RepoObject) -> Option<u32> {
        let tree = self.tree.get()?;
        (0..tree.n_items()).find(|&position| {
            tree.row(position)
                .and_then(|row| row.item())
                .is_some_and(|item| item == *repo.upcast_ref::<glib::Object>())
        })
    }

    pub(super) fn select(&self, repo: &RepoObject) {
        self.selected.replace(Some(repo.clone()));
        self.update_selection();
        if let Some(position) = self.position(repo) {
            self.list_view
                .scroll_to(position, gtk::ListScrollFlags::NONE, None);
        }
    }

    pub(super) fn unselect(&self) {
        self.selected.replace(None);
        self.update_selection();
    }

    pub(super) fn focus_repo(&self, repo: &RepoObject) {
        let Some(position) = self.position(repo) else {
            return;
        };
        self.list_view
            .scroll_to(position, gtk::ListScrollFlags::FOCUS, None);
        // Scrolling doesn't take the focus from outside the list, e.g. from a closed menu.
        match self.item_widget(repo) {
            Some(item) => {
                item.grab_focus();
            }
            // Not created yet: the list was scrolled to it just now.
            None => {
                glib::idle_add_local_once(clone!(
                    #[weak(rename_to = imp)]
                    self,
                    #[strong]
                    repo,
                    move || {
                        if let Some(item) = imp.item_widget(&repo) {
                            item.grab_focus();
                        }
                    }
                ));
            }
        }
    }

    /// The list item widget showing `repo`, if there is one.
    fn item_widget(&self, repo: &RepoObject) -> Option<gtk::Widget> {
        let mut child = self.list_view.first_child();
        while let Some(item) = child {
            if item
                .first_child()
                .and_downcast::<gtk::TreeExpander>()
                .is_some_and(|expander| row_of(&expander).repo().as_ref() == Some(repo))
            {
                return Some(item);
            }
            child = item.next_sibling();
        }
        None
    }

    /// The row with the keyboard focus.
    fn focused_row(&self) -> Option<RepoSidebarRow> {
        let expander = self
            .list_view
            .focus_child()?
            .first_child()
            .and_downcast::<gtk::TreeExpander>()?;
        Some(row_of(&expander))
    }

    /// Enter opens the focused repository, the menu key or Shift+F10 opens its context menu.
    fn list_shortcuts(&self) -> gtk::ShortcutController {
        let controller = gtk::ShortcutController::new();
        // List items take Enter themselves, even when not activatable.
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let shortcut = |trigger: &str, action: fn(&RepoSidebarRow)| {
            gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(gtk::CallbackAction::new(clone!(
                    #[weak(rename_to = imp)]
                    self,
                    #[upgrade_or]
                    glib::Propagation::Proceed,
                    move |_, _| match imp.focused_row() {
                        Some(row) => {
                            action(&row);
                            glib::Propagation::Stop
                        }
                        None => glib::Propagation::Proceed,
                    }
                ))),
            )
        };
        controller.add_shortcut(shortcut(
            "Return|KP_Enter|space",
            RepoSidebarRow::activate_repo,
        ));
        controller.add_shortcut(shortcut("<Shift>F10|Menu", |row| row.popup_menu(None)));
        controller
    }

    pub(super) fn popup_menu(&self, row: &RepoSidebarRow, x: f64, y: f64) {
        let obj = self.obj();
        let popover = self.popover.get().expect("set in constructed");
        let focused = self.focused_row().as_ref() == Some(row);
        self.menu_focus
            .replace(focused.then(|| row.repo()).flatten());
        let Some(point) = row.compute_point(&*obj, &graphene::Point::new(x as f32, y as f32))
        else {
            return;
        };
        popover.set_pointing_to(Some(&gdk::Rectangle::new(
            point.x() as i32,
            point.y() as i32,
            1,
            1,
        )));
        popover.popup();
    }

    pub(super) fn set_drop_marker(&self, row: &RepoSidebarRow) {
        if let Some(previous) = self.drop_marker.upgrade()
            && previous != *row
        {
            previous.clear_drop_marker();
        }
        self.drop_marker.set(Some(row));
    }

    /// The position of `dragged` after dropping it before or after `target`, if it moves.
    pub(super) fn drop_index(
        &self,
        dragged: &RepoObject,
        target: &RepoObject,
        after: bool,
    ) -> Option<u32> {
        let model = self.model.get()?;
        let from = model.find(dragged)?;
        let to = model.find(target)? + u32::from(after);
        // Removing the dragged repository first shifts everything after it up by one.
        let index = if from < to { to - 1 } else { to };
        (index != from).then_some(index)
    }
}

fn row_of(expander: &gtk::TreeExpander) -> RepoSidebarRow {
    expander
        .child()
        .and_downcast()
        .expect("expanders hold sidebar rows")
}
