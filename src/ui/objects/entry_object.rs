use gtk::{glib, prelude::*, subclass::prelude::*};
use restic_gtk::backend::{Entry, EntryKind, jiff::Timestamp};

use crate::ui::format;

mod imp {
    use std::cell::{OnceCell, RefCell};

    use super::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::EntryObject)]
    pub struct EntryObject {
        pub entry: OnceCell<Entry>,
        #[property(get)]
        pub name: RefCell<String>,
        #[property(get)]
        pub icon_name: RefCell<String>,
        #[property(get)]
        pub size_label: RefCell<String>,
        #[property(get)]
        pub modified_label: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EntryObject {
        const NAME: &'static str = "ResticGtkEntryObject";
        type Type = super::EntryObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for EntryObject {}
}

glib::wrapper! {
    /// A row of the file browser.
    pub struct EntryObject(ObjectSubclass<imp::EntryObject>);
}

impl EntryObject {
    pub fn new(entry: Entry, now: Timestamp) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.name.replace(entry.name.clone());
        imp.icon_name.replace(
            match entry.kind {
                EntryKind::Dir => "folder-symbolic",
                EntryKind::File => "text-x-generic-symbolic",
                EntryKind::Symlink { .. } => "emblem-symbolic-link-symbolic",
                EntryKind::Special => "application-x-executable-symbolic",
                EntryKind::DiskImage => "drive-harddisk-symbolic",
                EntryKind::Partition => "drive-harddisk-system-symbolic",
                EntryKind::Encrypted => "changes-prevent-symbolic",
                EntryKind::Unavailable { .. } => "action-unavailable-symbolic",
            }
            .into(),
        );
        imp.size_label.replace(match entry.kind {
            EntryKind::File
            | EntryKind::DiskImage
            | EntryKind::Partition
            | EntryKind::Encrypted
            | EntryKind::Unavailable { .. } => format::size(entry.size),
            _ => String::new(),
        });
        imp.modified_label.replace(
            entry
                .mtime
                .map(|t| format::modified(t, now))
                .unwrap_or_default(),
        );
        imp.entry.set(entry).expect("set once");
        obj
    }

    pub fn entry(&self) -> &Entry {
        self.imp().entry.get().expect("EntryObject has an entry")
    }
}
