//! Shared application state handed to every handler via `State<AppState>`.

use std::sync::Arc;

use sqlx::SqlitePool;

use crate::config::Config;
use crate::storage::Storage;
use crate::upstream::UpstreamClient;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub pool: SqlitePool,
    pub storage: Arc<Storage>,
    /// `None` when no upstream registry is configured — proxying is off.
    pub upstream: Option<Arc<UpstreamClient>>,
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        pool: SqlitePool,
        storage: Storage,
        upstream: Option<UpstreamClient>,
    ) -> Self {
        Self {
            config,
            pool,
            storage: Arc::new(storage),
            upstream: upstream.map(Arc::new),
        }
    }
}
