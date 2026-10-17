use std::sync::Arc;

use gtk::{glib, glib::clone, prelude::*, subclass::prelude::*};
use restic_gtk::backend::{ProgressUpdate, RepoConfig, Session};

use crate::ui::run_blocking;

mod imp {
    use std::cell::{Cell, RefCell};

    use super::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::RepoObject)]
    pub struct RepoObject {
        #[property(name = "name", get = Self::name, type = String)]
        #[property(name = "display-location", get = Self::display_location, type = String)]
        pub config: RefCell<Option<RepoConfig>>,

        /// The unlocked repository; only in memory, dropped on lock.
        #[property(name = "unlocked", get = Self::unlocked, type = bool)]
        pub session: RefCell<Option<Arc<Session>>>,

        #[property(get, set)]
        indexing: Cell<bool>,
        #[property(get, set)]
        indexed: Cell<bool>,
        /// Fraction of the index loaded, negative while unknown.
        #[property(get, set, minimum = -1.0, maximum = 1.0, default = -1.0)]
        index_fraction: Cell<f64>,
        /// Why loading the index failed, empty otherwise.
        #[property(get, set)]
        index_error: RefCell<String>,
    }

    impl RepoObject {
        fn name(&self) -> String {
            self.config
                .borrow()
                .as_ref()
                .map(|c| c.name.clone())
                .unwrap_or_default()
        }

        fn display_location(&self) -> String {
            self.config
                .borrow()
                .as_ref()
                .map(RepoConfig::display_location)
                .unwrap_or_default()
        }

        fn unlocked(&self) -> bool {
            self.session.borrow().is_some()
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RepoObject {
        const NAME: &'static str = "ResticGtkRepoObject";
        type Type = super::RepoObject;
    }

    #[glib::derived_properties]
    impl ObjectImpl for RepoObject {}
}

glib::wrapper! {
    /// A configured repository and, once unlocked, its session.
    pub struct RepoObject(ObjectSubclass<imp::RepoObject>);
}

impl RepoObject {
    pub fn new(config: RepoConfig) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().config.replace(Some(config));
        obj
    }

    pub fn config(&self) -> RepoConfig {
        self.imp()
            .config
            .borrow()
            .clone()
            .expect("RepoObject has a config")
    }

    /// Replaces the configuration. A changed location locks the repository.
    pub fn set_config(&self, config: RepoConfig) {
        let location_changed = self.config().location != config.location;
        self.imp().config.replace(Some(config));
        self.notify_name();
        self.notify_display_location();
        if location_changed {
            self.lock();
        }
    }

    pub fn session(&self) -> Option<Arc<Session>> {
        self.imp().session.borrow().clone()
    }

    pub fn set_session(&self, session: Session) {
        self.replace_session(Some(Arc::new(session)));
    }

    /// Forgets the session and with it the decrypted key.
    pub fn lock(&self) {
        self.replace_session(None);
    }

    fn replace_session(&self, session: Option<Arc<Session>>) {
        self.imp().session.replace(session);
        self.set_indexing(false);
        self.set_indexed(false);
        self.set_index_fraction(-1.0);
        self.set_index_error(String::new());
        self.notify_unlocked();
    }

    /// Starts loading the index in the background, unless loaded or already loading.
    ///
    /// Progress and outcome are reported through the `index-*` properties.
    pub fn ensure_indexed(&self) {
        let Some(session) = self.session() else {
            return;
        };
        if self.indexed() || self.indexing() {
            return;
        }
        self.set_indexing(true);
        self.set_index_fraction(-1.0);
        self.set_index_error(String::new());

        let (sender, receiver) = async_channel::unbounded::<ProgressUpdate>();
        glib::spawn_future_local(clone!(
            #[weak(rename_to = repo)]
            self,
            async move {
                while let Ok(update) = receiver.recv().await {
                    repo.set_index_fraction(update.fraction().unwrap_or(-1.0));
                }
            }
        ));

        glib::spawn_future_local(clone!(
            #[weak(rename_to = repo)]
            self,
            async move {
                let loading = session.clone();
                let result = run_blocking(move || {
                    loading.load_index(move |update| {
                        let _ = sender.send_blocking(update);
                    })
                })
                .await;

                // Locked or re-unlocked meanwhile: this result belongs to an old session.
                if !repo.session().is_some_and(|s| Arc::ptr_eq(&s, &session)) {
                    return;
                }
                repo.set_indexing(false);
                match result {
                    Ok(()) => repo.set_indexed(true),
                    Err(err) => repo.set_index_error(err.to_string()),
                }
            }
        ));
    }
}
