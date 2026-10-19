//! Read-only details of repositories and snapshots, as lists of properties.

use adw::prelude::*;
use gtk::{glib, glib::clone};
use restic_gtk::backend::{RepoStats, Snapshot, SnapshotSummary};

use crate::ui::{format, objects::RepoObject, run_blocking, set_plain_description};

/// Shows a snapshot and the summary of the backup that created it.
pub fn show_snapshot(parent: &impl IsA<gtk::Widget>, snapshot: &Snapshot) {
    let host = if snapshot.username.is_empty() {
        snapshot.hostname.clone()
    } else {
        format!("{}@{}", snapshot.username, snapshot.hostname)
    };
    let page = adw::PreferencesPage::new();
    page.add(&group(
        "",
        &[
            ("Time", format::snapshot_time_full(&snapshot.time)),
            ("Host", host),
            ("Paths", snapshot.paths.join("\n")),
            ("Tags", snapshot.tags.join(", ")),
            ("ID", snapshot.id.clone()),
            ("Created By", snapshot.program_version.clone()),
        ],
    ));
    let backup = match &snapshot.summary {
        Some(summary) => group("Backup", &summary_rows(summary)),
        None => {
            let group = group("Backup", &[]);
            group.set_description(Some(
                "Not recorded, the snapshot was created by restic before 0.17",
            ));
            group
        }
    };
    page.add(&backup);
    present(parent, "Snapshot Details", &page);
}

fn summary_rows(summary: &SnapshotSummary) -> [(&'static str, String); 5] {
    let added = format::size(summary.data_added);
    let stored = format::size(summary.data_added_packed);
    // restic doesn't write rustic's `backup_duration`, so take the difference.
    let duration = summary
        .backup_end
        .duration_since(&summary.backup_start)
        .unsigned_abs();
    [
        ("Size", format::size(summary.total_bytes_processed)),
        (
            "Files",
            format!(
                "{}: {} new, {} changed, {} unchanged",
                format::count(summary.total_files_processed, "file", "files"),
                summary.files_new,
                summary.files_changed,
                summary.files_unmodified
            ),
        ),
        ("Folders", summary.total_dirs_processed.to_string()),
        (
            "Added to Repository",
            if added == stored {
                added
            } else {
                format!("{added}, {stored} compressed")
            },
        ),
        ("Duration", format::duration(duration)),
    ]
}

/// Shows size and format of an unlocked repository, read in the background.
pub fn show_repo(parent: &impl IsA<gtk::Widget>, repo: &RepoObject) {
    let Some(session) = repo.session() else {
        return;
    };
    let loading = status_page(
        "Reading Repository",
        "All repository files are listed and the index is read, which can take a while for large repositories",
    );
    loading.set_paintable(Some(&adw::SpinnerPaintable::new(Some(&loading))));
    let view = present(parent, &repo.name(), &loading);

    let location = repo.display_location();
    glib::spawn_future_local(clone!(
        #[weak]
        view,
        async move {
            let content: gtk::Widget = match run_blocking(move || session.repo_stats()).await {
                Ok(stats) => repo_page(&location, &stats).upcast(),
                Err(err) => {
                    let status = status_page("Could Not Read Repository", &err.to_string());
                    status.set_icon_name(Some("dialog-error-symbolic"));
                    status.upcast()
                }
            };
            view.set_content(Some(&content));
        }
    ));
}

fn repo_page(location: &str, stats: &RepoStats) -> adw::PreferencesPage {
    let compression = if stats.version < 2 {
        "Not supported by format version 1".into()
    } else if stats.packed_size == 0 {
        "No data yet".into()
    } else {
        format!(
            "{:.2}×, saves {}",
            stats.raw_size as f64 / stats.packed_size as f64,
            format::size(stats.raw_size.saturating_sub(stats.packed_size))
        )
    };
    let page = adw::PreferencesPage::new();
    page.add(&group(
        "",
        &[
            ("Location", location.to_owned()),
            ("Repository ID", stats.id.clone()),
            ("Format", format!("Version {}", stats.version)),
            ("Snapshots", stats.snapshots.to_string()),
        ],
    ));
    page.add(&group(
        "Storage",
        &[
            ("Size on Storage", format::size(stats.stored_size)),
            ("Unique Data", format::size(stats.raw_size)),
            ("Compression", compression),
            ("Pack Files", stats.packs.to_string()),
        ],
    ));
    page
}

/// One property row per non-empty value.
fn group(title: &str, rows: &[(&str, String)]) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    for (title, value) in rows.iter().filter(|(_, value)| !value.is_empty()) {
        group.add(
            &adw::ActionRow::builder()
                .title(*title)
                .subtitle(value)
                // Values are plain text, e.g. paths may contain “&”.
                .use_markup(false)
                .subtitle_selectable(true)
                .css_classes(["property"])
                .build(),
        );
    }
    group
}

fn status_page(title: &str, description: &str) -> adw::StatusPage {
    let status = adw::StatusPage::builder()
        .title(title)
        .css_classes(["compact"])
        .build();
    set_plain_description(&status, description);
    status
}

/// Presents a dialog with `content`; returns its view to replace the content later.
fn present(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    content: &impl IsA<gtk::Widget>,
) -> adw::ToolbarView {
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(content));
    adw::Dialog::builder()
        .title(title)
        .content_width(460)
        .content_height(640)
        .child(&view)
        .build()
        .present(Some(parent));
    view
}
