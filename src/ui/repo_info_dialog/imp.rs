use std::sync::Arc;

use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, glib, glib::clone};
use restic_gtk::backend::{RepoStats, Session};

use crate::ui::{format, objects::RepoObject, run_blocking};

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/repo-info-dialog.ui")]
pub struct RepoInfoDialog {
    #[template_child]
    stack: TemplateChild<gtk::Stack>,
    #[template_child]
    error_status: TemplateChild<adw::StatusPage>,
    #[template_child]
    location_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    id_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    version_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    snapshots_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    stored_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    raw_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    compression_row: TemplateChild<adw::ActionRow>,
    #[template_child]
    packs_row: TemplateChild<adw::ActionRow>,
}

#[glib::object_subclass]
impl ObjectSubclass for RepoInfoDialog {
    const NAME: &'static str = "ResticGtkRepoInfoDialog";
    type Type = super::RepoInfoDialog;
    type ParentType = adw::Dialog;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
    }

    fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
        obj.init_template();
    }
}

impl RepoInfoDialog {
    pub fn load(&self, repo: &RepoObject, session: Arc<Session>) {
        self.obj().set_title(&repo.name());
        self.location_row.set_subtitle(&repo.display_location());
        self.stack.set_visible_child_name("loading");
        glib::spawn_future_local(clone!(
            #[weak(rename_to = dialog)]
            self.obj(),
            async move {
                let result = run_blocking(move || session.repo_stats()).await;
                let imp = dialog.imp();
                match result {
                    Ok(stats) => {
                        imp.show_stats(&stats);
                        imp.stack.set_visible_child_name("stats");
                    }
                    Err(err) => {
                        imp.error_status.set_description(Some(&err.to_string()));
                        imp.stack.set_visible_child_name("error");
                    }
                }
            }
        ));
    }

    fn show_stats(&self, stats: &RepoStats) {
        self.id_row.set_subtitle(&stats.id);
        self.version_row
            .set_subtitle(&format!("Version {}", stats.version));
        self.snapshots_row
            .set_subtitle(&stats.snapshots.to_string());
        self.stored_row
            .set_subtitle(&format::size(stats.stored_size));
        self.raw_row.set_subtitle(&format::size(stats.raw_size));
        self.compression_row.set_subtitle(&compression(stats));
        self.packs_row.set_subtitle(&stats.packs.to_string());
    }
}

fn compression(stats: &RepoStats) -> String {
    if stats.version < 2 {
        return "Not supported by format version 1".into();
    }
    match stats.compression_ratio() {
        Some(ratio) => format!(
            "{ratio:.2}×, saves {}",
            format::size(stats.raw_size.saturating_sub(stats.packed_size))
        ),
        None => "No data yet".into(),
    }
}

impl ObjectImpl for RepoInfoDialog {}
impl WidgetImpl for RepoInfoDialog {}
impl AdwDialogImpl for RepoInfoDialog {}
