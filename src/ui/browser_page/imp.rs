use std::{
    cell::{Cell, OnceCell, RefCell},
    path::{Path, PathBuf},
    time::Duration,
};

use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, gdk, gio, glib, glib::clone};
use restic_gtk::{
    backend::{EntryKind, Error, RestoreRequest, Snapshot, is_disk_image, jiff::Timestamp},
    config,
};

use crate::ui::{
    add_toast, details, format,
    objects::{EntryObject, RepoObject},
    path_bar::PathBar,
    restore_dialog::RestoreDialog,
    run_blocking, set_plain_description, show_toast,
    volume_dialog::VolumeDialog,
};

/// How long a folder may take to load before the spinner shows, to avoid flicker.
const SPINNER_DELAY: Duration = Duration::from_millis(200);

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/browser-page.ui")]
pub struct BrowserPage {
    #[template_child]
    window_title: TemplateChild<adw::WindowTitle>,
    #[template_child]
    path_bar: TemplateChild<PathBar>,
    #[template_child]
    stack: TemplateChild<gtk::Stack>,
    #[template_child]
    index_progress: TemplateChild<gtk::ProgressBar>,
    #[template_child]
    index_error_status: TemplateChild<adw::StatusPage>,
    #[template_child]
    error_status: TemplateChild<adw::StatusPage>,
    #[template_child]
    column_view: TemplateChild<gtk::ColumnView>,
    #[template_child]
    selection: TemplateChild<gtk::MultiSelection>,
    #[template_child]
    entries: TemplateChild<gio::ListStore>,
    #[template_child]
    selection_label: TemplateChild<gtk::Label>,

    repo: OnceCell<RepoObject>,
    snapshot: OnceCell<Snapshot>,
    repo_handlers: RefCell<Vec<glib::SignalHandlerId>>,
    /// The folder shown, absolute inside the snapshot.
    path: RefCell<PathBuf>,
    /// Incremented per folder load, so results of superseded loads are dropped.
    generation: Cell<u64>,
    started: Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for BrowserPage {
    const NAME: &'static str = "ResticGtkBrowserPage";
    type Type = super::BrowserPage;
    type ParentType = adw::NavigationPage;

    fn class_init(klass: &mut Self::Class) {
        // Referenced by the template and its list item factories.
        EntryObject::ensure_type();
        PathBar::ensure_type();
        klass.bind_template();
        klass.bind_template_callbacks();

        klass.install_action("browser.go-up", None, |page, _, _| page.imp().go_up());
        klass.install_action("browser.open", None, |page, _, _| {
            page.imp().open_selected()
        });
        klass.install_action("browser.restore", None, |page, _, _| {
            page.imp().restore_selected()
        });
        klass.install_action("browser.details", None, |page, _, _| {
            details::show_snapshot(page, page.imp().snapshot())
        });
        klass.add_binding_action(gdk::Key::Up, gdk::ModifierType::ALT_MASK, "browser.go-up");
        klass.add_binding_action(
            gdk::Key::BackSpace,
            gdk::ModifierType::empty(),
            "browser.go-up",
        );
    }

    fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for BrowserPage {
    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        self.path_bar.connect_navigate(clone!(
            #[weak]
            obj,
            move |_, path| obj.imp().load_dir(path, false)
        ));
        self.selection.connect_selection_changed(clone!(
            #[weak]
            obj,
            move |_, _, _| obj.imp().update_actions()
        ));
        self.update_actions();
    }

    fn dispose(&self) {
        if let Some(repo) = self.repo.get() {
            for handler in self.repo_handlers.take() {
                repo.disconnect(handler);
            }
        }
    }
}

impl WidgetImpl for BrowserPage {}
impl NavigationPageImpl for BrowserPage {}

#[gtk::template_callbacks]
impl BrowserPage {
    #[template_callback]
    fn on_activate(&self, position: u32) {
        if let Some(entry) = self.entries.item(position).and_downcast::<EntryObject>() {
            self.activate_entry(&entry);
        }
    }

    #[template_callback]
    fn on_retry_index(&self) {
        self.repo().ensure_indexed();
    }
}

impl BrowserPage {
    fn repo(&self) -> &RepoObject {
        self.repo.get().expect("set in setup")
    }

    fn snapshot(&self) -> &Snapshot {
        self.snapshot.get().expect("set in setup")
    }

