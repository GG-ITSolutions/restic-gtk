use std::cell::{Cell, OnceCell, RefCell};

use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, glib, glib::clone};
use restic_gtk::backend::{Snapshot, jiff::Zoned};

use crate::ui::{
    browser_page::BrowserPage,
    details,
    format::{self, DateGroup},
    objects::RepoObject,
    run_blocking,
    unlock_dialog::UnlockDialog,
};

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/snapshots-page.ui")]
pub struct SnapshotsPage {
    #[template_child]
    window_title: TemplateChild<adw::WindowTitle>,
    #[template_child]
    lock_button: TemplateChild<gtk::Button>,
    #[template_child]
    details_button: TemplateChild<gtk::Button>,
    #[template_child]
    reload_button: TemplateChild<gtk::Button>,
    #[template_child]
    stack: TemplateChild<gtk::Stack>,
    #[template_child]
    error_status: TemplateChild<adw::StatusPage>,
    #[template_child]
    scrolled_window: TemplateChild<gtk::ScrolledWindow>,
    #[template_child]
    groups_box: TemplateChild<gtk::Box>,

    pub repo: OnceCell<RepoObject>,
    repo_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    bindings: RefCell<Vec<glib::Binding>>,
    /// Incremented per load, so results of superseded loads are dropped.
    generation: Cell<u64>,
}

#[glib::object_subclass]
impl ObjectSubclass for SnapshotsPage {
    const NAME: &'static str = "ResticGtkSnapshotsPage";
    type Type = super::SnapshotsPage;
    type ParentType = adw::NavigationPage;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();

        klass.install_action("snapshots.unlock", None, |page, _, _| page.imp().unlock());
        klass.install_action("snapshots.lock", None, |page, _, _| {
            page.imp().repo().lock()
        });
        klass.install_action("snapshots.reload", None, |page, _, _| page.imp().reload());
        klass.install_action("snapshots.details", None, |page, _, _| {
            details::show_repo(page, page.imp().repo())
        });
    }

    fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for SnapshotsPage {
    fn dispose(&self) {
        // The repository outlives its pages.
        if let Some(repo) = self.repo.get() {
            for handler in self.repo_handlers.take() {
                repo.disconnect(handler);
            }
        }
        for binding in self.bindings.take() {
            binding.unbind();
        }
    }
}

impl WidgetImpl for SnapshotsPage {}
impl NavigationPageImpl for SnapshotsPage {}

impl SnapshotsPage {
    fn repo(&self) -> &RepoObject {
        self.repo.get().expect("repo is set on construction")
    }

    pub fn set_repo(&self, repo: RepoObject) {
        let obj = self.obj();
        self.bindings.replace(vec![
            repo.bind_property("name", &*obj, "title")
                .sync_create()
                .build(),
            repo.bind_property("name", &*self.window_title, "title")
                .sync_create()
                .build(),
            repo.bind_property("display-location", &*self.window_title, "subtitle")
                .sync_create()
                .build(),
        ]);
        let handler = repo.connect_unlocked_notify(clone!(
            #[weak(rename_to = page)]
            obj,
            move |_| page.imp().on_lock_changed()
        ));
        self.repo_handlers.replace(vec![handler]);
        self.repo.set(repo).expect("repo is set once");
        self.on_lock_changed();
    }

    fn on_lock_changed(&self) {
        let unlocked = self.repo().unlocked();
        self.lock_button.set_visible(unlocked);
        self.details_button.set_visible(unlocked);
        self.reload_button.set_visible(unlocked);
        if unlocked {
            self.reload();
        } else {
            self.generation.set(self.generation.get() + 1);
            self.clear_groups();
            self.stack.set_visible_child_name("locked");
            // Pages of the locked repository must not stay reachable.
            if let Some(view) = self.obj().ancestor(adw::NavigationView::static_type()) {
                view.downcast::<adw::NavigationView>()
                    .unwrap()
                    .pop_to_page(&*self.obj());
            }
        }
    }

