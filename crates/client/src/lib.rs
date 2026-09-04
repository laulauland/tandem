//! Remote store adapters that let stock jj use a Tandem repository.

use jj_lib::repo::StoreFactories;

pub mod backend;
pub mod cache;
pub mod env;
pub mod http_client;
pub mod op_heads_store;
pub mod op_store;
mod pending_files;
pub mod repo_link;

pub use http_client::TandemClient;

/// Register Tandem's three remote stores with jj.
pub fn tandem_factories() -> StoreFactories {
    let mut factories = StoreFactories::empty();
    factories.add_backend(
        "tandem",
        Box::new(|settings, path| Ok(Box::new(backend::TandemBackend::load(settings, path)?))),
    );
    factories.add_op_store(
        "tandem_op_store",
        Box::new(|settings, path, root| {
            Ok(Box::new(op_store::TandemOpStore::load(
                settings, path, root,
            )?))
        }),
    );
    factories.add_op_heads_store(
        "tandem_op_heads_store",
        Box::new(|settings, path| {
            Ok(Box::new(op_heads_store::TandemOpHeadsStore::load(
                settings, path,
            )?))
        }),
    );
    factories
}

pub fn tandem_factories_with_defaults() -> StoreFactories {
    let mut factories = StoreFactories::default();
    factories.merge(tandem_factories());
    factories
}