    pub fn setup(&self, repo: RepoObject, snapshot: Snapshot) {
        let obj = self.obj();
        let title = format::snapshot_time_full(&snapshot.time);
        obj.set_title(&title);
        self.window_title.set_title(&title);
        self.window_title.set_subtitle(&format!(
            "{} · {}",
            snapshot.hostname,
            snapshot.paths.join(", ")
        ));

        let handlers = ["indexed", "indexing", "index-fraction", "index-error"]
            .into_iter()
            .map(|property| {
                repo.connect_notify_local(
                    Some(property),
                    clone!(
                        #[weak]
                        obj,
                        move |_, _| obj.imp().update_index_state()
                    ),
                )
            })
            .collect();
        self.repo_handlers.replace(handlers);
        self.repo.set(repo).expect("setup runs once");
        self.snapshot.set(snapshot).expect("setup runs once");

        self.repo().ensure_indexed();
        self.update_index_state();
    }

    /// Shows the index loading state until the index is there, then the first folder.
    fn update_index_state(&self) {
        let repo = self.repo();
        if repo.indexed() {
            if !self.started.replace(true) {
                self.load_start_folder();
            }
            return;
        }
        let error = repo.index_error();
        if !error.is_empty() {
            set_plain_description(&self.index_error_status, &error);
            self.stack.set_visible_child_name("index-error");
            return;
        }
        let fraction = repo.index_fraction();
        self.index_progress.set_visible(fraction >= 0.0);
        self.index_progress.set_fraction(fraction.max(0.0));
        self.stack.set_visible_child_name("index");
    }

    /// Starts in the backed up folder if there is exactly one, which is where the files are.
    ///
    /// Not in a disk image though: the other files of the snapshot, e.g. a
    /// `system.json` next to it, would be out of sight.
    fn load_start_folder(&self) {
        match self.snapshot().paths.as_slice() {
            [single] if !is_disk_image(Path::new(single)) => {
                self.load_dir(PathBuf::from(single), true)
            }
            _ => self.load_dir(PathBuf::from("/"), false),
        }
    }

