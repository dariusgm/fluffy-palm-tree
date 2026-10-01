use std::sync::Arc;

use crate::config::Config;
use crate::db::Db;
use crate::jobs::JobRegistry;
use crate::staging::Staging;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub staging: Staging,
    pub jobs: JobRegistry,
}

impl AppState {
    pub fn new(config: Config, db: Db) -> anyhow::Result<Self> {
        let staging = Staging::new(&config.staging.dir, config.staging.max_bytes)?;
        Ok(Self {
            config: Arc::new(config),
            jobs: JobRegistry::new(db.clone()),
            db,
            staging,
        })
    }
}
