use std::fs;

use adw::subclass::prelude::*;
use gtk::{gio, glib, prelude::*};
use restic_gtk::config;

use crate::ui::ResticGtkWindow;

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct ResticGtkApplication;

    #[glib::object_subclass]
    impl ObjectSubclass for ResticGtkApplication {
        const NAME: &'static str = "ResticGtkApplication";
        type Type = super::ResticGtkApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for ResticGtkApplication {}

    impl ApplicationImpl for ResticGtkApplication {
        fn startup(&self) {
            self.parent_startup();
            let app = self.obj();
            // Left over if the last run crashed; the app is single-instance, so nothing uses it.
            let _ = fs::remove_dir_all(config::runtime_dir());

            // Closing the windows (instead of quitting directly) lets a running restore ask first.
            let quit = gio::ActionEntry::builder("quit")
                .activate(|app: &super::ResticGtkApplication, _, _| {
                    for window in app.windows() {
                        window.close();
                    }
                })
                .build();
            app.add_action_entries([quit]);
            app.set_accels_for_action("app.quit", &["<Control>q"]);
            app.set_accels_for_action("window.close", &["<Control>w"]);
            app.set_accels_for_action("win.add-repository", &["<Control>n"]);
        }

        fn activate(&self) {
            self.parent_activate();
            let app = self.obj();
            let window = app
                .active_window()
                .unwrap_or_else(|| ResticGtkWindow::new(app.upcast_ref()).upcast());
            window.present();
        }

        fn shutdown(&self) {
            // Files extracted for viewing must not outlive the session.
            let _ = fs::remove_dir_all(config::runtime_dir());
            self.parent_shutdown();
        }
    }

    impl GtkApplicationImpl for ResticGtkApplication {}
    impl AdwApplicationImpl for ResticGtkApplication {}
}

glib::wrapper! {
    pub struct ResticGtkApplication(ObjectSubclass<imp::ResticGtkApplication>)
        @extends adw::Application, gtk::Application, gio::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl ResticGtkApplication {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("application-id", config::APP_ID)
            .property("resource-base-path", "/de/gg_itsystemhaus/ResticGtk")
            .build()
    }
}
