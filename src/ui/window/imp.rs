use std::cell::{Cell, OnceCell, RefCell};

use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, glib, glib::clone};
use restic_gtk::{backend::RepoStore, config};

use crate::ui::{
    objects::{RepoList, RepoObject},
    repo_dialog::RepoDialog,
    repo_info_dialog::RepoInfoDialog,
    restore_dialog::RestoreDialog,
    show_toast,
    snapshots_page::SnapshotsPage,
};

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/window.ui")]
pub struct ResticGtkWindow {
    #[template_child]
    pub toast_overlay: TemplateChild<adw::ToastOverlay>,
    #[template_child]
    split_view: TemplateChild<adw::NavigationSplitView>,
    #[template_child]
    sidebar: TemplateChild<adw::Sidebar>,
    #[template_child]
    content_view: TemplateChild<adw::NavigationView>,
    #[template_child]
    welcome_page: TemplateChild<adw::NavigationPage>,
    #[template_child]
    welcome_stack: TemplateChild<gtk::Stack>,

    repos: OnceCell<RepoList>,
    /// The repository shown in the content area.
    shown: RefCell<Option<RepoObject>>,
    /// The repository whose context menu was opened last.
    menu_target: RefCell<Option<RepoObject>>,
    /// Set once the user confirmed closing during a restore.
    force_close: Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for ResticGtkWindow {
    const NAME: &'static str = "ResticGtkWindow";
    type Type = super::ResticGtkWindow;
    type ParentType = adw::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();

        klass.install_action("win.add-repository", None, |win, _, _| {
            win.imp().add_repository()
        });
        klass.install_action("win.edit-repository", None, |win, _, _| {
            win.imp().edit_repository()
        });
        klass.install_action("win.repository-details", None, |win, _, _| {
            if let Some(repo) = win.imp().action_target() {
                RepoInfoDialog::show(win, &repo);
            }
        });
        klass.install_action("win.lock-repository", None, |win, _, _| {
            if let Some(repo) = win.imp().action_target() {
                repo.lock();
            }
        });
        klass.install_action("win.remove-repository", None, |win, _, _| {
            win.imp().remove_repository()
        });
    }

    fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for ResticGtkWindow {
    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();

        let (repos, error) = RepoList::load(RepoStore::new(&config::config_dir()));
        if let Some(error) = error {
            show_toast(
                &*obj,
                &format!("The repository list could not be read and was moved aside: {error}"),
            );
        }

        let section = adw::SidebarSection::new();
        section.bind_model(Some(repos.model()), |item| {
            let repo = item
                .downcast_ref::<RepoObject>()
                .expect("RepoList holds RepoObjects");
            let sidebar_item = adw::SidebarItem::new("");
            repo.bind_property("name", &sidebar_item, "title")
                .sync_create()
                .build();
            repo.bind_property("display-location", &sidebar_item, "subtitle")
                .sync_create()
                .build();
            repo.bind_property("unlocked", &sidebar_item, "icon-name")
                .transform_to(|_, unlocked: bool| {
                    Some(if unlocked {
                        "changes-allow-symbolic"
                    } else {
                        "changes-prevent-symbolic"
                    })
                })
                .sync_create()
                .build();
            sidebar_item
        });
        self.sidebar.append(section);
        // Nothing is shown yet, so nothing is selected.
        self.sidebar.set_selected(gtk::INVALID_LIST_POSITION);

        self.sidebar.connect_activated(clone!(
            #[weak]
            obj,
            move |_, index| {
                if let Some(repo) = obj.imp().repos().get(index) {
                    obj.imp().show_repository(&repo);
                }
            }
        ));
        self.sidebar.connect_setup_menu(clone!(
            #[weak]
            obj,
            move |_, item| {
                let imp = obj.imp();
                let repo = item.and_then(|item| imp.repos().get(item.section_index()));
                let unlocked = repo.as_ref().is_some_and(RepoObject::unlocked);
                obj.action_set_enabled("win.repository-details", unlocked);
                obj.action_set_enabled("win.lock-repository", unlocked);
                imp.menu_target.replace(repo);
            }
        ));
        repos.model().connect_items_changed(clone!(
            #[weak]
            obj,
            move |_, _, _, _| obj.imp().update_welcome()
        ));

        self.repos.set(repos).expect("constructed once");
        self.update_welcome();
    }
}

