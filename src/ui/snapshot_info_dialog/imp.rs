use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, glib};
use restic_gtk::backend::{BackupSummary, Snapshot};

use crate::ui::format;

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/snapshot-info-dialog.ui")]
pub struct SnapshotInfoDialog {
    #[template_child]
    time_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    host_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    paths_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    tags_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    id_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    program_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    backup_group: TemplateChild<adw::PreferencesGroup>,
    #[template_child]
    size_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    files_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    dirs_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    added_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    duration_row: TemplateChild<adw::ActionRow>,
}

#[glib::object_subclass]
impl ObjectSubclass for SnapshotInfoDialog {
    const NAME: &'static str = "ResticGtkSnapshotInfoDialog";
    type Type = super::SnapshotInfoDialog;
    type ParentType = adw::Dialog;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
    }

    fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
        obj.init_template();
    }
}

impl SnapshotInfoDialog {
    pub fn set_snapshot(&self, snapshot: &Snapshot) {
        self.time_row
            .set_subtitle(&format::snapshot_time_full(&snapshot.time));
        let host = if snapshot.username.is_empty() {
            snapshot.hostname.clone()
        } else {
            format!("{}@{}", snapshot.username, snapshot.hostname)
        };
        self.host_row.set_subtitle(&host);
        self.paths_row.set_subtitle(&snapshot.paths.join("\n"));
        self.tags_row.set_visible(!snapshot.tags.is_empty());
        self.tags_row.set_subtitle(&snapshot.tags.join(", "));
        self.id_row.set_subtitle(&snapshot.id);
        self.program_row
            .set_visible(!snapshot.program_version.is_empty());
        self.program_row.set_subtitle(&snapshot.program_version);

        match &snapshot.summary {
            Some(summary) => self.show_summary(summary),
            None => {
                self.backup_group.set_description(Some(
                    "Not recorded, the snapshot was created by restic before 0.17",
                ));
                for row in [
                    &self.size_row,
                    &self.files_row,
                    &self.dirs_row,
                    &self.added_row,
                    &self.duration_row,
                ] {
                    row.set_visible(false);
                }
            }
        }
    }

    fn show_summary(&self, summary: &BackupSummary) {
        self.size_row
            .set_subtitle(&format::size(summary.total_bytes));
        self.files_row.set_subtitle(&format!(
            "{}: {} new, {} changed, {} unchanged",
            format::count(summary.total_files, "file", "files"),
            summary.files_new,
            summary.files_changed,
            summary.files_unmodified
        ));
        self.dirs_row.set_subtitle(&summary.total_dirs.to_string());
        let added = format::size(summary.data_added);
        let stored = format::size(summary.data_added_packed);
        self.added_row.set_subtitle(&if added == stored {
            added
        } else {
            format!("{added}, {stored} compressed")
        });
        self.duration_row
            .set_subtitle(&format::duration(summary.duration()));
    }
}

impl ObjectImpl for SnapshotInfoDialog {}
impl WidgetImpl for SnapshotInfoDialog {}
impl AdwDialogImpl for SnapshotInfoDialog {}
