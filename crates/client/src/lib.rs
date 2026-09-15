//! Remote store adapters that let stock jj use a Tandem repository.

use std::sync::Arc;

use jj_lib::repo::StoreFactories;

pub mod backend;
pub mod cache;
pub mod env;
pub mod http_client;
pub mod op_heads_store;
pub mod op_store;
mod pending_files;
pub mod prepared;
pub mod repo_link;
mod sessions;

pub use http_client::TandemClient;

/// Register Tandem's three remote stores with jj.
pub fn tandem_factories() -> StoreFactories {
    let mut factories = StoreFactories::empty();
    let sessions = Arc::new(sessions::RepositorySessions::default());
    let backend_sessions = sessions.clone();
    let op_sessions = sessions.clone();
    factories.add_backend(
        "tandem",
        Box::new(move |_settings, path| {
            Ok(Box::new(backend::TandemBackend::from_client(
                backend_sessions.load(path)?,
            )))
        }),
    );
    factories.add_op_store(
        "tandem_op_store",
        Box::new(move |_settings, path, root| {
            Ok(Box::new(op_store::TandemOpStore::from_client(
                op_sessions.load(path)?,
                root,
            )))
        }),
    );
    factories.add_op_heads_store(
        "tandem_op_heads_store",
        Box::new(move |_settings, path| {
            Ok(Box::new(op_heads_store::TandemOpHeadsStore::from_client(
                sessions.load(path)?,
                path,
            )?))
        }),
    );
    factories
}

pub fn tandem_factories_with_defaults() -> StoreFactories {
    let mut factories = jj_lib::default_backend_factories::default_backend_factories();
    factories.merge(tandem_factories());
    factories
}
