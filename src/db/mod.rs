pub mod fts;
pub mod media;
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
        Self::init(conn, false)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?, true)
    }

    /// With `eager_fts` false the FTS index is not built here (it can take minutes on a
    /// large database); text search uses ILIKE until [`Db::rebuild_fts_in_background`] is done.
    fn init(mut conn: Connection, eager_fts: bool) -> anyhow::Result<Self> {
        schema::migrate(&mut conn)?;
        if !eager_fts {
            return Ok(Self {
                conn: Arc::new(Mutex::new(conn)),
                fts_available: Arc::new(AtomicBool::new(false)),
            });
        }
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

    /// Builds the FTS index on a cloned connection so requests are not blocked on the
    /// connection mutex meanwhile, then enables BM25 search. Failures are logged and
    /// leave the ILIKE fallback active.
    pub fn rebuild_fts_in_background(&self) {
        let db = self.clone();
        tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let result = (|| -> anyhow::Result<()> {
                let worker = db
                    .conn
                    .lock()
                    .map_err(|_| anyhow::anyhow!("database mutex poisoned"))?
                    .try_clone()?;
                worker.execute_batch("INSTALL fts; LOAD fts;")?;
                fts::rebuild_sync(&worker)?;
                db.conn
                    .lock()
                    .map_err(|_| anyhow::anyhow!("database mutex poisoned"))?
                    .execute_batch("LOAD fts;")?;
                Ok(())
            })();
            match result {
                Ok(()) => {
                    db.set_fts_available(true);
                    tracing::info!(
                        ms = started.elapsed().as_millis() as u64,
                        "FTS index ready"
                    );
                }
                Err(e) => tracing::warn!(
                    error = format!("{e:#}"),
                    "DuckDB FTS unavailable, text search falls back to ILIKE"
                ),
            }
        });
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
