//! Cron service — thin orchestration layer
//!
//! Composes parser, registry, scheduler, and persistence submodules.

use chrono::Utc;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::{debug, info, instrument, warn};

use super::parser;
use super::persistence::CronPersistence;
use super::registry::CronRegistry;
use super::scheduler;
use super::types::{CronJob, RefreshNextRunEntry, RefreshReport};

/// Cron service for scheduled tasks.
///
/// **Hybrid Architecture**:
/// - Config lives in `~/.gasket/cron/*.md` files
/// - Execution state lives in SQLite `cron_state` table
pub struct CronService {
    registry: RwLock<CronRegistry>,
    persistence: CronPersistence,
    workspace: PathBuf,
}

impl CronService {
    /// Create a new cron service.
    pub async fn new(workspace: PathBuf, db: gasket_storage::CronStore) -> Self {
        let registry = RwLock::new(CronRegistry::new());
        let persistence = CronPersistence::new(db);
        let service = Self {
            registry,
            persistence,
            workspace: workspace.clone(),
        };
        service.load_all_jobs(&workspace).await;
        service
    }

    async fn load_all_jobs(&self, workspace: &Path) {
        let cron_dir = workspace.join("cron");
        if !cron_dir.exists() {
            let _ = std::fs::create_dir_all(&cron_dir);
            return;
        }
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(&cron_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("md") {
                    continue;
                }
                match self.parse_and_restore(&path).await {
                    Ok(job) => {
                        info!("Loaded cron job from markdown: {}", job.id);
                        self.registry.write().insert(job);
                        count += 1;
                    }
                    Err(e) => warn!("Failed to load cron job from {:?}: {}", path, e),
                }
            }
        }
        if count > 0 {
            info!("Loaded {} cron jobs from files", count);
        }
    }

    async fn parse_and_restore(&self, path: &Path) -> anyhow::Result<CronJob> {
        let mut job = parser::parse_markdown_file(path)?;
        match self.persistence.restore_state(&job.id).await? {
            Some((last_run, next_run)) => {
                job.last_run = last_run;
                job.next_run = next_run;
                debug!("Restored cron state for {} from database", job.id);
            }
            None => {
                if job.next_run.is_none() {
                    warn!("Cron job {} has invalid schedule, disabling", job.id);
                } else if let Some(next_run) = job.next_run {
                    if let Err(e) = self
                        .persistence
                        .save_state(&job.id, None, Some(&next_run))
                        .await
                    {
                        warn!("Failed to persist initial cron state for {}: {}", job.id, e);
                    }
                }
            }
        }
        Ok(job)
    }

    pub async fn refresh_all_jobs(&self) -> anyhow::Result<RefreshReport> {
        let mut report = RefreshReport::default();
        let (changed, meta_errors) = self.get_changed_files();
        report.errors += meta_errors;
        for (ref path, ref job_id) in changed {
            let is_update = self.registry.read().contains(job_id);
            match self.parse_and_restore(path).await {
                Ok(job) => {
                    self.registry.write().insert(job);
                    if is_update {
                        report.updated += 1;
                    } else {
                        report.loaded += 1;
                    }
                }
                Err(_) => report.errors += 1,
            }
        }
        let current_ids = self.scan_disk_ids();
        let orphaned = self.registry.write().gc_orphaned(&current_ids);
        for id in &orphaned {
            if let Err(e) = self.persistence.delete_state(id).await {
                warn!("Failed to delete cron state for {}: {}", id, e);
            }
        }
        report.removed = orphaned.len();
        Ok(report)
    }

    fn get_changed_files(&self) -> (Vec<(PathBuf, String)>, usize) {
        let cron_dir = self.workspace.join("cron");
        if !cron_dir.exists() {
            return (Vec::new(), 0);
        }
        let mut changed = Vec::new();
        let mut errors = 0;
        if let Ok(entries) = std::fs::read_dir(&cron_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("md") {
                    continue;
                }
                if std::fs::metadata(&path).is_err() {
                    errors += 1;
                    continue;
                };
                let job_id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                changed.push((path, job_id));
            }
        }
        (changed, errors)
    }

    fn scan_disk_ids(&self) -> HashSet<String> {
        let mut ids = HashSet::new();
        let cron_dir = self.workspace.join("cron");
        if let Ok(entries) = std::fs::read_dir(&cron_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    if let Some(id) = path.file_stem().and_then(|s| s.to_str()) {
                        ids.insert(id.to_string());
                    }
                }
            }
        }
        ids
    }

    #[instrument(name = "cron.add_job", skip_all, fields(job_id = %job.id))]
    pub async fn add_job(&self, job: CronJob) -> anyhow::Result<()> {
        let cron_dir = self.workspace.join("cron");
        if !cron_dir.exists() {
            tokio::fs::create_dir_all(&cron_dir).await?;
        }
        let file_path = cron_dir.join(format!("{}.md", job.id));
        let content = parser::serialize_to_markdown(&job)?;
        gasket_storage::fs::atomic_write(&file_path, content).await?;
        let job_id = job.id.clone();
        let next_run = job.next_run;
        self.registry.write().insert(job);
        if let Some(next_run) = next_run {
            if let Err(e) = self
                .persistence
                .save_state(&job_id, None, Some(&next_run))
                .await
            {
                warn!("Failed to persist initial cron state for {}: {}", job_id, e);
            }
        }
        info!("Added cron job: {} ({})", job_id, job_id);
        Ok(())
    }

    #[instrument(name = "cron.remove_job", skip(self), fields(job_id = %id))]
    pub async fn remove_job(&self, id: &str) -> anyhow::Result<bool> {
        if let Err(e) = self.persistence.delete_state(id).await {
            warn!("Failed to delete cron state for {}: {}", id, e);
        }
        let file_path = self.workspace.join("cron").join(format!("{}.md", id));
        if !file_path.exists() {
            return Ok(self.registry.write().remove(id).is_some());
        }
        self.registry.write().remove(id);
        tokio::fs::remove_file(&file_path).await?;
        info!("Removed cron job: {}", id);
        Ok(true)
    }

    pub fn get_job(&self, id: &str) -> Option<CronJob> {
        self.registry.read().get(id).cloned()
    }
    pub fn list_jobs(&self) -> Vec<CronJob> {
        self.registry.read().list()
    }
    pub fn get_due_jobs(&self) -> Vec<CronJob> {
        self.registry.read().get_due(Utc::now())
    }

    pub fn has_missed_ticks(&self, job_id: &str) -> anyhow::Result<bool> {
        let reg = self.registry.read();
        let job = reg
            .get(job_id)
            .ok_or_else(|| anyhow::anyhow!("Job not found: {}", job_id))?;
        Ok(scheduler::has_missed_ticks(job))
    }

    #[instrument(name = "cron.advance_tick", skip_all, fields(job_id = %job_id))]
    pub async fn advance_job_tick(
        &self,
        job_id: &str,
    ) -> anyhow::Result<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
        let now = Utc::now();
        let next_run = {
            let mut reg = self.registry.write();
            let job = reg
                .get_mut(job_id)
                .ok_or_else(|| anyhow::anyhow!("Job not found: {}", job_id))?;
            job.last_run = Some(now);
            let schedule = job
                .schedule
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Job {} has no valid schedule", job_id))?;
            let next = schedule.after(&now).next().ok_or_else(|| {
                anyhow::anyhow!("Failed to calculate next run for job {}", job_id)
            })?;
            job.next_run = Some(next);
            next
        };
        self.persistence
            .save_state(job_id, Some(&now), Some(&next_run))
            .await?;
        debug!(
            "Advanced job {} tick: last_run={}, next_run={}",
            job_id, now, next_run
        );
        Ok((now, next_run))
    }

    #[instrument(name = "cron.refresh_next_run", skip_all)]
    pub async fn refresh_next_run(
        &self,
        job_id: Option<&str>,
    ) -> anyhow::Result<Vec<RefreshNextRunEntry>> {
        let mut results = Vec::new();
        if let Some(id) = job_id {
            let (name, last_run, next_run) = {
                let mut reg = self.registry.write();
                let job = reg
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("Job not found: {}", id))?;
                job.update_next_run();
                (job.name.clone(), job.last_run, job.next_run)
            };
            if let Some(nr) = &next_run {
                self.persistence
                    .save_state(id, last_run.as_ref(), Some(nr))
                    .await?;
            }
            results.push((id.to_string(), name, next_run));
        } else {
            let job_ids = self.registry.read().ids();
            for id in job_ids {
                let (name, last_run, next_run) = {
                    let mut reg = self.registry.write();
                    match reg.get_mut(&id) {
                        Some(job) => {
                            job.update_next_run();
                            (job.name.clone(), job.last_run, job.next_run)
                        }
                        None => continue,
                    }
                };
                if let Some(nr) = &next_run {
                    if let Err(e) = self
                        .persistence
                        .save_state(&id, last_run.as_ref(), Some(nr))
                        .await
                    {
                        warn!("Failed to persist refreshed next_run for {}: {}", id, e);
                    }
                }
                results.push((id, name, next_run));
            }
        }
        Ok(results)
    }

    pub async fn ensure_system_cron_jobs(&self) {
        let system_jobs = [
            (
                "system-cron-refresh",
                "Cron Reload",
                "0 0 * * * * *",
                Some("cron".to_string()),
                Some(serde_json::json!({"action": "refresh"})),
            ),
            (
                "system-evolution",
                "Evolution",
                "0 0 * * * * *",
                Some("evolution".to_string()),
                Some(serde_json::json!({"threshold": 20})),
            ),
        ];
        for (id, name, cron_expr, tool, tool_args) in &system_jobs {
            if self.registry.read().contains(id) {
                continue;
            }
            let mut job = CronJob::new(*id, *name, *cron_expr, "system maintenance");
            job.tool = tool.clone();
            job.tool_args = tool_args.clone();
            match self.add_job(job).await {
                Ok(()) => info!("Created system cron job: {} ({})", name, id),
                Err(e) => warn!("Failed to create system cron job '{}': {}", id, e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_extract_frontmatter_raw_basic() {
        let content = "---\nname: Test Job\ncron: \"0 9 * * *\"\n---\n\nHello World";
        let (yaml, body) = super::parser::extract_frontmatter_raw(content).unwrap();
        assert!(yaml.contains("name: Test Job"));
        assert_eq!(body, "Hello World");
    }
    #[test]
    fn test_extract_frontmatter_raw_with_crlf() {
        let content = "---\r\nname: Test\r\ncron: \"0 9 * * *\"\r\n---\r\n\r\nBody content";
        let (yaml, body) = super::parser::extract_frontmatter_raw(content).unwrap();
        assert!(yaml.contains("name: Test"));
        assert_eq!(body, "Body content");
    }
    #[test]
    fn test_extract_frontmatter_raw_with_code_block() {
        let content = "---\nname: Code Job\ncron: \"*/5 * * * *\"\n---\n\nSome code:\n```\n---\n```\n\nMore content";
        let (yaml, body) = super::parser::extract_frontmatter_raw(content).unwrap();
        assert!(yaml.contains("name: Code Job"));
        assert!(body.contains("---"));
    }
    #[test]
    fn test_extract_frontmatter_raw_missing_start() {
        assert!(super::parser::extract_frontmatter_raw("No frontmatter here").is_err());
    }
    #[test]
    fn test_extract_frontmatter_raw_missing_end() {
        assert!(
            super::parser::extract_frontmatter_raw("---\nname: Test\nNo end delimiter").is_err()
        );
    }
    #[test]
    fn test_parse_markdown_complete() {
        let content = "---\nname: My Job\ncron: \"0 0 9 * * Mon *\"\nchannel: telegram\nto: \"12345\"\nenabled: true\n---\n\nSend daily report";
        let path = Path::new("/tmp/test-job.md");
        let job = parser::parse_markdown(content, path).unwrap();
        assert_eq!(job.name, "My Job");
        assert_eq!(job.cron, "0 0 9 * * Mon *");
        assert_eq!(job.channel, Some("telegram".to_string()));
        assert!(job.enabled);
        assert_eq!(job.message, "Send daily report");
    }
    #[test]
    fn test_cron_job_parse_schedule() {
        let (schedule, next_run) = CronJob::parse_schedule("0 * * * * * *");
        assert!(schedule.is_some());
        assert!(next_run.is_some());
    }
    #[test]
    fn test_cron_job_invalid_cron() {
        let job = CronJob::new("test", "Test", "invalid cron", "Message");
        assert!(job.next_run.is_none());
        assert!(job.schedule.is_none());
    }
}