    fn load_dir(&self, path: PathBuf, fall_back_to_root: bool) {
        let Some(session) = self.repo().session() else {
            return;
        };
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let obj = self.obj();
        // What to go back to if the folder turns out to be a locked volume.
        let previous = self
            .stack
            .visible_child_name()
            .filter(|name| name != "loading")
            .unwrap_or_else(|| "list".into());

        glib::timeout_add_local_once(
            SPINNER_DELAY,
            clone!(
                #[weak]
                obj,
                move || {
                    if obj.imp().generation.get() == generation {
                        obj.imp().stack.set_visible_child_name("loading");
                    }
                }
            ),
        );

        let snapshot = self.snapshot().clone();
        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move {
                let listed = path.clone();
                let (worker, listed_snapshot) = (session.clone(), snapshot.clone());
                let result = run_blocking(move || worker.list_dir(&listed_snapshot, &listed)).await;
                let imp = obj.imp();
                if imp.generation.get() != generation {
                    return;
                }
                // Invalidates the pending spinner.
                imp.generation.set(generation + 1);
                match result {
                    Ok(entries) => {
                        let now = Timestamp::now();
                        let objects: Vec<EntryObject> = entries
                            .into_iter()
                            .map(|e| EntryObject::new(e, now))
                            .collect();
                        imp.entries.splice(0, imp.entries.n_items(), &objects);
                        imp.show_path(&path);
                        if objects.is_empty() {
                            imp.stack.set_visible_child_name("empty");
                        } else {
                            imp.stack.set_visible_child_name("list");
                            imp.column_view
                                .scroll_to(0, None, gtk::ListScrollFlags::FOCUS, None);
                        }
                    }
                    // Keep showing the folder we came from, ask for the key and try again.
                    Err(Error::VolumeLocked(partition)) => {
                        imp.stack.set_visible_child_name(&previous);
                        if VolumeDialog::run(&obj, session, snapshot, &partition).await {
                            imp.load_dir(path, false);
                        }
                    }
                    Err(_) if fall_back_to_root => imp.load_dir(PathBuf::from("/"), false),
                    Err(err) => {
                        imp.entries.remove_all();
                        imp.show_path(&path);
                        set_plain_description(&imp.error_status, &err.to_string());
                        imp.stack.set_visible_child_name("error");
                    }
                }
                imp.update_actions();
            }
        ));
    }

    fn show_path(&self, path: &Path) {
        self.path.replace(path.to_path_buf());
        self.path_bar.set_path(path);
        self.obj()
            .action_set_enabled("browser.go-up", path.parent().is_some());
    }

    fn go_up(&self) {
        let parent = self.path.borrow().parent().map(Path::to_path_buf);
        if let Some(parent) = parent {
            self.load_dir(parent, false);
        }
    }

    fn selected_entries(&self) -> Vec<EntryObject> {
        let selected = self.selection.selection();
        (0..selected.size())
            .filter_map(|i| self.entries.item(selected.nth(i as u32)).and_downcast())
            .collect()
    }

    fn update_actions(&self) {
        let selected = self.selected_entries();
        let openable = matches!(
            selected.as_slice(),
            [entry] if matches!(
                entry.entry().kind,
                EntryKind::Dir
                    | EntryKind::File
                    | EntryKind::DiskImage
                    | EntryKind::Partition
                    | EntryKind::Encrypted
            )
        );
        let restorable = !selected.is_empty()
            && selected
                .iter()
                .all(|e| !matches!(e.entry().kind, EntryKind::Unavailable { .. }));
        let obj = self.obj();
        obj.action_set_enabled("browser.open", openable);
        obj.action_set_enabled("browser.restore", restorable);
        self.selection_label.set_label(&match selected.len() {
            0 => String::new(),
            1 => "1 item selected".to_owned(),
            n => format!("{n} items selected"),
        });
    }

    fn open_selected(&self) {
        if let [entry] = self.selected_entries().as_slice() {
            self.activate_entry(entry);
        }
    }

    fn activate_entry(&self, entry: &EntryObject) {
        let entry = entry.entry();
        match &entry.kind {
            EntryKind::Dir | EntryKind::DiskImage | EntryKind::Partition | EntryKind::Encrypted => {
                self.load_dir(entry.path.clone(), false)
            }
            EntryKind::File => self.open_file(entry.path.clone()),
            EntryKind::Symlink { target } => show_toast(
                &*self.obj(),
                &format!("“{}” is a link to “{}”", entry.name, target.display()),
            ),
            EntryKind::Special => show_toast(
                &*self.obj(),
                &format!("“{}” is a special file and can't be opened", entry.name),
            ),
            EntryKind::Unavailable { reason } => {
                show_toast(&*self.obj(), &format!("“{}”: {reason}", entry.name))
            }
        }
    }

    /// Extracts the file to the runtime folder and opens it with its default app.
    fn open_file(&self, path: PathBuf) {
        let Some(session) = self.repo().session() else {
            return;
        };
        let snapshot = self.snapshot().clone();
        let obj = self.obj();
        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let extracted = run_blocking(move || {
                    session.extract_file(&snapshot, &path, &config::runtime_dir())
                })
                .await;
                let result = match extracted {
                    Ok(file) => gtk::FileLauncher::new(Some(&gio::File::for_path(file)))
                        .launch_future(obj.root().and_downcast_ref::<gtk::Window>())
                        .await
                        .map_err(|err| err.to_string()),
                    Err(err) => Err(err.to_string()),
                };
                if let Err(err) = result {
                    show_toast(&obj, &format!("Could not open “{name}”: {err}"));
                }
            }
        ));
    }

    fn restore_selected(&self) {
        let Some(session) = self.repo().session() else {
            return;
        };
        let items: Vec<PathBuf> = self
            .selected_entries()
            .iter()
            .map(|entry| entry.entry().path.clone())
            .collect();
        if items.is_empty() {
            return;
        }
        let snapshot = self.snapshot().clone();
        let obj = self.obj();
        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move {
                let window = obj.root().and_downcast::<gtk::Window>();
                let chooser = gtk::FileDialog::builder()
                    .title("Restore To")
                    .accept_label("_Restore Here")
                    .modal(true)
                    .build();
                let target = match chooser.select_folder_future(window.as_ref()).await {
                    Ok(folder) => folder.path(),
                    Err(err) if err.matches(gtk::DialogError::Dismissed) => return,
                    Err(err) => {
                        show_toast(&obj, &format!("Could not choose a folder: {err}"));
                        return;
                    }
                };
                let Some(target) = target else {
                    show_toast(&obj, "Only local folders can be restored to");
                    return;
                };

                let request = RestoreRequest { items, target };
                if let Some(summary) =
                    RestoreDialog::run(&obj, session, snapshot, request.clone()).await
                {
                    show_restored_toast(&obj, &request.target, summary.skipped);
                    obj.imp().selection.unselect_all();
                }
            }
        ));
    }
}

fn show_restored_toast(widget: &impl IsA<gtk::Widget>, target: &Path, skipped: u64) {
    let mut title = format!("Restored to “{}”", target.display());
    if skipped > 0 {
        title += &format!(
            ", {} skipped",
            format::count(
                skipped,
                "link or unsupported file",
                "links or unsupported files"
            )
        );
    }
    let toast = adw::Toast::builder()
        .title(glib::markup_escape_text(&title))
        .button_label("Open Folder")
        .build();
    let folder = gio::File::for_path(target);
    let window = widget.root().and_downcast::<gtk::Window>();
    toast.connect_button_clicked(move |_| {
        let launcher = gtk::FileLauncher::new(Some(&folder));
        let window = window.clone();
        glib::spawn_future_local(async move {
            let _ = launcher.launch_future(window.as_ref()).await;
        });
    });
    add_toast(widget, toast);
}
