mod application;
mod ui;

use gtk::{gio, glib, prelude::*};

use crate::application::ResticGtkApplication;

fn main() -> glib::ExitCode {
    gio::resources_register_include!("resources.gresource").expect("failed to register gresources");
    ResticGtkApplication::new().run()
}
