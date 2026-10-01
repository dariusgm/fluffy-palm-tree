use std::sync::Arc;

use crate::config::Config;
use crate::db::Db;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
}

impl AppState {
    pub fn new(config: Config, db: Db) -> Self {
        Self {
            config: Arc::new(config),
            db,
        }
    }
}
