use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use duckdb::{OptionalExt, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::db::Db;

const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RECENT_ERRORS: usize = 50;
const LIST_LIMIT: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobError {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobView {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub params: Value,
    pub found: u64,
    pub processed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub recent_errors: Vec<JobError>,
}

struct JobInner {
    status: JobStatus,
    finished_at: Option<DateTime<Utc>>,
    error: Option<String>,
    recent_errors: VecDeque<JobError>,
}

/// A running background job. Counters are lock-free so workers can update them cheaply.
pub struct Job {
    pub id: String,
    pub kind: &'static str,
    params: Value,
    started_at: DateTime<Utc>,
    found: AtomicU64,
    processed: AtomicU64,
    failed: AtomicU64,
    skipped: AtomicU64,
    cancel: CancellationToken,
    inner: Mutex<JobInner>,
}

impl Job {
    pub fn inc_found(&self) {
        self.found.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add_found(&self, n: u64) {
        self.found.fetch_add(n, Ordering::Relaxed);
    }
    pub fn inc_processed(&self) {
        self.processed.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a failed item and keeps the most recent errors for the status endpoint.
    pub fn record_failure(&self, path: impl Into<String>, error: &anyhow::Error) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        self.record_error(path, error);
    }

    /// Keeps an error for the status endpoint without counting a failed item.
    pub fn record_error(&self, path: impl Into<String>, error: &anyhow::Error) {
        let path = path.into();
        tracing::warn!(job = %self.id, %path, error = format!("{error:#}"), "job item error");
        let mut inner = self.inner.lock().expect("job mutex");
        if inner.recent_errors.len() == MAX_RECENT_ERRORS {
            inner.recent_errors.pop_front();
        }
        inner.recent_errors.push_back(JobError {
            path,
            error: format!("{error:#}"),
        });
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn view(&self) -> JobView {
        let inner = self.inner.lock().expect("job mutex");
        JobView {
            id: self.id.clone(),
            kind: self.kind.to_string(),
            status: inner.status.as_str().to_string(),
            params: self.params.clone(),
            found: self.found.load(Ordering::Relaxed),
            processed: self.processed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            started_at: self.started_at,
            finished_at: inner.finished_at,
            error: inner.error.clone(),
            recent_errors: inner.recent_errors.iter().cloned().collect(),
        }
    }

    fn finish(&self, status: JobStatus, error: Option<String>) {
        let mut inner = self.inner.lock().expect("job mutex");
        inner.status = status;
        inner.error = error;
        inner.finished_at = Some(Utc::now());
    }
}

/// Tracks live jobs in memory and persists their state to the `jobs` table.
#[derive(Clone)]
pub struct JobRegistry {
    db: Db,
    live: Arc<RwLock<HashMap<String, Arc<Job>>>>,
}

impl JobRegistry {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            live: Arc::default(),
        }
    }

    /// Persists a new job and runs `work` in the background.
    pub async fn start<F, Fut>(
        &self,
        kind: &'static str,
        params: Value,
        work: F,
    ) -> anyhow::Result<Arc<Job>>
    where
        F: FnOnce(Arc<Job>) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let job = Arc::new(Job {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            params,
            started_at: Utc::now(),
            found: AtomicU64::new(0),
            processed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            cancel: CancellationToken::new(),
            inner: Mutex::new(JobInner {
                status: JobStatus::Running,
                finished_at: None,
                error: None,
                recent_errors: VecDeque::new(),
            }),
        });

        let view = job.view();
        self.db
            .call(move |c| {
                c.execute(
                    "INSERT INTO jobs (id, kind, params, status, started_at) VALUES (?, ?, ?, ?, ?)",
                    params![view.id, view.kind, view.params.to_string(), view.status, view.started_at],
                )?;
                Ok(())
            })
            .await?;
        self.live
            .write()
            .expect("jobs lock")
            .insert(job.id.clone(), job.clone());

        let registry = self.clone();
        let running = job.clone();
        tokio::spawn(async move {
            let mut task = tokio::spawn(work(running.clone()));
            let mut tick = tokio::time::interval(FLUSH_INTERVAL);
            let result = loop {
                tokio::select! {
                    r = &mut task => break r,
                    _ = tick.tick() => registry.flush(&running).await,
                }
            };
            match result {
                Ok(Ok(())) if running.is_cancelled() => running.finish(JobStatus::Cancelled, None),
                Ok(Ok(())) => running.finish(JobStatus::Completed, None),
                Ok(Err(e)) => running.finish(JobStatus::Failed, Some(format!("{e:#}"))),
                Err(e) => running.finish(JobStatus::Failed, Some(format!("job panicked: {e}"))),
            }
            registry.flush(&running).await;
            registry
                .live
                .write()
                .expect("jobs lock")
                .remove(&running.id);
            tracing::info!(job = %running.id, kind = running.kind, view = ?running.view(), "job finished");
        });
        Ok(job)
    }

    async fn flush(&self, job: &Job) {
        let v = job.view();
        let res = self
            .db
            .call(move |c| {
                c.execute(
                    r#"UPDATE jobs SET status = ?, found = ?, processed = ?, failed = ?, skipped = ?,
                              finished_at = ?, error = ?, recent_errors = ?
                       WHERE id = ?"#,
                    params![
                        v.status,
                        v.found as i64,
                        v.processed as i64,
                        v.failed as i64,
                        v.skipped as i64,
                        v.finished_at,
                        v.error,
                        serde_json::to_string(&v.recent_errors)?,
                        v.id
                    ],
                )?;
                Ok(())
            })
            .await;
        if let Err(e) = res {
            tracing::error!(job = %job.id, error = %e, "failed to persist job state");
        }
    }

    pub fn cancel(&self, id: &str) -> bool {
        match self.live.read().expect("jobs lock").get(id) {
            Some(job) => {
                job.cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<JobView>> {
        if let Some(job) = self.live.read().expect("jobs lock").get(id) {
            return Ok(Some(job.view()));
        }
        let id = id.to_string();
        self.db
            .call(move |c| {
                Ok(c.query_row(
                    &format!("{SELECT_JOB} WHERE id = ?"),
                    params![id],
                    job_from_row,
                )
                .optional()?)
            })
            .await
    }

    pub async fn list(&self) -> anyhow::Result<Vec<JobView>> {
        let mut jobs = self
            .db
            .call(|c| {
                let mut stmt = c.prepare(&format!(
                    "{SELECT_JOB} ORDER BY started_at DESC LIMIT {LIST_LIMIT}"
                ))?;
                let rows = stmt.query_map([], job_from_row)?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await?;
        let live = self.live.read().expect("jobs lock");
        for j in &mut jobs {
            if let Some(l) = live.get(&j.id) {
                *j = l.view();
            }
        }
        Ok(jobs)
    }

    /// Jobs still marked running at startup were killed with the previous process.
    pub async fn mark_interrupted(&self) -> anyhow::Result<usize> {
        self.db
            .call(|c| {
                Ok(c.execute(
                    "UPDATE jobs SET status = 'interrupted', finished_at = now() WHERE status = 'running'",
                    [],
                )?)
            })
            .await
    }
}

const SELECT_JOB: &str = r#"SELECT id, kind, CAST(params AS VARCHAR), status, found, processed,
    failed, skipped, started_at, finished_at, error, CAST(recent_errors AS VARCHAR) FROM jobs"#;

fn job_from_row(r: &Row<'_>) -> duckdb::Result<JobView> {
    let params: Option<String> = r.get(2)?;
    let errors: Option<String> = r.get(11)?;
    Ok(JobView {
        id: r.get(0)?,
        kind: r.get(1)?,
        params: params
            .and_then(|p| serde_json::from_str(&p).ok())
            .unwrap_or(Value::Null),
        status: r.get(3)?,
        found: r.get::<_, i64>(4)? as u64,
        processed: r.get::<_, i64>(5)? as u64,
        failed: r.get::<_, i64>(6)? as u64,
        skipped: r.get::<_, i64>(7)? as u64,
        started_at: r.get(8)?,
        finished_at: r.get(9)?,
        error: r.get(10)?,
        recent_errors: errors
            .and_then(|e| serde_json::from_str(&e).ok())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_done(reg: &JobRegistry, id: &str) -> JobView {
        for _ in 0..200 {
            let v = reg.get(id).await.unwrap().unwrap();
            if v.status != "running" {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("job did not finish");
    }

    #[tokio::test]
    async fn job_lifecycle_is_persisted() {
        let reg = JobRegistry::new(Db::open_in_memory().unwrap());
        let job = reg
            .start("test", serde_json::json!({"a": 1}), |job| async move {
                job.add_found(3);
                job.inc_processed();
                job.inc_skipped();
                job.record_failure("/x", &anyhow::anyhow!("boom"));
                Ok(())
            })
            .await
            .unwrap();
        let v = wait_done(&reg, &job.id).await;
        assert_eq!(v.status, "completed");
        assert_eq!((v.found, v.processed, v.skipped, v.failed), (3, 1, 1, 1));
        assert_eq!(v.recent_errors[0].error, "boom");
        assert_eq!(v.params["a"], 1);
        assert!(v.finished_at.is_some());
        assert_eq!(reg.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn job_can_be_cancelled_and_errors_fail() {
        let reg = JobRegistry::new(Db::open_in_memory().unwrap());
        let job = reg
            .start("test", Value::Null, |job| async move {
                job.cancel_token().cancelled().await;
                Ok(())
            })
            .await
            .unwrap();
        assert!(reg.cancel(&job.id));
        assert_eq!(wait_done(&reg, &job.id).await.status, "cancelled");

        let job = reg
            .start("test", Value::Null, |_| async { anyhow::bail!("nope") })
            .await
            .unwrap();
        let v = wait_done(&reg, &job.id).await;
        assert_eq!(v.status, "failed");
        assert_eq!(v.error.as_deref(), Some("nope"));
    }
}
