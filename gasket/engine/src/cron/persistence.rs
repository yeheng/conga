//! JSON-file state persistence for cron jobs.
//!
//! Replaces the SQLite `cron_state` table with a single JSON state file
//! (`CronStateFile`).

use chrono::{DateTime, Utc};
use gasket_storage::CronStateFile;
use std::path::PathBuf;
use tracing::{debug, warn};

pub(super) struct CronPersistence {
    path: PathBuf,
}

impl CronPersistence {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub async fn restore_state(
        &self,
        job_id: &str,
    ) -> anyhow::Result<Option<(Option<DateTime<Utc>>, Option<DateTime<Utc>>)>> {
        match CronStateFile::get(&self.path, job_id).await {
            Ok(Some((last_run_str, next_run_str))) => {
                let last_run = last_run_str
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc));
                let next_run = next_run_str
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc));
                debug!("Restored cron state for {} from file", job_id);
                Ok(Some((last_run, next_run)))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                warn!("Failed to load cron state for {}: {}", job_id, e);
                Err(anyhow::anyhow!(
                    "Failed to load cron state for {}: {}",
                    job_id,
                    e
                ))
            }
        }
    }

    pub async fn save_state(
        &self,
        job_id: &str,
        last_run: Option<&DateTime<Utc>>,
        next_run: Option<&DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        CronStateFile::upsert(
            &self.path,
            job_id,
            last_run.map(|t| t.to_rfc3339()).as_deref(),
            next_run.map(|t| t.to_rfc3339()).as_deref(),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_state(&self, job_id: &str) -> anyhow::Result<()> {
        CronStateFile::delete(&self.path, job_id)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("Failed to delete cron state for {}: {}", job_id, e))
    }
}