    /// Asks for the password; on success `unlocked` changes and the snapshots load.
    pub fn unlock(&self) {
        let obj = self.obj();
        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move {
                let repo = obj.imp().repo().clone();
                UnlockDialog::run(&obj, &repo).await;
            }
        ));
    }

    fn reload(&self) {
        let Some(session) = self.repo().session() else {
            return;
        };
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.stack.set_visible_child_name("loading");

        glib::spawn_future_local(clone!(
            #[weak(rename_to = page)]
            self.obj(),
            async move {
                let result = run_blocking(move || session.snapshots()).await;
                let imp = page.imp();
                if imp.generation.get() != generation {
                    return;
                }
                match result {
                    Ok(snapshots) if snapshots.is_empty() => {
                        imp.clear_groups();
                        imp.stack.set_visible_child_name("empty");
                    }
                    Ok(snapshots) => {
                        imp.show_snapshots(snapshots);
                        imp.stack.set_visible_child_name("list");
                    }
                    Err(err) => {
                        imp.error_status.set_description(Some(&err.to_string()));
                        imp.stack.set_visible_child_name("error");
                        return;
                    }
                }
                // Browsing needs the index; load it while the user picks a snapshot.
                imp.repo().ensure_indexed();
            }
        ));
    }

    fn clear_groups(&self) {
        while let Some(child) = self.groups_box.first_child() {
            self.groups_box.remove(&child);
        }
    }

    /// One boxed list per date group; `snapshots` are sorted newest first.
    fn show_snapshots(&self, snapshots: Vec<Snapshot>) {
        self.clear_groups();
        let now = Zoned::now();
        let mut current: Option<(DateGroup, adw::PreferencesGroup)> = None;
        for snapshot in snapshots {
            let group = DateGroup::of(&snapshot.time, &now);
            if current.as_ref().is_none_or(|(g, _)| *g != group) {
                let widget = adw::PreferencesGroup::builder()
                    .title(group.title())
                    .build();
                self.groups_box.append(&widget);
                current = Some((group.clone(), widget));
            }
            let (group, widget) = current.as_ref().unwrap();
            widget.add(&self.snapshot_row(snapshot, group));
        }
        self.scrolled_window.vadjustment().set_value(0.0);
    }

    fn snapshot_row(&self, snapshot: Snapshot, group: &DateGroup) -> adw::ActionRow {
        let mut subtitle = format!("{} · {}", snapshot.hostname, snapshot.paths.join(", "));
        if !snapshot.tags.is_empty() {
            subtitle.push_str(" · ");
            subtitle.push_str(&snapshot.tags.join(", "));
        }
        let row = adw::ActionRow::builder()
            .title(format::snapshot_time(&snapshot.time, group))
            .subtitle(glib::markup_escape_text(&subtitle))
            .subtitle_lines(1)
            .activatable(true)
            .build();

        let details = gtk::Box::builder().spacing(12).build();
        if let Some(size) = snapshot.size() {
            details.append(&dim_label(&format::size(size), &["numeric"]));
        }
        details.append(&dim_label(snapshot.short_id(), &["monospace"]));
        row.add_suffix(&details);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

        row.connect_activated(clone!(
            #[weak(rename_to = page)]
            self.obj(),
            move |_| page.imp().open_snapshot(&snapshot)
        ));
        row
    }

    fn open_snapshot(&self, snapshot: &Snapshot) {
        let Some(view) = self.obj().ancestor(adw::NavigationView::static_type()) else {
            return;
        };
        let page = BrowserPage::new(self.repo(), snapshot.clone());
        view.downcast::<adw::NavigationView>().unwrap().push(&page);
    }
}

fn dim_label(text: &str, classes: &[&str]) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("dimmed");
    for class in classes {
        label.add_css_class(class);
    }
    label
}
