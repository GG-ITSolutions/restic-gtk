//! A breadcrumb bar for the folder shown in the file browser.

use std::path::{Component, Path, PathBuf};

use adw::{prelude::*, subclass::prelude::*};
use gtk::{glib, glib::clone};

mod imp {
    use std::{cell::RefCell, sync::OnceLock};

    use glib::subclass::Signal;

    use super::*;

    #[derive(Debug, Default)]
    pub struct PathBar {
        pub scrolled_window: gtk::ScrolledWindow,
        pub segments: gtk::Box,
        pub path: RefCell<PathBuf>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PathBar {
        const NAME: &'static str = "ResticGtkPathBar";
        type Type = super::PathBar;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("pathbar");
        }
    }

    impl ObjectImpl for PathBar {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                // Emitted with the number of path components of the clicked segment.
                vec![
                    Signal::builder("navigate")
                        .param_types([u32::static_type()])
                        .build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.segments.set_spacing(2);
            self.scrolled_window
                .set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
            self.scrolled_window.set_child(Some(&self.segments));
            self.obj().set_child(Some(&self.scrolled_window));
        }
    }

    impl WidgetImpl for PathBar {}
    impl BinImpl for PathBar {}
}

glib::wrapper! {
    pub struct PathBar(ObjectSubclass<imp::PathBar>)
        @extends adw::Bin, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl PathBar {
    /// Shows `path` (absolute) as one button per folder, the last one marked current.
    pub fn set_path(&self, path: &Path) {
        let imp = self.imp();
        imp.path.replace(path.to_path_buf());
        while let Some(child) = imp.segments.first_child() {
            imp.segments.remove(&child);
        }

        let components: Vec<_> = path.components().collect();
        for (index, component) in components.iter().enumerate() {
            let button = match component {
                Component::RootDir => gtk::Button::builder()
                    .icon_name("drive-harddisk-symbolic")
                    .tooltip_text("Snapshot Root")
                    .build(),
                component => {
                    if index > 1 {
                        let separator = gtk::Label::new(Some("/"));
                        separator.add_css_class("dimmed");
                        imp.segments.append(&separator);
                    }
                    let label = gtk::Label::builder()
                        .label(component.as_os_str().to_string_lossy())
                        .ellipsize(gtk::pango::EllipsizeMode::Middle)
                        .max_width_chars(24)
                        .build();
                    gtk::Button::builder().child(&label).build()
                }
            };
            button.add_css_class("flat");
            if index + 1 == components.len() {
                button.add_css_class("current");
            }
            let depth = index as u32 + 1;
            button.connect_clicked(clone!(
                #[weak(rename_to = bar)]
                self,
                move |_| bar.emit_by_name::<()>("navigate", &[&depth])
            ));
            imp.segments.append(&button);
        }

        // Keep the current folder in view once the new buttons are allocated.
        glib::idle_add_local_once(clone!(
            #[weak(rename_to = bar)]
            self,
            move || {
                let adjustment = bar.imp().scrolled_window.hadjustment();
                adjustment.set_value(adjustment.upper());
            }
        ));
    }

    pub fn connect_navigate<F: Fn(&Self, PathBuf) + 'static>(&self, f: F) -> glib::SignalHandlerId {
        self.connect_closure(
            "navigate",
            false,
            // Rebuilt from the stored path, so non-UTF-8 names survive.
            glib::closure_local!(move |bar: &Self, depth: u32| {
                let path = bar
                    .imp()
                    .path
                    .borrow()
                    .components()
                    .take(depth as usize)
                    .collect();
                f(bar, path)
            }),
        )
    }
}
