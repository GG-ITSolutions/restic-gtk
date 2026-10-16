mod config;
mod window;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::window::ResticGtkWindow;

fn main() -> glib::ExitCode {
    gio::resources_register_include!("resources.gresource").expect("failed to register gresources");

    let app = adw::Application::builder()
        .application_id(config::APP_ID)
        .resource_base_path("/de/gg_itsystemhaus/ResticGtk")
        .build();

    app.connect_activate(|app| {
        let window = app
            .active_window()
            .unwrap_or_else(|| ResticGtkWindow::new(app).upcast());
        window.present();
    });

    app.run()
}
