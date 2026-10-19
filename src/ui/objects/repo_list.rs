use gtk::{gio, prelude::*};
use restic_gtk::backend::{Error, RepoConfig, RepoStore, Result};

use super::RepoObject;

/// The configured repositories: a list model for the UI, persisted on every change.
#[derive(Debug, Clone)]
pub struct RepoList {
    store: RepoStore,
    model: gio::ListStore,
}

impl RepoList {
    /// Loads the list, starting empty on errors; see [`RepoStore::load`] for what happens to the
    /// file. The error is returned for display.
    pub fn load(mut store: RepoStore) -> (Self, Option<Error>) {
        let model = gio::ListStore::new::<RepoObject>();
        let error = match store.load() {
            Ok(configs) => {
                for config in configs {
                    model.append(&RepoObject::new(config));
                }
                None
            }
            Err(err) => Some(err),
        };
        (Self { store, model }, error)
    }

    pub fn model(&self) -> &gio::ListStore {
        &self.model
    }

    pub fn get(&self, position: u32) -> Option<RepoObject> {
        self.model.item(position).and_downcast()
    }

    fn configs(&self) -> Vec<RepoConfig> {
        self.model
            .iter::<RepoObject>()
            .map(|repo| repo.expect("list unchanged while iterating").config())
            .collect()
    }

    pub fn add(&self, config: RepoConfig) -> Result<RepoObject> {
        let mut configs = self.configs();
        configs.push(config.clone());
        self.store.save(&configs)?;
        let repo = RepoObject::new(config);
        self.model.append(&repo);
        Ok(repo)
    }

    pub fn update(&self, repo: &RepoObject, config: RepoConfig) -> Result<()> {
        let configs: Vec<_> = self
            .configs()
            .into_iter()
            .map(|c| if c.id == config.id { config.clone() } else { c })
            .collect();
        self.store.save(&configs)?;
        repo.set_config(config);
        Ok(())
    }

    pub fn remove(&self, repo: &RepoObject) -> Result<()> {
        let id = repo.config().id;
        let configs: Vec<_> = self.configs().into_iter().filter(|c| c.id != id).collect();
        self.store.save(&configs)?;
        repo.lock();
        if let Some(position) = self.model.find(repo) {
            self.model.remove(position);
        }
        Ok(())
    }
}