impl WidgetImpl for ResticGtkWindow {}

impl WindowImpl for ResticGtkWindow {
    fn close_request(&self) -> glib::Propagation {
        let restoring = self
            .obj()
            .visible_dialog()
            .and_downcast::<RestoreDialog>()
            .is_some_and(|dialog| dialog.is_restoring());
        if !restoring || self.force_close.get() {
            return self.parent_close_request();
        }

        let dialog = adw::AlertDialog::builder()
            .heading("Restore in Progress")
            .body("Closing now leaves the restored files incomplete.")
            .close_response("cancel")
            .default_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", "_Keep Restoring"), ("close", "_Close Anyway")]);
        dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
        let obj = self.obj();
        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move {
                if dialog.choose_future(Some(&obj)).await == "close" {
                    obj.imp().force_close.set(true);
                    obj.close();
                }
            }
        ));
        glib::Propagation::Stop
    }
}

impl ApplicationWindowImpl for ResticGtkWindow {}
impl AdwApplicationWindowImpl for ResticGtkWindow {}

impl ResticGtkWindow {
    fn repos(&self) -> &RepoList {
        self.repos.get().expect("set in constructed")
    }

    /// The repository a menu action applies to: the one the menu was opened for,
    /// otherwise the one shown.
    fn action_target(&self) -> Option<RepoObject> {
        self.menu_target
            .borrow()
            .clone()
            .or_else(|| self.shown.borrow().clone())
    }

    fn update_welcome(&self) {
        let empty = self.repos().model().n_items() == 0;
        self.welcome_stack
            .set_visible_child_name(if empty { "empty" } else { "select" });
    }

    fn show_repository(&self, repo: &RepoObject) {
        let already_shown = self.shown.borrow().as_ref() == Some(repo);
        if !already_shown {
            self.shown.replace(Some(repo.clone()));
            let page = SnapshotsPage::new(repo);
            self.content_view.replace(&[page.upcast()]);
        }
        self.split_view.set_show_content(true);
        if !repo.unlocked()
            && let Some(page) = self.content_view.find_page("snapshots")
        {
            page.downcast::<SnapshotsPage>().unwrap().unlock();
        }
    }

    fn show_welcome(&self) {
        self.shown.replace(None);
        self.sidebar.set_selected(gtk::INVALID_LIST_POSITION);
        self.content_view.replace(&[self.welcome_page.get()]);
        self.split_view.set_show_content(false);
    }

    fn add_repository(&self) {
        let obj = self.obj();
        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move {
                let Some(config) = RepoDialog::run(&obj, None).await else {
                    return;
                };
                let imp = obj.imp();
                match imp.repos().add(config) {
                    Ok(repo) => {
                        if let Some(position) = imp.repos().model().find(&repo) {
                            imp.sidebar.set_selected(position);
                        }
                        imp.show_repository(&repo);
                    }
                    Err(err) => show_toast(&obj, &format!("Could not save the repository: {err}")),
                }
            }
        ));
    }

    fn edit_repository(&self) {
        let Some(repo) = self.action_target() else {
            return;
        };
        let obj = self.obj();
        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move {
                let Some(config) = RepoDialog::run(&obj, Some(repo.config())).await else {
                    return;
                };
                if let Err(err) = obj.imp().repos().update(&repo, config) {
                    show_toast(&obj, &format!("Could not save the repository: {err}"));
                }
            }
        ));
    }

    fn remove_repository(&self) {
        let Some(repo) = self.action_target() else {
            return;
        };
        let dialog = adw::AlertDialog::builder()
            .heading(format!("Remove “{}”?", repo.name()))
            .body("It is only removed from this list. The repository and its snapshots are not touched.")
            .close_response("cancel")
            .default_response("cancel")
            .build();
        dialog.add_responses(&[("cancel", "_Cancel"), ("remove", "_Remove")]);
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);

        let obj = self.obj();
        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move {
                if dialog.choose_future(Some(&obj)).await != "remove" {
                    return;
                }
                let imp = obj.imp();
                if let Err(err) = imp.repos().remove(&repo) {
                    show_toast(&obj, &format!("Could not remove the repository: {err}"));
                    return;
                }
                if imp.shown.borrow().as_ref() == Some(&repo) {
                    imp.show_welcome();
                }
                imp.menu_target.replace(None);
            }
        ));
    }
}
