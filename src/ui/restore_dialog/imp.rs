use std::{cell::RefCell, sync::Arc};

use adw::{prelude::*, subclass::prelude::*};
use gtk::{CompositeTemplate, glib, glib::clone};
use restic_gtk::backend::{ProgressKind, ProgressUpdate, RestoreRequest, Session, Snapshot};

use crate::ui::{format, run_blocking};

#[derive(Debug, Default, CompositeTemplate)]
#[template(resource = "/de/gg_itsystemhaus/ResticGtk/ui/restore-dialog.ui")]
pub struct RestoreDialog {
    #[template_child]
    stack: TemplateChild<gtk::Stack>,
    #[template_child]
    confirm_status: TemplateChild<adw::StatusPage>,
    #[template_child]
    progress_title: TemplateChild<gtk::Label>,
    #[template_child]
    progress_bar: TemplateChild<gtk::ProgressBar>,
    #[template_child]
    progress_label: TemplateChild<gtk::Label>,
    #[template_child]
    error_status: TemplateChild<adw::StatusPage>,

    /// Present until the dialog is closed; sending signals success.
    pub result: RefCell<Option<async_channel::Sender<()>>>,
    /// The restore waiting for confirmation.
    pending: RefCell<Option<(Arc<Session>, Snapshot, RestoreRequest)>>,
}

#[glib::object_subclass]
impl ObjectSubclass for RestoreDialog {
    const NAME: &'static str = "ResticGtkRestoreDialog";
    type Type = super::RestoreDialog;
    type ParentType = adw::Dialog;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
        obj.init_template();
    }
}

#[gtk::template_callbacks]
impl RestoreDialog {
    #[template_callback]
    fn on_cancel(&self) {
        self.obj().close();
    }

    #[template_callback]
    fn on_replace(&self) {
        if let Some((session, snapshot, request)) = self.pending.take() {
            self.restore(session, snapshot, request);
        }
    }
}

impl RestoreDialog {
    fn is_open(&self) -> bool {
        self.result.borrow().is_some()
    }

    /// Checks the target first; asks only if existing files would be replaced.
    pub fn start(&self, session: Arc<Session>, snapshot: Snapshot, request: RestoreRequest) {
        self.stack.set_visible_child_name("checking");
        glib::spawn_future_local(clone!(
            #[weak(rename_to = dialog)]
            self.obj(),
            async move {
                let (previewing, snap, req) = (session.clone(), snapshot.clone(), request.clone());
                let preview = run_blocking(move || previewing.preview_restore(&snap, &req)).await;
                let imp = dialog.imp();
                // Cancelled while checking.
                if !imp.is_open() {
                    return;
                }
                match preview {
                    Ok(preview) if preview.conflicts > 0 => {
                        imp.confirm_status.set_description(Some(&format!(
                            "{} in “{}” will be replaced by the version from the snapshot",
                            format::count(preview.conflicts, "file", "files"),
                            request.target.display()
                        )));
                        imp.pending.replace(Some((session, snapshot, request)));
                        imp.stack.set_visible_child_name("confirm");
                    }
                    Ok(_) => imp.restore(session, snapshot, request),
                    Err(err) => imp.show_error(&err.to_string()),
                }
            }
        ));
    }

    fn restore(&self, session: Arc<Session>, snapshot: Snapshot, request: RestoreRequest) {
        let obj = self.obj();
        obj.set_can_close(false);
        self.progress_title.set_label(&format!(
            "Restoring {} to “{}”",
            format::count(request.items.len() as u64, "item", "items"),
            request.target.display()
        ));
        self.show_progress(None);
        self.stack.set_visible_child_name("progress");

        let (sender, receiver) = async_channel::unbounded::<ProgressUpdate>();
        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move {
                while let Ok(update) = receiver.recv().await {
                    obj.imp().show_progress(Some(&update));
                }
            }
        ));

        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move {
                let result = run_blocking(move || {
                    session.restore(&snapshot, &request, move |update| {
                        let _ = sender.send_blocking(update);
                    })
                })
                .await;
                obj.set_can_close(true);
                let imp = obj.imp();
                match result {
                    Ok(()) => {
                        if let Some(sender) = imp.result.take() {
                            let _ = sender.try_send(());
                        }
                        obj.close();
                    }
                    Err(err) => imp.show_error(&err.to_string()),
                }
            }
        ));
    }

    fn show_progress(&self, update: Option<&ProgressUpdate>) {
        match update {
            Some(update) if update.kind == ProgressKind::Bytes => {
                self.progress_bar
                    .set_fraction(update.fraction().unwrap_or(1.0));
                self.progress_label.set_label(&format!(
                    "{} of {}",
                    format::size(update.position),
                    format::size(update.length)
                ));
            }
            _ => {
                self.progress_bar.set_fraction(0.0);
                self.progress_label.set_label("Preparing…");
            }
        }
    }

    fn show_error(&self, message: &str) {
        self.error_status.set_description(Some(message));
        self.stack.set_visible_child_name("error");
    }
}

impl ObjectImpl for RestoreDialog {}
impl WidgetImpl for RestoreDialog {}
impl AdwDialogImpl for RestoreDialog {}
