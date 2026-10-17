//! GObject wrappers around backend data, for list models and property bindings.

mod entry_object;
mod repo_list;
mod repo_object;

pub use entry_object::EntryObject;
pub use repo_list::RepoList;
pub use repo_object::RepoObject;
