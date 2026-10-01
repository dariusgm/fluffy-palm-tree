pub mod fts;
pub mod models;
mod schema;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use duckdb::Connection;

/// Shared DuckDB handle. DuckDB connections are blocking, so all access goes through
/// [`Db::call`], which runs the closure on the blocking thread pool. A single mutex
/// serializes access, which matches DuckDB's single-writer model.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    fts_available: Arc<AtomicBool>,
}

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> anyhow::Result<Self> {
        schema::migrate(&mut conn)?;
        let fts = match conn
            .execute_batch("INSTALL fts; LOAD fts;")
            .map_err(anyhow::Error::from)
            .and_then(|()| fts::rebuild_sync(&conn))
        {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "DuckDB FTS unavailable, text search falls back to ILIKE"
                );
                false
            }
        };
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            fts_available: Arc::new(AtomicBool::new(fts)),
        })
    }

    pub fn fts_available(&self) -> bool {
        self.fts_available.load(Ordering::Relaxed)
    }

    /// Forces substring search instead of BM25 (used to test the fallback path).
    pub fn set_fts_available(&self, available: bool) {
        self.fts_available.store(available, Ordering::Relaxed);
    }

    pub async fn call<F, R>(&self, f: F) -> anyhow::Result<R>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| anyhow::anyhow!("database mutex poisoned"))?;
            f(&mut guard)
        })
        .await
        .context("database task panicked")?
    }
}
